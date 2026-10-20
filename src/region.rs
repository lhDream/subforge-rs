//! 地区判定（纯函数层，AGENTS.md §3.5）。
//!
//! 分层要求：本模块**不碰网络与文件**。域名解析与 MMDB 查询由 `geoip` 模块在
//! IO 层完成后，把结果装进 [`RegionInputs`] 再调用这里。

use crate::store::model::{RegionAlias, RegionSource};
use std::collections::BTreeSet;

/// IO 层准备好的输入。
#[derive(Debug, Clone, Default)]
pub struct RegionInputs {
    pub node_name: String,
    /// 节点 `server` 字段（IP 或域名），由调用方用于展示冲突详情。
    pub server: String,
    /// IO 层查到的国家码，如 `["US"]`。
    pub geo_countries: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct RegionDecision {
    /// 最终采用的地区集合（可能有多个 → 歧义）。
    pub regions: Vec<String>,
    /// 节点名匹配到的地区。
    pub name_regions: Vec<String>,
    /// GeoIP 匹配到的地区。
    pub geo_regions: Vec<String>,
    /// 两个来源都命中且不一致。
    pub conflict: bool,
    /// 采用的集合大小 > 1。
    pub ambiguous: bool,
}

/// 别名是否命中节点名。
///
/// 规则（AGENTS.md §3.5）：
/// - 含非 ASCII（CJK、emoji）的别名 → 子串匹配；
/// - 纯 ASCII 且长度 ≤ 3 的别名 → **前后不得是 ASCII 字母**。
///
/// 后者不能用 `\b`：`\bUS\b` 匹配不到常见的 `US01`（`0` 是单词字符）。
pub fn match_alias(name: &str, alias: &str) -> bool {
    let alias = alias.trim();
    if alias.is_empty() {
        return false;
    }
    let ascii_short = alias.is_ascii() && alias.chars().count() <= 3;
    if ascii_short {
        bounded_ascii_match(name, alias)
    } else {
        name.to_lowercase().contains(&alias.to_lowercase())
    }
}

fn bounded_ascii_match(name: &str, alias: &str) -> bool {
    let name_l = name.to_lowercase();
    let alias_l = alias.to_lowercase();
    let hay = name_l.as_bytes();
    let needle = alias_l.as_bytes();
    if needle.is_empty() || needle.len() > hay.len() {
        return false;
    }
    for start in 0..=(hay.len() - needle.len()) {
        if &hay[start..start + needle.len()] != needle {
            continue;
        }
        let end = start + needle.len();
        // ASCII 别名的字节都 < 0x80，UTF-8 续字节 ≥ 0x80，因此不会命中多字节字符内部。
        let before_ok = start == 0 || !hay[start - 1].is_ascii_alphabetic();
        let after_ok = end >= hay.len() || !hay[end].is_ascii_alphabetic();
        if before_ok && after_ok {
            return true;
        }
    }
    false
}

/// 节点名命中的全部地区。
pub fn regions_from_name(name: &str, aliases: &[RegionAlias]) -> Vec<String> {
    let mut out = BTreeSet::new();
    for r in aliases {
        if r.aliases.iter().any(|a| match_alias(name, a)) {
            out.insert(r.region.clone());
        }
    }
    out.into_iter().collect()
}

/// 国家码 → 地区名。使用别名表的**精确**匹配（国家码本身就是一种别名）。
pub fn region_for_country(country: &str, aliases: &[RegionAlias]) -> Option<String> {
    let c = country.trim();
    if c.is_empty() {
        return None;
    }
    aliases
        .iter()
        .find(|r| r.aliases.iter().any(|a| a.eq_ignore_ascii_case(c)))
        .map(|r| r.region.clone())
}

/// 地区判定主入口。
pub fn decide_region(
    inputs: &RegionInputs,
    aliases: &[RegionAlias],
    source: RegionSource,
) -> RegionDecision {
    let name_regions = regions_from_name(&inputs.node_name, aliases);

    let mut geo_set = BTreeSet::new();
    for c in &inputs.geo_countries {
        if let Some(r) = region_for_country(c, aliases) {
            geo_set.insert(r);
        }
    }
    let geo_regions: Vec<String> = geo_set.into_iter().collect();

    let both_present = !name_regions.is_empty() && !geo_regions.is_empty();
    let conflict = both_present && name_regions != geo_regions;

    let chosen = match source {
        // 节点名优先：中转/CDN 会让 GeoIP 判出入口归属地而非落地地区。
        RegionSource::NameFirst => {
            if !name_regions.is_empty() {
                name_regions.clone()
            } else {
                geo_regions.clone()
            }
        }
        RegionSource::IpFirst => {
            if !geo_regions.is_empty() {
                geo_regions.clone()
            } else {
                name_regions.clone()
            }
        }
    };

    RegionDecision {
        ambiguous: chosen.len() > 1,
        regions: chosen,
        name_regions,
        geo_regions,
        conflict,
    }
}

/// 别名冲突检测：同一别名不得挂在两个地区下（AGENTS.md §6.1 第 8 条）。
pub fn find_alias_conflicts(aliases: &[RegionAlias]) -> Vec<String> {
    let mut seen: Vec<(String, String)> = Vec::new(); // (小写别名, 地区)
    let mut out = Vec::new();
    for r in aliases {
        for a in &r.aliases {
            let key = a.trim().to_lowercase();
            if key.is_empty() {
                continue;
            }
            if let Some((_, owner)) = seen.iter().find(|(k, _)| *k == key) {
                if owner != &r.region {
                    out.push(format!(
                        "别名「{a}」同时属于地区「{owner}」和「{}」",
                        r.region
                    ));
                }
            } else {
                seen.push((key, r.region.clone()));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::model::default_aliases;

    #[test]
    fn short_ascii_alias_does_not_match_inside_words() {
        // 子串匹配会把这些误判为美国。
        assert!(!match_alias("Russia", "US"));
        assert!(!match_alias("Belarus", "US"));
        assert!(!match_alias("Australia", "US"));
        assert!(!match_alias("Cyprus", "US"));
    }

    #[test]
    fn short_ascii_alias_still_matches_common_node_names() {
        // `\bUS\b` 会漏掉这些。
        assert!(match_alias("US01", "US"));
        assert!(match_alias("US-01", "US"));
        assert!(match_alias("美国US", "US"));
        assert!(match_alias("HK01", "HK"));
        assert!(match_alias("JP-03", "JP"));
    }

    #[test]
    fn flag_emoji_is_not_the_letters_us() {
        // 🇺🇸 是区域指示符 U+1F1FA U+1F1F8，字节里并不含 ASCII 的 "US"。
        // 这类节点靠别名表里的 "🇺🇸" / "美" 命中，不能靠 "US"。
        assert!(!match_alias("🇺🇸 美国 01", "US"));

        let regions = regions_from_name("🇺🇸 美国 01", &default_aliases());
        assert_eq!(regions, vec!["美国".to_string()]);
    }

    #[test]
    fn cjk_alias_uses_substring() {
        assert!(match_alias("🇭🇰香港01", "港"));
        assert!(match_alias("香港中转", "香港"));
    }

    #[test]
    fn multi_region_hit_is_ambiguous() {
        let aliases = default_aliases();
        let d = decide_region(
            &RegionInputs {
                node_name: "🇭🇰香港中转→日本".into(),
                ..Default::default()
            },
            &aliases,
            RegionSource::NameFirst,
        );
        assert!(d.regions.contains(&"香港".to_string()));
        assert!(d.regions.contains(&"日本".to_string()));
        assert!(d.ambiguous);
    }

    #[test]
    fn geoip_used_only_when_name_has_no_region() {
        let aliases = default_aliases();

        // 节点名命中 → 采用节点名，即使 GeoIP 说是别处（中转场景）。
        let d = decide_region(
            &RegionInputs {
                node_name: "🇯🇵 日本 01".into(),
                geo_countries: vec!["HK".into()],
                ..Default::default()
            },
            &aliases,
            RegionSource::NameFirst,
        );
        assert_eq!(d.regions, vec!["日本".to_string()]);
        assert!(d.conflict, "节点名与 GeoIP 冲突应被记录");

        // 节点名无地区信息 → 回退 GeoIP。
        let d = decide_region(
            &RegionInputs {
                node_name: "专线 01".into(),
                geo_countries: vec!["US".into()],
                ..Default::default()
            },
            &aliases,
            RegionSource::NameFirst,
        );
        assert_eq!(d.regions, vec!["美国".to_string()]);
        assert!(!d.conflict);
    }

    #[test]
    fn alias_conflicts_are_detected() {
        let aliases = vec![
            RegionAlias {
                region: "香港".into(),
                aliases: vec!["HK".into()],
            },
            RegionAlias {
                region: "日本".into(),
                aliases: vec!["hk".into()],
            },
        ];
        let c = find_alias_conflicts(&aliases);
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn default_aliases_have_no_conflict() {
        assert!(find_alias_conflicts(&default_aliases()).is_empty());
    }
}
