//! 规则解析与拼装。
//!
//! 语义严格对齐 Mihomo `rules/common/base.go` 的 `ParseRulePayload`：
//!
//! ```text
//! tp,payload,target(,params...)     // 默认
//! MATCH,target                      // MATCH 无 payload 无 params
//! tp,payload,target                 // 逻辑/正则类：payload 含逗号，target 在末尾
//! ```
//!
//! **关键**：默认形态下 target 是**第 3 个字段**，不是最后一列。把目标追加到末尾
//! 会让 `no-resolve` 之类的参数被当成目标（AGENTS.md §3.7）。

use crate::store::model::{RuleDef, RuleSection};
use anyhow::{Result, bail};

/// payload 可能含逗号、且没有 params 的规则类型（target 在末尾）。
const LOGICAL_TYPES: &[&str] = &[
    "NOT",
    "OR",
    "AND",
    "SUB-RULE",
    "DOMAIN-REGEX",
    "PROCESS-NAME-REGEX",
    "PROCESS-PATH-REGEX",
];

/// 内置策略名，不可用作分组名。
pub const BUILTIN_POLICIES: &[&str] = &["DIRECT", "REJECT", "PASS", "GLOBAL", "COMPATIBLE"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleShape {
    /// `MATCH,target`
    Match,
    /// `tp,payload,target`（payload 含逗号，无 params）
    Logical,
    /// `tp,payload,target(,params...)`
    Default,
}

pub fn shape_of(tp: &str) -> RuleShape {
    let t = tp.trim().to_uppercase();
    if t == "MATCH" {
        RuleShape::Match
    } else if LOGICAL_TYPES.contains(&t.as_str()) {
        RuleShape::Logical
    } else {
        RuleShape::Default
    }
}

pub fn is_builtin_policy(name: &str) -> bool {
    BUILTIN_POLICIES.contains(&name.trim().to_uppercase().as_str())
}

/// 解析结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedRule {
    pub tp: String,
    pub payload: String,
    pub target: String,
    pub params: Vec<String>,
}

/// 按 Mihomo 语义解析一行规则。
///
/// `need_target` 对应 Go 侧的 `needTarget`：为 true 时把第 3 段（或逻辑类末段）
/// 解释为 target，其余为 params。
pub fn parse_rule_text(raw: &str, need_target: bool) -> ParsedRule {
    let mut item: Vec<String> = raw.split(',').map(|s| s.trim().to_string()).collect();
    let mut out = ParsedRule::default();
    if item.is_empty() || item[0].is_empty() {
        return out;
    }
    out.tp = item[0].to_uppercase();

    if item.len() > 1 {
        match shape_of(&out.tp) {
            RuleShape::Match => {
                out.target = item[1].clone();
            }
            RuleShape::Logical => {
                if need_target {
                    let last = item.len() - 1;
                    out.target = item[last].clone();
                    item.truncate(last);
                }
                out.payload = item[1..].join(",");
            }
            RuleShape::Default => {
                out.payload = item[1].clone();
                if item.len() > 2 {
                    if need_target {
                        out.target = item[2].clone();
                        if item.len() > 3 {
                            out.params = item[3..].to_vec();
                        }
                    } else {
                        out.params = item[2..].to_vec();
                    }
                }
            }
        }
    }
    out
}

/// 把结构化规则序列化为一行文本。
pub fn serialize_structured(
    tp: &str,
    payload: Option<&str>,
    target: &str,
    params: &[String],
) -> Result<String> {
    let tp = tp.trim().to_uppercase();
    if tp.is_empty() {
        bail!("规则缺少 type");
    }
    if target.trim().is_empty() {
        bail!("规则 {tp} 缺少 target");
    }
    let target = target.trim();

    let line = match shape_of(&tp) {
        RuleShape::Match => format!("{tp},{target}"),
        RuleShape::Logical => {
            let payload = payload.unwrap_or("").trim();
            if payload.is_empty() {
                bail!("规则 {tp} 缺少 payload");
            }
            format!("{tp},{payload},{target}")
        }
        RuleShape::Default => {
            let payload = payload.unwrap_or("").trim();
            if payload.is_empty() {
                bail!("规则 {tp} 缺少 payload");
            }
            let mut line = format!("{tp},{payload},{target}");
            for p in params {
                let p = p.trim();
                if !p.is_empty() {
                    line.push(',');
                    line.push_str(p);
                }
            }
            line
        }
    };
    Ok(line)
}

/// 序列化一条库内规则。
pub fn serialize_rule(rule: &RuleDef) -> Result<String> {
    if let Some(raw) = &rule.raw {
        let raw = raw.trim();
        if raw.is_empty() {
            bail!("规则 {} 的 raw 内容为空", rule.id);
        }
        return Ok(raw.to_string());
    }
    serialize_structured(
        rule.r#type.as_deref().unwrap_or(""),
        rule.payload.as_deref(),
        rule.target.as_deref().unwrap_or(""),
        &rule.params,
    )
}

// ---------------------------------------------------------------- 分段排序

/// 规则分段顺序：override → normal → final。
pub fn section_rank(section: RuleSection) -> u8 {
    match section {
        RuleSection::Override => 0,
        RuleSection::Normal => 2,
        RuleSection::Final => 3,
    }
}

/// 顺序与可达性检查（AGENTS.md §6.2）。
///
/// `lines` 是**已展开**的最终规则文本，按输出顺序排列。
/// 返回 (errors, warnings)。
pub fn check_order(lines: &[String]) -> (Vec<String>, Vec<String>) {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();

    let mut seen_match_at: Option<usize> = None;
    for (i, line) in lines.iter().enumerate() {
        let parsed = parse_rule_text(line, true);
        if parsed.tp == "MATCH" {
            if seen_match_at.is_none() {
                seen_match_at = Some(i);
            } else {
                errors.push(format!(
                    "存在多条 MATCH 兜底规则（第 {} 条与第 {} 条）",
                    seen_match_at.unwrap() + 1,
                    i + 1
                ));
            }
        }
    }

    match seen_match_at {
        None => errors.push(
            "缺少最终兜底规则（MATCH）。未命中的流量会被 Mihomo 静默回退到 DIRECT，\
             绕过你的路由与隐私意图"
                .to_string(),
        ),
        Some(idx) => {
            if idx != lines.len() - 1 {
                let unreachable = lines.len() - idx - 1;
                errors.push(format!(
                    "MATCH 兜底位于第 {} 条，其后还有 {} 条规则永远不会被匹配",
                    idx + 1,
                    unreachable
                ));
            }
        }
    }

    if lines.is_empty() {
        warnings.push("规则列表为空".to_string());
    }

    (errors, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_are_not_the_target() {
        // 这正是「目标追加到末尾」会踩的坑。
        let parsed = parse_rule_text("IP-CIDR,1.2.3.0/24,no-resolve", true);
        assert_eq!(parsed.tp, "IP-CIDR");
        assert_eq!(parsed.payload, "1.2.3.0/24");
        assert_eq!(parsed.target, "no-resolve");
        assert!(parsed.params.is_empty());
    }

    #[test]
    fn correct_order_puts_target_before_params() {
        let line = serialize_structured(
            "IP-CIDR",
            Some("1.2.3.0/24"),
            "美国节点",
            &["no-resolve".to_string()],
        )
        .unwrap();
        assert_eq!(line, "IP-CIDR,1.2.3.0/24,美国节点,no-resolve");

        let parsed = parse_rule_text(&line, true);
        assert_eq!(parsed.target, "美国节点");
        assert_eq!(parsed.params, vec!["no-resolve".to_string()]);
    }

    #[test]
    fn match_has_no_payload_or_params() {
        let line = serialize_structured("MATCH", None, "PROXY", &[]).unwrap();
        assert_eq!(line, "MATCH,PROXY");
        let parsed = parse_rule_text(&line, true);
        assert_eq!(parsed.target, "PROXY");
        assert_eq!(parsed.payload, "");
    }

    #[test]
    fn logical_rule_keeps_target_at_end() {
        let raw = "AND,((NETWORK,tcp),(DOMAIN-SUFFIX,example.com)),美国节点";
        let parsed = parse_rule_text(raw, true);
        assert_eq!(parsed.tp, "AND");
        assert_eq!(
            parsed.payload,
            "((NETWORK,tcp),(DOMAIN-SUFFIX,example.com))"
        );
        assert_eq!(parsed.target, "美国节点");
    }

    #[test]
    fn order_check_requires_final_match() {
        let (errors, _) = check_order(&["GEOIP,CN,DIRECT".to_string()]);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("缺少最终兜底规则"));

        let (errors, _) = check_order(&["MATCH,PROXY".to_string(), "GEOIP,CN,DIRECT".to_string()]);
        assert!(errors.iter().any(|e| e.contains("永远不会被匹配")));

        let (errors, _) = check_order(&["GEOIP,CN,DIRECT".to_string(), "MATCH,PROXY".to_string()]);
        assert!(errors.is_empty(), "{errors:?}");
    }

    #[test]
    fn builtin_policies_detected() {
        assert!(is_builtin_policy("DIRECT"));
        assert!(is_builtin_policy("reject"));
        assert!(!is_builtin_policy("美国节点"));
    }
}
