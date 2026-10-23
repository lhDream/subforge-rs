//! wasm 实现：store/备份/status 等小对象走 Durable Object「Core」，
//! 缓存信封与 GeoIP mmdb 等大对象走 R2。
//!
//! 为什么所有小对象都过 DO：DO 是单写者，天然串行化「读-改-写」，
//! 与 native 的写锁语义一一对应（AGENTS.md §8.2）。R2 只承担不可变的大对象。
//!
//! 所有对外方法都用 `SendFuture` 包住内部实现：worker 绑定的 `JsFuture` 是 `!Send`，
//! 而共享的 axum handler 要求 future 为 `Send`（Workers 单线程，包装是安全的）。

use super::{ObjectMeta, valid_key};
use anyhow::{Result, bail};
use chrono::{DateTime, Utc};
use worker::send::{SendFuture, SendWrapper};
use worker::{Bucket, Env, Method, Request, RequestInit};

/// R2 绑定名（见 `wrangler.jsonc`）。
pub const R2_BINDING: &str = "CACHE";

#[derive(Clone)]
pub struct Storage {
    env: SendWrapper<Env>,
}

impl Storage {
    pub fn new(env: Env) -> Self {
        Self {
            env: SendWrapper::new(env),
        }
    }

    /// 对象存储没有目录概念；保留与 native 同名方法方便共用启动逻辑。
    pub fn ensure_dirs(&self) -> Result<()> {
        Ok(())
    }

    pub fn env(&self) -> &Env {
        &self.env
    }

    fn bucket(&self) -> Result<Bucket> {
        self.env
            .bucket(R2_BINDING)
            .map_err(|e| anyhow::anyhow!("绑定 {R2_BINDING} 不可用：{e}"))
    }

    async fn do_request(
        &self,
        method: Method,
        path: &str,
        body: Option<String>,
    ) -> Result<Vec<u8>> {
        let url = format!("https://core{path}");
        let mut init = RequestInit::new();
        init.with_method(method);
        if let Some(text) = body {
            init.with_body(Some(wasm_bindgen::JsValue::from_str(&text)));
        }
        let request = Request::new_with_init(&url, &init)
            .map_err(|e| anyhow::anyhow!("构造 DO 请求失败：{e}"))?;
        // 必须走 Durable Object stub；普通 Fetch 会发往外网。
        let stub = self
            .env
            .durable_object("CORE")
            .map_err(|e| anyhow::anyhow!("绑定 CORE 不可用：{e}"))?
            .id_from_name("singleton")
            .map_err(|e| anyhow::anyhow!("DO 实例名无效：{e}"))?
            .get_stub()
            .map_err(|e| anyhow::anyhow!("获取 DO stub 失败：{e}"))?;
        let mut resp = stub
            .fetch_with_request(request)
            .await
            .map_err(|e| anyhow::anyhow!("调用 DO 失败：{e}"))?;
        let status = resp.status_code();
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| anyhow::anyhow!("读取 DO 响应失败：{e}"))?;
        if !(200..300).contains(&status) {
            bail!("DO 返回 HTTP {status}：{}", String::from_utf8_lossy(&bytes));
        }
        Ok(bytes)
    }

    pub async fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let key = key.to_string();
        let this = self.clone();
        SendFuture::new(async move { this.get_inner(&key).await }).await
    }

    async fn get_inner(&self, key: &str) -> Result<Option<Vec<u8>>> {
        if !valid_key(key) {
            bail!("非法的存储 key：{key}");
        }
        if is_r2_key(key) {
            let bucket = self.bucket()?;
            let object = bucket
                .get(key.to_string())
                .execute()
                .await
                .map_err(|e| anyhow::anyhow!("读取 R2 失败：{e}"))?;
            let Some(object) = object else {
                return Ok(None);
            };
            let body = object
                .body()
                .ok_or_else(|| anyhow::anyhow!("R2 对象没有正文：{key}"))?;
            let bytes = body
                .bytes()
                .await
                .map_err(|e| anyhow::anyhow!("读取 R2 正文失败：{e}"))?;
            return Ok(Some(bytes));
        }

        match self
            .do_request(Method::Get, &format!("/obj/{key}"), None)
            .await
        {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.to_string().contains("HTTP 404") => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub async fn get_string(&self, key: &str) -> Result<Option<String>> {
        let key = key.to_string();
        let this = self.clone();
        SendFuture::new(async move { this.get_string_inner(&key).await }).await
    }

    async fn get_string_inner(&self, key: &str) -> Result<Option<String>> {
        match self.get_inner(key).await? {
            Some(bytes) => Ok(Some(
                String::from_utf8(bytes).map_err(|_| anyhow::anyhow!("内容不是 UTF-8：{key}"))?,
            )),
            None => Ok(None),
        }
    }

    pub async fn put(&self, key: &str, bytes: &[u8]) -> Result<()> {
        let key = key.to_string();
        let bytes = bytes.to_vec();
        let this = self.clone();
        SendFuture::new(async move { this.put_inner(&key, &bytes).await }).await
    }

    async fn put_inner(&self, key: &str, bytes: &[u8]) -> Result<()> {
        if !valid_key(key) {
            bail!("非法的存储 key：{key}");
        }
        if is_r2_key(key) {
            let bucket = self.bucket()?;
            let data = String::from_utf8_lossy(bytes).to_string();
            bucket
                .put(key.to_string(), data)
                .execute()
                .await
                .map_err(|e| anyhow::anyhow!("写入 R2 失败：{e}"))?;
            return Ok(());
        }
        let text = String::from_utf8(bytes.to_vec())
            .map_err(|_| anyhow::anyhow!("DO 只支持 UTF-8 文本：{key}"))?;
        self.do_request(Method::Post, &format!("/obj/{key}"), Some(text))
            .await?;
        Ok(())
    }

    pub async fn put_string(&self, key: &str, text: &str) -> Result<()> {
        self.put(key, text.as_bytes()).await
    }

    pub async fn delete(&self, key: &str) -> Result<()> {
        let key = key.to_string();
        let this = self.clone();
        SendFuture::new(async move { this.delete_inner(&key).await }).await
    }

    async fn delete_inner(&self, key: &str) -> Result<()> {
        if !valid_key(key) {
            bail!("非法的存储 key：{key}");
        }
        if is_r2_key(key) {
            let bucket = self.bucket()?;
            bucket
                .delete(key.to_string())
                .await
                .map_err(|e| anyhow::anyhow!("删除 R2 对象失败：{e}"))?;
            return Ok(());
        }
        match self
            .do_request(Method::Delete, &format!("/obj/{key}"), None)
            .await
        {
            Ok(_) => Ok(()),
            Err(e) if e.to_string().contains("HTTP 404") => Ok(()),
            Err(e) => Err(e),
        }
    }

    pub async fn list(&self, prefix: &str) -> Result<Vec<ObjectMeta>> {
        let prefix = prefix.to_string();
        let this = self.clone();
        SendFuture::new(async move { this.list_inner(&prefix).await }).await
    }

    async fn list_inner(&self, prefix: &str) -> Result<Vec<ObjectMeta>> {
        let mut out = Vec::new();
        if prefix.starts_with("cache/") || prefix.starts_with("geoip/") {
            let bucket = self.bucket()?;
            let objects = bucket
                .list()
                .prefix(prefix.to_string())
                .execute()
                .await
                .map_err(|e| anyhow::anyhow!("列举 R2 失败：{e}"))?;
            for object in objects.objects() {
                out.push(ObjectMeta {
                    key: object.key(),
                    size: object.size(),
                    modified: None,
                });
            }
            return Ok(out);
        }
        let resp = self
            .do_request(
                Method::Get,
                &format!("/list?prefix={}", urlencode(prefix)),
                None,
            )
            .await?;
        let text = String::from_utf8(resp).map_err(|_| anyhow::anyhow!("列表不是 UTF-8"))?;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let (key, size) = match line.rsplit_once('\t') {
                Some((key, size)) => (key.to_string(), size.parse::<u64>().unwrap_or(0)),
                None => (line.to_string(), 0),
            };
            out.push(ObjectMeta {
                key,
                size,
                modified: None,
            });
        }
        Ok(out)
    }

    pub fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

fn is_r2_key(key: &str) -> bool {
    key.starts_with("cache/") || (key.starts_with("geoip/") && key.ends_with(".mmdb"))
}

fn urlencode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(byte as char)
            }
            b' ' => out.push_str("%20"),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}
