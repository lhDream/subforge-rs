# SubForge.rs → Cloudflare Workers 迁移设计

> 状态：P0 完成（2026-10-23）
> 创建：2026-10-23
> 关联：`AGENTS.md`（内部开发文档，不随仓库分发）、[docs/release-design.md](release-design.md)

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
| **P0** ✅ | lib/bin 拆分；wasm 依赖与 cfg 门控；Worker 入口 + axum 适配；`/healthz`；设计文档 | native `cargo test` 绿；`cargo check --target wasm32-unknown-unknown` 绿 |
| **P1** ✅ | Workers MVP：DO「Core」单写者存 store（revision + 上一版）、R2 缓存、导出链路、Basic Auth、`/admin/store` 导入；`wrangler.jsonc` | `worker-build` + `wrangler dev` 下：导入 native `store.yaml` → `/sub/{slug}` 返回可加载的 Clash YAML |
| **P2** ✅ | 存储抽象：`Storage`（native 文件系统 / wasm DO+R2）；`cache`/`lastgood`/`store` 异步化；原生 205 测试保持绿 | native 行为不变；`cargo test` / clippy 双目标通过 |
| **P3** ✅ | wasm 平台实现：`config`（vars/secrets）、`refresh`（Fetch）、`geoip`（R2 + 无 DNS 降级）、`geoip_update`（Fetch + R2）、`proxy`（校验/脱敏）；解除 `web`/`pipeline` 门控，管理页整体上线 | 线上 9 个管理页 + 静态资源 200；表单写入 303；`/sub/{slug}` 导出正常 |
| **P4** ✅ | Cron 定时刷新（`15 */6 * * *`）+ GeoIP 按期更新；`#[event(scheduled)]` 复用 `refresh_due` 与到期判断 | 本地 `/cdn-cgi/local/scheduled` 200；部署输出注册 `schedule: 15 */6 * * *` |
| **P5** | 部署文档、数据导入工具、CI；Workers 上的 ETag/304 与 `last_good` 实测 | 可重复部署；双目标文档齐 |

### 4.1 迁移策略：向共享层收敛（当前）

不再在 Workers 侧另写一套页面。当前路线是**把 native 的 `store`/`cache`/`pipeline`/`web`
做成两个目标共用**，只把 IO 换成平台实现：

- 已落地：`src/storage.rs` + `store`/`cache`/`lastgood` 异步化（native 文件系统，wasm DO+R2）；
- 下一步：给 `config`/`refresh`/`geoip`/`geoip_update` 补 wasm 实现，然后解除
  `web`/`pipeline` 的 `cfg(not(wasm32))`，管理页与导出共用同一份 handler 与模板。

这样功能对齐是"同一份代码跑两个目标"，而不是两套实现各自漂移。代价是 wasm 侧必须
提供与 native 同名的模块 API（例如 `Refresher::refresh_one`、`GeoResolver::region_inputs`），
在 P3 补齐。

## 5. 关键决策与取舍

1. **D1 不引入**：单人自用、数据量小，DO SQLite + R2 已覆盖；少一个组件少一层一致性风险。
2. **GeoIP 域名解析**：Workers 无 DNS API，改用 DoH；解析失败按 §3.5 降级（只用节点名），
   不阻断导出。
3. **`proxy` 设置**：Workers 上忽略并提示；保留字段以兼容数据格式。
4. **配置**：`config.toml` 不参与 wasm 构建；密码/CSRF 密钥走 secrets，其余走 vars。
   `csrf_secret` 必须固定（否则多 isolate 下 CSRF token 全部失效）。
5. **测试**：纯逻辑继续用 native 的 203 项测试覆盖；wasm 侧用 `worker-build` + `wrangler dev`
   做端到端验收，避免为 wasm 维护一套测试运行时。
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
worker-build --release          # 产出 build/index.js + build/index_bg.wasm

# 本地 Worker（首次会由 npx 下载 wrangler；需要 Node）
npx wrangler dev --port 8787
```

Windows 上跑测试时还需把 `TEMP`/`TMP` 指到工作区内（`target/test-temp`），
系统临时目录在当前环境会被拒绝访问。本地 `wrangler dev` 读取 `.dev.vars`
（已被 `.gitignore` 忽略）里的 `SUBFORGE_PASSWORD`。

### 7.1 从 native 导入数据并验证导出

```bash
# 导出 native 的 store（含 token，注意不要提交/贴出）
cp data/store.yaml target/import.yaml

# 校验/导入（DO 会迁移 + 结构校验 + revision 递增）
curl -u admin:$SUBFORGE_PASSWORD --data-binary @target/import.yaml \
  http://127.0.0.1:8787/admin/store

# 订阅导出
curl -u admin:$SUBFORGE_PASSWORD http://127.0.0.1:8787/sub/<slug>
```

首次导出会从 R2 读缓存；缺失时由 Worker Fetch 同步拉取一次并写回 R2，
之后复用缓存（与 native 的「导出时缓存缺失才拉取」一致）。

### 7.2 线上部署（已验证）

测试实例（Cloudflare 账号 `luhuadream@gmail.com`）：

```
https://subforge-rs.luhuadream.workers.dev
```

部署步骤（`wrangler` 已登录后）：

```bash
# 1. R2 桶只需建一次
npx wrangler r2 bucket create subforge-cache

# 2. 管理密码（随机、只存 Cloudflare secret）
npx wrangler secret put SUBFORGE_PASSWORD

# 3. 部署（wrangler 会调用 worker-build --release）
npx wrangler deploy

# 4. 导入 native 的 store（含 token，命令不要贴到公开场合）
curl -u admin:<密码> --data-binary @data/store.yaml \
  https://subforge-rs.<subdomain>.workers.dev/admin/store

# 5. 手动刷新全部上游（Cron 落地前用它）
curl -u admin:<密码> -X POST \
  https://subforge-rs.<subdomain>.workers.dev/admin/refresh-all
```

注意：`wrangler r2 object get/put` **默认操作本地模拟存储**，查看线上对象要加 `--remote`：

```bash
npx wrangler r2 object get subforge-cache/cache/<id>.yaml --remote --file out.yaml
```

管理端点在线上均要求 Basic Auth；`/healthz`、`/sub/{slug}`（Basic Auth 或方案令牌）按设计暴露。

## 8. 验证记录

| 日期 | 阶段 | 结果 |
| --- | --- | --- |
| 2026-10-23 | P0 | `cargo test` **203 通过**；`cargo check`/`cargo build --lib --target wasm32-unknown-unknown` 通过；`cargo fmt --check`、`cargo clippy --all-targets -- -D warnings` 通过（native）。wasm 产物 `subforge_rs.wasm`（debug）已生成。 |
| 2026-10-23 | P1（MVP） | `worker-build --release` 产出 `build/index.js` + `build/index_bg.wasm`（约 2.5MB）；`wrangler dev 4.152.0` 本地启动成功（DO + R2 绑定）。实测：`/healthz` 公开 200；`/` 未认证 401、认证后 200；`POST /admin/store` 导入后 `revision=1`；`GET /sub/test` 返回 `text/yaml`，含 4 个节点、2 个分组、`MATCH` 规则。上游拉取使用本地 fixture 服务（避免请求真实机场），缓存写入 R2。 |
| 2026-10-23 | P1 线上 | 部署到 `https://subforge-rs.luhuadream.workers.dev`（Worker Startup 7ms；上传 2.5MB / gzip 856KB）。实测：`/healthz` 200、`/` 401/200；导入 store `revision=2`；`/sub/test` 200 `text/yaml`；上游 `https://example.com/` 解析失败按 §4.4 降级（`x-subforge-warnings` 可见），R2 写入 `cache/r2test.yaml`（587B，`--remote` 读取确认）；`POST /admin/refresh-all` 返回各上游状态。测试数据不含真实订阅 token。 |
| 2026-10-23 | P2 存储抽象 | 新增 `Storage`（get/put/delete/list）：native = 文件系统原子写，wasm = DO `/obj/*` + R2。`cache`/`lastgood`/`store` 改为 async + Storage；备份改用对象名。`cargo test` **205 通过**，native/wasm clippy、fmt 通过。线上版本未重新部署。 |
| 2026-10-23 | P3 页面整体上线 | 线上 `https://subforge-rs.luhuadream.workers.dev` 实测：`/`、`/upstreams`、`/nodes`、`/groups`、`/rules`、`/aliases`、`/general`、`/backups`、`/profiles/new`、`/static/*` 全部 200（Basic Auth `admin`）。表单写入：POST 新建上游 303 并落库；新建方案（含依赖闭包）303；`/sub/test` 200 `text/yaml`，含手动节点、拓扑排序后的分组与 `MATCH` 规则；上游刷新 303；备份列表 4 份、`?preview=` 200。期间修复：DO 调用误用普通 Fetch、`/list` 未解析 query 导致备份名错乱。 |
| 2026-10-23 | 工具链 | Rust 从 1.88 升到 **1.99.1**（worker 0.8.7 要求 1.91+）；升级后 clippy 新增的 15 条 lint 已修复。`worker-build 0.8.7` 已安装。 |
