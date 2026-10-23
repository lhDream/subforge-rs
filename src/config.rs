//! 服务设置（`config.toml`）。**网页不管理本文件**（含密码，属密钥）。
//!
//! 注意与 `data/store.yaml` 的 `general` 区分：这里是**服务自身**的设置，
//! `general` 是**会进入 Clash 导出结果**的客户端配置（AGENTS.md §5.1）。
//!
//! 发布形态（`docs/release-design.md`）：
//! - 便携包：默认配置/数据/静态资源都在可执行文件所在目录；
//! - Docker：通过 `SUBFORGE_*` 环境变量指定路径；
//! - 两种形态都**不携带密码**，首次运行自动生成或由环境变量指定。

use anyhow::{Context, Result, bail};
use base64::Engine;
use rand::RngCore;
use serde::{Deserialize, Deserializer};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default = "default_listen")]
    pub listen: String,

    /// 管理页访问密码。禁止硬编码；首次运行可自动生成。
    #[serde(default)]
    pub password: String,

    /// 上游定期刷新间隔。
    #[serde(default = "default_refresh", deserialize_with = "de_duration")]
    pub refresh_interval: Duration,

    /// 单个上游拉取超时。
    #[serde(default = "default_timeout", deserialize_with = "de_duration")]
    pub fetch_timeout: Duration,

    /// 拉取并发上限，避免被机场限流。
    #[serde(default = "default_concurrency")]
    pub fetch_concurrency: usize,

    /// 拉取上游、下载 GeoIP 时使用的出口代理（默认直连）。
    ///
    /// 只支持 `http(s)://` 与 `socks5(h)://`，通常填本机 Mihomo/Clash 的
    /// `mixed-port` 或 `socks-port`。空串 = 直连。
    #[serde(default)]
    pub proxy: String,

    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: default_listen(),
            password: String::new(),
            refresh_interval: default_refresh(),
            fetch_timeout: default_timeout(),
            fetch_concurrency: default_concurrency(),
            proxy: String::new(),
            data_dir: default_data_dir(),
        }
    }
}

fn default_listen() -> String {
    "127.0.0.1:8080".to_string()
}
fn default_refresh() -> Duration {
    Duration::from_secs(6 * 3600)
}
fn default_timeout() -> Duration {
    Duration::from_secs(30)
}
fn default_concurrency() -> usize {
    4
}
fn default_data_dir() -> PathBuf {
    PathBuf::from("data")
}

fn de_duration<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
    let s = String::deserialize(d)?;
    humantime::parse_duration(&s).map_err(serde::de::Error::custom)
}

/// 环境变量覆盖。测试直接构造，生产用 [`EnvOverrides::from_env`]。
#[derive(Debug, Clone, Default)]
pub struct EnvOverrides {
    pub password: Option<String>,
    pub password_file: Option<PathBuf>,
    pub config: Option<PathBuf>,
    pub data_dir: Option<PathBuf>,
    pub static_dir: Option<PathBuf>,
    pub listen: Option<String>,
}

impl EnvOverrides {
    pub fn from_env() -> Self {
        fn non_empty(key: &str) -> Option<String> {
            std::env::var(key).ok().filter(|v| !v.trim().is_empty())
        }
        fn path(key: &str) -> Option<PathBuf> {
            non_empty(key).map(PathBuf::from)
        }
        Self {
            password: non_empty("SUBFORGE_PASSWORD"),
            password_file: path("SUBFORGE_PASSWORD_FILE"),
            config: path("SUBFORGE_CONFIG"),
            data_dir: path("SUBFORGE_DATA_DIR"),
            static_dir: path("SUBFORGE_STATIC_DIR"),
            listen: non_empty("SUBFORGE_LISTEN"),
        }
    }
}

/// 启动引导结果。
#[derive(Debug, Clone)]
pub struct Bootstrap {
    pub config: Config,
    pub config_path: PathBuf,
    pub data_dir: PathBuf,
    pub static_dir: PathBuf,
    /// 本次首次运行自动生成的密码（仅用于打印一次；已有密码时为 None）。
    pub generated_password: Option<String>,
}

/// 可执行文件所在目录；开发环境（`cargo run`）下若 exe 目录没有 `static/`
/// 而当前目录有，则回退到当前目录，保证 `cargo run` 依旧可用。
pub fn base_dir() -> PathBuf {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf));
    let cwd = std::env::current_dir().ok();
    match (exe_dir, cwd) {
        (Some(exe), Some(cwd)) if !exe.join("static").is_dir() && cwd.join("static").is_dir() => {
            cwd
        }
        (Some(exe), _) => exe,
        (None, Some(cwd)) => cwd,
        (None, None) => PathBuf::from("."),
    }
}

/// 默认配置文件路径：显式 CLI / 环境变量优先，否则 `<base>/config.toml`。
pub fn default_config_path(base: &Path, env: &EnvOverrides) -> PathBuf {
    if let Some(p) = &env.config {
        return p.clone();
    }
    base.join("config.toml")
}

fn generate_password() -> String {
    let mut bytes = [0u8; 24];
    rand::thread_rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn toml_string(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn set_private_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

fn write_new_config(path: &Path, cfg: &Config) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("创建配置目录失败：{}", parent.display()))?;
    }
    let text = format!(
        "# SubForge.rs 配置文件（首次运行自动生成）\n\
         # 修改后重启服务生效。此文件含密码，不要提交到版本库。\n\
         \n\
         listen = {listen}\n\
         password = {password}\n\
         \n\
         # 上游定期刷新间隔（humantime 格式：30s / 10m / 6h / 1d）\n\
         refresh_interval = {refresh}\n\
         # 单个上游拉取超时\n\
         fetch_timeout = {timeout}\n\
         # 拉取并发上限\n\
         fetch_concurrency = {concurrency}\n\
         # 可选出口代理：拉取上游与下载 GeoIP 时使用；留空 = 直连。\n\
         # 支持 http:// 与 socks5://，通常填本机 Mihomo/Clash 的 mixed-port。\n\
         proxy = \"\"\n\
         # 业务数据与缓存目录（相对路径相对本配置文件所在目录解析）\n\
         data_dir = {data_dir}\n",
        listen = toml_string(&cfg.listen),
        password = toml_string(&cfg.password),
        refresh = toml_string(&humantime::format_duration(cfg.refresh_interval).to_string()),
        timeout = toml_string(&humantime::format_duration(cfg.fetch_timeout).to_string()),
        concurrency = cfg.fetch_concurrency,
        data_dir = toml_string(&cfg.data_dir.to_string_lossy()),
    );
    crate::store::atomic_write(path, &text)?;
    set_private_permissions(path);
    Ok(())
}

/// 只替换已有配置里的 `password = ...`，保留注释与其它字段。
fn write_password_into(path: &Path, password: &str) -> Result<()> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("读取配置文件失败：{}", path.display()))?;
    let mut out = String::with_capacity(text.len() + 64);
    let mut replaced = false;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if !replaced && trimmed.starts_with("password") && trimmed.contains('=') {
            out.push_str(&format!("password = {}\n", toml_string(password)));
            replaced = true;
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    if !replaced {
        out.push_str(&format!("password = {}\n", toml_string(password)));
    }
    crate::store::atomic_write(path, &out)?;
    set_private_permissions(path);
    Ok(())
}

/// 读取密码文件并去掉首尾空白。
fn read_password_file(path: &Path) -> Result<String> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("读取密码文件失败：{}", path.display()))?;
    let password = text.trim().to_string();
    if password.is_empty() {
        bail!("密码文件为空：{}", path.display());
    }
    Ok(password)
}

/// 加载或初始化配置。首次运行（文件不存在、密码为空或仍是占位符）时自动生成密码并写盘。
pub fn load_or_init(path: &Path, base: &Path, env: &EnvOverrides) -> Result<Bootstrap> {
    let existed = path.exists();
    let mut cfg: Config = if existed {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("读取配置文件失败：{}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("解析配置文件失败：{}", path.display()))?
    } else {
        Config::default()
    };

    if let Some(listen) = &env.listen {
        cfg.listen = listen.clone();
    }

    // 密码优先级：环境变量 > 密码文件 > 已有配置 > 自动生成。
    let env_password = match (&env.password, &env.password_file) {
        (Some(p), _) => Some(p.clone()),
        (None, Some(file)) => Some(read_password_file(file)?),
        (None, None) => None,
    };

    let mut generated_password = None;
    if let Some(password) = env_password {
        cfg.password = password;
    } else if cfg.password.trim().is_empty() || cfg.password == "changeme" {
        let password = generate_password();
        cfg.password = password.clone();
        generated_password = Some(password);
    }

    validate(&mut cfg)?;

    if !existed {
        write_new_config(path, &cfg)?;
    } else if generated_password.is_some() {
        write_password_into(path, &cfg.password)?;
    }

    // data_dir：环境变量优先；相对路径相对配置文件所在目录解析，避免依赖 CWD。
    let data_dir = match &env.data_dir {
        Some(dir) if dir.is_absolute() => dir.clone(),
        Some(dir) => std::env::current_dir()
            .context("无法获取当前目录")?
            .join(dir),
        None if cfg.data_dir.is_absolute() => cfg.data_dir.clone(),
        None => path.parent().unwrap_or(base).join(&cfg.data_dir),
    };

    let static_dir = env
        .static_dir
        .clone()
        .unwrap_or_else(|| base.join("static"));

    Ok(Bootstrap {
        config: cfg,
        config_path: path.to_path_buf(),
        data_dir,
        static_dir,
        generated_password,
    })
}

impl Bootstrap {
    /// 启动服务前检查静态资源目录。`--init` 只生成配置，不要求 static 已存在。
    pub fn ensure_static_dir(&self) -> Result<()> {
        if self.static_dir.is_dir() {
            return Ok(());
        }
        bail!(
            "静态资源目录不存在：{}。请确认发布包完整（应包含 static/ 目录），或用 SUBFORGE_STATIC_DIR 指定。",
            self.static_dir.display()
        )
    }
}

/// 打印首次启动横幅。只在自动生成密码时调用。
pub fn print_generated_password(password: &str, config_path: &Path) {
    println!();
    println!("============================================================");
    println!(" SubForge.rs 首次启动");
    println!(" 已自动生成管理密码：");
    println!();
    println!("   {password}");
    println!();
    println!(" 请立即保存。此密码不会再次显示。");
    println!(" 配置文件：{}", config_path.display());
    println!(" 如需自行指定，请设置 SUBFORGE_PASSWORD 后删除该配置文件。");
    println!("============================================================");
    println!();
}

fn validate(cfg: &mut Config) -> Result<()> {
    if cfg.password.trim().is_empty() {
        bail!("password 不能为空");
    }
    if cfg.fetch_concurrency == 0 {
        bail!("fetch_concurrency 必须大于 0");
    }
    if cfg.refresh_interval.is_zero() {
        bail!("refresh_interval 必须大于 0");
    }
    if cfg.fetch_timeout.is_zero() {
        bail!("fetch_timeout 必须大于 0");
    }
    // 代理写错不能拖垮启动：只告警并回落直连。默认直连，行为与此前一致。
    match crate::proxy::normalize(&cfg.proxy) {
        Ok(Some(address)) => {
            cfg.proxy = address.clone();
            tracing::info!("出口代理：{}", crate::proxy::redact(&address));
        }
        Ok(None) => {
            cfg.proxy.clear();
            tracing::info!("出口代理：未配置（直连）");
        }
        Err(e) => {
            tracing::warn!("{e}；代理配置无效，本次仍按直连运行，请修正 config.toml 的 proxy");
            cfg.proxy.clear();
        }
    }
    Ok(())
}

/// 规范化后的代理地址（`None` = 直连）。
pub fn effective_proxy(cfg: &Config) -> Option<String> {
    crate::proxy::normalize(&cfg.proxy).ok().flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(tag: &str) -> PathBuf {
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "subforge-config-test-{tag}-{}-{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn touch_static(base: &Path) {
        std::fs::create_dir_all(base.join("static")).unwrap();
    }

    #[test]
    fn first_run_generates_password_and_config() {
        let dir = temp_dir("first-run");
        touch_static(&dir);
        let path = dir.join("config.toml");
        let boot = load_or_init(&path, &dir, &EnvOverrides::default()).unwrap();
        assert!(boot.generated_password.is_some());
        assert!(path.exists());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains(&boot.config.password));
        assert_eq!(boot.data_dir, dir.join("data"));
        assert_eq!(boot.static_dir, dir.join("static"));
    }

    #[test]
    fn second_run_reuses_password() {
        let dir = temp_dir("second-run");
        touch_static(&dir);
        let path = dir.join("config.toml");
        let first = load_or_init(&path, &dir, &EnvOverrides::default()).unwrap();
        let password = first.config.password.clone();
        let second = load_or_init(&path, &dir, &EnvOverrides::default()).unwrap();
        assert!(second.generated_password.is_none());
        assert_eq!(second.config.password, password);
    }

    #[test]
    fn env_password_wins_and_is_not_printed() {
        let dir = temp_dir("env-password");
        touch_static(&dir);
        let path = dir.join("config.toml");
        let env = EnvOverrides {
            password: Some("from-env".to_string()),
            ..Default::default()
        };
        let boot = load_or_init(&path, &dir, &env).unwrap();
        assert_eq!(boot.config.password, "from-env");
        assert!(boot.generated_password.is_none());
    }

    #[test]
    fn placeholder_password_is_replaced() {
        let dir = temp_dir("placeholder");
        touch_static(&dir);
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            "listen = \"127.0.0.1:8080\"\npassword = \"changeme\"\n",
        )
        .unwrap();
        let boot = load_or_init(&path, &dir, &EnvOverrides::default()).unwrap();
        assert!(boot.generated_password.is_some());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("changeme"));
        assert!(text.contains(&boot.config.password));
        // 注释/其它字段保留
        assert!(text.contains("listen"));
    }

    #[test]
    fn relative_data_dir_is_resolved_against_config_dir() {
        let dir = temp_dir("data-dir");
        touch_static(&dir);
        let sub = dir.join("etc");
        std::fs::create_dir_all(&sub).unwrap();
        let path = sub.join("config.toml");
        std::fs::write(&path, "password = \"x\"\ndata_dir = \"mydata\"\n").unwrap();
        let boot = load_or_init(&path, &dir, &EnvOverrides::default()).unwrap();
        assert_eq!(boot.data_dir, sub.join("mydata"));
    }

    #[test]
    fn env_listen_overrides_config() {
        let dir = temp_dir("listen");
        touch_static(&dir);
        let path = dir.join("config.toml");
        let env = EnvOverrides {
            listen: Some("0.0.0.0:9999".to_string()),
            ..Default::default()
        };
        let boot = load_or_init(&path, &dir, &env).unwrap();
        assert_eq!(boot.config.listen, "0.0.0.0:9999");
    }

    #[test]
    fn missing_static_dir_is_an_error() {
        let dir = temp_dir("no-static");
        let path = dir.join("config.toml");
        let boot = load_or_init(&path, &dir, &EnvOverrides::default()).unwrap();
        let err = boot.ensure_static_dir().unwrap_err();
        assert!(err.to_string().contains("静态资源目录不存在"));
    }
}
