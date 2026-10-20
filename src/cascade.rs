//! 引用完整性：级联重命名/删除与依赖闭包（AGENTS.md §3.3）。
//!
//! 总原则：**检查所有直接与间接依赖，确认修改后的完整状态有效，再提交。**
//! 不允许静默断链，也不允许「清理掉引用」了事。

use crate::rules::is_builtin_policy;
use crate::store::model::{Group, GroupMatch, Mapping, Store};
use anyhow::{Result, bail};
use std::collections::{BTreeSet, HashMap, HashSet};

/// 新建或更新一个分组。
///
/// `original` 为空（或指向已不存在的分组）表示新建；否则为编辑，名称变化时级联重命名。
///
/// 抽成独立函数是为了让「新建时确实写入了分组」这件事可被单测覆盖——这个分支曾经
/// 只做了重名检查却忘记 push。
#[allow(clippy::too_many_arguments)]
pub fn upsert_group(
    store: &mut Store,
    original: &str,
    name: &str,
    clash: Mapping,
    gmatch: GroupMatch,
    manual_groups: Vec<String>,
    manual_nodes: Vec<String>,
    fallback_group: Option<String>,
) -> Result<()> {
    let target_name = name.trim().to_string();
    if target_name.is_empty() {
        bail!("分组名不能为空");
    }
    if is_builtin_policy(&target_name) {
        bail!("分组名「{target_name}」与内置策略重名（DIRECT/REJECT/PASS/GLOBAL）");
    }
    let editing = !original.is_empty() && store.group(original).is_some();

    if editing {
        if original != target_name {
            if store.group(&target_name).is_some() {
                bail!("分组「{target_name}」已存在");
            }
            rename_group(store, original, &target_name)?;
        }
    } else {
        if store.group(&target_name).is_some() {
            bail!("分组「{target_name}」已存在");
        }
        store.groups.push(Group {
            name: target_name.clone(),
            clash: Mapping::new(),
            r#match: GroupMatch::default(),
            manual_groups: Vec::new(),
            manual_nodes: Vec::new(),
            fallback_group: None,
        });
    }

    let g = store
        .groups
        .iter_mut()
        .find(|g| g.name == target_name)
        .ok_or_else(|| anyhow::anyhow!("分组「{target_name}」写入失败"))?;
    g.clash = clash;
    g.r#match = gmatch;
    g.manual_groups = manual_groups;
    g.manual_nodes = manual_nodes;
    g.fallback_group = fallback_group;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    /// 「分组」「规则」「方案」
    pub kind: &'static str,
    /// 引用方名称。
    pub owner: String,
    /// 引用发生的字段。
    pub field: String,
}

/// 在库内顺序中上移/下移一个分组。
///
/// 只调整 `store.groups` 的数组顺序；导出时仍会先做拓扑排序，
/// 因此该顺序只影响「同层分组」的相对顺序与页面展示。
pub fn move_group(store: &mut Store, name: &str, direction: &str) -> Result<()> {
    let index = store
        .groups
        .iter()
        .position(|g| g.name == name)
        .ok_or_else(|| anyhow::anyhow!("分组「{name}」不存在"))?;
    let target = match direction {
        "up" if index > 0 => index - 1,
        "down" if index + 1 < store.groups.len() => index + 1,
        _ => return Ok(()),
    };
    store.groups.swap(index, target);
    Ok(())
}

/// 按给定的完整分组名列表重排库内顺序。
///
/// 必须**恰好包含全部现有分组**（不重不漏）；用于拖动排序保存。
pub fn reorder_groups(store: &mut Store, ordered: &[String]) -> Result<()> {
    if ordered.len() != store.groups.len() {
        bail!(
            "排序列表数量不匹配：收到 {} 个，当前有 {} 个分组",
            ordered.len(),
            store.groups.len()
        );
    }
    let mut seen = HashSet::new();
    for name in ordered {
        if !seen.insert(name.clone()) {
            bail!("排序列表中有重复分组「{name}」");
        }
    }
    let existing: HashSet<String> = store.groups.iter().map(|g| g.name.clone()).collect();
    if seen != existing {
        bail!("排序列表与当前分组不一致，请刷新页面后重试");
    }

    let mut groups: HashMap<String, Group> = std::mem::take(&mut store.groups)
        .into_iter()
        .map(|g| (g.name.clone(), g))
        .collect();
    store.groups = ordered
        .iter()
        .filter_map(|name| groups.remove(name))
        .collect();
    Ok(())
}

impl Reference {
    pub fn describe(&self) -> String {
        format!("{}「{}」的 {}", self.kind, self.owner, self.field)
    }
}

/// 找出所有引用该分组的位置（直接引用）。
pub fn group_references(store: &Store, group: &str) -> Vec<Reference> {
    let mut out = Vec::new();

    for g in &store.groups {
        if g.name == group {
            continue;
        }
        if g.manual_groups.iter().any(|x| x == group) {
            out.push(Reference {
                kind: "分组",
                owner: g.name.clone(),
                field: "manual_groups".to_string(),
            });
        }
        if g.fallback_group.as_deref() == Some(group) {
            out.push(Reference {
                kind: "分组",
                owner: g.name.clone(),
                field: "fallback_group".to_string(),
            });
        }
    }

    for r in &store.rules {
        // raw 规则的目标无法可靠解析，不参与级联（§3.7）
        if r.is_raw() {
            continue;
        }
        if r.target.as_deref() == Some(group) {
            out.push(Reference {
                kind: "规则",
                owner: if r.id.is_empty() {
                    r.rule_type()
                } else {
                    r.id.clone()
                },
                field: "target".to_string(),
            });
        }
    }

    for p in &store.profiles {
        if p.groups.iter().any(|x| x == group) {
            out.push(Reference {
                kind: "方案",
                owner: p.name.clone(),
                field: "groups".to_string(),
            });
        }
    }

    out
}

/// raw 规则中**可能**提到该名字的规则（无法自动改写，只能提示人工核对）。
pub fn raw_rules_mentioning(store: &Store, text: &str) -> Vec<String> {
    store
        .rules
        .iter()
        .filter(|r| r.is_raw() && r.raw_text().contains(text))
        .map(|r| {
            if r.id.is_empty() {
                r.raw_text().to_string()
            } else {
                r.id.clone()
            }
        })
        .collect()
}

/// 分组的依赖闭包（含自身）。用于导出界面的自动补全。
pub fn dependency_closure(store: &Store, seeds: &[String]) -> Vec<String> {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut stack: Vec<String> = seeds.to_vec();
    while let Some(name) = stack.pop() {
        if !seen.insert(name.clone()) {
            continue;
        }
        if let Some(g) = store.group(&name) {
            for dep in &g.manual_groups {
                if !seen.contains(dep) {
                    stack.push(dep.clone());
                }
            }
        }
    }
    seen.into_iter().collect()
}

/// 级联重命名分组。返回受影响的引用清单（供确认页展示）。
pub fn rename_group(store: &mut Store, from: &str, to: &str) -> Result<Vec<Reference>> {
    let to = to.trim();
    if to.is_empty() {
        bail!("新分组名不能为空");
    }
    if to == from {
        bail!("新分组名与原名相同");
    }
    if is_builtin_policy(to) {
        bail!("分组名「{to}」与内置策略重名");
    }
    if store.group(from).is_none() {
        bail!("分组「{from}」不存在");
    }
    if store.group(to).is_some() {
        bail!("分组「{to}」已存在");
    }

    let refs = group_references(store, from);

    for g in &mut store.groups {
        for m in &mut g.manual_groups {
            if m == from {
                *m = to.to_string();
            }
        }
        if g.fallback_group.as_deref() == Some(from) {
            g.fallback_group = Some(to.to_string());
        }
    }
    for r in &mut store.rules {
        if !r.is_raw() && r.target.as_deref() == Some(from) {
            r.target = Some(to.to_string());
        }
    }
    for p in &mut store.profiles {
        for g in &mut p.groups {
            if g == from {
                *g = to.to_string();
            }
        }
    }
    if let Some(g) = store.groups.iter_mut().find(|g| g.name == from) {
        g.name = to.to_string();
    }

    Ok(refs)
}

/// 删除分组。
///
/// 被引用时**必须**提供替代目标；否则阻止删除（不做静默清理）。
pub fn delete_group(
    store: &mut Store,
    name: &str,
    replacement: Option<&str>,
) -> Result<Vec<Reference>> {
    if store.group(name).is_none() {
        bail!("分组「{name}」不存在");
    }
    let refs = group_references(store, name);

    if !refs.is_empty() {
        let Some(rep) = replacement.map(|s| s.trim()) else {
            let list = refs
                .iter()
                .map(|r| format!("  - {}", r.describe()))
                .collect::<Vec<_>>()
                .join("\n");
            bail!("分组「{name}」仍被以下位置引用，必须先指定替代分组：\n{list}");
        };
        if rep.is_empty() {
            bail!("替代分组不能为空");
        }
        if rep == name {
            bail!("替代分组不能是自身");
        }
        if store.group(rep).is_none() && !is_builtin_policy(rep) {
            bail!("替代分组「{rep}」不存在");
        }

        for g in &mut store.groups {
            for m in &mut g.manual_groups {
                if m == name {
                    *m = rep.to_string();
                }
            }
            if g.fallback_group.as_deref() == Some(name) {
                g.fallback_group = Some(rep.to_string());
            }
        }
        for r in &mut store.rules {
            if !r.is_raw() && r.target.as_deref() == Some(name) {
                r.target = Some(rep.to_string());
            }
        }
        for p in &mut store.profiles {
            for g in &mut p.groups {
                if g == name {
                    *g = rep.to_string();
                }
            }
        }
    }

    store.groups.retain(|g| g.name != name);
    Ok(refs)
}

/// 删除上游：从所有方案中移除，并删除缓存。
pub fn delete_upstream(store: &mut Store, id: &str) -> Result<Vec<String>> {
    if store.upstream(id).is_none() {
        bail!("上游「{id}」不存在");
    }
    let mut affected = Vec::new();
    for p in &mut store.profiles {
        if p.upstreams.iter().any(|x| x == id) {
            affected.push(p.name.clone());
            p.upstreams.retain(|x| x != id);
        }
    }
    store.upstreams.retain(|u| u.id != id);
    Ok(affected)
}

/// 删除规则：从所有方案中移除。
pub fn delete_rule(store: &mut Store, id: &str) -> Result<Vec<String>> {
    if store.rule(id).is_none() {
        bail!("规则「{id}」不存在");
    }
    let mut affected = Vec::new();
    for p in &mut store.profiles {
        if p.rules.iter().any(|x| x == id) {
            affected.push(p.name.clone());
            p.rules.retain(|x| x != id);
        }
    }
    store.rules.retain(|r| r.id != id);
    Ok(affected)
}

/// 供 UI：为一个分组集合计算需要一起勾选的依赖。
pub fn required_dependencies(store: &Store, selected: &[String]) -> Vec<String> {
    let closure = dependency_closure(store, selected);
    closure
        .into_iter()
        .filter(|n| !selected.contains(n))
        .collect()
}

/// 便捷：构造一个最小分组（测试用）。
#[cfg(test)]
pub fn mk_group(name: &str, manual: &[&str]) -> crate::store::model::Group {
    use crate::store::model::Group;
    Group {
        name: name.to_string(),
        clash: Default::default(),
        r#match: Default::default(),
        manual_groups: manual.iter().map(|s| s.to_string()).collect(),
        manual_nodes: vec![],
        fallback_group: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::model::{Group, Profile, RuleDef, RuleSection, Upstream};

    #[test]
    fn move_group_reorders_and_clamps_at_edges() {
        let mut store = Store::default();
        store.groups.clear();
        store.groups.push(mk_group("A", &[]));
        store.groups.push(mk_group("B", &[]));
        store.groups.push(mk_group("C", &[]));

        move_group(&mut store, "B", "up").unwrap();
        let names: Vec<&str> = store.groups.iter().map(|g| g.name.as_str()).collect();
        assert_eq!(names, vec!["B", "A", "C"]);

        // 已经在顶部，再上移不变。
        move_group(&mut store, "B", "up").unwrap();
        move_group(&mut store, "B", "down").unwrap();
        let names: Vec<&str> = store.groups.iter().map(|g| g.name.as_str()).collect();
        assert_eq!(names, vec!["A", "B", "C"]);

        // 已经在底部，再下移不变。
        move_group(&mut store, "C", "down").unwrap();
        let names: Vec<&str> = store.groups.iter().map(|g| g.name.as_str()).collect();
        assert_eq!(names, vec!["A", "B", "C"]);

        assert!(move_group(&mut store, "missing", "up").is_err());
    }

    #[test]
    fn reorder_groups_requires_exact_set_and_preserves_objects() {
        let mut store = Store::default();
        store.groups.clear();
        store.groups.push(mk_group("A", &[]));
        store.groups.push(mk_group("B", &[]));
        store.groups.push(mk_group("C", &[]));

        reorder_groups(
            &mut store,
            &["C".to_string(), "A".to_string(), "B".to_string()],
        )
        .unwrap();
        let names: Vec<&str> = store.groups.iter().map(|g| g.name.as_str()).collect();
        assert_eq!(names, vec!["C", "A", "B"]);

        // 少一个 / 多一个 / 重复都拒绝。
        assert!(reorder_groups(&mut store, &["A".to_string(), "B".to_string()]).is_err());
        assert!(
            reorder_groups(
                &mut store,
                &[
                    "A".to_string(),
                    "B".to_string(),
                    "C".to_string(),
                    "D".to_string()
                ]
            )
            .is_err()
        );
        assert!(
            reorder_groups(
                &mut store,
                &["A".to_string(), "A".to_string(), "B".to_string()]
            )
            .is_err()
        );
        // 校验失败不得破坏原顺序。
        let names: Vec<&str> = store.groups.iter().map(|g| g.name.as_str()).collect();
        assert_eq!(names, vec!["C", "A", "B"]);
    }

    fn fixture() -> Store {
        let mut s = Store::default();
        // 默认分组只是首次运行的种子；本 fixture 需要干净的分组集合。
        s.groups.clear();
        s.rules.clear();
        s.upstreams.push(Upstream {
            id: "u1".into(),
            name: "机场A".into(),
            url: "https://example.com".into(),
            note: None,
        });
        s.groups.push(mk_group("自动选择", &[]));
        s.groups.push(mk_group("美国节点", &["自动选择"]));
        s.rules.push(RuleDef {
            id: "r1".into(),
            section: RuleSection::Normal,
            r#type: Some("GEOIP".into()),
            payload: Some("CN".into()),
            target: Some("美国节点".into()),
            ..Default::default()
        });
        s.profiles.push(Profile {
            slug: "home".into(),
            name: "家庭".into(),
            upstreams: vec!["u1".into()],
            groups: vec!["自动选择".into(), "美国节点".into()],
            rules: vec!["r1".into()],
            on_failure: Default::default(),
            token: None,
        });
        s
    }

    #[test]
    fn upsert_creates_new_group() {
        let mut s = fixture();
        upsert_group(
            &mut s,
            "",
            "新分组",
            Mapping::new(),
            GroupMatch {
                regions: vec!["日本".into()],
                ..Default::default()
            },
            vec![],
            vec![],
            None,
        )
        .unwrap();
        let g = s.group("新分组").expect("新建分组必须真的写进去");
        assert_eq!(g.r#match.regions, vec!["日本".to_string()]);
    }

    #[test]
    fn upsert_edits_existing_group_in_place() {
        let mut s = fixture();
        upsert_group(
            &mut s,
            "美国节点",
            "美国节点",
            Mapping::new(),
            GroupMatch {
                keywords: vec!["GPT".into()],
                ..Default::default()
            },
            vec!["自动选择".into()],
            vec![],
            None,
        )
        .unwrap();
        assert_eq!(s.groups.len(), 2, "编辑不应产生新分组");
        assert_eq!(
            s.group("美国节点").unwrap().r#match.keywords,
            vec!["GPT".to_string()]
        );
    }

    #[test]
    fn upsert_renames_and_cascades() {
        let mut s = fixture();
        upsert_group(
            &mut s,
            "美国节点",
            "美区",
            Mapping::new(),
            GroupMatch::default(),
            vec![],
            vec![],
            None,
        )
        .unwrap();
        assert!(s.group("美区").is_some());
        assert!(s.group("美国节点").is_none());
        assert_eq!(
            s.rule("r1").unwrap().target.as_deref(),
            Some("美区"),
            "重命名必须级联改写规则目标"
        );
    }

    #[test]
    fn upsert_rejects_duplicate_name() {
        let mut s = fixture();
        assert!(
            upsert_group(
                &mut s,
                "",
                "自动选择",
                Mapping::new(),
                GroupMatch::default(),
                vec![],
                vec![],
                None
            )
            .is_err()
        );
    }

    #[test]
    fn upsert_rejects_builtin_policy_name() {
        let mut s = fixture();
        assert!(
            upsert_group(
                &mut s,
                "",
                "DIRECT",
                Mapping::new(),
                GroupMatch::default(),
                vec![],
                vec![],
                None
            )
            .is_err()
        );
    }

    #[test]
    fn references_cover_all_places() {
        let s = fixture();
        let refs = group_references(&s, "美国节点");
        let fields: Vec<&str> = refs.iter().map(|r| r.field.as_str()).collect();
        assert!(fields.contains(&"target"), "{fields:?}");
        assert!(fields.contains(&"groups"), "{fields:?}");
    }

    #[test]
    fn rename_cascades_everywhere_including_profile_groups() {
        let mut s = fixture();
        let refs = rename_group(&mut s, "美国节点", "美区").unwrap();
        assert!(!refs.is_empty());

        assert!(s.group("美区").is_some());
        assert!(s.group("美国节点").is_none());
        assert_eq!(s.rule("r1").unwrap().target.as_deref(), Some("美区"));
        // Profile.groups 必须一并改写
        assert!(
            s.profile("home")
                .unwrap()
                .groups
                .contains(&"美区".to_string())
        );
        assert!(
            !s.profile("home")
                .unwrap()
                .groups
                .contains(&"美国节点".to_string())
        );
        // 其他分组的 manual_groups
        assert!(
            !group_references(&s, "美国节点")
                .iter()
                .any(|r| r.kind == "分组")
        );
    }

    #[test]
    fn rename_to_existing_is_rejected() {
        let mut s = fixture();
        assert!(rename_group(&mut s, "美国节点", "自动选择").is_err());
    }

    #[test]
    fn rename_to_builtin_policy_is_rejected() {
        let mut s = fixture();
        assert!(rename_group(&mut s, "美国节点", "DIRECT").is_err());
    }

    #[test]
    fn delete_referenced_group_without_replacement_is_blocked() {
        let mut s = fixture();
        let err = delete_group(&mut s, "美国节点", None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("替代分组"), "{err}");
        assert!(s.group("美国节点").is_some(), "被阻止时不得删除");
    }

    #[test]
    fn delete_referenced_group_with_replacement_rewrites() {
        let mut s = fixture();
        delete_group(&mut s, "美国节点", Some("自动选择")).unwrap();
        assert!(s.group("美国节点").is_none());
        assert_eq!(s.rule("r1").unwrap().target.as_deref(), Some("自动选择"));
    }

    #[test]
    fn delete_unreferenced_group_succeeds_directly() {
        let mut s = fixture();
        s.groups.push(mk_group("孤儿", &[]));
        delete_group(&mut s, "孤儿", None).unwrap();
        assert!(s.group("孤儿").is_none());
    }

    #[test]
    fn indirect_reference_counts_as_used() {
        let s = fixture();
        // 「自动选择」没有被方案外的对象直接引用，但被「美国节点」引用
        assert!(!group_references(&s, "自动选择").is_empty());
        let mut s2 = s.clone();
        s2.profiles.clear();
        s2.rules.clear();
        assert!(
            !group_references(&s2, "自动选择").is_empty(),
            "间接引用（被其他分组依赖）也算被使用"
        );
    }

    #[test]
    fn delete_upstream_removes_from_profiles() {
        let mut s = fixture();
        let affected = delete_upstream(&mut s, "u1").unwrap();
        assert_eq!(affected, vec!["家庭".to_string()]);
        assert!(s.upstream("u1").is_none());
        assert!(s.profile("home").unwrap().upstreams.is_empty());
    }

    #[test]
    fn raw_rules_are_flagged_not_rewritten() {
        let mut s = fixture();
        s.rules.push(RuleDef {
            id: "raw1".into(),
            raw: Some("AND,((NETWORK,tcp)),美国节点".into()),
            ..Default::default()
        });
        let mentions = raw_rules_mentioning(&s, "美国节点");
        assert_eq!(mentions, vec!["raw1".to_string()]);

        // 重命名不会改写 raw 规则
        rename_group(&mut s, "美国节点", "美区").unwrap();
        assert!(
            s.rule("raw1").unwrap().raw_text().contains("美国节点"),
            "raw 规则必须保持原样，由用户在确认页人工核对"
        );
    }

    #[test]
    fn dependency_closure_collects_transitively() {
        let mut s = fixture();
        s.groups.push(mk_group("日本节点", &["自动选择"]));
        let need = required_dependencies(&s, &["美国节点".into()]);
        assert!(need.contains(&"自动选择".to_string()));
    }

    #[test]
    fn fallback_group_is_a_reference() {
        let mut s = fixture();
        s.groups.push(Group {
            fallback_group: Some("自动选择".into()),
            ..mk_group("备用", &[])
        });
        let refs = group_references(&s, "自动选择");
        assert!(refs.iter().any(|r| r.field == "fallback_group"), "{refs:?}");
    }
}
