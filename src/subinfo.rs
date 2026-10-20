//! 订阅信息提取与假节点过滤（AGENTS.md §4.6）。
//!
//! 机场常在节点列表里塞信息条目（`剩余流量：123.45 GB`、`套餐到期：2025-12-31`）。
//! 它们是**合法节点结构**，透传设计会把它们当正常节点收进来，必须显式过滤。

use regex::Regex;
use std::sync::LazyLock;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SubInfo {
    pub upload: Option<u64>,
    pub download: Option<u64>,
    pub total: Option<u64>,
    /// 到期时间（Unix 秒）。
    pub expire: Option<i64>,
}

impl SubInfo {
    /// 用 `other` 中存在的字段补齐本结构（本结构已有的字段不覆盖）。
    pub fn fill_from(&mut self, other: &SubInfo) {
        if self.upload.is_none() {
            self.upload = other.upload;
        }
        if self.download.is_none() {
            self.download = other.download;
        }
        if self.total.is_none() {
            self.total = other.total;
        }
        if self.expire.is_none() {
            self.expire = other.expire;
        }
    }

    pub fn used(&self) -> Option<u64> {
        match (self.upload, self.download) {
            (Some(u), Some(d)) => Some(u.saturating_add(d)),
            (Some(u), None) => Some(u),
            (None, Some(d)) => Some(d),
            (None, None) => None,
        }
    }

    /// 剩余流量（total 为 0 视为不限量）。
    pub fn remaining(&self) -> Option<u64> {
        match (self.total, self.used()) {
            (Some(t), Some(used)) if t > 0 => Some(t.saturating_sub(used)),
            _ => None,
        }
    }
}

/// 解析 Clash 订阅标准响应头 `subscription-userinfo`。
///
/// 形如：`upload=1234; download=5678; total=107374182400; expire=1735689600`
pub fn parse_userinfo(header: &str) -> SubInfo {
    let mut info = SubInfo::default();
    for part in header.split(';') {
        let Some((k, v)) = part.split_once('=') else {
            continue;
        };
        let key = k.trim().to_ascii_lowercase();
        let val = v.trim().trim_matches('"');
        match key.as_str() {
            "upload" => info.upload = val.parse().ok(),
            "download" => info.download = val.parse().ok(),
            "total" => info.total = val.parse().ok(),
            "expire" => info.expire = val.parse().ok(),
            _ => {}
        }
    }
    info
}

static TRAFFIC_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)([\d.]+)\s*(TB|GB|MB|KB|B)").expect("内置正则必须有效"));

static DATE_DASH_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(\d{4})-(\d{1,2})-(\d{1,2})").expect("内置正则必须有效"));

static DATE_SLASH_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(\d{4})/(\d{1,2})/(\d{1,2})").expect("内置正则必须有效"));

/// 从一段文本中解析流量（返回字节数）。
pub fn parse_traffic_bytes(text: &str) -> Option<u64> {
    let caps = TRAFFIC_RE.captures(text)?;
    let value: f64 = caps.get(1)?.as_str().parse().ok()?;
    let unit = caps.get(2)?.as_str().to_ascii_uppercase();
    let factor: f64 = match unit.as_str() {
        "TB" => 1024f64.powi(4),
        "GB" => 1024f64.powi(3),
        "MB" => 1024f64.powi(2),
        "KB" => 1024f64,
        _ => 1f64,
    };
    Some((value * factor) as u64)
}

/// 从一段文本中解析日期（返回当天 00:00 UTC 的 Unix 秒）。
pub fn parse_expire_date(text: &str) -> Option<i64> {
    use chrono::{NaiveDate, TimeZone, Utc};

    let (y, m, d) = if let Some(c) = DATE_DASH_RE.captures(text) {
        (
            c.get(1)?.as_str().parse().ok()?,
            c.get(2)?.as_str().parse().ok()?,
            c.get(3)?.as_str().parse().ok()?,
        )
    } else if let Some(c) = DATE_SLASH_RE.captures(text) {
        (
            c.get(1)?.as_str().parse().ok()?,
            c.get(2)?.as_str().parse().ok()?,
            c.get(3)?.as_str().parse().ok()?,
        )
    } else {
        return None;
    };

    let date = NaiveDate::from_ymd_opt(y, m, d)?;
    Some(
        Utc.from_utc_datetime(&date.and_hms_opt(0, 0, 0)?)
            .timestamp(),
    )
}

/// 从信息节点的名称文本中回退提取订阅信息。
pub fn extract_from_names<'a>(names: impl IntoIterator<Item = &'a str>) -> SubInfo {
    let mut info = SubInfo::default();
    for name in names {
        let n = name.trim();
        if n.is_empty() {
            continue;
        }
        if n.contains("剩余流量") || n.contains("流量") {
            if let Some(b) = parse_traffic_bytes(n) {
                info.total = info.total.or(Some(b));
            }
        }
        if n.contains("已用") || n.contains("已使用") {
            if let Some(b) = parse_traffic_bytes(n) {
                info.upload = info.upload.or(Some(b));
            }
        }
        if n.contains("到期") || n.contains("过期") || n.contains("有效期") {
            if let Some(ts) = parse_expire_date(n) {
                info.expire = info.expire.or(Some(ts));
            }
        }
    }
    info
}

/// 是否为信息/假节点。
pub fn is_fake_node(name: &str, keywords: &[String]) -> bool {
    let n = name.trim();
    if n.is_empty() {
        return true;
    }
    keywords
        .iter()
        .map(|k| k.trim())
        .filter(|k| !k.is_empty())
        .any(|k| n.contains(k))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_standard_userinfo_header() {
        let info =
            parse_userinfo("upload=1234; download=5678; total=107374182400; expire=1735689600");
        assert_eq!(info.upload, Some(1234));
        assert_eq!(info.download, Some(5678));
        assert_eq!(info.total, Some(107374182400));
        assert_eq!(info.expire, Some(1735689600));
    }

    #[test]
    fn userinfo_tolerates_partial_and_spaces() {
        let info = parse_userinfo(" upload = 10 ; total = 0 ; junk ");
        assert_eq!(info.upload, Some(10));
        assert_eq!(info.total, Some(0));
        assert!(info.expire.is_none());
        // total=0 视为不限量
        assert_eq!(info.remaining(), None);
    }

    #[test]
    fn parses_traffic_units() {
        assert_eq!(parse_traffic_bytes("1 GB"), Some(1024 * 1024 * 1024));
        assert_eq!(parse_traffic_bytes("1 TB"), Some(1024u64.pow(4)));
        assert_eq!(parse_traffic_bytes("512MB"), Some(512 * 1024 * 1024));
        assert_eq!(parse_traffic_bytes("2048 KB"), Some(2048 * 1024));
        assert_eq!(
            parse_traffic_bytes("剩余流量：123.45 GB"),
            Some((123.45f64 * 1024f64.powi(3)) as u64)
        );
        assert_eq!(parse_traffic_bytes("没有数字"), None);
    }

    #[test]
    fn parses_expire_dates() {
        let ts = parse_expire_date("套餐到期：2025-12-31").unwrap();
        assert_eq!(ts, 1767139200);
        assert!(parse_expire_date("套餐到期：2025/01/02").is_some());
        assert!(parse_expire_date("无日期").is_none());
    }

    #[test]
    fn extracts_info_from_fake_node_names() {
        let info = extract_from_names(vec![
            "剩余流量：100 GB",
            "套餐到期：2026-01-01",
            "🇭🇰 香港 01",
        ]);
        assert_eq!(info.total, Some(100 * 1024 * 1024 * 1024));
        assert!(info.expire.is_some());
    }

    #[test]
    fn fake_node_detection_uses_blacklist() {
        let kw = crate::store::model::default_filter_keywords();
        assert!(is_fake_node("剩余流量：123GB", &kw));
        assert!(is_fake_node("套餐到期：2025-12-31", &kw));
        assert!(is_fake_node("官网：https://example.com", &kw));
        assert!(!is_fake_node("🇭🇰 香港 01", &kw));
        assert!(!is_fake_node("🇯🇵 日本 02 | GPT", &kw));
    }

    #[test]
    fn fill_from_does_not_overwrite() {
        let mut a = SubInfo {
            upload: Some(1),
            ..Default::default()
        };
        a.fill_from(&SubInfo {
            upload: Some(99),
            total: Some(5),
            ..Default::default()
        });
        assert_eq!(a.upload, Some(1));
        assert_eq!(a.total, Some(5));
    }
}
