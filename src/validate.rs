//! 校验层。
//!
//! 分两级：
//! - [`validate_structure`]：**永远非法**的结构问题，每次写入与加载都执行。
//! - [`Report`]：导出期校验（AGENTS.md §6.1），错误阻断导出，警告放行。

use crate::region::find_alias_conflicts;
use crate::rules::is_builtin_policy;
use crate::store::model::Store;
use crate::store::valid_slug;
use anyhow::{Result, bail};
use std::collections::HashSet;

/// 导出期校验结果。
#[derive(Debug, Default, Clone)]
pub struct Report {
    /// 阻断导出。
    pub errors: Vec<String>,
    /// 放行但需提示。
    pub warnings: Vec<String>,
    /// 名称冲突提示（不阻断）。
    pub conflicts: Vec<String>,
    /// 供预览展示的统计信息。
    pub info: Vec<String>,
}

impl Report {
    pub fn error(&mut self, msg: impl Into<String>) {
        self.errors.push(msg.into());
    }
    pub fn warn(&mut self, msg: impl Into<String>) {
        self.warnings.push(msg.into());
    }
    pub fn conflict(&mut self, msg: impl Into<String>) {
        self.conflicts.push(msg.into());
    }
    pub fn note(&mut self, msg: impl Into<String>) {
        self.info.push(msg.into());
    }

    pub fn ok(&self) -> bool {
        self.errors.is_empty()
    }
}

/// `general` 中禁止出现的键——这三个键由方案决定（§6.1 第 11 条）。
const RESERVED_GENERAL_KEYS: &[&str] = &["proxies", "proxy-groups", "rules"];

/// 结构校验：**永远非法**的问题。不检查跨对象引用（那属于导出期校验）。
pub fn validate_structure(store: &Store) -> Result<()> {
    let mut errors: Vec<String> = Vec::new();

    // ---- 上游
    let mut up_ids = HashSet::new();
    for u in &store.upstreams {
        if !valid_slug(&u.id) {
            errors.push(format!(
                "上游 id「{}」非法：只允许小写字母、数字与连字符",
                u.id
            ));
        }
        if !up_ids.insert(u.id.clone()) {
            errors.push(format!("上游 id「{}」重复", u.id));
        }
        if u.name.trim().is_empty() {
            errors.push(format!("上游「{}」的名称为空", u.id));
        }
        if u.url.trim().is_empty() {
            errors.push(format!("上游「{}」的 URL 为空", u.id));
        }
    }

    // ---- 手动节点
    let mut manual_ids = HashSet::new();
    let mut manual_names = HashSet::new();
    for n in &store.manual_nodes {
        if !valid_slug(&n.id) {
            errors.push(format!(
                "手动节点 id「{}」非法：只允许小写字母、数字与连字符",
                n.id
            ));
        }
        if !manual_ids.insert(n.id.clone()) {
            errors.push(format!("手动节点 id「{}」重复", n.id));
        }
        if n.link.trim().is_empty() {
            errors.push(format!("手动节点「{}」的链接为空", n.id));
            continue;
        }
        match crate::links::parse_link(&n.link) {
            Ok(proxy) => {
                let name = crate::store::model::mstr(&proxy, "name")
                    .unwrap_or("")
                    .trim()
                    .to_string();
                if name.is_empty() {
                    errors.push(format!("手动节点「{}」缺少节点名", n.id));
                } else if !manual_names.insert(name.clone()) {
                    errors.push(format!("手动节点名「{name}」重复"));
                }
            }
            Err(e) => errors.push(format!("手动节点「{}」解析失败：{e}", n.id)),
        }
    }

    // ---- 分组
    let mut group_names = HashSet::new();
    for g in &store.groups {
        let name = g.name.trim();
        if name.is_empty() {
            errors.push("存在名称为空的分组".to_string());
            continue;
        }
        if is_builtin_policy(name) {
            errors.push(format!(
                "分组名「{name}」与内置策略重名（DIRECT/REJECT/PASS/GLOBAL）"
            ));
        }
        if !group_names.insert(name.to_string()) {
            errors.push(format!("分组名「{name}」重复"));
        }
    }

    // ---- 规则
    let mut rule_ids = HashSet::new();
    for r in &store.rules {
        if r.id.trim().is_empty() {
            errors.push("存在 id 为空的规则".to_string());
            continue;
        }
        if !rule_ids.insert(r.id.clone()) {
            errors.push(format!("规则 id「{}」重复", r.id));
        }
        if r.is_raw() {
            if r.raw_text().trim().is_empty() {
                errors.push(format!("规则「{}」的 raw 内容为空", r.id));
            }
        } else {
            if r.r#type.as_deref().unwrap_or("").trim().is_empty() {
                errors.push(format!("规则「{}」缺少 type", r.id));
            }
            if r.target.as_deref().unwrap_or("").trim().is_empty() {
                errors.push(format!("规则「{}」缺少 target", r.id));
            }
        }
    }

    // ---- 方案
    let mut slugs = HashSet::new();
    for p in &store.profiles {
        if !valid_slug(&p.slug) {
            errors.push(format!(
                "方案 slug「{}」非法：只允许小写字母、数字与连字符",
                p.slug
            ));
        }
        if !slugs.insert(p.slug.clone()) {
            errors.push(format!("方案 slug「{}」重复", p.slug));
        }
        if p.name.trim().is_empty() {
            errors.push(format!("方案「{}」的名称为空", p.slug));
        }
    }

    // ---- 地区别名
    for c in find_alias_conflicts(&store.aliases) {
        errors.push(c);
    }

    // ---- 通用设置不得侵占导出键
    for key in RESERVED_GENERAL_KEYS {
        if crate::store::model::mget(&store.general, key).is_some() {
            errors.push(format!(
                "通用设置（general）不得包含「{key}」——该键由方案决定"
            ));
        }
    }

    if !errors.is_empty() {
        bail!("数据校验失败：\n  - {}", errors.join("\n  - "));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::model::{
        Group, GroupMatch, ManualNode, Mapping, Profile, RuleDef, Upstream, Yaml,
    };

    fn base() -> Store {
        Store::default()
    }

    #[test]
    fn default_store_is_valid() {
        validate_structure(&base()).unwrap();
    }

    #[test]
    fn manual_node_link_and_duplicate_name_validated() {
        let mut s = base();
        s.manual_nodes.push(ManualNode {
            id: "n-a".into(),
            link: "ss://YWVzLTI1Ni1nY206cHc=@1.2.3.4:443#HK".into(),
        });
        validate_structure(&s).unwrap();

        s.manual_nodes.push(ManualNode {
            id: "n-b".into(),
            link: "ss://YWVzLTI1Ni1nY206cHc=@5.6.7.8:443#HK".into(),
        });
        let err = validate_structure(&s).unwrap_err().to_string();
        assert!(err.contains("节点名「HK」重复"), "{err}");

        s.manual_nodes.truncate(1);
        s.manual_nodes[0].link = "ss://bad".into();
        let err = validate_structure(&s).unwrap_err().to_string();
        assert!(err.contains("解析失败"), "{err}");
    }

    #[test]
    fn duplicate_upstream_id_rejected() {
        let mut s = base();
        s.upstreams.push(Upstream {
            id: "a".into(),
            name: "A".into(),
            url: "https://example.com".into(),
            note: None,
        });
        s.upstreams.push(Upstream {
            id: "a".into(),
            name: "B".into(),
            url: "https://example.com".into(),
            note: None,
        });
        assert!(validate_structure(&s).is_err());
    }

    #[test]
    fn path_traversal_slug_rejected() {
        let mut s = base();
        s.upstreams.push(Upstream {
            id: "../evil".into(),
            name: "x".into(),
            url: "https://example.com".into(),
            note: None,
        });
        assert!(validate_structure(&s).is_err());
    }

    #[test]
    fn builtin_policy_group_name_rejected() {
        let mut s = base();
        s.groups.push(Group {
            name: "DIRECT".into(),
            clash: Mapping::new(),
            r#match: GroupMatch::default(),
            manual_groups: vec![],
            manual_nodes: vec![],
            fallback_group: None,
        });
        let err = validate_structure(&s).unwrap_err().to_string();
        assert!(err.contains("内置策略"), "{err}");
    }

    #[test]
    fn general_cannot_shadow_export_keys() {
        let mut s = base();
        s.general
            .insert(Yaml::String("rules".into()), Yaml::Sequence(vec![]));
        assert!(validate_structure(&s).is_err());
    }

    #[test]
    fn profile_slug_validated() {
        let mut s = base();
        s.profiles.push(Profile {
            slug: "Home".into(), // 大写非法
            name: "家".into(),
            upstreams: vec![],
            groups: vec![],
            rules: vec![],
            on_failure: Default::default(),
            token: None,
        });
        assert!(validate_structure(&s).is_err());
    }

    #[test]
    fn raw_rule_without_target_is_allowed() {
        let mut s = base();
        s.rules.push(RuleDef {
            id: "r1".into(),
            raw: Some("MATCH,PROXY".into()),
            ..Default::default()
        });
        validate_structure(&s).unwrap();
    }

    #[test]
    fn structured_rule_requires_target() {
        let mut s = base();
        s.rules.push(RuleDef {
            id: "r1".into(),
            r#type: Some("GEOIP".into()),
            payload: Some("CN".into()),
            target: None,
            ..Default::default()
        });
        assert!(validate_structure(&s).is_err());
    }
}
