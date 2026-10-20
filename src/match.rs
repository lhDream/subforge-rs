//! 分组匹配求值（纯函数，AGENTS.md §3.4）。
//!
//! `Group.members = manual_groups ⊕ manual_nodes ⊕ auto_matched`
//!
//! 本模块不碰网络与文件，给定「节点池 + 分组定义 + 地区表」即可计算结果。

use crate::region::match_alias;
use crate::store::model::{Group, MatchMode};
use anyhow::{Context, Result};
use regex::Regex;
use std::collections::{BTreeMap, HashSet};

/// 参与匹配的节点视图。
#[derive(Debug, Clone)]
pub struct NodeInfo {
    pub name: String,
    /// 由 `region::decide_region` 判定的地区集合。
    pub regions: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct GroupEval {
    /// 最终成员列表（已去重、已排序）。
    pub members: Vec<String>,
    /// 仅来自自动匹配的成员。
    pub auto_matched: Vec<String>,
    /// 入组原因：节点名 → 原因列表（预览页展示，AGENTS.md §5）。
    pub reasons: BTreeMap<String, Vec<String>>,
}

/// 关键词/排除项模式：`re/` 前缀为正则，否则为不区分大小写子串。
enum Pattern {
    Substr(String),
    Regex(Regex),
}

fn compile_patterns(items: &[String]) -> Result<Vec<Pattern>> {
    let mut out = Vec::new();
    for raw in items {
        let item = raw.trim();
        if item.is_empty() {
            continue;
        }
        if let Some(re) = item.strip_prefix("re/") {
            let compiled = Regex::new(&format!("(?i){re}"))
                .with_context(|| format!("正则表达式无效：{re}"))?;
            out.push(Pattern::Regex(compiled));
        } else {
            out.push(Pattern::Substr(item.to_lowercase()));
        }
    }
    Ok(out)
}

impl Pattern {
    fn is_match(&self, name: &str) -> bool {
        match self {
            Pattern::Substr(s) => name.to_lowercase().contains(s),
            Pattern::Regex(r) => r.is_match(name),
        }
    }
}

/// 求值一个分组的成员列表。
///
/// `covered` 仅对**兜底分组**（`match.catch_all`）有意义，见 [`evaluate_group_with_covered`]。
pub fn evaluate_group(group: &Group, nodes: &[NodeInfo]) -> Result<GroupEval> {
    evaluate_group_with_covered(group, nodes, None)
}

/// 求值一个分组，并支持兜底语义。
///
/// `covered` 是「已被本方案其他分组收录的节点名」集合：
///
/// - `group` 不是兜底分组时，该参数被忽略，传 `None` 即可。
/// - `group` 是兜底分组（`match.catch_all`）时，它**取代** `all`/`mode`/`regions`/`keywords`，
///   自动成员 = 节点池中不在 `covered` 里的那些（仍受 `exclude` 约束）。
///
/// 兜底分组必须在其他分组**求值之后**再算，`covered` 才完整。
pub fn evaluate_group_with_covered(
    group: &Group,
    nodes: &[NodeInfo],
    covered: Option<&HashSet<String>>,
) -> Result<GroupEval> {
    let m = &group.r#match;
    let keywords = compile_patterns(&m.keywords)?;
    let excludes = compile_patterns(&m.exclude)?;

    let mut eval = GroupEval::default();

    // ---- 自动匹配
    if m.catch_all || m.all || !m.regions.is_empty() || !keywords.is_empty() {
        for node in nodes {
            // exclude 优先级高于一切（包括 all / catch_all）。
            if is_excluded(node, &m.exclude, &excludes) {
                continue;
            }

            let mut why: Vec<String> = Vec::new();

            if m.catch_all {
                // 兜底：只收别的分组不要的节点。
                if covered.is_some_and(|c| c.contains(&node.name)) {
                    continue;
                }
                why.push("未被其他分组收录".to_string());
            } else if m.all {
                why.push("match.all".to_string());
            } else {
                let region_hit: Vec<String> = m
                    .regions
                    .iter()
                    .filter(|r| node.regions.iter().any(|nr| nr == *r))
                    .cloned()
                    .collect();
                let kw_hit: Vec<String> = m
                    .keywords
                    .iter()
                    .zip(keywords.iter())
                    .filter(|(_, p)| p.is_match(&node.name))
                    .map(|(raw, _)| raw.clone())
                    .collect();

                let regions_empty = m.regions.is_empty();
                let keywords_empty = m.keywords.is_empty();

                let hit = match m.mode {
                    MatchMode::Any => !region_hit.is_empty() || !kw_hit.is_empty(),
                    // all 模式下，空的一维视为「不约束」，避免只写 regions 时永远不命中。
                    MatchMode::All => {
                        (regions_empty || !region_hit.is_empty())
                            && (keywords_empty || !kw_hit.is_empty())
                    }
                };
                if !hit {
                    continue;
                }
                for r in region_hit {
                    why.push(format!("地区：{r}"));
                }
                for k in kw_hit {
                    why.push(format!("关键词：{k}"));
                }
            }

            eval.auto_matched.push(node.name.clone());
            eval.reasons
                .entry(node.name.clone())
                .or_default()
                .extend(why);
        }
    }

    // ---- 拼装：手动子分组 → 手动节点 → 自动匹配
    let mut members: Vec<String> = Vec::new();
    for g in &group.manual_groups {
        if !members.contains(g) {
            members.push(g.clone());
        }
        eval.reasons
            .entry(g.clone())
            .or_default()
            .push("手动子分组".to_string());
    }
    for n in &group.manual_nodes {
        if !members.contains(n) {
            members.push(n.clone());
        }
        eval.reasons
            .entry(n.clone())
            .or_default()
            .push("手动节点".to_string());
    }
    for n in &eval.auto_matched {
        if !members.contains(n) {
            members.push(n.clone());
        }
    }

    eval.members = members;
    Ok(eval)
}

fn is_excluded(node: &NodeInfo, raw_excludes: &[String], patterns: &[Pattern]) -> bool {
    for raw in raw_excludes {
        let item = raw.trim();
        if item.is_empty() {
            continue;
        }
        // 排除项既可以是地区名，也可以是关键词/正则。
        if node.regions.iter().any(|r| r == item) {
            return true;
        }
        if match_alias(&node.name, item) {
            return true;
        }
    }
    patterns.iter().any(|p| p.is_match(&node.name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::model::{GroupMatch, Mapping, Yaml};

    fn group(name: &str, m: GroupMatch) -> Group {
        Group {
            name: name.to_string(),
            clash: Mapping::new(),
            r#match: m,
            manual_groups: vec![],
            manual_nodes: vec![],
            fallback_group: None,
        }
    }

    fn node(name: &str, regions: &[&str]) -> NodeInfo {
        NodeInfo {
            name: name.to_string(),
            regions: regions.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn any_mode_broadens_scope() {
        // 这正是文档警告的直觉偏差：regions 与 keywords 是「或」关系。
        let g = group(
            "美国节点",
            GroupMatch {
                regions: vec!["美国".into()],
                keywords: vec!["GPT".into()],
                ..Default::default()
            },
        );
        let nodes = vec![
            node("🇺🇸 美国 01", &["美国"]),
            node("🇯🇵 日本 GPT", &["日本"]),
            node("🇯🇵 日本 02", &["日本"]),
        ];
        let ev = evaluate_group(&g, &nodes).unwrap();
        assert_eq!(ev.members, vec!["🇺🇸 美国 01", "🇯🇵 日本 GPT"]);
    }

    #[test]
    fn all_mode_narrows_scope() {
        let g = group(
            "美国节点",
            GroupMatch {
                mode: MatchMode::All,
                regions: vec!["美国".into()],
                keywords: vec!["GPT".into()],
                ..Default::default()
            },
        );
        let nodes = vec![
            node("🇺🇸 美国 01", &["美国"]),
            node("🇯🇵 日本 GPT", &["日本"]),
            node("🇺🇸 美国 GPT", &["美国"]),
        ];
        let ev = evaluate_group(&g, &nodes).unwrap();
        assert_eq!(ev.members, vec!["🇺🇸 美国 GPT"]);
    }

    #[test]
    fn all_mode_ignores_empty_dimension() {
        let g = group(
            "美国节点",
            GroupMatch {
                mode: MatchMode::All,
                regions: vec!["美国".into()],
                keywords: vec![],
                ..Default::default()
            },
        );
        let nodes = vec![node("🇺🇸 美国 01", &["美国"]), node("🇯🇵 日本 01", &["日本"])];
        let ev = evaluate_group(&g, &nodes).unwrap();
        assert_eq!(ev.members, vec!["🇺🇸 美国 01"]);
    }

    #[test]
    fn exclude_overrides_all() {
        let g = group(
            "全部",
            GroupMatch {
                all: true,
                exclude: vec!["香港".into()],
                ..Default::default()
            },
        );
        let nodes = vec![node("🇭🇰 香港 01", &["香港"]), node("🇯🇵 日本 01", &["日本"])];
        let ev = evaluate_group(&g, &nodes).unwrap();
        assert_eq!(ev.members, vec!["🇯🇵 日本 01"]);
    }

    #[test]
    fn members_order_and_dedup() {
        let mut g = group(
            "PROXY",
            GroupMatch {
                keywords: vec!["日本".into()],
                ..Default::default()
            },
        );
        g.manual_groups = vec!["自动选择".into()];
        g.manual_nodes = vec!["🇯🇵 日本 01".into()]; // 与自动匹配重复
        let nodes = vec![node("🇯🇵 日本 01", &["日本"]), node("🇯🇵 日本 02", &["日本"])];
        let ev = evaluate_group(&g, &nodes).unwrap();
        assert_eq!(ev.members, vec!["自动选择", "🇯🇵 日本 01", "🇯🇵 日本 02"]);
    }

    #[test]
    fn catch_all_collects_only_uncovered_nodes() {
        let g = group(
            "漏网之鱼",
            GroupMatch {
                catch_all: true,
                ..Default::default()
            },
        );
        let nodes = vec![
            node("🇺🇸 美国 01", &["美国"]),
            node("🇯🇵 日本 01", &["日本"]),
            node("🇸🇬 新加坡 01", &["新加坡"]),
        ];
        let mut covered = HashSet::new();
        covered.insert("🇺🇸 美国 01".to_string());

        let ev = evaluate_group_with_covered(&g, &nodes, Some(&covered)).unwrap();
        assert_eq!(ev.members, vec!["🇯🇵 日本 01", "🇸🇬 新加坡 01"]);
        assert_eq!(
            ev.reasons.get("🇯🇵 日本 01").unwrap(),
            &vec!["未被其他分组收录".to_string()]
        );
    }

    #[test]
    fn catch_all_supersedes_region_conditions() {
        let g = group(
            "漏网之鱼",
            GroupMatch {
                catch_all: true,
                regions: vec!["美国".into()],
                keywords: vec!["GPT".into()],
                ..Default::default()
            },
        );
        let nodes = vec![node("🇺🇸 美国 01", &["美国"]), node("🇯🇵 日本 01", &["日本"])];
        let ev = evaluate_group_with_covered(&g, &nodes, Some(&HashSet::new())).unwrap();
        assert_eq!(ev.members.len(), 2, "catch_all 应取代 regions/keywords");
    }

    #[test]
    fn catch_all_still_respects_exclude() {
        let g = group(
            "漏网之鱼",
            GroupMatch {
                catch_all: true,
                exclude: vec!["香港".into()],
                ..Default::default()
            },
        );
        let nodes = vec![node("🇭🇰 香港 01", &["香港"]), node("🇯🇵 日本 01", &["日本"])];
        let ev = evaluate_group_with_covered(&g, &nodes, Some(&HashSet::new())).unwrap();
        assert_eq!(ev.members, vec!["🇯🇵 日本 01"]);
    }

    #[test]
    fn catch_all_keeps_manual_items() {
        let mut g = group(
            "漏网之鱼",
            GroupMatch {
                catch_all: true,
                ..Default::default()
            },
        );
        g.manual_groups = vec!["自动选择".into()];
        let ev = evaluate_group_with_covered(&g, &[], Some(&HashSet::new())).unwrap();
        assert_eq!(ev.members, vec!["自动选择"]);
    }

    #[test]
    fn catch_all_without_coverage_includes_everything() {
        let g = group(
            "漏网之鱼",
            GroupMatch {
                catch_all: true,
                ..Default::default()
            },
        );
        let nodes = vec![node("a", &[]), node("b", &[])];
        let ev = evaluate_group_with_covered(&g, &nodes, Some(&HashSet::new())).unwrap();
        assert_eq!(ev.members, vec!["a", "b"]);
    }

    #[test]
    fn empty_match_yields_no_auto_members() {
        let g = group("空", GroupMatch::default());
        let nodes = vec![node("🇯🇵 日本 01", &["日本"])];
        let ev = evaluate_group(&g, &nodes).unwrap();
        assert!(ev.members.is_empty());
    }

    #[test]
    fn regex_keyword_works() {
        let g = group(
            "正则",
            GroupMatch {
                keywords: vec!["re/^🇯🇵".into()],
                ..Default::default()
            },
        );
        let nodes = vec![node("🇯🇵 日本 01", &["日本"]), node("🇺🇸 美国 01", &["美国"])];
        let ev = evaluate_group(&g, &nodes).unwrap();
        assert_eq!(ev.members, vec!["🇯🇵 日本 01"]);
    }

    #[test]
    fn reasons_are_recorded() {
        let g = group(
            "美国节点",
            GroupMatch {
                regions: vec!["美国".into()],
                ..Default::default()
            },
        );
        let nodes = vec![node("🇺🇸 美国 01", &["美国"])];
        let ev = evaluate_group(&g, &nodes).unwrap();
        assert_eq!(
            ev.reasons.get("🇺🇸 美国 01").unwrap(),
            &vec!["地区：美国".to_string()]
        );
    }

    #[test]
    fn invalid_regex_reports_error() {
        let g = group(
            "坏正则",
            GroupMatch {
                keywords: vec!["re/[".into()],
                ..Default::default()
            },
        );
        assert!(evaluate_group(&g, &[]).is_err());
    }

    #[test]
    fn clash_fields_passthrough_into_group() {
        let mut clash = Mapping::new();
        clash.insert(Yaml::String("type".into()), Yaml::String("url-test".into()));
        clash.insert(Yaml::String("interval".into()), Yaml::Number(300.into()));
        let g = Group {
            name: "自动选择".into(),
            clash,
            r#match: GroupMatch::default(),
            manual_groups: vec![],
            manual_nodes: vec![],
            fallback_group: None,
        };
        let out = g.to_clash(&["节点A".to_string()]);
        assert_eq!(out.get("type").unwrap().as_str(), Some("url-test"));
        assert_eq!(out.get("interval").unwrap().as_i64(), Some(300));
        assert_eq!(out.get("proxies").unwrap().as_sequence().unwrap().len(), 1);
        assert_eq!(out.get("name").unwrap().as_str(), Some("自动选择"));
    }
}
