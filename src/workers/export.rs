//! 导出链路（Workers 版，MVP）。
//!
//! 与 native `pipeline.rs` 的导出语义一致：取缓存（缺失才拉取）→ 解析/过滤 →
//! 地区判定 → `render`。当前 MVP 只做**按节点名**的地区判定（GeoIP 在后续阶段接入，
//! 缺失时按 AGENTS.md §3.5 降级，不阻断导出）。

use crate::region::{RegionInputs, decide_region};
use crate::render::{PreparedNode, RegionNotes, RenderInput, RenderOutput};
use crate::store::model::{Profile, Store, mstr};
use worker::{Env, Error, Result};

pub struct ExportOutcome {
    pub yaml: String,
    pub warnings: Vec<String>,
}

fn werr(context: &str, e: impl std::fmt::Display) -> Error {
    Error::RustError(format!("{context}：{e}"))
}

/// 导出某个方案。失败返回可读错误（不产出半成品配置）。
pub async fn export(env: &Env, slug: &str) -> Result<ExportOutcome> {
    let store = crate::workers::store::load(env).await?;
    let Some(profile) = store.profile(slug).cloned() else {
        return Err(Error::RustError(format!("方案「{slug}」不存在")));
    };

    let mut nodes: Vec<PreparedNode> = Vec::new();
    let mut filtered: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut notes = RegionNotes::default();

    // 手动节点优先入池（与 native 一致：同名先出现者保留）。
    for manual in &store.manual_nodes {
        let proxy = match crate::links::parse_link(&manual.link) {
            Ok(proxy) => proxy,
            Err(e) => {
                warnings.push(format!("手动节点「{}」解析失败：{e}", manual.id));
                continue;
            }
        };
        let name = mstr(&proxy, "name").unwrap_or("").to_string();
        if name.trim().is_empty() {
            warnings.push(format!("手动节点「{}」缺少节点名", manual.id));
            continue;
        }
        let server = mstr(&proxy, "server").unwrap_or("").to_string();
        let decision = decide_region(
            &RegionInputs {
                node_name: name.clone(),
                server,
                geo_countries: Vec::new(),
            },
            &store.aliases,
            store.settings.region_source,
        );
        if decision.ambiguous {
            notes
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
        let Some(upstream) = store.upstream(uid) else {
            warnings.push(format!("上游「{uid}」不存在，已跳过"));
            continue;
        };

        // 缓存缺失/无内容时同步拉取一次。
        let mut cached = crate::workers::cache::load(env, uid).await?;
        if cached.as_ref().map(|e| !e.has_content()).unwrap_or(true) {
            tracing::info!("上游 {uid} 无可用缓存，导出时同步拉取一次");
            if let Err(e) = crate::workers::upstream::refresh(env, &store, uid).await {
                warnings.push(format!("上游「{}」拉取失败：{e}", upstream.name));
            }
            cached = crate::workers::cache::load(env, uid).await?;
        }

        let Some(envelope) = cached else {
            warnings.push(format!(
                "上游「{}」没有任何可用数据，本次贡献 0 个节点",
                upstream.name
            ));
            continue;
        };
        if let Some(warning) = crate::workers::upstream::status_warning(&upstream.name, &envelope) {
            warnings.push(warning);
        }
        if !envelope.has_content() {
            warnings.push(format!(
                "上游「{}」没有可用缓存，本次贡献 0 个节点",
                upstream.name
            ));
            continue;
        }

        match crate::fetch::validate_and_parse(
            &envelope.body,
            &store.settings.filter_keywords,
            store.settings.filter_enabled,
        ) {
            Ok(parsed) => {
                filtered.extend(parsed.filtered);
                for proxy in parsed.proxies {
                    let name = mstr(&proxy, "name").unwrap_or("").to_string();
                    let server = mstr(&proxy, "server").unwrap_or("").to_string();
                    let decision = decide_region(
                        &RegionInputs {
                            node_name: name.clone(),
                            server,
                            geo_countries: Vec::new(),
                        },
                        &store.aliases,
                        store.settings.region_source,
                    );
                    if decision.ambiguous {
                        notes
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
            Err((kind, message)) => warnings.push(format!(
                "上游「{}」的缓存解析失败（{}）：{message}",
                upstream.name,
                kind.label()
            )),
        }
    }

    output(store, profile, nodes, filtered, notes, warnings)
}

/// 调 `render` 并整理告警；错误含具体原因，避免静默输出半成品。
fn output(
    store: Store,
    profile: Profile,
    nodes: Vec<PreparedNode>,
    filtered: Vec<String>,
    region_notes: RegionNotes,
    warnings: Vec<String>,
) -> Result<ExportOutcome> {
    let input = RenderInput {
        nodes,
        filtered,
        region_notes,
        upstream_warnings: warnings.clone(),
    };
    let rendered: RenderOutput =
        crate::render::render(&store, &profile, &input).map_err(|e| werr("渲染配置失败", e))?;
    if !rendered.report.ok() {
        return Err(Error::RustError(format!(
            "配置校验失败：{}",
            rendered.report.errors.join("；")
        )));
    }
    Ok(ExportOutcome {
        yaml: rendered.yaml,
        warnings: rendered.report.warnings,
    })
}
