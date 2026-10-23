// SPDX-License-Identifier: AGPL-3.0-only

//! native 二进制入口。
//!
//! 真正的启动逻辑在 `subforge_rs::native::run()`（`src/native.rs`）；
//! 这样同一份业务代码可以被 wasm 目标复用（见 `src/workers/`）。

#[cfg(not(target_arch = "wasm32"))]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    subforge_rs::native::run().await
}

/// wasm 目标不需要这个二进制（Worker 入口在 lib 的 `workers` 模块）。
///
/// 保留一个空 `main`，这样 `cargo check --target wasm32-unknown-unknown`
/// 能覆盖整个包，而不必额外记 `--lib` 参数。
#[cfg(target_arch = "wasm32")]
fn main() {}
