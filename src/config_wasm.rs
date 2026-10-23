//! Workers 侧配置：没有 `config.toml`，全部来自 bindings。
//!
//! 与 native `config::Config` 保持同名字段，供共享的 `web`/`pipeline` 直接使用。

use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Config {
    /// Workers 上没有本地监听地址；保留字段只为与 native 结构一致。
    pub listen: String,
    pub password: String,
    pub refresh_interval: Duration,
    pub fetch_timeout: Duration,
    pub fetch_concurrency: usize,
    pub proxy: String,
    pub data_dir: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: String::new(),
            password: String::new(),
            refresh_interval: Duration::from_secs(6 * 3600),
            fetch_timeout: Duration::from_secs(30),
            fetch_concurrency: 4,
            proxy: String::new(),
            data_dir: PathBuf::new(),
        }
    }
}

impl Config {
    /// 从 Worker bindings 读取运行配置。
    pub fn from_env(env: &worker::Env) -> Self {
        let password = env
            .secret("SUBFORGE_PASSWORD")
            .ok()
            .map(|s| s.to_string())
            .or_else(|| env.var("SUBFORGE_PASSWORD").ok().map(|v| v.to_string()))
            .unwrap_or_default();
        let refresh_hours = env
            .var("SUBFORGE_REFRESH_HOURS")
            .ok()
            .map(|v| v.to_string())
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(6);
        let concurrency = env
            .var("SUBFORGE_FETCH_CONCURRENCY")
            .ok()
            .map(|v| v.to_string())
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(4);
        Self {
            listen: String::new(),
            password,
            refresh_interval: Duration::from_secs(refresh_hours.max(1) * 3600),
            fetch_timeout: Duration::from_secs(30),
            fetch_concurrency: concurrency.clamp(1, 6),
            proxy: String::new(),
            data_dir: PathBuf::new(),
        }
    }
}

/// Workers Fetch 不支持自定义出口代理（AGENTS.md 迁移文档 §5）。
pub fn effective_proxy(_cfg: &Config) -> Option<String> {
    None
}
