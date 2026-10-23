# 上游配置预览与手动编辑 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在上游管理页面打开当前缓存原文，支持编辑、校验、保存；首次同步失败、没有缓存时也可以粘贴有效配置使用。

**Architecture:** 继续使用原始文本缓存信封，不增加独立手动覆盖层。新增受管理认证保护的 GET/POST `/upstreams/{id}/config`，使用 Askama 服务端页面及普通表单；保存替换当前缓存，后续同步成功可覆盖。复用 `fetch::validate_and_parse`，手动内容必须通过与同步相同的校验；提交前使用缓存版本指纹防止旧表单或在途同步覆盖新内容。

**Tech Stack:** Rust 2024、axum 0.8、Askama 0.14、tokio、serde_yaml_ng、现有 sha2。

---

## 已确认的行为与边界

- 用户选择：**替换当前缓存，后续同步可覆盖**，不新增持久手动版本与模式切换。
- 预览显示缓存中的原始文本，而非重新序列化的节点；页面不主动拉取上游。
- 支持 Clash YAML、现有解析器支持的明文/整体 base64 分享链接。
- 无缓存时文本框为空，说明可以粘贴配置；不可用、空内容、过滤后零节点拒绝保存。
- 手动保存清除旧的 HTTP ETag、Last-Modified 及 forced_at，流量/到期只从本次文本回退提取，不沿用旧响应头数据。
- 手动保存更新缓存成功时间与统计，明确显示“手动保存可用，并非网络同步成功”；建议缓存 meta 新增可选 `manual_saved_at`，成功网络同步清除此标记，失败保留。
- 导出仍仅从上游正文抽取节点。上游自己的 general、分组、规则不会因此成为方案的输出配置。
- 页面认证、CSRF、Origin/Referer 检查沿用现有机制。含代理密码的原文不进入日志，HTML 自动转义，配置页面响应 `Cache-Control: no-store`。
- 错误页必须保留用户输入；成功 POST 返回 303 到同一配置页面，显示成功提示。
- 不更改真实 data、config.toml，不自动部署、不提交 Git。

## 文件职责

| 文件 | 职责 |
| --- | --- |
| `src/cache.rs` | 可选手动保存标记、缓存指纹、手动正文提交及其单元测试 |
| `src/refresh.rs` | 手动保存入口、提交互斥、刷新结果版本检查，避免在途请求覆盖手动保存 |
| `src/store/mod.rs` | 如需覆盖删除竞态，提供只读快照锁内执行短提交回调，不增加 store 写入或备份 |
| `src/web/upstream_config.rs`（新建） | 配置页面视图模型、GET/POST handler、错误回填与页面测试；避免继续扩大 pages.rs |
| `src/web/mod.rs` | 声明新模块，注册 GET/POST 管理路由 |
| `templates/upstream_config.html`（新建） | 原文预览、文本编辑、格式/节点/过滤统计、保存操作与覆盖提示 |
| `templates/upstreams.html` | 每行新增“预览 / 编辑配置”普通链接，无缓存时同样可用 |
| `README.md`、`AGENTS.md` | 使用方法、缓存替换语义及新路由 |

## Task 1: 原文缓存提交与指纹

**Files:** Modify `src/cache.rs`; tests in its existing `tests` module.

- [ ] **Step 1: 增加失败测试，约束原文、元数据及兼容性**

```rust
#[test]
fn manual_body_keeps_text_and_clears_remote_validators() {
    let mut env = CacheEnvelope::default();
    env.commit_success("old".into(), &info(), Some("remote".into()), Some("date".into()), 2, Utc::now());
    env.meta.forced_at = Some(Utc::now());
    let body = "# 保留注释\nproxies:\n  - name: HK\n    type: socks5\n    server: 127.0.0.1\n    port: 1080\n";
    let parsed = crate::fetch::validate_and_parse(body, &[], false).unwrap();
    let now = Utc::now();
    env.commit_manual(body.into(), &parsed, now);
    assert_eq!(env.body, body);
    assert_eq!(env.meta.node_count, 1);
    assert!(env.meta.etag.is_none());
    assert!(env.meta.last_modified.is_none());
    assert!(env.meta.expire.is_none());
    assert!(env.meta.forced_at.is_none());
    assert_eq!(env.meta.manual_saved_at, Some(now));
    assert!(env.has_content());
}

#[test]
fn cache_fingerprint_changes_for_equal_length_body_edits() {
    let mut env = CacheEnvelope::default();
    env.body = "one".into();
    let before = fingerprint(Some(&env)).unwrap();
    env.body = "two".into();
    assert_ne!(before, fingerprint(Some(&env)).unwrap());
    assert_ne!(before, fingerprint(None).unwrap());
}
```

- [ ] **Step 2: 运行 `cargo test cache::tests`，确认新增方法/字段未实现导致失败。**
- [ ] **Step 3: 添加字段和实现。**

`CacheMeta` 添加 `#[serde(default)] pub manual_saved_at: Option<DateTime<Utc>>`；`commit_success` 清除此字段，`commit_failure` 不改它。新增：

```rust
pub fn fingerprint(env: Option<&CacheEnvelope>) -> Result<String> {
    use sha2::{Digest, Sha256};
    let serialized = serde_yaml_ng::to_string(&env)?;
    Ok(format!("{:x}", Sha256::digest(serialized.as_bytes())))
}
```

在 `impl CacheEnvelope` 添加：

```rust
pub fn commit_manual(&mut self, body: String, parsed: &crate::fetch::ParsedBody, now: DateTime<Utc>) {
    self.commit_success(body, &parsed.fallback_info, None, None, parsed.proxies.len(), now);
    self.meta.source_format = Some(parsed.source_format.clone());
    self.meta.skipped_links = parsed.skipped_links.len();
    self.meta.forced_at = None;
    self.meta.manual_saved_at = Some(now);
}
```

- [ ] **Step 4: 运行 `cargo test cache::tests`，所有缓存测试通过，旧信封反序列化测试覆盖缺失新字段。**

## Task 2: 并发安全的手动保存与刷新提交

**Files:** Modify `src/refresh.rs`, optionally `src/store/mod.rs`; tests next to implementation.

- [ ] **Step 1: 为以下行为分别添加测试：无缓存可保存；校验失败不改磁盘；旧缓存指纹冲突拒绝；store revision/URL 已变拒绝；同步请求发出后手动保存，在途成功与失败均不得覆盖手动正文或状态。使用 `std::env::temp_dir()` 加随机后缀创建独立测试目录，测试数据使用有效 socks5 节点，不访问真实上游。**
- [ ] **Step 2: 执行 `cargo test refresh::`，确认新测试先失败。**
- [ ] **Step 3: 在 `Refresher` 添加短提交互斥 `cache_commits: Mutex<()>`。新增 `save_manual(id, expected_revision, expected_fingerprint, body)` 异步方法，解析在锁外，锁内重读 store 与缓存校验版本后调用 `commit_manual` 原子保存；禁止网络 IO 放在提交锁内。**
- [ ] **Step 4: `do_refresh` 拉取前记录缓存指纹。成功、304、失败的提交路径都在同一互斥内比较当前指纹，已变则返回 discarded，避免旧请求覆盖手动保存。网络同步的下一次新请求仍可以覆盖手动内容。**
- [ ] **Step 5: 保存前在 store 的只读快照锁持有期间再次检查上游存在与 URL/revision 一致并执行短文件提交，防止删除后重建缓存；`upstream_accept` 如继续写缓存，应走同一个提交互斥入口。不得让普通同步失败覆盖刚保存的手动内容。**
- [ ] **Step 6: 执行 `cargo test refresh::` 与 `cargo test cache::tests`，确认所有测试通过；如改动 store 锁接口，执行 `cargo test store::`。**

## Task 3: 服务端配置编辑页

**Files:** Create `src/web/upstream_config.rs`, `templates/upstream_config.html`; modify `src/web/mod.rs`, `templates/upstreams.html`.

- [ ] **Step 1: 编写模板测试：含 `</textarea><script>` 的正文被转义；无缓存显示粘贴说明；缓存正文及统计显示；错误回填不丢弃空白与原文；手动来源提示及后续同步覆盖警告可见。测试使用 `Template::render`，不引入前端依赖。**
- [ ] **Step 2: 使用 `cargo test upstream_config` 确认测试先失败。**
- [ ] **Step 3: 新模块定义 Askama 视图模型，提供 GET 和 POST handler；模块内部封装构造模板响应，并统一加入 `Cache-Control: no-store`。GET 只读 store/cache，不调用网络；缓存不存在与损坏必须分别提示。**
- [ ] **Step 4: 在管理 Router 注册：**

```rust
.route(
    "/upstreams/{id}/config",
    get(upstream_config::page).post(upstream_config::save),
)
```

`src/web/mod.rs` 新增 `mod upstream_config;`。新 handler 先检查合法 slug 与上游存在，POST 先 `form.verify_csrf`；`body` 使用 `form.get("body").unwrap_or("")` 而不是会 trim 的 `get_owned`。隐藏字段包含 store revision、缓存 fingerprint；缺失/无效版本拒绝提交。失败通过 `FormEcho::fail` 重渲染保留正文和原始版本，不能自动换成新版指纹后允许覆盖。

- [ ] **Step 5: 新模板沿用 `base.html`、`partials/form_errors.html`。提供独立标签与 `textarea.code`、`spellcheck=false`、字段错误、预览摘要、成功提示、返回上游列表链接。正文不得使用 `safe`。保存按钮明确为“校验并保存到缓存”；说明可能包含密码且只有节点参与方案输出。HTML 表单支持无 JS 操作。**
- [ ] **Step 6: 每行上游操作增加普通链接：**

```html
<a class="btn small ghost" href="/upstreams/{{ r.id }}/config">预览 / 编辑配置</a>
```

- [ ] **Step 7: 保存成功返回 303 `/upstreams/{id}/config?saved=1`；立即失效 `state.render_cache` 和 `state.node_names_cache`，确保修改即时反映到订阅输出与节点选择列表。**
- [ ] **Step 8: 执行 `cargo test upstream_config` 与 `cargo test`，确认模板编译和所有回归通过。**

## Task 4: 文档、真实请求验收与质量检查

**Files:** Modify `README.md`, `AGENTS.md`; tests remain in Rust modules, no private runtime files committed.

- [ ] **Step 1: README“上游刷新”下新增预览/编辑说明，明确缓存替换、失败可填、校验失败不覆盖、下次同步成功覆盖、仅提取节点。AGENTS §4.1 增补人工写入语义、§5 增加 GET/POST 路由，§12 添加变更记录但只记录实际完成的验证。**
- [ ] **Step 2: 运行 `cargo fmt --check`，有格式差异使用 `cargo fmt` 后复查；执行 `cargo test`、`cargo clippy --all-targets -- -D warnings`、`cargo build`。**
- [ ] **Step 3: 使用独立临时数据目录启动测试实例，不接触现有应用和真实订阅。HTTP 验收认证、无/错误 CSRF、错误 Origin、合法保存 303、无缓存手填后导出包含节点、非法保存原文回填且旧正文不变、旧表单冲突、条件请求清除、下一次成功同步覆盖。测试启动的后台进程必须在验收后结束并收集输出。**
- [ ] **Step 4: 浏览器验证上游列表入口、原文预览/编辑、错误回填、无 JS 保存、窄屏无横向溢出。进行浏览器操作前加载 agent-browser 技能。若本机没有可用 Mihomo，明确注明未进行真实内核加载验收，不以 YAML 解析替代。**
- [ ] **Step 5: `git diff --check` 与 `git diff --stat`，确认只改预期源码、模板、文档，不含本地密码、URL token、缓存或测试运行目录。不自动执行 git commit。**

## 自查

- 覆盖预览、手工编辑、无成功缓存时粘贴三项用户需求。
- 后续同步覆盖策略已由用户确认；没有引入持久手工覆盖层。
- 缓存仍保存原文，分享链接转换不会改变落盘正文。
- 关注 ETag 304、旧表单、在途同步、删除/URL 变更、渲染缓存失效、敏感正文 HTML 转义与 no-store。
- 不增加依赖、JSON API、前端框架或构建器。
