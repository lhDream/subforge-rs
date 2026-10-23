//! 应用状态与导出管线粘合层。
//!
//! 管线（AGENTS.md §6）：引用校验 → 取缓存（缺失才同步拉取）→ 解析/过滤/去重 →
//! 地区判定 → 分组求值 → 拓扑排序 → 规则分段 → 拼装 → 外部依赖校验 → 序列化。
//!
//! **一次导出全程使用同一份数据快照**（§8.2），不得在中途重新读取。

use crate::config::Config;
use crate::geoip::GeoResolver;
use crate::geoip_update::GeoipUpdater;
use crate::lastgood::{self, LastGood};
use crate::refresh::Refresher;
use crate::region::decide_region;
use crate::render::{PreparedNode, RegionNotes, RenderInput, RenderOutput, render};
use crate::store::model::{OnFailure, Profile, Store, mstr};
use crate::store::{StoreHandle, model};
use crate::{cache, fetch, subinfo};
use anyhow::{Result, bail};
use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

pub struct AppState {
    pub cfg: Config,
    pub store: StoreHandle,
    pub refresher: Arc<Refresher>,
    pub geo: Arc<GeoResolver>,
    /// GeoIP 库的下载/安装/定期更新。
    pub geoip: Arc<GeoipUpdater>,
    /// CSRF 密钥（进程启动时随机生成）。
    pub csrf_secret: String,
    /// 静态资源目录（便携包与 Docker 均可配置，不依赖 CWD）。
    pub static_dir: std::path::PathBuf,
    /// 导出结果缓存：key = profile slug，value = 签名 + 结果。
    pub render_cache: RwLock<HashMap<String, CachedRender>>,
    /// 分组页节点名的共享缓存：key = 数据快照 + 缓存文件元数据签名。
    pub node_names_cache: RwLock<Option<(String, Vec<String>)>>,
}

#[derive(Clone)]
pub struct CachedRender {
    pub signature: String,
    pub outcome: ExportOutcome,
}

#[derive(Debug, Clone)]
pub struct ExportOutcome {
    pub yaml: String,
    pub report: crate::validate::Report,
    pub group_stats: Vec<crate::render::GroupStat>,
    /// 是否使用了 last_good 回退。
    pub used_last_good: bool,
    pub last_good_at: Option<DateTime<Utc>>,
    pub generated_at: DateTime<Utc>,
    /// 导出输入签名（用于 HTTP ETag）。
    pub etag: String,
}

impl AppState {
    pub async fn export(&self, slug: &str) -> Result<ExportOutcome> {
        // 全程使用同一份快照
        let snapshot = self.store.snapshot().await;
        let Some(profile) = snapshot.profile(slug).cloned() else {
            bail!("方案「{slug}」不存在");
        };
        let signature = self.render_signature(&snapshot, &profile);

        // 相同输入直接复用渲染结果，避免客户端轮询时重复解析/计算。
        {
            let cache = self.render_cache.read().await;
            if let Some(cached) = cache.get(slug)
                && cached.signature == signature
            {
                return Ok(cached.outcome.clone());
            }
        }

        let mut outcome = self.export_with(&snapshot, &profile).await?;
        outcome.etag = format!("{:x}", Sha256::digest(signature.as_bytes()));

        if outcome.report.ok() {
            // last_good 只对启用该策略的方案有意义，避免每次成功导出都写盘。
            if profile.on_failure == OnFailure::LastGood
                && let Err(e) = lastgood::save(
                    &self.store.rendered_dir(),
                    slug,
                    &LastGood {
                        generated_at: outcome.generated_at,
                        reason: "导出成功".to_string(),
                        yaml: outcome.yaml.clone(),
                    },
                )
            {
                tracing::warn!("写入 last_good 失败：{e}");
            }
            self.render_cache.write().await.insert(
                slug.to_string(),
                CachedRender {
                    signature,
                    outcome: outcome.clone(),
                },
            );
            return Ok(outcome);
        }

        // 失败时按 on_failure 决定是否回退
        if profile.on_failure == OnFailure::LastGood {
            match lastgood::load(&self.store.rendered_dir(), slug) {
                Ok(Some(lg)) => {
                    let mut report = outcome.report.clone();
                    report.warn(format!(
                        "本次导出失败，已回退到 {} 生成的配置",
                        lg.generated_at.format("%Y-%m-%d %H:%M:%S UTC")
                    ));
                    return Ok(ExportOutcome {
                        yaml: lg.yaml,
                        report,
                        group_stats: outcome.group_stats,
                        used_last_good: true,
                        last_good_at: Some(lg.generated_at),
                        generated_at: outcome.generated_at,
                        etag: outcome.etag,
                    });
                }
                Ok(None) => {
                    let mut report = outcome.report.clone();
                    report.error("本次导出失败，且没有可用的 last_good 配置");
                    return Ok(ExportOutcome { report, ..outcome });
                }
                Err(e) => {
                    let mut report = outcome.report.clone();
                    report.error(format!("读取 last_good 失败：{e}"));
                    return Ok(ExportOutcome { report, ..outcome });
                }
            }
        }

        Ok(outcome)
    }

    /// 渲染输入签名：store revision + GeoIP 库修改时间 + 所选上游缓存文件元数据。
    fn render_signature(&self, snapshot: &Store, profile: &Profile) -> String {
        let mut raw = format!("rev={};", snapshot.revision);
        if let Some(info) = self.geo.db_info()
            && let Some(modified) = info.modified
        {
            raw.push_str(&format!("geo={modified:?};"));
        }
        for id in &profile.upstreams {
            let path = self.store.cache_dir().join(format!("{id}.yaml"));
            match std::fs::metadata(&path) {
                Ok(meta) => {
                    raw.push_str(&format!("{id}={:?}:{};", meta.modified().ok(), meta.len()))
                }
                Err(_) => raw.push_str(&format!("{id}=missing;")),
            }
        }
        raw
    }

    async fn export_with(&self, snapshot: &Store, profile: &Profile) -> Result<ExportOutcome> {
        let generated_at = Utc::now();
        let settings = &snapshot.settings;
        let filter_keywords = settings.filter_keywords.clone();
        let filter_enabled = settings.filter_enabled;

        let mut nodes: Vec<PreparedNode> = Vec::new();
        let mut filtered: Vec<String> = Vec::new();
        let mut upstream_warnings: Vec<String> = Vec::new();
        let mut region_notes = RegionNotes::default();

        // 手动节点先入池：同名时按「先出现者」保留，因此手动节点优先于订阅节点。
        for manual in &snapshot.manual_nodes {
            let proxy = match crate::links::parse_link(&manual.link) {
                Ok(proxy) => proxy,
                Err(e) => {
                    upstream_warnings.push(format!("手动节点「{}」解析失败：{e}", manual.id));
                    continue;
                }
            };
            let name = mstr(&proxy, "name").unwrap_or("").to_string();
            if name.trim().is_empty() {
                upstream_warnings.push(format!("手动节点「{}」缺少节点名", manual.id));
                continue;
            }
            let server = mstr(&proxy, "server").unwrap_or("").to_string();
            let inputs = self.geo.region_inputs(&name, &server).await;
            let decision = decide_region(&inputs, &snapshot.aliases, settings.region_source);

            if decision.conflict {
                region_notes.conflicts.push(format!(
                    "「{name}」节点名判定为 [{}]，IP({}) 判定为 [{}]",
                    decision.name_regions.join("、"),
                    inputs.server,
                    decision.geo_regions.join("、")
                ));
            }
            if decision.ambiguous {
                region_notes
                    .ambiguous
                    .push(format!("「{name}」命中 {}", decision.regions.join("、")));
            }

            nodes.push(PreparedNode {
                name,
                proxy,
                regions: decision.regions,
                upstream: "手动节点".to_string(),
            });
        }

        for uid in &profile.upstreams {
            let Some(upstream) = snapshot.upstream(uid) else {
                upstream_warnings.push(format!("上游「{uid}」不存在，已跳过"));
                continue;
            };

            // ② 取缓存；缺失才同步拉取一次
            let mut env = cache::load(&self.store.cache_dir(), uid)?;
            if env.as_ref().map(|e| !e.has_content()).unwrap_or(true) {
                tracing::info!("上游 {} 无可用缓存，导出时同步拉取一次", uid);
                let _ = self.refresher.refresh_one(uid).await;
                env = cache::load(&self.store.cache_dir(), uid)?;
            }

            let Some(env) = env else {
                upstream_warnings.push(format!(
                    "上游「{}」没有任何可用数据，本次贡献 0 个节点",
                    upstream.name
                ));
                continue;
            };

            self.collect_status_warnings(upstream, &env, &mut upstream_warnings);

            if !env.has_content() {
                upstream_warnings.push(format!(
                    "上游「{}」没有可用缓存，本次贡献 0 个节点",
                    upstream.name
                ));
                continue;
            }

            // ③ 解析 + 过滤（解析失败纳入同一降级模型，§4.5）
            match fetch::validate_and_parse(&env.body, &filter_keywords, filter_enabled) {
                Ok(parsed) => {
                    filtered.extend(parsed.filtered);
                    for proxy in parsed.proxies {
                        let name = mstr(&proxy, "name").unwrap_or("").to_string();
                        let server = mstr(&proxy, "server").unwrap_or("").to_string();

                        // ④ 地区判定：IO 层准备数据 → 纯函数判定
                        let inputs = self.geo.region_inputs(&name, &server).await;
                        let decision =
                            decide_region(&inputs, &snapshot.aliases, settings.region_source);

                        if decision.conflict {
                            region_notes.conflicts.push(format!(
                                "「{name}」节点名判定为 [{}]，IP({}) 判定为 [{}]",
                                decision.name_regions.join("、"),
                                inputs.server,
                                decision.geo_regions.join("、")
                            ));
                        }
                        if decision.ambiguous {
                            region_notes
                                .ambiguous
                                .push(format!("「{name}」命中 {}", decision.regions.join("、")));
                        }

                        nodes.push(PreparedNode {
                            name,
                            proxy,
                            regions: decision.regions,
                            upstream: uid.clone(),
                        });
                    }
                }
                Err((kind, msg)) => {
                    upstream_warnings.push(format!(
                        "上游「{}」的缓存解析失败（{}）：{msg}",
                        upstream.name,
                        kind.label()
                    ));
                }
            }
        }

        let input = RenderInput {
            nodes,
            filtered,
            region_notes,
            upstream_warnings,
        };

        let out: RenderOutput = render(snapshot, profile, &input)?;

        Ok(ExportOutcome {
            yaml: out.yaml,
            report: out.report,
            group_stats: out.group_stats,
            used_last_good: false,
            last_good_at: None,
            generated_at,
            etag: String::new(),
        })
    }

    /// 上游状态四维（§4.7）：刷新状态 / 数据年龄 / 套餐到期 / 流量。
    fn collect_status_warnings(
        &self,
        upstream: &model::Upstream,
        env: &cache::CacheEnvelope,
        out: &mut Vec<String>,
    ) {
        let now = Utc::now();

        // 刷新状态
        match env.meta.status {
            cache::CacheStatus::Ok => {}
            cache::CacheStatus::Stale => out.push(format!(
                "上游「{}」最近一次刷新失败（{}），正在使用旧数据：{}",
                upstream.name,
                env.meta
                    .last_error_kind
                    .map(|k| k.label())
                    .unwrap_or("未知"),
                env.meta.last_error.as_deref().unwrap_or("无详情")
            )),
            cache::CacheStatus::Empty => out.push(format!(
                "上游「{}」返回了零节点响应，正在使用旧数据",
                upstream.name
            )),
            cache::CacheStatus::Failed => {
                out.push(format!("上游「{}」没有可用数据", upstream.name))
            }
        }

        // 数据年龄
        if let Some(success_at) = env.meta.last_success_at {
            let age = now.signed_duration_since(success_at);
            if age.num_hours() >= 24 {
                out.push(format!(
                    "上游「{}」的数据已有 {} 小时未更新（最后成功：{}）",
                    upstream.name,
                    age.num_hours(),
                    success_at.format("%Y-%m-%d %H:%M UTC")
                ));
            }
        }

        // 套餐到期
        if let Some(expire) = env.meta.expire
            && expire > 0
        {
            let exp = chrono::DateTime::from_timestamp(expire, 0);
            if let Some(exp) = exp
                && exp < now
            {
                out.push(format!(
                    "上游「{}」的套餐已于 {} 过期",
                    upstream.name,
                    exp.format("%Y-%m-%d")
                ));
            }
        }

        // 流量
        let info = subinfo::SubInfo {
            upload: env.meta.upload,
            download: env.meta.download,
            total: env.meta.total,
            expire: env.meta.expire,
        };
        if let Some(remaining) = info.remaining()
            && remaining == 0
        {
            out.push(format!("上游「{}」的流量已用尽", upstream.name));
        }
    }
}
