// SPDX-License-Identifier: AGPL-3.0-only

//! native 入口（HTTP 服务）。仅为 `cfg(not(target_arch = "wasm32"))` 编译。
//!
//! 模块声明在 `lib.rs`；这里只放进程级启动逻辑（参数、配置、监听、优雅停机）。
//! Workers 入口见 `src/workers/`。

use crate::{auth, config, geoip, geoip_update, pipeline, refresh, store, web};
use anyhow::{Context, Result, bail};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Debug, Default)]
struct Args {
    init: bool,
    config: Option<PathBuf>,
    version: bool,
    help: bool,
}

fn parse_args() -> Result<Args> {
    let mut out = Args::default();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--init" => out.init = true,
            "--version" | "-V" => out.version = true,
            "--help" | "-h" => out.help = true,
            "--config" => {
                out.config = Some(PathBuf::from(
                    args.next().context("--config 需要一个路径参数")?,
                ));
            }
            other if other.starts_with("--config=") => {
                out.config = Some(PathBuf::from(&other["--config=".len()..]));
            }
            other if other.starts_with('-') => bail!("未知参数：{other}（用 --help 查看用法）"),
            // 兼容旧用法：subforge-rs config.toml
            other => out.config = Some(PathBuf::from(other)),
        }
    }
    Ok(out)
}

fn print_help() {
    println!(
        "SubForge.rs {version}\n\
         \n\
         用法：\n\
           subforge-rs [--config PATH]        启动服务\n\
           subforge-rs --init [--config PATH] 只初始化配置与密码，然后退出\n\
           subforge-rs --version              显示版本\n\
           subforge-rs --help                 显示帮助\n\
         \n\
         环境变量：\n\
           SUBFORGE_PASSWORD       管理密码（优先级最高）\n\
           SUBFORGE_PASSWORD_FILE  从文件读取管理密码（适合 Docker secret）\n\
           SUBFORGE_CONFIG         配置文件路径\n\
           SUBFORGE_DATA_DIR       数据目录\n\
           SUBFORGE_STATIC_DIR     静态资源目录\n\
           SUBFORGE_LISTEN         监听地址，覆盖配置文件\n\
           RUST_LOG                日志级别，默认 info\n",
        version = env!("CARGO_PKG_VERSION")
    );
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
    tracing::info!("收到退出信号，正在优雅停止服务");
}

/// 启动 native 服务（由 `src/main.rs` 调用）。
pub async fn run() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args = parse_args()?;
    if args.version {
        println!("subforge-rs {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if args.help {
        print_help();
        return Ok(());
    }

    let env = config::EnvOverrides::from_env();
    let base = config::base_dir();
    let config_path = match args.config.clone() {
        Some(path) if path.is_absolute() => path,
        Some(path) => std::env::current_dir()
            .context("无法获取当前目录")?
            .join(path),
        None => config::default_config_path(&base, &env),
    };

    let boot = config::load_or_init(&config_path, &base, &env)?;
    if let Some(password) = &boot.generated_password {
        config::print_generated_password(password, &boot.config_path);
    }
    if args.init {
        tracing::info!("配置已就绪：{}", boot.config_path.display());
        tracing::info!("数据目录：{}", boot.data_dir.display());
        return Ok(());
    }
    boot.ensure_static_dir()?;

    let cfg = boot.config.clone();

    let storage = Arc::new(crate::storage::Storage::new(boot.data_dir.clone()));
    let store = store::StoreHandle::open(storage).await?;
    {
        let snapshot = store.snapshot().await;
        tracing::info!(
            "数据已加载：revision={} 上游={} 分组={} 规则={} 方案={}",
            snapshot.revision,
            snapshot.upstreams.len(),
            snapshot.groups.len(),
            snapshot.rules.len(),
            snapshot.profiles.len()
        );
    }
    tracing::info!("配置文件：{}", boot.config_path.display());
    tracing::info!("数据目录：{}", boot.data_dir.display());
    tracing::info!("静态资源：{}", boot.static_dir.display());
    let startup_proxy = {
        let snapshot = store.snapshot().await;
        let stored = (
            snapshot.settings.proxy.enabled,
            snapshot.settings.proxy.url.as_str(),
            snapshot.settings.proxy.configured,
        );
        let from_config = config::effective_proxy(&cfg);
        let resolved = crate::proxy::resolve(Some(stored), from_config);
        tracing::info!(
            "出口代理来源：{}（{}）",
            if snapshot.settings.proxy.configured {
                "网页设置"
            } else {
                "config.toml"
            },
            resolved
                .as_deref()
                .map(crate::proxy::redact)
                .unwrap_or_else(|| "直连".to_string())
        );
        resolved
    };

    let refresher = Arc::new(refresh::Refresher::with_proxy(
        store.clone(),
        cfg.fetch_timeout,
        cfg.fetch_concurrency,
        startup_proxy.as_deref(),
    )?);
    let geo = Arc::new(geoip::GeoResolver::load(&boot.data_dir.join("geoip"))?);
    let geoip_updater = Arc::new(geoip_update::GeoipUpdater::with_proxy(
        store.clone(),
        geo.clone(),
        startup_proxy.as_deref(),
    )?);

    let state = Arc::new(pipeline::AppState {
        cfg: cfg.clone(),
        store: store.clone(),
        refresher: refresher.clone(),
        geo,
        geoip: geoip_updater.clone(),
        csrf_secret: auth::random_hex(32),
        static_dir: boot.static_dir.clone(),
        render_cache: tokio::sync::RwLock::new(std::collections::HashMap::new()),
        node_names_cache: tokio::sync::RwLock::new(None),
    });

    // 启动后只刷新到期项（缓存缺失或超过 refresh_interval），未到期的不重拉；
    // 之后按 refresh_interval 定期检查（不阻塞监听）。
    let _periodic = refresher.spawn_periodic(cfg.refresh_interval);
    // GeoIP 库：启动后检查并按设置的间隔自动更新（不阻塞监听）
    let _geoip_periodic = geoip_updater.spawn_periodic();

    let app = web::router(state);
    let listener = tokio::net::TcpListener::bind(&cfg.listen)
        .await
        .with_context(|| format!("监听 {} 失败", cfg.listen))?;

    tracing::info!("管理界面：http://{}/", cfg.listen);
    tracing::info!("订阅输出：http://{}/sub/<slug>", cfg.listen);
    if !cfg.listen.starts_with("127.0.0.1") && !cfg.listen.starts_with("localhost") {
        tracing::warn!(
            "正在监听 {}，Basic Auth 在明文 HTTP 下会泄露凭据，请务必置于 HTTPS 之后",
            cfg.listen
        );
    }

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("HTTP 服务异常退出")?;
    Ok(())
}
