# SubForge.rs → Cloudflare Workers 迁移设计

> 状态：P0 完成（2026-10-23）
> 创建：2026-10-23
> 关联：[AGENTS.md](../AGENTS.md)、[docs/release-design.md](release-design.md)

## 1. 目标与边界

**目标**：让同一份业务代码既能以现有单体服务运行，也能编译为 wasm 运行在 Cloudflare Workers，
本地 `wrangler dev` 可启动，线上可部署。

**硬约束**（不可破坏）：

- native 构建、现有测试、`/sub/{slug}` 输出语义保持不变；Workers 是**新增目标**，不是替换。
- 数据格式（`store.yaml`、缓存信封、`FORMAT_VERSION`）不变，便于 native ↔ Workers 迁移。
- 认证/CSRF/引用完整性/导出管线等业务语义不变（AGENTS.md §6、§7）。
- 工作区里未提交的本地改动不丢（迁移期间不执行 `git reset`/`checkout` 覆盖）。

**明确不做**：

- 不承诺 Workers 上保留 `proxy` 出口代理（Workers Fetch 不支持自定义代理）。
- 不承诺 GeoIP 的域名解析与 native 完全等价（改用 DoH，见 §5）。
- 不引入前端构建链（§2 约束不变）。

## 2. 目标架构

```
                        ┌──────────────────────────────┐
 浏览器/客户端 ────────>│ Worker (wasm)                │
                        │  · axum Router（适配层）      │
                        │  · Basic Auth / CSRF         │
                        │  · 静态资源（include_str!）  │
                        └───────────┬──────────────────┘
                                    │ RPC / fetch
                        ┌───────────▼──────────────────┐
                        │ Durable Object «core»        │
                        │  · Store 快照（唯一写者）     │
                        │  · revision 乐观锁            │
                        │  · 刷新 in-flight 合并        │
                        │  · 渲染缓存 / last_good       │
                        └───────┬───────────┬──────────┘
                                │           │
                    ┌───────────▼──┐   ┌────▼─────────────┐
                    │ DO SQLite    │   │ R2               │
                    │ store 元数据  │   │ 上游缓存信封      │
                    │ 备份索引      │   │ 备份正文          │
                    └──────────────┘   │ GeoIP mmdb       │
                                       └──────────────────┘
  Cron Trigger ──> scheduled handler ──> DO.refresh_due() / GeoIP 周检
```

**为什么用 DO 而不是 D1 直接写**：现有 `StoreHandle` 的语义是「单写者 + 内存快照 +
revision 乐观锁 + 所有读共用同一份快照」。DO 每实例单线程执行、天然串行，和这套语义
一一对应；用 D1 则要在每个 isolate 里重建快照并自己解决跨 isolate 竞态，改动更大。

**为什么缓存仍用单对象信封**：AGENTS.md §4.1 要求正文与元数据一起提交。R2 单对象
`put` 是原子的，把整个 `CacheEnvelope` 序列化成 YAML 存一个对象即可，逻辑可以原样复用。

## 3. 模块映射

| 现有 | Workers 实现 |
| --- | --- |
| `std::fs`（store/cache/lastgood/geoip） | 存储 trait：native = 文件系统；wasm = DO SQLite / R2 |
| `StoreHandle` 的写锁 + 原子 rename | DO 串行 RPC + DO SQLite 事务 |
| `data/backups/` | DO SQLite 索引 + R2 正文 |
| `config.toml` | Worker `vars` + `secrets`（`SUBFORGE_PASSWORD`、`SUBFORGE_CSRF_SECRET`） |
| `#[tokio::main]` + axum serve | `#[event(fetch)]` + axum `oneshot` 适配层 |
| `tokio::spawn` 定期刷新 | `#[event(scheduled)]` + DO alarm |
| reqwest | `worker::Fetch`（条件请求、UA、超时用 Delay 竞速） |
| `tokio::net::lookup_host` | DoH（`cloudflare-dns.com/dns-query`），结果缓存 |
| `tower_http::ServeDir` | `include_str!` 内嵌三个静态文件（约 59KB） |
| `Utc::now()` | chrono `wasmbind` |
| `rand` | getrandom `js` 特性 |
| `tracing-subscriber` | `worker::console_log` / console_error_panic_hook |

**保留不动的纯逻辑**（约 6,000 行）：`render`、`match`、`region`、`rules`、`links`、
`validate`、`cascade`、`store/model`、`store/migrate`、`subinfo`。

## 4. 分阶段计划

| 阶段 | 内容 | 验收 |
| --- | --- | --- |
| **P0** | lib/bin 拆分；wasm 依赖与 cfg 门控；Worker 入口 + axum 适配；`/healthz`；设计文档 | native `cargo test` 绿；`cargo check --target wasm32-unknown-unknown` 绿 |
| **P1** | 存储 trait：`StorePersistence` / `CachePersistence` / `ArtifactPersistence`；native 文件实现；全部调用点改造 | native 行为与测试不变（203 项） |
| **P2** | DO `core` + R2 实现；Worker 启动时加载快照；`/healthz`+只读页面 | `wrangler dev` 可打开首页（空数据） |
| **P3** | 导出链路：缓存解析、地区判定、渲染、`/sub/{slug}`；`Fetch` 拉取上游 | `wrangler dev` 下导入 `store.yaml` 后能导出 YAML |
| **P4** | 管理页写入：表单、CSRF、revision、备份/恢复；静态资源 | 主要 CRUD 在 `wrangler dev` 可用 |
| **P5** | Cron 刷新、GeoIP（R2 + DoH + 自动更新）、`last_good` | 定时触发与 GeoIP 降级符合 §3.5/§4.2 |
| **P6** | `wrangler.toml`、部署文档、数据导入工具、CI | 可重复部署；native 与 Workers 双目标文档齐 |

## 5. 关键决策与取舍

1. **D1 不引入**：单人自用、数据量小，DO SQLite + R2 已覆盖；少一个组件少一层一致性风险。
2. **GeoIP 域名解析**：Workers 无 DNS API，改用 DoH；解析失败按 §3.5 降级（只用节点名），
   不阻断导出。
3. **`proxy` 设置**：Workers 上忽略并提示；保留字段以兼容数据格式。
4. **配置**：`config.toml` 不参与 wasm 构建；密码/CSRF 密钥走 secrets，其余走 vars。
   `csrf_secret` 必须固定（否则多 isolate 下 CSRF token 全部失效）。
5. **测试**：存储 trait 让纯逻辑测试继续在 native 跑；wasm 侧只做 `cargo check` + 手工
   `wrangler dev` 验收，避免为 wasm 维护一套测试运行时。
6. **部署形态**：Worker 单实例 + 单 DO；不按用户分片（单人自用）。

## 6. 风险

| 风险 | 影响 | 缓解 |
| --- | --- | --- |
| axum 0.8 在 wasm 上的兼容性 | P0 阻塞 | 先做 spike；必要时降级为手写 `worker::Router`，但尽量保 axum |
| 128MB 内存 | 大订阅解析失败 | 单上游解析、及时释放；必要时限制单次导出规模 |
| 6 并发出站连接 | 刷新排队 | `fetch_concurrency` 上限 6，其余排队 |
| CPU 10ms（免费版） | 免费版不可用 | 文档明确要求 Workers Paid |
| DO 单点 | 全站串行 | 单人自用可接受；监控 P99 |
| 缓存对象体积 | R2 单对象过大 | 上限检查，超限告警 |

## 7. 本地验证

```bash
# native（本机 MSVC 环境需要关闭 debuginfo，否则 ring 的 C 编译会触发 cl.exe D8050）
CARGO_PROFILE_DEV_DEBUG=0 cargo test
CARGO_PROFILE_DEV_DEBUG=0 cargo clippy --all-targets -- -D warnings
cargo fmt --check

# wasm 编译检查 / 产物
cargo check --target wasm32-unknown-unknown
cargo build --target wasm32-unknown-unknown --lib
```

Windows 上跑测试时还需把 `TEMP`/`TMP` 指到工作区内（`target/test-temp`），
系统临时目录在当前环境会被拒绝访问。

## 8. 验证记录

| 日期 | 阶段 | 结果 |
| --- | --- | --- |
| 2026-10-23 | P0 | `cargo test` **203 通过**；`cargo check`/`cargo build --lib --target wasm32-unknown-unknown` 通过；`cargo fmt --check`、`cargo clippy --all-targets -- -D warnings` 通过（native）。wasm 产物 `subforge_rs.wasm`（debug）已生成，尚未做 `worker-build` 打包与 `wrangler dev` 运行验证（P2）。 |
| 2026-10-23 | 工具链 | Rust 从 1.88 升到 **1.99.1**（worker 0.8.7 要求 1.91+）；升级后 clippy 新增的 15 条 lint 已修复。 |
