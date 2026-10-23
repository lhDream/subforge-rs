// SPDX-License-Identifier: AGPL-3.0-only

//! SubForge.rs 库入口。
//!
//! 同一份 crate 服务两个目标：
//! - native（默认）：`src/main.rs` → [`native::run`]，文件系统 + axum serve；
//! - wasm32（Cloudflare Workers）：[`workers`]，Worker fetch + 存储后端。
//!
//! 纯逻辑模块（render/match/region/rules/links/validate/cascade/store::model）
//! 两个目标共用；只有 IO/运行时相关的模块按目标门控。
//! 迁移设计见 `docs/workers-migration.md`。

// ---------------------------------------------------------------- 两个目标共用

pub mod auth;
pub mod cache;
pub mod cascade;
/// 响应校验/解析部分是纯逻辑；网络部分（`Fetcher`）仅 native。
pub mod fetch;
pub mod links;
pub mod region;
pub mod render;
pub mod rules;
pub mod storage;
pub mod store;
pub mod subinfo;
pub mod validate;

/// `match` 是 Rust 关键字，通过 `#[path]` 声明为 `matcher` 模块（AGENTS.md §9）。
#[path = "match.rs"]
pub mod matcher;

// ---------------------------------------------------------------- 平台实现
//
// 同名模块、两套实现：共享的 `web`/`pipeline` 只依赖 API，不关心后端。

#[cfg(not(target_arch = "wasm32"))]
#[path = "config.rs"]
pub mod config;
#[cfg(target_arch = "wasm32")]
#[path = "config_wasm.rs"]
pub mod config;

#[cfg(not(target_arch = "wasm32"))]
#[path = "geoip.rs"]
pub mod geoip;
#[cfg(target_arch = "wasm32")]
#[path = "geoip_wasm.rs"]
pub mod geoip;

#[cfg(not(target_arch = "wasm32"))]
#[path = "geoip_update.rs"]
pub mod geoip_update;
#[cfg(target_arch = "wasm32")]
#[path = "geoip_update_wasm.rs"]
pub mod geoip_update;

#[cfg(not(target_arch = "wasm32"))]
pub mod refresh;
#[cfg(target_arch = "wasm32")]
#[path = "refresh_wasm.rs"]
pub mod refresh;

pub mod lastgood;
pub mod pipeline;
pub mod web;

#[cfg(not(target_arch = "wasm32"))]
#[path = "proxy.rs"]
pub mod proxy;
#[cfg(target_arch = "wasm32")]
#[path = "proxy_wasm.rs"]
pub mod proxy;

// ---------------------------------------------------------------- native only

#[cfg(not(target_arch = "wasm32"))]
pub mod native;

// ---------------------------------------------------------------- wasm only

#[cfg(target_arch = "wasm32")]
pub mod workers;
