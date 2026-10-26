//! 各管理页 handler。一页一职责（AGENTS.md §5）。

use super::{AppError, FormData, FormEcho, HtmlTemplate, RequestContext, see_other};
use super::{content_disposition, normalize_base_url, safe_yaml_filename, sub_url};
use crate::cache::{self, CacheStatus};
use crate::cascade;
use crate::geoip_update;
use crate::pipeline::AppState;
use crate::rules::{RuleShape, is_builtin_policy, parse_rule_text, shape_of};
use crate::store::model::*;
use crate::store::valid_slug;
use askama::Template;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use serde::Deserialize;
use std::sync::Arc;

/// 统一的提前返回。
///
/// 用 `return` 使其类型为 `!`，因此可用于 `let ... else` 与 `match` 分支；
/// `.into()` 让它在返回 `AppError` 的 handler 与返回 `anyhow::Result` 的闭包里都成立。
macro_rules! bail {
    ($($arg:tt)*) => {
        return Err(anyhow::anyhow!($($arg)*).into())
    };
}

// ================================================================ 视图模型

#[derive(Template)]
#[template(path = "index.html")]
pub struct IndexTpl {
    /// 主导航高亮键，取值见 templates/base.html。
    pub nav: &'static str,
    pub csrf: String,
    pub profiles: Vec<ProfileRow>,
    pub upstreams: Vec<UpstreamRow>,
    pub geo_available: bool,
    pub geo_hint: String,
    /// 已加载的 GeoIP 库路径。
    pub geo_db: String,
    pub geo_db_type: String,
    pub geo_build: String,
    pub geo_auto_update: bool,
    pub geo_interval_days: u32,
}

pub struct ProfileRow {
    pub slug: String,
    pub name: String,
    pub upstreams: usize,
    pub groups: usize,
    pub rules: usize,
    pub on_failure: String,
    /// 是否设置了订阅令牌。为空时外部客户端拉取会 401。
    pub has_token: bool,
    /// 完整订阅链接（含 BASEURL 与可选令牌），用于展示/复制。
    pub sub_url: String,
    /// 带 `?download=1` 的下载链接。
    pub download_url: String,
    pub last_good_at: String,
}

#[derive(Template)]
#[template(path = "upstreams.html")]
pub struct UpstreamsTpl {
    pub nav: &'static str,
    pub csrf: String,
    pub rows: Vec<UpstreamRow>,
    pub concurrency: usize,
    pub interval: String,
    /// 「新增上游」弹窗是否直接展开（深链 `?new=1` 或校验失败回填）。
    pub create_open: bool,
    /// 提交失败时的回显与字段级错误；正常打开页面时为空。
    pub form: FormEcho,
}

pub struct UpstreamRow {
    pub id: String,
    pub name: String,
    pub url: String,
    pub status: String,
    pub status_key: String,
    pub last_attempt: String,
    pub last_success: String,
    pub data_age: String,
    pub expire: String,
    pub traffic: String,
    pub node_count: usize,
    /// 响应实际格式与转换说明（如「分享链接（base64），跳过 2 条」）。
    pub format: String,
    pub last_error: String,
    pub error_kind: String,
    pub note: String,
}

#[derive(Template)]
#[template(path = "groups.html")]
pub struct GroupsTpl {
    pub nav: &'static str,
    pub csrf: String,
    pub groups: Vec<GroupRow>,
    pub all_names: Vec<String>,
    /// 新建分组弹窗里的「手动子分组」复选框。
    pub create_group_choices: Vec<GroupChoice>,
    /// 可用于「手动节点」的节点名（手动节点 + 上游缓存中的节点 + 内置策略）。
    pub node_names: Vec<String>,
    /// 「新建分组」弹窗是否**直接展开**。
    ///
    /// 为真时服务端给 `<dialog>` 带上 `open`：没有 JS、或浏览器不支持
    /// `showModal` 时它会退化成页内卡片，表单照常可填可提交；脚本可用时
    /// 再由 `static/app.js` 升级成模态弹窗。
    pub create_open: bool,
    /// 提交失败时的回显与字段级错误；正常打开页面时为空。
    pub form: FormEcho,
}

pub struct GroupRow {
    /// 表单 id 用的安全标识（分组名可能含中文/emoji，不能直接当 HTML id）。
    pub index_id: String,
    pub name: String,
    pub group_type: String,
    pub mode: String,
    pub all: bool,
    pub catch_all: bool,
    pub regions: String,
    pub keywords: String,
    pub exclude: String,
    pub manual_groups: String,
    /// 「手动子分组」复选框（已排除自身）。
    pub manual_group_choices: Vec<GroupChoice>,
    pub manual_nodes: String,
    pub fallback_group: String,
    pub extra_yaml: String,
    pub used_by: String,
    /// 文本中出现该分组名、但无法自动改写的原始规则（重命名时必须人工核对）。
    pub raw_mentions: String,
    /// 排序按钮的边界状态。
    pub is_first: bool,
    pub is_last: bool,
}

/// 分组选择器中的一项。
pub struct GroupChoice {
    pub name: String,
    pub checked: bool,
}

#[derive(Template)]
#[template(path = "rules.html")]
pub struct RulesTpl {
    pub nav: &'static str,
    pub csrf: String,
    pub rows: Vec<RuleRow>,
    pub group_names: Vec<String>,
    /// 「手动新增规则」弹窗是否直接展开。
    pub add_open: bool,
    /// 「批量粘贴导入」弹窗是否直接展开。
    pub import_open: bool,
    /// 提交失败时的回显与字段级错误；正常打开页面时为空。
    /// 作用域：`""` = 手动新增表单，`"import"` = 批量导入表单。
    pub form: FormEcho,
}

pub struct RuleRow {
    pub index: usize,
    pub id: String,
    pub section_label: String,
    pub is_raw: bool,
    pub effective: String,
}

#[derive(Template)]
#[template(path = "aliases.html")]
pub struct AliasesTpl {
    pub nav: &'static str,
    pub csrf: String,
    pub rows: Vec<AliasRow>,
    /// 「新增地区」弹窗是否直接展开（深链 `?new=1` 或校验失败回填）。
    pub create_open: bool,
    /// 提交失败时的回显与字段级错误；正常打开页面时为空。
    pub form: FormEcho,
}

pub struct AliasRow {
    pub index_id: String,
    pub region: String,
    pub aliases: String,
    pub count: usize,
}

#[derive(Template)]
#[template(path = "nodes.html")]
pub struct ManualNodesTpl {
    pub nav: &'static str,
    pub csrf: String,
    pub rows: Vec<ManualNodeRow>,
    /// 「添加手动节点」弹窗是否直接展开（深链 `?new=1` 或校验失败回填）。
    pub create_open: bool,
    /// 提交失败时的回显与字段级错误；正常打开页面时为空。
    pub form: FormEcho,
}

pub struct ManualNodeRow {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub server: String,
    pub port: String,
    /// 哪些分组通过 manual_nodes 引用了这个节点名。
    pub used_by: String,
    /// 解析失败原因（正常情况下为空）。
    pub parse_error: String,
}

#[derive(Template)]
#[template(path = "general.html")]
pub struct GeneralTpl {
    pub nav: &'static str,
    pub csrf: String,
    pub region_source: String,
    /// 这几个复选框标志是模板里直接引用的名字（不是 fetch 派生模板的局部变量），
    /// 字段名保持不变；渲染时取 `self.<名字>`。
    pub filter_enabled: bool,
    pub filter_keywords: String,
    /// 订阅链接前缀（BASEURL），空表示相对路径。
    pub base_url: String,
    pub advanced_yaml: String,
    pub revision: u64,
    pub mixed_port: String,
    pub socks_port: String,
    pub port: String,
    pub allow_lan: String,
    pub mode: String,
    pub log_level: String,
    pub ipv6: String,
    // 名字必须与模板里的变量一致（askama 对派生模板做局部同名遮蔽）。
    pub proxy_enabled: bool,
    pub proxy_url: String,
    pub proxy_effective: String,
    /// GeoIP 库的安装状态与自动更新设置。
    pub geoip: GeoipView,
    /// 提交失败时的回显与字段级错误；正常打开页面时为空。
    pub form: FormEcho,
}

/// 通用设置页的 GeoIP 卡片视图模型。
pub struct GeoipView {
    pub installed: bool,
    pub db_path: String,
    pub db_type: String,
    pub build: String,
    pub ip_version: String,
    pub size: String,
    pub modified: String,
    pub last_attempt: String,
    pub last_success: String,
    pub last_error: String,
    pub last_error_kind: String,
    /// 实际使用的下载地址（已脱敏）。
    pub source: String,
    /// 内置来源在本月的地址（只展示，不写回设置）。
    pub default_url: String,
    pub auto_update: bool,
    pub interval_days: String,
    pub custom_url: String,
    pub next_update: String,
    /// 页面顶部的操作结果提示（来自重定向后的查询参数）。
    pub notice_kind: String,
    pub notice: String,
}

#[derive(Template)]
#[template(path = "profile.html")]
pub struct ProfileTpl {
    pub nav: &'static str,
    pub csrf: String,
    pub is_new: bool,
    pub slug: String,
    pub name: String,
    pub revision: u64,
    pub on_failure: String,
    pub token: String,
    /// 完整订阅链接与下载链接（新建方案时为空）。
    pub sub_url: String,
    pub download_url: String,
    /// 已勾选分组的依赖闭包中、尚未勾选的部分。
    pub missing_deps: Vec<String>,
    pub all_upstreams: Vec<Selectable>,
    pub all_groups: Vec<Selectable>,
    pub all_rules: Vec<Selectable>,
    /// 提交失败时的回显与字段级错误；正常打开页面时为空。
    pub form: FormEcho,
}

/// 多选列表中的一项。
pub struct Selectable {
    pub value: String,
    pub label: String,
    pub checked: bool,
    /// 分组依赖（JSON 数组字符串），供方案页自动勾选依赖闭包。
    pub deps_json: String,
    /// 该分组是否因依赖被自动勾选（仅分组选择器使用）。
    pub auto: bool,
}

#[derive(Template)]
#[template(path = "preview.html")]
pub struct PreviewTpl {
    pub nav: &'static str,
    pub slug: String,
    /// 是否设置了订阅令牌。为空时外部客户端拉取会 401。
    pub has_token: bool,
    /// 完整订阅链接与下载链接。
    pub sub_url: String,
    pub download_url: String,
    pub ok: bool,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
    pub conflicts: Vec<String>,
    pub info: Vec<String>,
    pub groups: Vec<PreviewGroup>,
    pub yaml: String,
    pub used_last_good: bool,
    pub last_good_at: String,
}

pub struct PreviewGroup {
    pub name: String,
    pub members: usize,
    pub auto: usize,
    pub reasons: Vec<String>,
}

#[derive(Template)]
#[template(path = "backups.html")]
pub struct BackupsTpl {
    pub nav: &'static str,
    pub csrf: String,
    pub rows: Vec<BackupRow>,
    pub preview: Option<BackupPreviewTpl>,
}

pub struct BackupRow {
    pub name: String,
    pub created_at: String,
    pub size: String,
}

/// 恢复备份前的差异预览。
pub struct BackupPreviewTpl {
    pub name: String,
    pub created_at: String,
    pub current_revision: u64,
    pub backup_revision: u64,
    pub empty: bool,
    pub rows: Vec<BackupDiffRow>,
}

pub struct BackupDiffRow {
    pub label: &'static str,
    pub added: String,
    pub removed: String,
    pub changed: String,
}

// ================================================================ 工具

fn mask_url(url: &str) -> String {
    let Some((base, query)) = url.split_once('?') else {
        return url.to_string();
    };
    let masked: Vec<String> = query
        .split('&')
        .map(|kv| match kv.split_once('=') {
            Some((k, _)) if is_secret_key(k) => format!("{k}=***"),
            _ => kv.to_string(),
        })
        .collect();
    format!("{base}?{}", masked.join("&"))
}

fn is_secret_key(k: &str) -> bool {
    let k = k.to_ascii_lowercase();
    [
        "token", "key", "secret", "password", "passwd", "auth", "sub",
    ]
    .iter()
    .any(|needle| k.contains(needle))
}

fn fmt_bytes(b: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{b} B")
    } else {
        format!("{v:.2} {}", UNITS[i])
    }
}

fn fmt_ts(ts: Option<chrono::DateTime<Utc>>) -> String {
    match ts {
        Some(t) => t.format("%Y-%m-%d %H:%M UTC").to_string(),
        None => "—".to_string(),
    }
}

async fn upstream_row(state: &AppState, u: &Upstream) -> UpstreamRow {
    let env = cache::load(&state.store.storage(), &u.id)
        .await
        .ok()
        .flatten();
    let now = Utc::now();

    let (status, status_key, last_attempt, last_success, node_count, last_error, error_kind) =
        match &env {
            Some(e) => (
                if e.meta.manual_saved_at.is_some() {
                    format!("{} · 手动配置", e.meta.status.label())
                } else {
                    e.meta.status.label().to_string()
                },
                match e.meta.status {
                    CacheStatus::Ok => "ok",
                    CacheStatus::Stale => "stale",
                    CacheStatus::Empty => "empty",
                    CacheStatus::Failed => "failed",
                }
                .to_string(),
                fmt_ts(e.meta.last_attempt_at),
                fmt_ts(e.meta.last_success_at),
                e.meta.node_count,
                e.meta.last_error.clone().unwrap_or_default(),
                e.meta
                    .last_error_kind
                    .map(|k| k.label().to_string())
                    .unwrap_or_default(),
            ),
            None => (
                "未刷新".to_string(),
                "failed".to_string(),
                "—".to_string(),
                "—".to_string(),
                0,
                String::new(),
                String::new(),
            ),
        };

    let data_age = env
        .as_ref()
        .and_then(|e| e.meta.last_success_at)
        .map(|t| {
            let d = now.signed_duration_since(t);
            if d.num_days() > 0 {
                format!("{} 天前", d.num_days())
            } else if d.num_hours() > 0 {
                format!("{} 小时前", d.num_hours())
            } else {
                format!("{} 分钟前", d.num_minutes().max(0))
            }
        })
        .unwrap_or_else(|| "—".to_string());

    let expire = env
        .as_ref()
        .and_then(|e| e.meta.expire)
        .filter(|v| *v > 0)
        .and_then(|ts| chrono::DateTime::from_timestamp(ts, 0))
        .map(|t| {
            if t < now {
                format!("已过期（{}）", t.format("%Y-%m-%d"))
            } else {
                t.format("%Y-%m-%d").to_string()
            }
        })
        .unwrap_or_else(|| "—".to_string());

    let traffic = env
        .as_ref()
        .and_then(|e| {
            let info = crate::subinfo::SubInfo {
                upload: e.meta.upload,
                download: e.meta.download,
                total: e.meta.total,
                expire: e.meta.expire,
            };
            info.remaining().map(|r| {
                let total = e.meta.total.unwrap_or(0);
                format!("剩余 {} / 共 {}", fmt_bytes(r), fmt_bytes(total))
            })
        })
        .unwrap_or_else(|| "—".to_string());

    UpstreamRow {
        id: u.id.clone(),
        name: u.name.clone(),
        url: mask_url(&u.url),
        status,
        status_key,
        last_attempt,
        last_success,
        data_age,
        expire,
        traffic,
        node_count,
        format: env
            .as_ref()
            .and_then(|e| e.meta.source_format.clone())
            .map(|f| {
                let skipped = env.as_ref().map(|e| e.meta.skipped_links).unwrap_or(0);
                if skipped > 0 {
                    format!("{f}（跳过 {skipped} 条）")
                } else {
                    f
                }
            })
            .unwrap_or_default(),
        last_error,
        error_kind,
        note: u.note.clone().unwrap_or_default(),
    }
}

fn slugify(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() {
        format!("item-{}", crate::auth::random_hex(3))
    } else {
        out
    }
}

// ================================================================ 首页

pub async fn index(
    State(state): State<Arc<AppState>>,
    axum::Extension(ctx): axum::Extension<RequestContext>,
) -> Result<Response, AppError> {
    let snapshot = state.store.snapshot().await;

    let mut profiles = Vec::new();
    for p in &snapshot.profiles {
        let lg = crate::lastgood::load(&state.store.storage(), &p.slug)
            .await
            .ok()
            .flatten();
        profiles.push(ProfileRow {
            slug: p.slug.clone(),
            name: p.name.clone(),
            upstreams: p.upstreams.len(),
            groups: p.groups.len(),
            rules: p.rules.len(),
            on_failure: match p.on_failure {
                OnFailure::Fail => "严格失败".to_string(),
                OnFailure::LastGood => "回退 last_good".to_string(),
            },
            has_token: p.token.as_deref().is_some_and(|t| !t.is_empty()),
            sub_url: sub_url(
                &snapshot.settings.base_url,
                &p.slug,
                p.token.as_deref(),
                false,
            ),
            download_url: sub_url(
                &snapshot.settings.base_url,
                &p.slug,
                p.token.as_deref(),
                true,
            ),
            last_good_at: lg
                .map(|l| l.generated_at.format("%Y-%m-%d %H:%M UTC").to_string())
                .unwrap_or_else(|| "—".to_string()),
        });
    }

    let mut upstreams = Vec::new();
    for u in &snapshot.upstreams {
        upstreams.push(upstream_row(&state, u).await);
    }

    Ok(HtmlTemplate(IndexTpl {
        nav: "profiles",
        csrf: ctx.csrf,
        profiles,
        upstreams,
        geo_available: state.geo.is_available(),
        geo_hint: state.store.storage_label(),
        geo_db: state
            .geo
            .db_path()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
        geo_db_type: state
            .geo
            .db_info()
            .map(|i| i.database_type)
            .unwrap_or_default(),
        geo_build: state
            .geo
            .db_info()
            .map(|i| crate::geoip::format_epoch(i.build_epoch))
            .unwrap_or_default(),
        geo_auto_update: snapshot.settings.geoip.auto_update,
        geo_interval_days: snapshot.settings.geoip.interval_days_clamped(),
    })
    .into_response())
}

// ================================================================ 上游

/// 上游页视图模型。校验失败时同样走这里，只是多带一份回显。
async fn upstreams_tpl(
    state: &AppState,
    ctx: &RequestContext,
    snapshot: &Store,
    form: FormEcho,
) -> UpstreamsTpl {
    let mut rows = Vec::new();
    for u in &snapshot.upstreams {
        rows.push(upstream_row(state, u).await);
    }
    // 新增表单的校验失败也走这里：作用域为空串表示「新增上游」。
    let create_open = form.scope.is_empty() && !form.is_empty();
    UpstreamsTpl {
        nav: "upstreams",
        csrf: ctx.csrf.clone(),
        rows,
        concurrency: state.cfg.fetch_concurrency,
        interval: humantime::format_duration(state.cfg.refresh_interval).to_string(),
        create_open,
        form,
    }
}

/// `/upstreams?new=1`：直接展开「新增上游」弹窗。
#[derive(Debug, Default, Deserialize)]
pub struct UpstreamsQuery {
    #[serde(rename = "new")]
    pub new_form: Option<String>,
}

pub async fn upstreams_page(
    State(state): State<Arc<AppState>>,
    axum::Extension(ctx): axum::Extension<RequestContext>,
    Query(q): Query<UpstreamsQuery>,
) -> Result<Response, AppError> {
    let snapshot = state.store.snapshot().await;
    let mut tpl = upstreams_tpl(&state, &ctx, &snapshot, FormEcho::default()).await;
    tpl.create_open = q.new_form.is_some();
    Ok(HtmlTemplate(tpl).into_response())
}

pub async fn upstreams_save(
    State(state): State<Arc<AppState>>,
    axum::Extension(ctx): axum::Extension<RequestContext>,
    body: Bytes,
) -> Result<Response, AppError> {
    let form = FormData::from_bytes(&body);
    form.verify_csrf(&state, &ctx)?;

    let original = form.get_owned("original_id");
    let name = form.get_owned("name");
    let url = form.get_owned("url");
    let note = form.get_owned("note");
    let mut id = form.get_owned("id");

    let snapshot = state.store.snapshot().await;

    // 校验失败时回填重渲染：保留已填内容，并把错误挂到对应字段（规范 §6）。
    // `original` 为空表示这是「新增上游」表单，否则是正在编辑的那一行。
    macro_rules! refill {
        ($field:expr, $label:expr, $msg:expr) => {
            return Ok(HtmlTemplate(
                upstreams_tpl(
                    &state,
                    &ctx,
                    &snapshot,
                    FormEcho::fail(&form, &original, $field, $label, $msg),
                )
                .await,
            )
            .into_response())
        };
    }

    if name.is_empty() {
        refill!("name", "名称", "上游名称不能为空".to_string());
    }
    if original.is_empty() && url.is_empty() {
        refill!("url", "订阅 URL", "订阅 URL 不能为空".to_string());
    }
    // 编辑时 URL 留空表示「保持原 URL 不变」——页面只展示脱敏后的地址，
    // 若把 *** 回填进表单会覆盖真实 token。
    if !original.is_empty() && !url.is_empty() && !url.starts_with("http") {
        refill!(
            "url",
            "订阅 URL",
            "订阅 URL 必须以 http:// 或 https:// 开头".to_string()
        );
    }
    if id.is_empty() {
        id = slugify(&name);
    }
    if !valid_slug(&id) {
        refill!(
            "id",
            "id",
            format!("上游 id「{id}」非法：只允许小写字母、数字与连字符")
        );
    }
    if !original.is_empty() && snapshot.upstream(&original).is_none() {
        refill!(
            "name",
            "名称",
            format!("上游「{original}」已不存在，可能已被其他操作删除，请刷新页面后重试")
        );
    }
    // 先按当前快照判定重名，才能把错误落到字段上；写锁内仍保留同样的检查兜底。
    // 新增表单有独立的 id 字段；编辑行没有，id 由名称推导，因此错误要挂在名称上。
    if snapshot
        .upstreams
        .iter()
        .any(|x| x.id == id && x.id != original)
    {
        if original.is_empty() {
            refill!("id", "id", format!("上游 id「{id}」已存在"));
        }
        refill!(
            "name",
            "名称",
            format!("该名称生成的 id「{id}」已被其他上游占用，请换一个名称")
        );
    }

    // 重命名 id 需要同步挪动缓存文件
    let renaming = !original.is_empty() && original != id;
    if renaming {
        cache::remove(&state.store.storage(), &original).await?;
    }

    state
        .store
        .update(None, |store| {
            if original.is_empty() {
                if store.upstream(&id).is_some() {
                    bail!("上游 id「{id}」已存在");
                }
                store.upstreams.push(Upstream {
                    id: id.clone(),
                    name: name.clone(),
                    url: url.clone(),
                    note: (!note.is_empty()).then(|| note.clone()),
                });
            } else {
                if store
                    .upstreams
                    .iter()
                    .any(|x| x.id == id && x.id != original)
                {
                    bail!("上游 id「{id}」已存在");
                }
                let Some(u) = store.upstreams.iter_mut().find(|u| u.id == original) else {
                    bail!("上游「{original}」不存在");
                };
                u.id = id.clone();
                u.name = name.clone();
                if !url.is_empty() {
                    u.url = url.clone();
                }
                u.note = (!note.is_empty()).then(|| note.clone());
            }
            Ok(())
        })
        .await?;

    Ok(see_other("/upstreams"))
}

pub async fn upstream_delete(
    State(state): State<Arc<AppState>>,
    axum::Extension(ctx): axum::Extension<RequestContext>,
    Path(id): Path<String>,
    body: Bytes,
) -> Result<Response, AppError> {
    let form = FormData::from_bytes(&body);
    form.verify_csrf(&state, &ctx)?;

    state
        .store
        .update(None, |store| cascade::delete_upstream(store, &id))
        .await?;
    cache::remove(&state.store.storage(), &id).await?;
    Ok(see_other("/upstreams"))
}

pub async fn upstream_refresh(
    State(state): State<Arc<AppState>>,
    axum::Extension(ctx): axum::Extension<RequestContext>,
    Path(id): Path<String>,
    body: Bytes,
) -> Result<Response, AppError> {
    let form = FormData::from_bytes(&body);
    form.verify_csrf(&state, &ctx)?;
    let _ = state.refresher.refresh_one(&id).await;
    Ok(see_other("/upstreams"))
}

pub async fn upstreams_refresh_all(
    State(state): State<Arc<AppState>>,
    axum::Extension(ctx): axum::Extension<RequestContext>,
    body: Bytes,
) -> Result<Response, AppError> {
    let form = FormData::from_bytes(&body);
    form.verify_csrf(&state, &ctx)?;
    let _ = state.refresher.refresh_all().await;
    Ok(see_other("/upstreams"))
}

/// 强制接受零节点响应（§4.4 逃生口）。
pub async fn upstream_accept(
    State(state): State<Arc<AppState>>,
    axum::Extension(ctx): axum::Extension<RequestContext>,
    Path(id): Path<String>,
    body: Bytes,
) -> Result<Response, AppError> {
    let form = FormData::from_bytes(&body);
    form.verify_csrf(&state, &ctx)?;

    state.refresher.force_accept(&id).await?;
    state.render_cache.write().await.clear();
    *state.node_names_cache.write().await = None;

    Ok(see_other("/upstreams"))
}

// ================================================================ 分组

async fn node_names_signature(state: &AppState, snapshot: &Store) -> String {
    let mut raw = format!("rev={};", snapshot.revision);
    let storage = state.store.storage();
    for upstream in &snapshot.upstreams {
        match cache::load(&storage, &upstream.id).await {
            Ok(env) => {
                let fingerprint = cache::fingerprint(env.as_ref()).unwrap_or_default();
                raw.push_str(&format!("{}={fingerprint};", upstream.id));
            }
            Err(_) => raw.push_str(&format!("{}=missing;", upstream.id)),
        }
    }
    raw
}

fn collect_node_names(snapshot: &Store, bodies: Vec<(String, String)>) -> Vec<String> {
    use crate::matcher::{NodeInfo, evaluate_group, evaluate_group_with_covered};
    use crate::region::regions_from_name;

    fn push(name: &str, names: &mut Vec<String>, seen: &mut std::collections::HashSet<String>) {
        let name = name.trim().to_string();
        if name.is_empty() || !seen.insert(name.clone()) {
            return;
        }
        names.push(name);
    }

    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut names: Vec<String> = Vec::new();

    // ① 节点池：手动节点 + 各上游缓存节点，保持到达顺序（不再按名字排序）。
    let mut pool: Vec<String> = Vec::new();
    for manual in &snapshot.manual_nodes {
        if let Ok(proxy) = crate::links::parse_link(&manual.link)
            && let Some(name) = mstr(&proxy, "name")
        {
            pool.push(name.to_string());
        }
    }
    for (_, body) in &bodies {
        let Ok(parsed) = crate::fetch::validate_and_parse(
            body,
            &snapshot.settings.filter_keywords,
            snapshot.settings.filter_enabled,
        ) else {
            continue;
        };
        for proxy in parsed.proxies {
            if let Some(name) = mstr(&proxy, "name") {
                pool.push(name.to_string());
            }
        }
    }

    // ② 按分组顺序展开成员：与导出 `proxies` 口径一致（最早收录者先排）。
    let infos: Vec<NodeInfo> = pool
        .iter()
        .map(|name| NodeInfo {
            name: name.clone(),
            regions: regions_from_name(name, &snapshot.aliases),
        })
        .collect();
    let in_pool: std::collections::HashSet<&str> = pool.iter().map(String::as_str).collect();
    let mut covered: std::collections::HashSet<String> = std::collections::HashSet::new();

    // 兜底分组必须在其他分组之后求值，否则 covered 不完整。
    let mut order: Vec<&crate::store::model::Group> = snapshot
        .groups
        .iter()
        .filter(|g| !g.r#match.catch_all)
        .collect();
    order.extend(snapshot.groups.iter().filter(|g| g.r#match.catch_all));

    for group in order {
        let evaluated = if group.r#match.catch_all {
            evaluate_group_with_covered(group, &infos, Some(&covered))
        } else {
            evaluate_group(group, &infos)
        };
        let Ok(eval) = evaluated else { continue };
        for member in &eval.members {
            // 成员里混有子分组名与内置策略，只取节点池里的真实节点。
            if in_pool.contains(member.as_str()) {
                push(member, &mut names, &mut seen);
            }
        }
        if !group.r#match.all {
            covered.extend(eval.auto_matched.iter().cloned());
        }
        covered.extend(group.manual_nodes.iter().cloned());
    }

    // ③ 没有进入任何分组的节点，保持池内顺序追加。
    for name in &pool {
        push(name, &mut names, &mut seen);
    }

    // ④ 内置策略放最后（可直接写进 manual_nodes）。
    for policy in ["DIRECT", "REJECT", "PASS", "GLOBAL"] {
        push(policy, &mut names, &mut seen);
    }
    names
}

/// 收集所有可作为「手动节点」引用的名字：内置策略 + 手动节点 + 上游缓存节点。
///
/// 结果按「store revision + 缓存文件元数据」缓存；解析放到阻塞线程池，避免拖慢
/// async handler（分组页每次打开都会用到）。
async fn available_node_names(state: &Arc<AppState>, snapshot: &Store) -> Vec<String> {
    let signature = node_names_signature(state, snapshot).await;
    {
        let cache = state.node_names_cache.read().await;
        if let Some((sig, names)) = cache.as_ref()
            && sig == &signature
        {
            return names.clone();
        }
    }

    let snapshot = snapshot.clone();
    let storage = state.store.storage();
    let mut bodies = Vec::new();
    for upstream in &snapshot.upstreams {
        if let Ok(Some(env)) = cache::load(&storage, &upstream.id).await {
            bodies.push((upstream.id.clone(), env.body));
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    let names = tokio::task::spawn_blocking(move || collect_node_names(&snapshot, bodies))
        .await
        .unwrap_or_default();
    #[cfg(target_arch = "wasm32")]
    let names = collect_node_names(&snapshot, bodies);
    *state.node_names_cache.write().await = Some((signature, names.clone()));
    names
}

fn groups_tpl(
    ctx: &RequestContext,
    snapshot: &Store,
    node_names: Vec<String>,
    form: FormEcho,
) -> GroupsTpl {
    let all_names: Vec<String> = snapshot.groups.iter().map(|g| g.name.clone()).collect();

    // 校验失败回填时，勾选状态以本次提交的重复键为准；正常打开则沿用已存配置。
    let create_selected: Vec<String> = if form.scope.is_empty() && !form.is_empty() {
        form.values("", "manual_groups")
    } else {
        Vec::new()
    };
    let create_group_choices = all_names
        .iter()
        .map(|name| GroupChoice {
            name: name.clone(),
            checked: create_selected.contains(name),
        })
        .collect();

    let groups = snapshot
        .groups
        .iter()
        .enumerate()
        .map(|(i, g)| {
            let refs = cascade::group_references(snapshot, &g.name);
            let used_by = refs
                .iter()
                .map(|r| r.describe())
                .collect::<Vec<_>>()
                .join("；");
            let mut extra = g.clash.clone();
            for k in ["name", "proxies", "type", "url", "interval"] {
                extra.remove(Yaml::String(k.to_string()));
            }
            let selected_manual = if form.scope == g.name {
                form.values(&g.name, "manual_groups")
            } else {
                g.manual_groups.clone()
            };
            GroupRow {
                index_id: format!("g{i}"),
                name: g.name.clone(),
                group_type: g.group_type().to_string(),
                mode: match g.r#match.mode {
                    MatchMode::Any => "any".to_string(),
                    MatchMode::All => "all".to_string(),
                },
                all: g.r#match.all,
                catch_all: g.r#match.catch_all,
                regions: g.r#match.regions.join(", "),
                keywords: g.r#match.keywords.join(", "),
                exclude: g.r#match.exclude.join(", "),
                manual_groups: g.manual_groups.join(", "),
                manual_group_choices: all_names
                    .iter()
                    .filter(|name| *name != &g.name)
                    .map(|name| GroupChoice {
                        name: name.clone(),
                        checked: selected_manual.contains(name),
                    })
                    .collect(),
                manual_nodes: g.manual_nodes.join(", "),
                fallback_group: g.fallback_group.clone().unwrap_or_default(),
                extra_yaml: serde_yaml_ng::to_string(&Yaml::Mapping(extra))
                    .unwrap_or_default()
                    .trim()
                    .to_string(),
                used_by,
                raw_mentions: cascade::raw_rules_mentioning(snapshot, &g.name).join("、"),
                is_first: i == 0,
                is_last: i + 1 == snapshot.groups.len(),
            }
        })
        .collect();

    // 新增表单的校验失败也走这里：作用域为空串表示「新增分组」表单。
    // 此时必须把弹窗展开，否则用户看不到回填的内容与字段错误。
    let create_open = form.scope.is_empty() && !form.is_empty();

    GroupsTpl {
        nav: "groups",
        csrf: ctx.csrf.clone(),
        groups,
        all_names,
        create_group_choices,
        node_names,
        create_open,
        form,
    }
}

/// `/groups?new=1`：直接展开「新建分组」表单。
///
/// 触发入口是一个**普通链接**，因此没有 JS 也能用：浏览器导航到这里，
/// 服务端把弹窗带着 `open` 渲染成页内表单（见 `GroupsTpl::create_open`）。
/// 脚本可用时链接会被拦截，改为打开模态弹窗。
#[derive(Debug, Default, Deserialize)]
pub struct GroupsQuery {
    #[serde(rename = "new")]
    pub new_form: Option<String>,
}

pub async fn groups_page(
    State(state): State<Arc<AppState>>,
    axum::Extension(ctx): axum::Extension<RequestContext>,
    Query(q): Query<GroupsQuery>,
) -> Result<Response, AppError> {
    let snapshot = state.store.snapshot().await;
    let node_names = available_node_names(&state, &snapshot).await;
    let mut tpl = groups_tpl(&ctx, &snapshot, node_names, FormEcho::default());
    tpl.create_open = q.new_form.is_some();
    Ok(HtmlTemplate(tpl).into_response())
}

pub async fn groups_save(
    State(state): State<Arc<AppState>>,
    axum::Extension(ctx): axum::Extension<RequestContext>,
    body: Bytes,
) -> Result<Response, AppError> {
    let form = FormData::from_bytes(&body);
    form.verify_csrf(&state, &ctx)?;

    let original = form.get_owned("original_name");
    let name = form.get_owned("name");

    let snapshot = state.store.snapshot().await;
    let node_names = available_node_names(&state, &snapshot).await;

    // 校验失败回填重渲染；`original` 为空表示「新增分组」表单。
    macro_rules! refill {
        ($field:expr, $label:expr, $msg:expr) => {
            return Ok(HtmlTemplate(groups_tpl(
                &ctx,
                &snapshot,
                node_names.clone(),
                FormEcho::fail(&form, &original, $field, $label, $msg),
            ))
            .into_response())
        };
    }

    if name.is_empty() {
        refill!("name", "分组名", "分组名不能为空".to_string());
    }
    if is_builtin_policy(&name) {
        refill!(
            "name",
            "分组名",
            format!("分组名「{name}」与内置策略（DIRECT / REJECT / PASS / GLOBAL）重名")
        );
    }

    let group_type = {
        let t = form.get_owned("group_type");
        if t.is_empty() {
            "select".to_string()
        } else {
            t
        }
    };

    let mut clash = Mapping::new();
    clash.insert(ystr("type"), ystr(group_type.clone()));
    if let Some(v) = form.get("url").map(str::trim).filter(|s| !s.is_empty()) {
        clash.insert(ystr("url"), ystr(v));
    }
    if let Some(v) = form.get_usize("interval") {
        clash.insert(ystr("interval"), Yaml::Number(v.into()));
    }
    if let Some(v) = form.get_usize("tolerance") {
        clash.insert(ystr("tolerance"), Yaml::Number(v.into()));
    }
    if let Some(v) = form.get("lazy").map(str::trim).filter(|s| !s.is_empty()) {
        clash.insert(
            ystr("lazy"),
            Yaml::Bool(matches!(v, "true" | "on" | "1" | "yes")),
        );
    }

    // 额外 Clash 字段（原文 YAML）
    let extra = form.get_owned("extra_yaml");
    if !extra.is_empty() {
        let parsed: Yaml = match serde_yaml_ng::from_str(&extra) {
            Ok(v) => v,
            Err(e) => refill!(
                "extra_yaml",
                "额外 Clash 字段",
                format!("不是合法 YAML：{e}")
            ),
        };
        if let Yaml::Mapping(m) = parsed {
            for (k, v) in m {
                let key = k.as_str().unwrap_or("").to_string();
                if key.is_empty() {
                    continue;
                }
                if [
                    "name",
                    "proxies",
                    "type",
                    "url",
                    "interval",
                    "tolerance",
                    "lazy",
                ]
                .contains(&key.as_str())
                {
                    refill!(
                        "extra_yaml",
                        "额外 Clash 字段",
                        format!("字段「{key}」已由表单管理，请从原文 YAML 中移除")
                    );
                }
                clash.insert(k, v);
            }
        } else if !extra.trim().is_empty() {
            refill!(
                "extra_yaml",
                "额外 Clash 字段",
                "必须是 YAML 映射，例如 `tolerance: 50`".to_string()
            );
        }
    }

    let mode = match form.get_owned("mode").as_str() {
        "all" => MatchMode::All,
        _ => MatchMode::Any,
    };
    let gmatch = GroupMatch {
        all: form.get_bool("all"),
        catch_all: form.get_bool("catch_all"),
        mode,
        regions: split_list(form.get("regions").unwrap_or("")),
        keywords: split_list(form.get("keywords").unwrap_or("")),
        exclude: split_list(form.get("exclude").unwrap_or("")),
    };
    let manual_groups = form.get_list("manual_groups");
    let manual_nodes = split_list(form.get("manual_nodes").unwrap_or(""));
    let fallback = form.get_owned("fallback_group");

    state
        .store
        .update(None, move |store| {
            let fallback = (!fallback.is_empty()).then_some(fallback);
            cascade::upsert_group(
                store,
                &original,
                &name,
                clash,
                gmatch,
                manual_groups,
                manual_nodes,
                fallback,
            )
        })
        .await?;

    Ok(see_other("/groups"))
}

pub async fn groups_rename(
    State(state): State<Arc<AppState>>,
    axum::Extension(ctx): axum::Extension<RequestContext>,
    body: Bytes,
) -> Result<Response, AppError> {
    let form = FormData::from_bytes(&body);
    form.verify_csrf(&state, &ctx)?;
    let from = form.get_owned("from");
    let to = form.get_owned("to");
    state
        .store
        .update(None, |store| cascade::rename_group(store, &from, &to))
        .await?;
    Ok(see_other("/groups"))
}

pub async fn groups_delete(
    State(state): State<Arc<AppState>>,
    axum::Extension(ctx): axum::Extension<RequestContext>,
    body: Bytes,
) -> Result<Response, AppError> {
    let form = FormData::from_bytes(&body);
    form.verify_csrf(&state, &ctx)?;
    let name = form.get_owned("name");
    let replacement = form.get_owned("replacement");
    state
        .store
        .update(None, |store| {
            cascade::delete_group(
                store,
                &name,
                (!replacement.is_empty()).then_some(replacement.as_str()),
            )
        })
        .await?;
    Ok(see_other("/groups"))
}

/// 上移 / 下移分组（只改库内顺序，导出仍按拓扑序）。
pub async fn groups_move(
    State(state): State<Arc<AppState>>,
    axum::Extension(ctx): axum::Extension<RequestContext>,
    body: Bytes,
) -> Result<Response, AppError> {
    let form = FormData::from_bytes(&body);
    form.verify_csrf(&state, &ctx)?;
    let name = form.get_owned("name");
    let direction = form.get_owned("direction");
    state
        .store
        .update(None, move |store| {
            cascade::move_group(store, &name, &direction)
        })
        .await?;
    Ok(see_other("/groups"))
}

/// 拖动排序后提交完整顺序。
pub async fn groups_reorder(
    State(state): State<Arc<AppState>>,
    axum::Extension(ctx): axum::Extension<RequestContext>,
    body: Bytes,
) -> Result<Response, AppError> {
    let form = FormData::from_bytes(&body);
    form.verify_csrf(&state, &ctx)?;
    let order = form.get_list("order");
    state
        .store
        .update(None, move |store| cascade::reorder_groups(store, &order))
        .await?;
    Ok(see_other("/groups"))
}

/// 预设的兜底分组名。
const CATCH_ALL_PRESET_NAME: &str = "漏网之鱼";

/// 一键创建「漏网之鱼」兜底分组。
///
/// 分组语义靠 `match.catch_all` 表达，**不依赖名字**——建成后可以随意改名。
/// 已存在同名分组时只补上兜底标志，不覆盖用户已有的配置。
pub async fn groups_preset_catch_all(
    State(state): State<Arc<AppState>>,
    axum::Extension(ctx): axum::Extension<RequestContext>,
    body: Bytes,
) -> Result<Response, AppError> {
    let form = FormData::from_bytes(&body);
    form.verify_csrf(&state, &ctx)?;

    state
        .store
        .update(None, |store| {
            if let Some(g) = store
                .groups
                .iter_mut()
                .find(|g| g.name == CATCH_ALL_PRESET_NAME)
            {
                g.r#match.catch_all = true;
                return Ok(());
            }
            let mut clash = Mapping::new();
            clash.insert(ystr("type"), ystr("select"));
            cascade::upsert_group(
                store,
                "",
                CATCH_ALL_PRESET_NAME,
                clash,
                GroupMatch {
                    catch_all: true,
                    ..Default::default()
                },
                Vec::new(),
                Vec::new(),
                None,
            )
        })
        .await?;

    Ok(see_other("/groups"))
}

fn split_list(raw: &str) -> Vec<String> {
    raw.split([',', '\n', '，'])
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

// ================================================================ 规则

/// 路由规则页视图模型。校验失败时同样走这里，只是多带一份回显。
fn rules_tpl(ctx: &RequestContext, snapshot: &Store, form: FormEcho) -> RulesTpl {
    let mut ordered: Vec<&RuleDef> = snapshot.rules.iter().collect();
    ordered.sort_by_key(|r| (crate::rules::section_rank(r.section), 0));

    let rows = ordered
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let effective = crate::rules::serialize_rule(r).unwrap_or_default();
            RuleRow {
                index: i + 1,
                id: r.id.clone(),
                section_label: match r.section {
                    RuleSection::Override => "强制覆盖".to_string(),
                    RuleSection::Normal => "普通".to_string(),
                    RuleSection::Final => "兜底".to_string(),
                },
                is_raw: r.is_raw(),
                effective,
            }
        })
        .collect();

    // 新增 / 导入表单的校验失败也走这里：作用域决定展开哪个弹窗。
    let add_open = form.scope.is_empty() && !form.is_empty();
    let import_open = form.scope == "import";

    RulesTpl {
        nav: "rules",
        csrf: ctx.csrf.clone(),
        rows,
        group_names: snapshot.groups.iter().map(|g| g.name.clone()).collect(),
        add_open,
        import_open,
        form,
    }
}

/// `/rules?new=1` / `/rules?import=1`：直接展开对应的输入弹窗。
#[derive(Debug, Default, Deserialize)]
pub struct RulesQuery {
    #[serde(rename = "new")]
    pub new_form: Option<String>,
    #[serde(rename = "import")]
    pub import_form: Option<String>,
}

pub async fn rules_page(
    State(state): State<Arc<AppState>>,
    axum::Extension(ctx): axum::Extension<RequestContext>,
    Query(q): Query<RulesQuery>,
) -> Result<Response, AppError> {
    let snapshot = state.store.snapshot().await;
    let mut tpl = rules_tpl(&ctx, &snapshot, FormEcho::default());
    tpl.add_open = q.new_form.is_some();
    tpl.import_open = q.import_form.is_some();
    Ok(HtmlTemplate(tpl).into_response())
}

pub async fn rules_save(
    State(state): State<Arc<AppState>>,
    axum::Extension(ctx): axum::Extension<RequestContext>,
    body: Bytes,
) -> Result<Response, AppError> {
    let form = FormData::from_bytes(&body);
    form.verify_csrf(&state, &ctx)?;

    let original = form.get_owned("original_id");
    let mut id = form.get_owned("id");
    let section = match form.get_owned("section").as_str() {
        "override" => RuleSection::Override,
        "final" => RuleSection::Final,
        _ => RuleSection::Normal,
    };
    let raw = form.get_owned("raw");

    let snapshot = state.store.snapshot().await;

    // 手动新增表单的作用域是空串。
    macro_rules! refill {
        ($field:expr, $label:expr, $msg:expr) => {
            return Ok(HtmlTemplate(rules_tpl(
                &ctx,
                &snapshot,
                FormEcho::fail(&form, "", $field, $label, $msg),
            ))
            .into_response())
        };
    }

    if id.is_empty() {
        id = format!("r-{}", crate::auth::random_hex(4));
    }

    let rule = if !raw.trim().is_empty() {
        RuleDef {
            id: id.clone(),
            section,
            raw: Some(raw.trim().to_string()),
            ..Default::default()
        }
    } else {
        let tp = form.get_owned("type").to_uppercase();
        if tp.is_empty() {
            refill!(
                "type",
                "类型",
                "必须填写规则类型（如 DOMAIN-SUFFIX），或改填下面的「原始规则」".to_string()
            );
        }
        RuleDef {
            id: id.clone(),
            section,
            r#type: Some(tp),
            payload: {
                let p = form.get_owned("payload");
                (!p.is_empty()).then_some(p)
            },
            target: {
                let t = form.get_owned("target");
                (!t.is_empty()).then_some(t)
            },
            params: split_list(form.get("params").unwrap_or("")),
            raw: None,
        }
    };

    if let Err(e) = state
        .store
        .update(None, |store| {
            if original.is_empty() {
                if store.rule(&id).is_some() {
                    bail!("规则 id「{id}」已存在");
                }
                store.rules.push(rule.clone());
            } else {
                let r = store
                    .rules
                    .iter_mut()
                    .find(|r| r.id == original)
                    .ok_or_else(|| anyhow::anyhow!("规则「{original}」不存在"))?;
                *r = rule.clone();
            }
            Ok(())
        })
        .await
    {
        // 规则本身不合法（缺 target、id 重复等）时也回填重渲染，
        // 而不是把用户填的内容丢在孤立错误页里（规范 §6「提交失败保留已填内容」）。
        // field 传空串：这类错误不属于某一个输入框，只进顶部摘要。
        return Ok(HtmlTemplate(rules_tpl(
            &ctx,
            &snapshot,
            FormEcho::fail(&form, "", "", "规则", format!("{e:#}")),
        ))
        .into_response());
    }

    Ok(see_other("/rules"))
}

pub async fn rules_delete(
    State(state): State<Arc<AppState>>,
    axum::Extension(ctx): axum::Extension<RequestContext>,
    Path(id): Path<String>,
    body: Bytes,
) -> Result<Response, AppError> {
    let form = FormData::from_bytes(&body);
    form.verify_csrf(&state, &ctx)?;
    state
        .store
        .update(None, |store| cascade::delete_rule(store, &id))
        .await?;
    Ok(see_other("/rules"))
}

/// 批量粘贴导入。逻辑类规则存为 raw（无法安全结构化）。
pub async fn rules_import(
    State(state): State<Arc<AppState>>,
    axum::Extension(ctx): axum::Extension<RequestContext>,
    body: Bytes,
) -> Result<Response, AppError> {
    let form = FormData::from_bytes(&body);
    form.verify_csrf(&state, &ctx)?;

    let section = match form.get_owned("section").as_str() {
        "override" => RuleSection::Override,
        "final" => RuleSection::Final,
        _ => RuleSection::Normal,
    };
    let text = form.get_owned("text");

    let snapshot = state.store.snapshot().await;

    let mut new_rules: Vec<RuleDef> = Vec::new();
    let mut bad_lines: Vec<String> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let parsed = parse_rule_text(line, true);
        if parsed.tp.is_empty() {
            bad_lines.push(line.to_string());
            continue;
        }
        let id = format!("r-{}", crate::auth::random_hex(4));
        if shape_of(&parsed.tp) == RuleShape::Logical {
            // payload 含逗号，结构化不可靠 → 存 raw，UI 会标注「不参与引用检查」
            new_rules.push(RuleDef {
                id,
                section,
                raw: Some(line.to_string()),
                ..Default::default()
            });
        } else {
            new_rules.push(RuleDef {
                id,
                section,
                r#type: Some(parsed.tp),
                payload: (!parsed.payload.is_empty()).then_some(parsed.payload),
                target: (!parsed.target.is_empty()).then_some(parsed.target),
                params: parsed.params,
                raw: None,
            });
        }
    }

    if new_rules.is_empty() {
        let msg = if bad_lines.is_empty() {
            "没有解析出任何规则。每行一条，格式为 `类型,内容,目标`，以 # 开头的行视为注释"
                .to_string()
        } else {
            format!(
                "没有解析出任何规则。这些行无法识别：{}",
                bad_lines.join("；")
            )
        };
        return Ok(HtmlTemplate(rules_tpl(
            &ctx,
            &snapshot,
            FormEcho::fail(&form, "import", "text", "规则文本", msg),
        ))
        .into_response());
    }

    if let Err(e) = state
        .store
        .update(None, move |store| {
            store.rules.extend(new_rules);
            Ok(())
        })
        .await
    {
        // 导入的规则通不过校验时保留用户粘贴的原文，并把原因显示在文本框旁。
        return Ok(HtmlTemplate(rules_tpl(
            &ctx,
            &snapshot,
            FormEcho::fail(&form, "import", "text", "规则文本", format!("{e:#}")),
        ))
        .into_response());
    }

    Ok(see_other("/rules"))
}

pub async fn rules_move(
    State(state): State<Arc<AppState>>,
    axum::Extension(ctx): axum::Extension<RequestContext>,
    body: Bytes,
) -> Result<Response, AppError> {
    let form = FormData::from_bytes(&body);
    form.verify_csrf(&state, &ctx)?;
    let id = form.get_owned("id");
    let dir = form.get_owned("dir");

    state
        .store
        .update(None, move |store| {
            let idx = store
                .rules
                .iter()
                .position(|r| r.id == id)
                .ok_or_else(|| anyhow::anyhow!("规则「{id}」不存在"))?;
            let section = store.rules[idx].section;
            // 只在同 section 内移动
            let candidates: Vec<usize> = store
                .rules
                .iter()
                .enumerate()
                .filter(|(_, r)| r.section == section)
                .map(|(i, _)| i)
                .collect();
            let pos = candidates
                .iter()
                .position(|i| *i == idx)
                .ok_or_else(|| anyhow::anyhow!("规则定位失败"))?;
            let swap_with = match dir.as_str() {
                "up" if pos > 0 => candidates[pos - 1],
                "down" if pos + 1 < candidates.len() => candidates[pos + 1],
                _ => return Ok(()),
            };
            store.rules.swap(idx, swap_with);
            Ok(())
        })
        .await?;

    Ok(see_other("/rules"))
}

// ================================================================ 地区别名

/// 地区别名页视图模型。校验失败时同样走这里，只是多带一份回显。
fn aliases_tpl(ctx: &RequestContext, snapshot: &Store, echo: FormEcho) -> AliasesTpl {
    let mut rows = Vec::new();
    for (i, a) in snapshot.aliases.iter().enumerate() {
        rows.push(AliasRow {
            index_id: format!("a{i}"),
            region: a.region.clone(),
            aliases: a.aliases.join(", "),
            count: a.aliases.len(),
        });
    }
    let create_open = echo.scope.is_empty() && !echo.is_empty();
    AliasesTpl {
        nav: "aliases",
        csrf: ctx.csrf.clone(),
        rows,
        create_open,
        form: echo,
    }
}

/// `/aliases?new=1`：直接展开「新增地区」弹窗。
#[derive(Debug, Default, Deserialize)]
pub struct AliasesQuery {
    #[serde(rename = "new")]
    pub new_form: Option<String>,
}

pub async fn aliases_page(
    State(state): State<Arc<AppState>>,
    axum::Extension(ctx): axum::Extension<RequestContext>,
    Query(q): Query<AliasesQuery>,
) -> Result<Response, AppError> {
    let snapshot = state.store.snapshot().await;
    let mut tpl = aliases_tpl(&ctx, &snapshot, FormEcho::default());
    tpl.create_open = q.new_form.is_some();
    Ok(HtmlTemplate(tpl).into_response())
}

pub async fn aliases_save(
    State(state): State<Arc<AppState>>,
    axum::Extension(ctx): axum::Extension<RequestContext>,
    body: Bytes,
) -> Result<Response, AppError> {
    let form = FormData::from_bytes(&body);
    form.verify_csrf(&state, &ctx)?;

    let original = form.get_owned("original_region");
    let region = form.get_owned("region");
    let aliases = split_list(form.get("aliases").unwrap_or(""));

    let snapshot = state.store.snapshot().await;

    // 校验失败回填重渲染；`original` 为空表示「新增地区」表单，否则是正在编辑的那一行。
    macro_rules! refill {
        ($field:expr, $label:expr, $msg:expr) => {
            return Ok(HtmlTemplate(aliases_tpl(
                &ctx,
                &snapshot,
                FormEcho::fail(&form, &original, $field, $label, $msg),
            ))
            .into_response())
        };
    }

    if region.is_empty() {
        refill!("region", "地区名", "地区名不能为空".to_string());
    }
    if aliases.is_empty() {
        refill!(
            "aliases",
            "别名",
            "至少需要一个别名，逗号分隔，例如 `HK, Hong Kong, 🇭🇰`".to_string()
        );
    }

    // 复用导出期的同一套冲突检测，保证「保存时拒绝」与「导出时拦截」判据一致。
    let mut prospective = snapshot.aliases.clone();
    prospective.retain(|a| a.region != original && a.region != region);
    prospective.push(RegionAlias {
        region: region.clone(),
        aliases: aliases.clone(),
    });
    if let Some(msg) = crate::region::find_alias_conflicts(&prospective).first() {
        refill!(
            "aliases",
            "别名",
            format!("{msg}；同一别名只能归属一个地区")
        );
    }

    state
        .store
        .update(None, move |store| {
            if !original.is_empty() && original != region {
                store.aliases.retain(|a| a.region != original);
            }
            match store.aliases.iter_mut().find(|a| a.region == region) {
                Some(a) => a.aliases = aliases.clone(),
                None => store.aliases.push(RegionAlias {
                    region: region.clone(),
                    aliases: aliases.clone(),
                }),
            }
            Ok(())
        })
        .await?;

    Ok(see_other("/aliases"))
}

pub async fn aliases_delete(
    State(state): State<Arc<AppState>>,
    axum::Extension(ctx): axum::Extension<RequestContext>,
    body: Bytes,
) -> Result<Response, AppError> {
    let form = FormData::from_bytes(&body);
    form.verify_csrf(&state, &ctx)?;
    let region = form.get_owned("region");
    state
        .store
        .update(None, move |store| {
            store.aliases.retain(|a| a.region != region);
            Ok(())
        })
        .await?;
    Ok(see_other("/aliases"))
}

// ================================================================ 手动节点

/// 手动节点页视图模型。解析失败的旧数据也展示出来，便于删除。
fn manual_nodes_tpl(ctx: &RequestContext, snapshot: &Store, form: FormEcho) -> ManualNodesTpl {
    let mut rows = Vec::new();
    for n in &snapshot.manual_nodes {
        let mut row = ManualNodeRow {
            id: n.id.clone(),
            name: n.id.clone(),
            kind: String::new(),
            server: String::new(),
            port: String::new(),
            used_by: String::new(),
            parse_error: String::new(),
        };
        match crate::links::parse_link(&n.link) {
            Ok(proxy) => {
                let name = mstr(&proxy, "name").unwrap_or("").to_string();
                row.name = name.clone();
                row.kind = mstr(&proxy, "type").unwrap_or("").to_string();
                row.server = mstr(&proxy, "server").unwrap_or("").to_string();
                row.port = mget(&proxy, "port")
                    .and_then(|v| v.as_u64())
                    .map(|p| p.to_string())
                    .unwrap_or_default();
                row.used_by = snapshot
                    .groups
                    .iter()
                    .filter(|g| g.manual_nodes.iter().any(|m| m == &name))
                    .map(|g| g.name.clone())
                    .collect::<Vec<_>>()
                    .join("、");
            }
            Err(e) => row.parse_error = e,
        }
        rows.push(row);
    }

    let create_open = form.scope.is_empty() && !form.is_empty();
    ManualNodesTpl {
        nav: "nodes",
        csrf: ctx.csrf.clone(),
        rows,
        create_open,
        form,
    }
}

/// `/nodes?new=1`：直接展开「添加手动节点」弹窗。
#[derive(Debug, Default, Deserialize)]
pub struct ManualNodesQuery {
    #[serde(rename = "new")]
    pub new_form: Option<String>,
}

pub async fn manual_nodes_page(
    State(state): State<Arc<AppState>>,
    axum::Extension(ctx): axum::Extension<RequestContext>,
    Query(q): Query<ManualNodesQuery>,
) -> Result<Response, AppError> {
    let snapshot = state.store.snapshot().await;
    let mut tpl = manual_nodes_tpl(&ctx, &snapshot, FormEcho::default());
    tpl.create_open = q.new_form.is_some();
    Ok(HtmlTemplate(tpl).into_response())
}

pub async fn manual_nodes_save(
    State(state): State<Arc<AppState>>,
    axum::Extension(ctx): axum::Extension<RequestContext>,
    body: Bytes,
) -> Result<Response, AppError> {
    let form = FormData::from_bytes(&body);
    form.verify_csrf(&state, &ctx)?;
    let snapshot = state.store.snapshot().await;
    let text = form.get_owned("links");

    let mut errors: Vec<String> = Vec::new();
    let mut new_nodes: Vec<ManualNode> = Vec::new();
    let mut names: std::collections::HashSet<String> = snapshot
        .manual_nodes
        .iter()
        .filter_map(|n| crate::links::parse_link(&n.link).ok())
        .filter_map(|p| mstr(&p, "name").map(|s| s.to_string()))
        .collect();

    for (line_no, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let proxy = match crate::links::parse_link(line) {
            Ok(proxy) => proxy,
            Err(e) => {
                errors.push(format!("第 {} 行：{e}", line_no + 1));
                continue;
            }
        };
        let name = mstr(&proxy, "name").unwrap_or("").trim().to_string();
        if name.is_empty() {
            errors.push(format!("第 {} 行：缺少节点名", line_no + 1));
            continue;
        }
        if !names.insert(name.clone()) {
            errors.push(format!("第 {} 行：节点名「{name}」重复", line_no + 1));
            continue;
        }
        new_nodes.push(ManualNode {
            id: format!("n-{}", crate::auth::random_hex(4)),
            link: line.to_string(),
        });
    }

    if !errors.is_empty() {
        return Ok(HtmlTemplate(manual_nodes_tpl(
            &ctx,
            &snapshot,
            FormEcho::fail(&form, "", "links", "分享链接", errors.join("；")),
        ))
        .into_response());
    }
    if new_nodes.is_empty() {
        return Ok(HtmlTemplate(manual_nodes_tpl(
            &ctx,
            &snapshot,
            FormEcho::fail(
                &form,
                "",
                "links",
                "分享链接",
                "没有解析出任何节点。每行一条分享链接，空行与 # 开头的行会被忽略".to_string(),
            ),
        ))
        .into_response());
    }

    state
        .store
        .update(None, move |store| {
            store.manual_nodes.extend(new_nodes);
            Ok(())
        })
        .await?;
    Ok(see_other("/nodes"))
}

pub async fn manual_nodes_delete(
    State(state): State<Arc<AppState>>,
    axum::Extension(ctx): axum::Extension<RequestContext>,
    body: Bytes,
) -> Result<Response, AppError> {
    let form = FormData::from_bytes(&body);
    form.verify_csrf(&state, &ctx)?;
    let id = form.get_owned("id");

    state
        .store
        .update(None, move |store| {
            let node = store
                .manual_node(&id)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("手动节点「{id}」不存在"))?;
            let proxy = crate::links::parse_link(&node.link)
                .map_err(|e| anyhow::anyhow!("手动节点「{id}」解析失败：{e}"))?;
            let name = mstr(&proxy, "name").unwrap_or("").to_string();
            let refs: Vec<String> = store
                .groups
                .iter()
                .filter(|g| g.manual_nodes.iter().any(|m| m == &name))
                .map(|g| g.name.clone())
                .collect();
            if !refs.is_empty() {
                anyhow::bail!(
                    "手动节点「{name}」仍被分组 {} 引用，请先从分组里移除",
                    refs.join("、")
                );
            }
            store.manual_nodes.retain(|n| n.id != id);
            Ok(())
        })
        .await?;
    Ok(see_other("/nodes"))
}

// ================================================================ 通用设置

const STRUCTURED_GENERAL_KEYS: &[&str] = &[
    "mixed-port",
    "socks-port",
    "port",
    "allow-lan",
    "mode",
    "log-level",
    "ipv6",
];

/// 通用设置页视图模型。校验失败时同样走这里，只是多带一份回显。
fn general_tpl(
    state: &AppState,
    ctx: &RequestContext,
    snapshot: &Store,
    form: FormEcho,
    notice: Option<(&str, &str)>,
) -> GeneralTpl {
    let get = |k: &str| -> String {
        mget(&snapshot.general, k)
            .map(|v| match v {
                Yaml::String(s) => s.clone(),
                Yaml::Bool(b) => b.to_string(),
                other => serde_yaml_ng::to_string(other)
                    .unwrap_or_default()
                    .trim()
                    .to_string(),
            })
            .unwrap_or_default()
    };

    let mut advanced = snapshot.general.clone();
    for k in STRUCTURED_GENERAL_KEYS {
        advanced.remove(Yaml::String((*k).to_string()));
    }
    let stored_advanced = serde_yaml_ng::to_string(&Yaml::Mapping(advanced))
        .unwrap_or_default()
        .trim()
        .to_string();
    let stored_region_source = match snapshot.settings.region_source {
        RegionSource::NameFirst => "name_first".to_string(),
        RegionSource::IpFirst => "ip_first".to_string(),
    };

    // 这两个键现在由下拉框管理，取值只可能是 true / false / 空。
    // 历史数据里可能存过 "yes" / "on" 这类写法，不归一化的话下拉会落空，
    // 用户一保存就把这个键悄悄删掉了。
    let norm_bool = |k: &str| -> String {
        match mget(&snapshot.general, k) {
            Some(Yaml::Bool(b)) => b.to_string(),
            Some(Yaml::String(s)) => {
                let s = s.trim().to_ascii_lowercase();
                match s.as_str() {
                    "" => String::new(),
                    "true" | "on" | "1" | "yes" => "true".to_string(),
                    _ => "false".to_string(),
                }
            }
            Some(Yaml::Number(_)) => "false".to_string(),
            _ => String::new(),
        }
    };

    // 出错时以提交值为准，保证「提交失败保留已填内容」。
    // 这些字段在模板里必须写成 `self.<字段名>`：本函数带 `fetch(path)` 前缀，
    // 不写 `self.` 时 askama 会把 `<字段名>` 当成普通变量（恒为 false）。
    GeneralTpl {
        nav: "general",
        csrf: ctx.csrf.clone(),
        region_source: form.value("", "region_source", &stored_region_source),
        filter_enabled: form.checked("", "filter_enabled", snapshot.settings.filter_enabled),
        filter_keywords: form.value(
            "",
            "filter_keywords",
            &snapshot.settings.filter_keywords.join(", "),
        ),
        base_url: form.value("", "base_url", &snapshot.settings.base_url),
        advanced_yaml: form.value("", "advanced_yaml", &stored_advanced),
        // 始终用当前版本号，用户修好后可以直接重试。
        revision: snapshot.revision,
        mixed_port: form.value("", "mixed-port", &get("mixed-port")),
        socks_port: form.value("", "socks-port", &get("socks-port")),
        port: form.value("", "port", &get("port")),
        allow_lan: form.value("", "allow-lan", &norm_bool("allow-lan")),
        mode: form.value("", "mode", &get("mode")),
        log_level: form.value("", "log-level", &get("log-level")),
        ipv6: form.value("", "ipv6", &norm_bool("ipv6")),
        proxy_enabled: form.checked("", "proxy_enabled", snapshot.settings.proxy.enabled),
        proxy_url: form.value("", "proxy_url", &snapshot.settings.proxy.url),
        proxy_effective: effective_proxy_view(state, snapshot),
        geoip: geoip_view(state, snapshot, &form, notice),
        form,
    }
}

fn effective_proxy_view(state: &AppState, snapshot: &Store) -> String {
    let proxy = if snapshot.settings.proxy.configured {
        snapshot
            .settings
            .proxy
            .enabled
            .then(|| snapshot.settings.proxy.url.clone())
    } else {
        crate::config::effective_proxy(&state.cfg)
    };
    proxy
        .as_deref()
        .map(crate::proxy::redact)
        .unwrap_or_else(|| "直连".to_string())
}

/// 构造 GeoIP 卡片：合并磁盘上的库信息、持久化状态与表单回显。
fn geoip_view(
    state: &AppState,
    snapshot: &Store,
    form: &FormEcho,
    notice: Option<(&str, &str)>,
) -> GeoipView {
    let settings = &snapshot.settings.geoip;
    let info = state.geo.db_info();
    let status = state.geoip.status();
    let now = Utc::now();

    let default_url = geoip_update::current_builtin_source_url(now);
    let source = status
        .source_url
        .as_deref()
        .map(geoip_update::redact_url)
        .unwrap_or_else(|| format!("内置 {}", geoip_update::BUILTIN_SOURCE_LABEL));

    let next_update = if !settings.auto_update {
        "自动更新已关闭".to_string()
    } else if let Some(last) = status.last_success_at {
        let interval = chrono::Duration::days(i64::from(settings.interval_days_clamped()));
        (last + interval).format("%Y-%m-%d %H:%M UTC").to_string()
    } else {
        "等待首次自动更新".to_string()
    };

    let (notice_kind, notice_text) = notice
        .map(|(k, m)| (k.to_string(), m.to_string()))
        .unwrap_or_default();

    GeoipView {
        installed: info.is_some(),
        db_path: info
            .as_ref()
            .map(|i| i.path.display().to_string())
            .unwrap_or_else(|| {
                format!(
                    "{}/geoip/{}",
                    state.store.storage_label(),
                    geoip_update::AUTO_DB_FILE
                )
            }),
        db_type: info
            .as_ref()
            .map(|i| i.database_type.clone())
            .unwrap_or_else(|| "—".to_string()),
        build: info
            .as_ref()
            .map(|i| crate::geoip::format_epoch(i.build_epoch))
            .unwrap_or_else(|| "—".to_string()),
        ip_version: info
            .as_ref()
            .map(|i| format!("IPv{}", i.ip_version))
            .unwrap_or_else(|| "—".to_string()),
        size: info
            .as_ref()
            .map(|i| geoip_update::human_size(i.size))
            .unwrap_or_else(|| "—".to_string()),
        modified: info
            .as_ref()
            .and_then(|i| i.modified)
            .map(|t| t.format("%Y-%m-%d %H:%M UTC").to_string())
            .unwrap_or_else(|| "—".to_string()),
        last_attempt: status
            .last_attempt_at
            .map(|t| t.format("%Y-%m-%d %H:%M UTC").to_string())
            .unwrap_or_else(|| "—".to_string()),
        last_success: status
            .last_success_at
            .map(|t| t.format("%Y-%m-%d %H:%M UTC").to_string())
            .unwrap_or_else(|| "—".to_string()),
        last_error: status.last_error.clone().unwrap_or_default(),
        last_error_kind: status
            .last_error_kind
            .map(|k| k.label().to_string())
            .unwrap_or_default(),
        source,
        default_url,
        auto_update: form.checked("geoip", "geoip_auto_update", settings.auto_update),
        interval_days: form.value(
            "geoip",
            "geoip_interval_days",
            &settings.interval_days_clamped().to_string(),
        ),
        custom_url: form.value("geoip", "geoip_custom_url", &settings.url),
        next_update,
        notice_kind,
        notice: notice_text,
    }
}

/// 通用设置页的查询参数：仅用于展示一次性的操作结果（PRG 模式）。
#[derive(Debug, Default, Deserialize)]
pub struct GeneralQuery {
    #[serde(default)]
    pub geoip: Option<String>,
}

fn geoip_notice(kind: Option<&str>) -> Option<(&'static str, &'static str)> {
    match kind {
        Some("ok") => Some(("ok", "GeoIP 库已下载并安装，立即生效。")),
        Some("err") => Some(("error", "GeoIP 库更新失败，原因见下方「最近一次尝试」。")),
        Some("busy") => Some((
            "info",
            "已有一次 GeoIP 更新正在进行，请稍后刷新页面查看结果。",
        )),
        Some("saved") => Some(("ok", "GeoIP 设置已保存。")),
        _ => None,
    }
}

pub async fn general_page(
    State(state): State<Arc<AppState>>,
    axum::Extension(ctx): axum::Extension<RequestContext>,
    Query(query): Query<GeneralQuery>,
) -> Result<Response, AppError> {
    let snapshot = state.store.snapshot().await;
    Ok(HtmlTemplate(general_tpl(
        &state,
        &ctx,
        &snapshot,
        FormEcho::default(),
        geoip_notice(query.geoip.as_deref()),
    ))
    .into_response())
}

pub async fn general_save(
    State(state): State<Arc<AppState>>,
    axum::Extension(ctx): axum::Extension<RequestContext>,
    body: Bytes,
) -> Result<Response, AppError> {
    let form = FormData::from_bytes(&body);
    form.verify_csrf(&state, &ctx)?;
    let expected = form.get_u64("revision");

    let snapshot = state.store.snapshot().await;

    macro_rules! refill {
        ($scope:expr, $field:expr, $label:expr, $msg:expr) => {
            return Ok(HtmlTemplate(general_tpl(
                &state,
                &ctx,
                &snapshot,
                FormEcho::fail(&form, $scope, $field, $label, $msg),
                None,
            ))
            .into_response())
        };
    }

    // 乐观并发校验：版本不一致时把原因带回页面，而不是丢到孤立错误页（§8.2）。
    if let Some(exp) = expected
        && exp != snapshot.revision
    {
        refill!(
            "",
            "revision",
            "整体",
            format!(
                "数据已被其他操作修改（当前版本 {}，你提交的是 {exp}），请确认本页内容后重新提交",
                snapshot.revision
            )
        );
    }

    // 高级 YAML 不得包含表单已暴露的键（§5.1）
    let advanced_text = form.get_owned("advanced_yaml");
    let mut advanced = Mapping::new();
    if !advanced_text.is_empty() {
        let parsed: Yaml = match serde_yaml_ng::from_str(&advanced_text) {
            Ok(v) => v,
            Err(e) => refill!(
                "",
                "advanced_yaml",
                "高级配置",
                format!("不是合法 YAML：{e}")
            ),
        };
        match parsed {
            Yaml::Mapping(m) => {
                for (k, v) in m {
                    let key = k.as_str().unwrap_or("").to_string();
                    if STRUCTURED_GENERAL_KEYS.contains(&key.as_str()) {
                        refill!(
                            "",
                            "advanced_yaml",
                            "高级配置",
                            format!("键「{key}」已由上方表单管理，请从原文 YAML 中移除")
                        );
                    }
                    advanced.insert(k, v);
                }
            }
            Yaml::Null => {}
            _ => refill!(
                "",
                "advanced_yaml",
                "高级配置",
                "必须是 YAML 映射，例如 `dns:` 开头的段落".to_string()
            ),
        }
    }

    let mut general = Mapping::new();
    for key in STRUCTURED_GENERAL_KEYS {
        let raw = form.get_owned(key);
        if raw.is_empty() {
            continue;
        }
        let value = match *key {
            "allow-lan" | "ipv6" => Yaml::Bool(matches!(raw.as_str(), "true" | "on" | "1" | "yes")),
            "mixed-port" | "socks-port" | "port" => match raw.trim().parse::<i64>() {
                Ok(n) if (1..=65535).contains(&n) => Yaml::Number(n.into()),
                // 与模板上的 min/max 一致：浏览器校验可被绕过，服务端必须自己拦。
                Ok(n) => refill!(
                    "",
                    key,
                    key,
                    format!("端口「{n}」超出范围：必须是 1–65535 之间的整数")
                ),
                Err(_) => refill!("", key, key, format!("「{raw}」不是数字")),
            },
            _ => ystr(raw),
        };
        general.insert(ystr(*key), value);
    }
    for (k, v) in advanced {
        general.insert(k, v);
    }

    let region_source = match form.get_owned("region_source").as_str() {
        "ip_first" => RegionSource::IpFirst,
        _ => RegionSource::NameFirst,
    };
    let filter_enabled = form.get_bool("filter_enabled");
    let filter_keywords = split_list(form.get("filter_keywords").unwrap_or(""));
    let base_url = match normalize_base_url(&form.get_owned("base_url")) {
        Ok(v) => v,
        Err(msg) => refill!("", "base_url", "BASEURL", msg),
    };
    let proxy_enabled = form.get_bool("proxy_enabled");
    let raw_proxy_url = form.get_owned("proxy_url");
    let proxy_url = match crate::proxy::normalize(&raw_proxy_url) {
        Ok(Some(value)) => value,
        Ok(None) if proxy_enabled => refill!(
            "",
            "proxy_url",
            "代理地址",
            "已启用代理时必须填写代理地址".to_string()
        ),
        Ok(None) => String::new(),
        Err(e) if proxy_enabled => refill!("", "proxy_url", "代理地址", e.to_string()),
        Err(_) => raw_proxy_url.trim().to_string(),
    };
    let effective_proxy = proxy_enabled.then_some(proxy_url.clone());

    state
        .store
        .update(expected, move |store| {
            store.general = general.clone();
            store.settings.region_source = region_source;
            store.settings.filter_enabled = filter_enabled;
            store.settings.filter_keywords = filter_keywords.clone();
            store.settings.base_url = base_url.clone();
            store.settings.proxy = ProxySettings {
                enabled: proxy_enabled,
                url: proxy_url.clone(),
                configured: true,
            };
            Ok(())
        })
        .await?;

    state.refresher.set_proxy(effective_proxy.clone()).await?;
    state.geoip.set_proxy(effective_proxy).await?;
    Ok(see_other("/general"))
}

/// 保存 GeoIP 自动更新设置（与 Clash 客户端配置分开的表单）。
pub async fn geoip_settings_save(
    State(state): State<Arc<AppState>>,
    axum::Extension(ctx): axum::Extension<RequestContext>,
    body: Bytes,
) -> Result<Response, AppError> {
    let form = FormData::from_bytes(&body);
    form.verify_csrf(&state, &ctx)?;
    let expected = form.get_u64("revision");
    let snapshot = state.store.snapshot().await;

    macro_rules! refill {
        ($field:expr, $label:expr, $msg:expr) => {
            return Ok(HtmlTemplate(general_tpl(
                &state,
                &ctx,
                &snapshot,
                FormEcho::fail(&form, "geoip", $field, $label, $msg),
                None,
            ))
            .into_response())
        };
    }

    if let Some(exp) = expected
        && exp != snapshot.revision
    {
        refill!(
            "revision",
            "整体",
            format!(
                "数据已被其他操作修改（当前版本 {}，你提交的是 {exp}），请刷新后重试",
                snapshot.revision
            )
        );
    }

    let auto_update = form.get_bool("geoip_auto_update");
    let raw_interval = form.get_owned("geoip_interval_days");
    let interval_days = if raw_interval.is_empty() {
        snapshot.settings.geoip.interval_days_clamped()
    } else {
        match raw_interval.parse::<u32>() {
            Ok(n) if (1..=365).contains(&n) => n,
            Ok(n) => refill!(
                "geoip_interval_days",
                "自动更新间隔",
                format!("{n} 超出范围：必须是 1–365 天")
            ),
            Err(_) => refill!(
                "geoip_interval_days",
                "自动更新间隔",
                format!("「{raw_interval}」不是整数")
            ),
        }
    };

    let custom_url = form.get_owned("geoip_custom_url");
    if !custom_url.is_empty()
        && let Err(e) = geoip_update::validate_source_url(&custom_url)
    {
        refill!("geoip_custom_url", "下载地址", format!("{e:#}"));
    }

    state
        .store
        .update(expected, move |store| {
            store.settings.geoip = GeoipSettings {
                auto_update,
                interval_days,
                url: custom_url.clone(),
            };
            Ok(())
        })
        .await?;

    Ok(see_other("/general?geoip=saved"))
}

/// 手动下载并安装一次 GeoIP 库（同步执行，完成后 303 回通用设置页）。
pub async fn geoip_download(
    State(state): State<Arc<AppState>>,
    axum::Extension(ctx): axum::Extension<RequestContext>,
    body: Bytes,
) -> Result<Response, AppError> {
    let form = FormData::from_bytes(&body);
    form.verify_csrf(&state, &ctx)?;

    let location = match state.geoip.update("manual", true).await {
        Ok(o) if o.skipped_busy => "/general?geoip=busy",
        Ok(o) => {
            tracing::info!(
                "手动更新 GeoIP：{}（changed={}，库={}）",
                o.message,
                o.changed,
                o.info
                    .as_ref()
                    .map(|i| i.database_type.as_str())
                    .unwrap_or("未变化")
            );
            "/general?geoip=ok"
        }
        Err(e) => {
            tracing::warn!("手动更新 GeoIP 库失败：{e:#}");
            "/general?geoip=err"
        }
    };
    Ok(see_other(location))
}

// ================================================================ 方案

pub async fn profile_new(
    State(state): State<Arc<AppState>>,
    axum::Extension(ctx): axum::Extension<RequestContext>,
) -> Result<Response, AppError> {
    let snapshot = state.store.snapshot().await;
    Ok(HtmlTemplate(build_profile_tpl(
        &snapshot,
        &ctx,
        None,
        true,
        FormEcho::default(),
    )?)
    .into_response())
}

fn profile_dependency_error(snapshot: &Store, groups: &[String]) -> Option<String> {
    let missing = cascade::required_dependencies(snapshot, groups);
    if missing.is_empty() {
        return None;
    }
    Some(format!(
        "方案缺少这些依赖分组：{}。请先勾选它们，或取消引用它们的分组。",
        missing.join("、")
    ))
}

fn build_profile_tpl(
    snapshot: &Store,
    ctx: &RequestContext,
    profile: Option<&Profile>,
    is_new: bool,
    form: FormEcho,
) -> Result<ProfileTpl, AppError> {
    // 校验失败时必须以**提交值**为准渲染勾选状态，否则用户取消掉的勾选会被悄悄恢复。
    // 注意「一个都没勾」是有效输入，不能与「没有回显」混为一谈，因此用 is_empty()
    // 判断本次是否来自一次失败提交，而不是看列表是否为空。
    let echoed = !form.is_empty();
    let selected_up = if echoed {
        form.values("", "upstreams")
    } else {
        profile.map(|p| p.upstreams.clone()).unwrap_or_default()
    };
    let selected_gr = if echoed {
        form.values("", "groups")
    } else {
        profile.map(|p| p.groups.clone()).unwrap_or_default()
    };
    let selected_ru = if echoed {
        form.values("", "rules")
    } else {
        profile.map(|p| p.rules.clone()).unwrap_or_default()
    };

    let all_upstreams = snapshot
        .upstreams
        .iter()
        .map(|u| Selectable {
            value: u.id.clone(),
            label: format!("{}（{}）", u.name, u.id),
            checked: selected_up.contains(&u.id),
            deps_json: "[]".to_string(),
            auto: false,
        })
        .collect();
    let missing_deps = cascade::required_dependencies(snapshot, &selected_gr);
    let all_groups = snapshot
        .groups
        .iter()
        .map(|g| {
            let auto = missing_deps.contains(&g.name);
            Selectable {
                value: g.name.clone(),
                label: format!("{} · {}", g.name, g.group_type()),
                checked: selected_gr.contains(&g.name) || auto,
                deps_json: serde_json::to_string(&g.manual_groups)
                    .unwrap_or_else(|_| "[]".to_string()),
                auto,
            }
        })
        .collect();
    let all_rules = snapshot
        .rules
        .iter()
        .map(|r| Selectable {
            value: r.id.clone(),
            label: format!(
                "[{}] {}",
                r.section.as_str(),
                crate::rules::serialize_rule(r).unwrap_or_default()
            ),
            checked: selected_ru.contains(&r.id),
            deps_json: "[]".to_string(),
            auto: false,
        })
        .collect();

    let token = profile.and_then(|p| p.token.clone()).unwrap_or_default();
    let sub_link = profile
        .map(|p| {
            sub_url(
                &snapshot.settings.base_url,
                &p.slug,
                p.token.as_deref(),
                false,
            )
        })
        .unwrap_or_default();
    let download_link = profile
        .map(|p| {
            sub_url(
                &snapshot.settings.base_url,
                &p.slug,
                p.token.as_deref(),
                true,
            )
        })
        .unwrap_or_default();

    Ok(ProfileTpl {
        nav: "profiles",
        csrf: ctx.csrf.clone(),
        is_new,
        slug: profile.map(|p| p.slug.clone()).unwrap_or_default(),
        name: profile.map(|p| p.name.clone()).unwrap_or_default(),
        revision: snapshot.revision,
        on_failure: match profile.map(|p| p.on_failure).unwrap_or_default() {
            OnFailure::Fail => "fail".to_string(),
            OnFailure::LastGood => "last_good".to_string(),
        },
        token,
        sub_url: sub_link,
        download_url: download_link,
        missing_deps,
        all_upstreams,
        all_groups,
        all_rules,
        form,
    })
}

pub async fn profile_page(
    State(state): State<Arc<AppState>>,
    axum::Extension(ctx): axum::Extension<RequestContext>,
    Path(slug): Path<String>,
) -> Result<Response, AppError> {
    let snapshot = state.store.snapshot().await;
    let profile = snapshot
        .profile(&slug)
        .ok_or_else(|| anyhow::anyhow!("方案「{slug}」不存在"))?
        .clone();
    Ok(HtmlTemplate(build_profile_tpl(
        &snapshot,
        &ctx,
        Some(&profile),
        false,
        FormEcho::default(),
    )?)
    .into_response())
}

pub async fn profile_create(
    State(state): State<Arc<AppState>>,
    axum::Extension(ctx): axum::Extension<RequestContext>,
    body: Bytes,
) -> Result<Response, AppError> {
    let form = FormData::from_bytes(&body);
    form.verify_csrf(&state, &ctx)?;

    let snapshot = state.store.snapshot().await;

    // 校验失败时不丢输入：回填并重渲染同一页（规范 §6「提交失败保留已填内容」）。
    macro_rules! refill {
        ($field:expr, $label:expr, $msg:expr) => {
            return Ok(HtmlTemplate(build_profile_tpl(
                &snapshot,
                &ctx,
                None,
                true,
                FormEcho::fail(&form, "", $field, $label, $msg),
            )?)
            .into_response())
        };
    }

    let mut slug = form.get_owned("slug");
    let name = form.get_owned("name");
    if name.is_empty() {
        refill!("name", "方案名", "方案名称不能为空".to_string());
    }
    if slug.is_empty() {
        slug = slugify(&name);
    }
    if !valid_slug(&slug) {
        refill!(
            "slug",
            "slug",
            format!("slug「{slug}」非法：只允许小写字母、数字与连字符")
        );
    }

    let upstreams = form.get_list("upstreams");
    let groups = form.get_list("groups");
    let rules = form.get_list("rules");
    if let Some(msg) = profile_dependency_error(&snapshot, &groups) {
        refill!("groups", "分组依赖", msg);
    }
    let on_failure = if form.get_owned("on_failure") == "last_good" {
        OnFailure::LastGood
    } else {
        OnFailure::Fail
    };
    let token_input = form.get_owned("token");

    // 先查重再写：这样冲突能回填到 slug 字段，而不是跳孤立错误页。
    if snapshot.profile(&slug).is_some() {
        refill!(
            "slug",
            "slug",
            format!("方案「{slug}」已存在，请换一个 slug")
        );
    }

    let slug_key = slug.clone();
    state
        .store
        .update(None, move |store| {
            if store.profile(&slug_key).is_some() {
                bail!("方案「{slug_key}」已存在");
            }
            store.profiles.push(Profile {
                slug: slug_key.clone(),
                name: name.clone(),
                upstreams: upstreams.clone(),
                groups: groups.clone(),
                rules: rules.clone(),
                on_failure,
                token: if token_input.is_empty() {
                    None
                } else {
                    Some(token_input.clone())
                },
            });
            Ok(())
        })
        .await?;

    Ok(see_other(&format!("/profiles/{slug}")))
}

pub async fn profile_save(
    State(state): State<Arc<AppState>>,
    axum::Extension(ctx): axum::Extension<RequestContext>,
    Path(slug): Path<String>,
    body: Bytes,
) -> Result<Response, AppError> {
    let form = FormData::from_bytes(&body);
    form.verify_csrf(&state, &ctx)?;
    let expected = form.get_u64("revision");

    let snapshot = state.store.snapshot().await;

    macro_rules! refill {
        ($field:expr, $label:expr, $msg:expr) => {{
            let profile = snapshot.profile(&slug);
            return Ok(HtmlTemplate(build_profile_tpl(
                &snapshot,
                &ctx,
                profile,
                false,
                FormEcho::fail(&form, "", $field, $label, $msg),
            )?)
            .into_response());
        }};
    }

    // 乐观并发校验：版本不一致时把原因带回页面（§8.2），而不是丢到孤立错误页。
    if let Some(exp) = expected
        && exp != snapshot.revision
    {
        refill!(
            "revision",
            "整体",
            format!(
                "数据已被其他操作修改（当前版本 {}，你提交的是 {exp}），请确认本页内容后重新提交",
                snapshot.revision
            )
        );
    }

    let name = form.get_owned("name");
    if name.is_empty() {
        refill!("name", "方案名", "方案名称不能为空".to_string());
    }
    let upstreams = form.get_list("upstreams");
    let groups = form.get_list("groups");
    let rules = form.get_list("rules");
    if let Some(msg) = profile_dependency_error(&snapshot, &groups) {
        refill!("groups", "分组依赖", msg);
    }
    let on_failure = if form.get_owned("on_failure") == "last_good" {
        OnFailure::LastGood
    } else {
        OnFailure::Fail
    };
    let token_input = form.get_owned("token");

    let slug_key = slug.clone();
    state
        .store
        .update(expected, move |store| {
            let p = store
                .profiles
                .iter_mut()
                .find(|p| p.slug == slug_key)
                .ok_or_else(|| anyhow::anyhow!("方案「{slug_key}」不存在"))?;
            p.name = name.clone();
            p.upstreams = upstreams.clone();
            p.groups = groups.clone();
            p.rules = rules.clone();
            p.on_failure = on_failure;
            p.token = if token_input.is_empty() {
                None
            } else {
                Some(token_input.clone())
            };
            Ok(())
        })
        .await?;

    Ok(see_other(&format!("/profiles/{slug}")))
}

pub async fn profile_delete(
    State(state): State<Arc<AppState>>,
    axum::Extension(ctx): axum::Extension<RequestContext>,
    Path(slug): Path<String>,
    body: Bytes,
) -> Result<Response, AppError> {
    let form = FormData::from_bytes(&body);
    form.verify_csrf(&state, &ctx)?;
    let slug_key = slug.clone();
    state
        .store
        .update(None, move |store| {
            store.profiles.retain(|p| p.slug != slug_key);
            Ok(())
        })
        .await?;
    let _ = crate::lastgood::remove(&state.store.storage(), &slug).await;
    Ok(see_other("/"))
}

pub async fn profile_preview(
    State(state): State<Arc<AppState>>,
    axum::Extension(ctx): axum::Extension<RequestContext>,
    Path(slug): Path<String>,
    body: Bytes,
) -> Result<Response, AppError> {
    let form = FormData::from_bytes(&body);
    form.verify_csrf(&state, &ctx)?;

    let outcome = state.export(&slug).await?;
    let snapshot = state.store.snapshot().await;
    let token = snapshot.profile(&slug).and_then(|p| p.token.clone());
    Ok(HtmlTemplate(PreviewTpl {
        nav: "profiles",
        slug: slug.clone(),
        has_token: token.as_deref().is_some_and(|t| !t.is_empty()),
        sub_url: sub_url(&snapshot.settings.base_url, &slug, token.as_deref(), false),
        download_url: sub_url(&snapshot.settings.base_url, &slug, token.as_deref(), true),
        ok: outcome.report.ok(),
        errors: outcome.report.errors.clone(),
        warnings: outcome.report.warnings.clone(),
        conflicts: outcome.report.conflicts.clone(),
        info: outcome.report.info.clone(),
        groups: outcome
            .group_stats
            .iter()
            .map(|g| PreviewGroup {
                name: g.name.clone(),
                members: g.member_count,
                auto: g.auto_count,
                reasons: g
                    .reasons
                    .iter()
                    .map(|(n, why)| format!("{n}：{}", why.join(" + ")))
                    .collect(),
            })
            .collect(),
        yaml: outcome.yaml.clone(),
        used_last_good: outcome.used_last_good,
        last_good_at: outcome
            .last_good_at
            .map(|t| t.format("%Y-%m-%d %H:%M UTC").to_string())
            .unwrap_or_default(),
    })
    .into_response())
}

// ================================================================ 订阅输出

#[derive(Deserialize)]
pub struct SubQuery {
    pub token: Option<String>,
    /// `?download=1` / `?download`：以附件形式下载。缺省为内联展示。
    #[serde(default)]
    pub download: Option<String>,
}

pub async fn subscribe(
    State(state): State<Arc<AppState>>,
    Path(slug): Path<String>,
    Query(q): Query<SubQuery>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let snapshot = state.store.snapshot().await;
    let profile = snapshot.profile(&slug).cloned();
    drop(snapshot);

    // 先判权限，再判存在性：否则未认证者可以通过 404/401 的差别枚举出有哪些方案。
    if !super::subscribe_authorized(
        &headers,
        &state,
        profile.as_ref().and_then(|p| p.token.as_deref()),
        q.token.as_deref(),
    ) {
        return Ok((
            StatusCode::UNAUTHORIZED,
            "需要认证：请提供 Basic Auth 或正确的订阅令牌",
        )
            .into_response());
    }

    let Some(profile) = profile else {
        return Ok((StatusCode::NOT_FOUND, "订阅不存在").into_response());
    };

    // 下载文件名使用**方案名称**（浏览器保存时显示），而不是 slug；
    // 非 ASCII 名称通过 filename* 传递，filename 保留 slug 作为安全回退。
    let download = q
        .download
        .as_deref()
        .is_some_and(|v| matches!(v, "" | "1" | "true" | "yes" | "on"));
    let filename = safe_yaml_filename(&profile.name, &slug);
    let disposition = content_disposition(
        if download { "attachment" } else { "inline" },
        &filename,
        &slug,
    );

    let outcome = state.export(&slug).await?;

    if !outcome.report.ok() {
        let msg = format!(
            "# 导出失败，未输出半成品配置\n# 原因：\n{}",
            outcome
                .report
                .errors
                .iter()
                .map(|e| format!("#   - {e}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
        return Ok((
            StatusCode::INTERNAL_SERVER_ERROR,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            msg,
        )
            .into_response());
    }

    let etag = format!("\"{}\"", outcome.etag);
    if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|value| value.split(',').any(|v| v.trim() == etag))
    {
        let mut resp = StatusCode::NOT_MODIFIED.into_response();
        if let Ok(v) = header::HeaderValue::from_str(&etag) {
            resp.headers_mut().insert(header::ETAG, v);
        }
        resp.headers_mut().insert(
            header::CACHE_CONTROL,
            header::HeaderValue::from_static("no-cache"),
        );
        return Ok(resp);
    }

    let mut resp = (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/yaml; charset=utf-8")],
        outcome.yaml.clone(),
    )
        .into_response();

    if let Ok(v) = header::HeaderValue::from_str(&etag) {
        resp.headers_mut().insert(header::ETAG, v);
    }
    resp.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-cache"),
    );

    if let Ok(v) = header::HeaderValue::from_str(&disposition) {
        resp.headers_mut().insert(header::CONTENT_DISPOSITION, v);
    }
    if let Ok(v) = header::HeaderValue::from_str(&outcome.generated_at.to_rfc3339()) {
        resp.headers_mut()
            .insert(header::HeaderName::from_static("x-generated-at"), v);
    }
    if outcome.used_last_good {
        resp.headers_mut().insert(
            header::HeaderName::from_static("x-last-good"),
            header::HeaderValue::from_static("true"),
        );
        if let Some(at) = outcome.last_good_at
            && let Ok(v) = header::HeaderValue::from_str(&at.to_rfc3339())
        {
            resp.headers_mut()
                .insert(header::HeaderName::from_static("x-last-good-at"), v);
        }
    }
    Ok(resp)
}

pub async fn healthz() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

// ================================================================ 备份

#[derive(Debug, Default, Deserialize)]
pub struct BackupsQuery {
    pub preview: Option<String>,
}

fn validate_backup_name(name: &str) -> Result<(), AppError> {
    if name.is_empty() || name.contains('/') || name.contains('\\') || name.contains("..") {
        bail!("非法的备份文件名");
    }
    Ok(())
}

fn diff_row(label: &'static str, set: &crate::store::DiffSet) -> BackupDiffRow {
    fn join(items: &[String]) -> String {
        if items.is_empty() {
            "—".to_string()
        } else {
            items.join("、")
        }
    }
    BackupDiffRow {
        label,
        added: join(&set.added),
        removed: join(&set.removed),
        changed: join(&set.changed),
    }
}

pub async fn backups_page(
    State(state): State<Arc<AppState>>,
    axum::Extension(ctx): axum::Extension<RequestContext>,
    Query(q): Query<BackupsQuery>,
) -> Result<Response, AppError> {
    let snapshot = state.store.snapshot().await;
    let mut rows: Vec<BackupRow> = Vec::new();
    for b in state.store.list_backups().await? {
        rows.push(BackupRow {
            name: b.name,
            created_at: b.created_at,
            size: fmt_bytes(b.size),
        });
    }

    let mut preview = None;
    if let Some(name) = q.preview {
        validate_backup_name(&name)?;
        let backup = state.store.read_backup(&name).await?;
        let diff = snapshot.diff(&backup);
        let mut diff_rows = vec![
            diff_row("上游", &diff.upstreams),
            diff_row("分组", &diff.groups),
            diff_row("规则", &diff.rules),
            diff_row("地区别名", &diff.aliases),
            diff_row("手动节点", &diff.manual_nodes),
            diff_row("方案", &diff.profiles),
        ];
        if diff.settings_changed {
            diff_rows.push(BackupDiffRow {
                label: "服务设置",
                added: "—".into(),
                removed: "—".into(),
                changed: "有变化".into(),
            });
        }
        if diff.general_changed {
            diff_rows.push(BackupDiffRow {
                label: "客户端通用配置",
                added: "—".into(),
                removed: "—".into(),
                changed: "有变化".into(),
            });
        }
        preview = Some(BackupPreviewTpl {
            name: name.clone(),
            created_at: rows
                .iter()
                .find(|r| r.name == name)
                .map(|r| r.created_at.clone())
                .unwrap_or_default(),
            current_revision: snapshot.revision,
            backup_revision: backup.revision,
            empty: diff.is_empty(),
            rows: diff_rows,
        });
    }

    Ok(HtmlTemplate(BackupsTpl {
        nav: "backups",
        csrf: ctx.csrf,
        rows,
        preview,
    })
    .into_response())
}

pub async fn backups_restore(
    State(state): State<Arc<AppState>>,
    axum::Extension(ctx): axum::Extension<RequestContext>,
    body: Bytes,
) -> Result<Response, AppError> {
    let form = FormData::from_bytes(&body);
    form.verify_csrf(&state, &ctx)?;
    let name = form.get_owned("name");
    validate_backup_name(&name)?;
    state.store.restore(&name, None).await?;
    Ok(see_other("/backups"))
}
