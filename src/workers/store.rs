//! 业务数据存储：Durable Object「Core」单写者 + DO SQLite。
//!
//! 与 native 的 `data/store.yaml` 对应：DO 里保存同一份 store YAML（含 revision），
//! 所有写入都经过单个 DO 实例串行化，保留 AGENTS.md §8.2 的乐观并发语义。
//! 备份暂时保留上一版（`store:previous`），完整的 20 份备份 + 恢复页在后续阶段补齐。

use crate::store::model::Store;
use worker::*;

/// DO 绑定名（见 `wrangler.jsonc`）。
pub const BINDING: &str = "CORE";
/// 单实例名：单人自用，全站共享一个 Core。
pub const INSTANCE: &str = "singleton";

/// 与 `storage` 抽象共用同一个 key，native 的 `data/store.yaml` 与 Workers 的
/// DO 对象因此可以走同一套 `StoreHandle`。
const CURRENT: &str = "store.yaml";
const PREVIOUS: &str = "store:previous";
const INDEX_KEY: &str = "_keys";

fn werr(context: &str, e: impl std::fmt::Display) -> Error {
    Error::RustError(format!("{context}：{e}"))
}

fn stub(env: &Env) -> Result<Stub> {
    env.durable_object(BINDING)?
        .id_from_name(INSTANCE)?
        .get_stub()
}

/// 读取当前 store YAML 原文（管理页导出/备份用）。
pub async fn load_yaml(env: &Env) -> Result<String> {
    let mut resp = stub(env)?.fetch_with_str("https://core/store").await?;
    if resp.status_code() != 200 {
        return Err(Error::RustError(format!(
            "读取 store 失败（HTTP {}）：{}",
            resp.status_code(),
            resp.text().await.unwrap_or_default()
        )));
    }
    resp.text().await
}

/// 读取并解析当前 store。
pub async fn load(env: &Env) -> Result<Store> {
    let text = load_yaml(env).await?;
    let store: Store = serde_yaml_ng::from_str(&text).map_err(|e| werr("解析 store 失败", e))?;
    crate::store::migrate::migrate(store).map_err(|e| werr("迁移 store 失败", e))
}

/// 覆盖当前 store（导入/迁移用）。返回新 revision。
pub async fn save_yaml(env: &Env, yaml: &str) -> Result<u64> {
    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_body(Some(wasm_bindgen::JsValue::from_str(yaml)));
    let req = Request::new_with_init("https://core/store", &init)?;
    let mut resp = stub(env)?.fetch_with_request(req).await?;
    if resp.status_code() != 200 {
        return Err(Error::RustError(format!(
            "保存 store 失败（HTTP {}）：{}",
            resp.status_code(),
            resp.text().await.unwrap_or_default()
        )));
    }
    resp.text()
        .await?
        .trim()
        .parse::<u64>()
        .map_err(|e| werr("解析 revision 失败", e))
}

// ---------------------------------------------------------------- Durable Object

#[durable_object]
pub struct Core {
    state: State,
    #[allow(dead_code)]
    env: Env,
}

impl DurableObject for Core {
    fn new(state: State, env: Env) -> Self {
        Self { state, env }
    }

    async fn fetch(&self, mut req: Request) -> Result<Response> {
        match (req.method(), req.path().as_str()) {
            (Method::Get, "/store") => {
                let text = self.current().await?;
                Response::ok(text)
            }
            (Method::Post, "/store") => {
                let body = req.text().await?;
                let revision = self.import(&body).await?;
                Response::ok(revision.to_string())
            }
            // 通用对象存储：store/备份/status/last_good 等小对象都走这里，
            // 由单写者 DO 串行化（对应 native 的写锁语义）。
            (Method::Get, path) if path.starts_with("/obj/") => {
                let key = &path["/obj/".len()..];
                match self.state.storage().get::<String>(key).await? {
                    Some(text) => Response::ok(text),
                    None => Response::error("not found", 404),
                }
            }
            (Method::Post, path) if path.starts_with("/obj/") => {
                let key = &path["/obj/".len()..];
                let body = req.text().await?;
                let size = body.len() as u64;
                self.state.storage().put(key, &body).await?;
                self.index_put(key, size).await?;
                Response::ok("ok")
            }
            (Method::Delete, path) if path.starts_with("/obj/") => {
                let key = &path["/obj/".len()..];
                let deleted = self.state.storage().delete(key).await?;
                self.index_remove(key).await?;
                if deleted {
                    Response::ok("ok")
                } else {
                    Response::error("not found", 404)
                }
            }
            (Method::Get, path) if path.starts_with("/list") => {
                // `req.path()` 不含 query，必须从完整 URL 里取 prefix。
                let prefix = req
                    .url()
                    .ok()
                    .and_then(|url| {
                        url.query_pairs()
                            .find(|(key, _)| key == "prefix")
                            .map(|(_, value)| value.to_string())
                    })
                    .unwrap_or_default();
                let entries = self.index().await?;
                let mut out = String::new();
                for (key, size) in entries {
                    if key.starts_with(&prefix) {
                        out.push_str(&format!("{key}\t{size}\n"));
                    }
                }
                Response::ok(out)
            }
            _ => Response::error("not found", 404),
        }
    }
}

impl Core {
    /// 当前 store；首次访问时写入默认数据（§8.2 首次运行行为）。
    async fn current(&self) -> Result<String> {
        if let Some(text) = self.state.storage().get::<String>(CURRENT).await? {
            return Ok(text);
        }
        let fresh = Store::default();
        let text = serde_yaml_ng::to_string(&fresh).map_err(|e| werr("初始化 store 失败", e))?;
        self.state.storage().put(CURRENT, text.clone()).await?;
        Ok(text)
    }

    /// 导入一份 store：迁移 → 结构校验 → revision 递增 → 保留上一版。
    async fn import(&self, yaml: &str) -> Result<u64> {
        let parsed: Store =
            serde_yaml_ng::from_str(yaml).map_err(|e| werr("解析 store 失败", e))?;
        let parsed =
            crate::store::migrate::migrate(parsed).map_err(|e| werr("迁移 store 失败", e))?;
        crate::validate::validate_structure(&parsed).map_err(|e| werr("store 校验失败", e))?;

        let previous = self.current().await?;
        let previous_revision = serde_yaml_ng::from_str::<Store>(&previous)
            .map(|s| s.revision)
            .unwrap_or(0);

        let mut next = parsed;
        next.revision = previous_revision.saturating_add(1);
        next.format_version = crate::store::model::FORMAT_VERSION;
        let text = serde_yaml_ng::to_string(&next).map_err(|e| werr("序列化 store 失败", e))?;

        self.state.storage().put(PREVIOUS, previous).await?;
        self.state.storage().put(CURRENT, text).await?;
        Ok(next.revision)
    }

    // ---------------------------------------------------------- 对象索引

    async fn index(&self) -> Result<Vec<(String, u64)>> {
        let text = self
            .state
            .storage()
            .get::<String>(INDEX_KEY)
            .await?
            .unwrap_or_default();
        let mut out = Vec::new();
        for line in text.lines() {
            if let Some((key, size)) = line.rsplit_once('\t') {
                out.push((key.to_string(), size.parse().unwrap_or(0)));
            }
        }
        Ok(out)
    }

    async fn index_write(&self, list: &[(String, u64)]) -> Result<()> {
        let text = list
            .iter()
            .map(|(key, size)| format!("{key}\t{size}"))
            .collect::<Vec<_>>()
            .join("\n");
        self.state.storage().put(INDEX_KEY, text).await?;
        Ok(())
    }

    async fn index_put(&self, key: &str, size: u64) -> Result<()> {
        let mut list = self.index().await?;
        list.retain(|(existing, _)| existing != key);
        list.push((key.to_string(), size));
        self.index_write(&list).await
    }

    async fn index_remove(&self, key: &str) -> Result<()> {
        let mut list = self.index().await?;
        list.retain(|(existing, _)| existing != key);
        self.index_write(&list).await
    }
}
