//! 统一的持久化抽象：native = 文件系统，wasm = Durable Object / R2。
//!
//! key 是相对 `data/` 的 POSIX 路径，例如：
//! - `store.yaml`
//! - `cache/<upstream-id>.yaml`
//! - `cache/rendered/<slug>.yaml`
//! - `geoip/*.mmdb`、`geoip/status.yaml`
//! - `backups/store.<ts>.yaml`
//!
//! 两个实现都必须保证：**一次 put 就是一次原子提交**（native 用临时文件 + rename，
//! R2 单对象写入天然原子）。这是 AGENTS.md §4.1 / §8.1 的不变量。

use chrono::{DateTime, Utc};

/// 列表项（备份恢复、GeoIP 选最新库时用）。
#[derive(Debug, Clone)]
pub struct ObjectMeta {
    pub key: String,
    pub size: u64,
    pub modified: Option<DateTime<Utc>>,
}

#[cfg(not(target_arch = "wasm32"))]
mod fs_impl;
#[cfg(target_arch = "wasm32")]
mod wasm_impl;

#[cfg(not(target_arch = "wasm32"))]
pub use fs_impl::Storage;
#[cfg(target_arch = "wasm32")]
pub use wasm_impl::Storage;

/// 把 `data/` 下的相对路径校验成安全 key：禁止绝对路径与 `..`。
pub fn valid_key(key: &str) -> bool {
    !key.is_empty()
        && !key.starts_with('/')
        && !key.contains('\\')
        && key
            .split('/')
            .all(|seg| !seg.is_empty() && seg != "." && seg != "..")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_validation_rejects_traversal() {
        assert!(valid_key("store.yaml"));
        assert!(valid_key("cache/a.yaml"));
        assert!(!valid_key("../x"));
        assert!(!valid_key("a/../../b"));
        assert!(!valid_key("/abs"));
        assert!(!valid_key("a//b"));
        assert!(!valid_key("a\\b"));
    }
}
