//! 导出管线（AGENTS.md §6）。核心逻辑为纯函数：给定已准备好的节点池与配置，
//! 不依赖网络即可产出最终配置。上游拉取与 GeoIP 查询由调用方在 IO 层完成。

use crate::matcher::{GroupEval, NodeInfo, evaluate_group, evaluate_group_with_covered};
use crate::rules::{check_order, is_builtin_policy, serialize_rule};
use crate::store::model::{
    Group, Mapping, Profile, RuleSection, Store, Yaml, mget, mseq, mstr, ystr,
};
use crate::validate::Report;
use anyhow::{Result, bail};
use std::collections::{BTreeMap, HashMap, HashSet};

/// 已由 IO 层准备好的节点。
#[derive(Debug, Clone)]
pub struct PreparedNode {
    /// 节点名（去重主键）。
    pub name: String,
    /// 原始节点（保序 YAML，含未知字段）。
    pub proxy: Mapping,
    /// 地区判定结果。
    pub regions: Vec<String>,
    /// 来源上游 id。
    pub upstream: String,
}

#[derive(Debug, Clone, Default)]
pub struct RegionNotes {
    /// 节点名与 GeoIP 判定冲突。
    pub conflicts: Vec<String>,
    /// 命中多个地区。
    pub ambiguous: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct GroupStat {
    pub name: String,
    pub member_count: usize,
    pub auto_count: usize,
    pub reasons: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Clone, Default)]
pub struct RenderInput {
    pub nodes: Vec<PreparedNode>,
    /// 被假节点过滤掉的名称（预览页必须展示，便于发现误杀）。
    pub filtered: Vec<String>,
    pub region_notes: RegionNotes,
    /// 上游层面的告警（陈旧、失败等）。
    pub upstream_warnings: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct RenderOutput {
    pub yaml: String,
    pub report: Report,
    pub group_stats: Vec<GroupStat>,
}

/// 校验未通过时的空输出（不产出半成品配置）。
fn failed(report: Report, group_stats: Vec<GroupStat>) -> RenderOutput {
    RenderOutput {
        yaml: String::new(),
        report,
        group_stats,
    }
}

/// 空分组处理（§3.4 + §6.3，以及兜底分组的特例）。
///
/// `has_catch_all` 表示本方案是否选了兜底分组——它会把空分组从「错误」降级为「警告 + 省略」。
fn resolve_empty_group(
    g: &Group,
    mut eval: GroupEval,
    profile: &Profile,
    has_catch_all: bool,
    report: &mut Report,
) -> GroupEval {
    if !eval.members.is_empty() {
        if eval.auto_matched.is_empty() {
            report.warn(format!(
                "分组「{}」没有任何自动匹配成员，成员全部来自手动项",
                g.name
            ));
        }
        return eval;
    }

    // 兜底分组为空是**正常状态**：所有节点都被别的分组收走了。
    // 不能按普通分组那样报错，否则用户一旦把节点全部分完就会导出失败。
    if g.r#match.catch_all {
        report.warn(format!(
            "兜底分组「{}」为空——所有节点都已被其他分组收录，该分组将从输出中省略",
            g.name
        ));
        return eval;
    }

    // 方案里有兜底分组时，普通分组为空同样是**正常状态**：没被它匹配到的节点
    // 已经由兜底收纳。机场经常没有某个地区的节点，为此让整份配置导不出来是本末倒置。
    if has_catch_all {
        report.warn(format!(
            "分组「{}」为空，已从输出中省略（本方案有兜底分组，未匹配的节点不会丢失）",
            g.name
        ));
        return eval;
    }

    if let Some(fb) = &g.fallback_group {
        if !profile.groups.contains(fb) {
            report.error(format!(
                "分组「{}」的备用分组「{}」不在本方案所选分组内",
                g.name, fb
            ));
        } else {
            eval.members.push(fb.clone());
            report.warn(format!(
                "分组「{}」为空，已按配置回退到备用分组「{}」",
                g.name, fb
            ));
        }
    } else {
        report.error(format!(
            "分组「{}」最终成员为空。Clash 拒绝空 proxies；\
             请调整匹配规则、指定 manual_groups/manual_nodes、配置 fallback_group，\
             或加一个兜底分组让它自动省略",
            g.name
        ));
    }
    eval
}

fn stat_of(g: &Group, eval: &GroupEval) -> GroupStat {
    GroupStat {
        name: g.name.clone(),
        member_count: eval.members.len(),
        auto_count: eval.auto_matched.len(),
        reasons: eval.reasons.clone(),
    }
}

/// 从分组成员里移除「已被省略的空分组」引用，并处理因此变空的分组。
///
/// 典型场景：`自动选择` 的 `manual_groups` 含 8 个地区组，但机场只覆盖其中几个；
/// 空地区组被省略后，上层组只是少一个选项，不应让整份配置导出失败。
fn prune_omitted_group_refs(
    selected: &[&Group],
    evals: &mut BTreeMap<String, GroupEval>,
    omitted: &mut HashSet<String>,
    has_catch_all: bool,
    report: &mut Report,
) {
    loop {
        let mut changed = false;
        for g in selected {
            if omitted.contains(&g.name) {
                continue;
            }
            let Some(eval) = evals.get_mut(&g.name) else {
                continue;
            };

            let removed: Vec<String> = eval
                .members
                .iter()
                .filter(|m| omitted.contains(*m))
                .cloned()
                .collect();
            if !removed.is_empty() {
                eval.members.retain(|m| !omitted.contains(m));
                for name in &removed {
                    eval.reasons.remove(name);
                }
                report.warn(format!(
                    "分组「{}」引用的空分组 {} 已从成员中省略",
                    g.name,
                    removed.join("、")
                ));
                changed = true;
            }

            if eval.members.is_empty() {
                let used_fallback = g.fallback_group.as_ref().is_some_and(|fb| {
                    selected.iter().any(|x| &x.name == fb) && !omitted.contains(fb)
                });
                if let Some(fb) = g.fallback_group.as_ref().filter(|_| used_fallback) {
                    eval.members.push(fb.clone());
                    report.warn(format!(
                        "分组「{}」最终成员为空，已按配置回退到备用分组「{fb}」",
                        g.name
                    ));
                } else if has_catch_all {
                    omitted.insert(g.name.clone());
                    report.warn(format!("分组「{}」最终成员为空，已从输出中省略", g.name));
                } else {
                    report.error(format!(
                        "分组「{}」最终成员为空。Clash 拒绝空 proxies；\
                         请调整匹配规则、指定 manual_groups/manual_nodes、配置 fallback_group，\
                         或加一个兜底分组让它自动省略",
                        g.name
                    ));
                    omitted.insert(g.name.clone());
                }
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
}

/// 执行导出。
pub fn render(store: &Store, profile: &Profile, input: &RenderInput) -> Result<RenderOutput> {
    let mut report = Report::default();

    for w in &input.upstream_warnings {
        report.warn(w.clone());
    }

    // ---------------------------------------------------------- ① 引用完整性
    validate_references(store, profile, &mut report)?;
    if !report.ok() {
        return Ok(failed(report, Vec::new()));
    }

    // ---------------------------------------------------------- ③ 去重
    let mut seen: HashSet<String> = HashSet::new();
    let mut nodes: Vec<PreparedNode> = Vec::new();
    for n in &input.nodes {
        if !seen.insert(n.name.clone()) {
            report.conflict(format!(
                "节点名「{}」重复（来自上游 {}），已保留先出现者",
                n.name, n.upstream
            ));
            continue;
        }
        nodes.push(n.clone());
    }

    if nodes.is_empty() {
        report.error("没有可用节点：所有上游均无数据");
        return Ok(failed(report, Vec::new()));
    }

    let node_infos: Vec<NodeInfo> = nodes
        .iter()
        .map(|n| NodeInfo {
            name: n.name.clone(),
            regions: n.regions.clone(),
        })
        .collect();
    let node_names: Vec<String> = nodes.iter().map(|n| n.name.clone()).collect();

    // 分组名与节点名重名 → proxies 引用歧义（§6.1 第 6 条）
    for g in &store.groups {
        if profile.groups.contains(&g.name) && node_names.contains(&g.name) {
            report.error(format!(
                "分组名「{}」与某个节点名重名，会导致 proxies 引用歧义",
                g.name
            ));
        }
    }

    // ---------------------------------------------------------- ⑤ 分组求值
    let selected_groups: Vec<&Group> = profile
        .groups
        .iter()
        .filter_map(|name| store.group(name))
        .collect();

    let mut evals: BTreeMap<String, GroupEval> = BTreeMap::new();
    let mut group_stats: Vec<GroupStat> = Vec::new();
    let mut omitted: HashSet<String> = HashSet::new();

    // 兜底分组必须**最后**求值：它要排除「已被其他分组收录」的节点。
    let (normal_groups, catch_all_groups): (Vec<&Group>, Vec<&Group>) =
        selected_groups.iter().partition(|g| !g.r#match.catch_all);

    if catch_all_groups.len() > 1 {
        report.warn(format!(
            "本方案有 {} 个兜底分组（{}），它们会得到完全相同的成员列表",
            catch_all_groups.len(),
            catch_all_groups
                .iter()
                .map(|g| g.name.as_str())
                .collect::<Vec<_>>()
                .join("、")
        ));
    }

    // 第一遍：普通分组，同时汇总兜底分组的「排除集」。
    let mut covered: HashSet<String> = HashSet::new();
    let has_catch_all = !catch_all_groups.is_empty();
    for g in &normal_groups {
        let eval = evaluate_group(g, &node_infos)?;

        // ⚠️ 关键：`match.all` 的分组（如「自动选择」）是**全收桶**，不是分类。
        // 若把它算作「已收录」，那么任何带「自动选择」的方案里兜底分组都会永远为空
        // ——而那正是最常见的配置形态。
        if !g.r#match.all {
            covered.extend(eval.auto_matched.iter().cloned());
        }
        // 手动指定的节点算「已被明确归置」，不应再落进兜底。
        covered.extend(g.manual_nodes.iter().cloned());

        let resolved = resolve_empty_group(g, eval, profile, has_catch_all, &mut report);
        if resolved.members.is_empty() {
            omitted.insert(g.name.clone());
        }
        group_stats.push(stat_of(g, &resolved));
        evals.insert(g.name.clone(), resolved);
    }

    // 第二遍：兜底分组。
    for g in &catch_all_groups {
        let eval = evaluate_group_with_covered(g, &node_infos, Some(&covered))?;
        let resolved = resolve_empty_group(g, eval, profile, has_catch_all, &mut report);
        if resolved.members.is_empty() {
            omitted.insert(g.name.clone());
        }
        group_stats.push(stat_of(g, &resolved));
        evals.insert(g.name.clone(), resolved);
    }

    // 空分组被省略后，先把上层分组对它的引用清掉；规则目标交给 build_rules
    // 跳过并告警，而不是阻断整份配置。
    prune_omitted_group_refs(
        &selected_groups,
        &mut evals,
        &mut omitted,
        has_catch_all,
        &mut report,
    );

    // 剔除/省略后重新统计，预览页与输出都以最终成员为准。
    group_stats = selected_groups
        .iter()
        .filter(|g| !omitted.contains(&g.name))
        .filter_map(|g| evals.get(&g.name).map(|eval| stat_of(g, eval)))
        .collect();

    if !report.ok() {
        return Ok(failed(report, group_stats));
    }

    // ---------------------------------------------------------- ⑥ 分组顺序
    let kept_groups: Vec<&Group> = selected_groups
        .iter()
        .filter(|g| !omitted.contains(&g.name))
        .copied()
        .collect();
    // 拓扑排序只用于**环检测**：Clash 的 `proxy-groups[].proxies` 按名字引用，
    // 不要求被引用者先定义，因此输出顺序直接沿用库内顺序（用户排序）。
    // 典型配置里「手动切换 / 自动选择」引用了地区组，它们应当排在最前。
    topological_order(&kept_groups)?;
    let ordered_names: Vec<String> = kept_groups.iter().map(|g| g.name.clone()).collect();
    let mut proxy_groups: Vec<Yaml> = Vec::new();
    for name in &ordered_names {
        let Some(g) = store.group(name) else { continue };
        let members = evals
            .get(name)
            .map(|e| e.members.clone())
            .unwrap_or_default();
        proxy_groups.push(Yaml::Mapping(g.to_clash(&members)));
    }

    // ---------------------------------------------------------- ⑦ 规则分段拼装
    let valid_targets: HashSet<String> = ordered_names.iter().cloned().collect();
    let (rule_lines, has_routing) = build_rules(store, profile, &valid_targets, &mut report)?;

    // ---------------------------------------------------------- ⑧ 拼装
    //
    // `proxies` 按**分组出现顺序**排列：按分组拓扑序依次代入成员，同一节点只出现
    // 一次（以最早收录它的分组为准）；没有进入任何分组的节点保持原有相对顺序排在
    // 最后。分组自身的 `proxies` 列表不受影响。
    let node_index: HashMap<&str, &PreparedNode> =
        nodes.iter().map(|n| (n.name.as_str(), n)).collect();
    let mut ordered_nodes: Vec<&PreparedNode> = Vec::new();
    let mut placed: HashSet<&str> = HashSet::new();
    for name in &ordered_names {
        let Some(eval) = evals.get(name) else {
            continue;
        };
        for member in &eval.members {
            // 成员里可能混有子分组名与内置策略（DIRECT 等），只取真实节点。
            let Some(node) = node_index.get(member.as_str()) else {
                continue;
            };
            if placed.insert(node.name.as_str()) {
                ordered_nodes.push(node);
            }
        }
    }
    for node in &nodes {
        if placed.insert(node.name.as_str()) {
            ordered_nodes.push(node);
        }
    }

    let mut config = Mapping::new();
    for (k, v) in store.general.iter() {
        config.insert(k.clone(), v.clone());
    }
    config.insert(
        ystr("proxies"),
        Yaml::Sequence(
            ordered_nodes
                .iter()
                .map(|n| Yaml::Mapping(n.proxy.clone()))
                .collect(),
        ),
    );
    if !proxy_groups.is_empty() {
        config.insert(ystr("proxy-groups"), Yaml::Sequence(proxy_groups));
    }
    if has_routing {
        config.insert(
            ystr("rules"),
            Yaml::Sequence(rule_lines.iter().map(|l| ystr(l.clone())).collect()),
        );
    }

    // ---------------------------------------------------------- ⑨ 外部依赖
    validate_external_deps(store, &config, &node_names, &rule_lines, &mut report)?;

    if !report.ok() {
        return Ok(failed(report, group_stats));
    }

    // ---------------------------------------------------------- ⑩ 序列化
    let yaml = serde_yaml_ng::to_string(&Yaml::Mapping(config.clone()))?;

    report.note(format!("节点数：{}", node_names.len()));
    report.note(format!("分组数：{}", ordered_names.len()));
    if has_routing {
        report.note(format!("规则数：{}", rule_lines.len()));
    }
    if !input.filtered.is_empty() {
        report.note(format!(
            "已过滤 {} 个信息/假节点：{}",
            input.filtered.len(),
            input.filtered.join("、")
        ));
    }
    for c in &input.region_notes.conflicts {
        report.warn(format!("地区判定冲突：{c}"));
    }
    for a in &input.region_notes.ambiguous {
        report.warn(format!("地区歧义（命中多个地区）：{a}"));
    }

    Ok(RenderOutput {
        yaml,
        report,
        group_stats,
    })
}

// ---------------------------------------------------------------- 引用校验

fn validate_references(store: &Store, profile: &Profile, report: &mut Report) -> Result<()> {
    for id in &profile.upstreams {
        if store.upstream(id).is_none() {
            report.error(format!("方案引用的上游「{id}」不存在"));
        }
    }
    for name in &profile.groups {
        if store.group(name).is_none() {
            report.error(format!("方案引用的分组「{name}」不存在"));
        }
    }
    for id in &profile.rules {
        if store.rule(id).is_none() {
            report.error(format!("方案引用的规则「{id}」不存在"));
        }
    }

    // 依赖闭包必须完整且无环（§3.3.2）
    let selected: HashSet<&str> = profile.groups.iter().map(|s| s.as_str()).collect();
    for name in &profile.groups {
        let Some(g) = store.group(name) else { continue };
        for dep in &g.manual_groups {
            if !selected.contains(dep.as_str()) {
                report.error(format!(
                    "分组「{name}」依赖分组「{dep}」，但后者未被本方案选中"
                ));
            }
        }
    }
    if let Err(e) = topological_order(
        &profile
            .groups
            .iter()
            .filter_map(|n| store.group(n))
            .collect::<Vec<_>>(),
    ) {
        report.error(e.to_string());
    }

    // 规则目标存在性不在这里报错：目标分组不存在/未选中/为空被省略时，
    // build_rules 会跳过该规则并给出警告（v37），保证其余配置仍可导出。

    Ok(())
}

/// 拓扑排序：被引用者先输出，同层保持库内顺序。
fn topological_order(groups: &[&Group]) -> Result<Vec<String>> {
    let by_name: HashMap<&str, &Group> = groups.iter().map(|g| (g.name.as_str(), *g)).collect();
    let mut visited: HashSet<String> = HashSet::new();
    let mut visiting: HashSet<String> = HashSet::new();
    let mut out: Vec<String> = Vec::new();

    fn visit(
        name: &str,
        by_name: &HashMap<&str, &Group>,
        visited: &mut HashSet<String>,
        visiting: &mut HashSet<String>,
        out: &mut Vec<String>,
    ) -> Result<()> {
        if visited.contains(name) {
            return Ok(());
        }
        if !visiting.insert(name.to_string()) {
            bail!("分组依赖存在环：{name}");
        }
        if let Some(g) = by_name.get(name) {
            for dep in &g.manual_groups {
                if by_name.contains_key(dep.as_str()) {
                    visit(dep, by_name, visited, visiting, out)?;
                }
            }
        }
        visiting.remove(name);
        visited.insert(name.to_string());
        out.push(name.to_string());
        Ok(())
    }

    for g in groups {
        visit(&g.name, &by_name, &mut visited, &mut visiting, &mut out)?;
    }
    Ok(out)
}

// ---------------------------------------------------------------- 规则拼装

/// 返回（规则行, 是否存在任何路由配置）。
///
/// 分段顺序（§6.2）：override → normal → final。
fn build_rules(
    store: &Store,
    profile: &Profile,
    valid_targets: &HashSet<String>,
    report: &mut Report,
) -> Result<(Vec<String>, bool)> {
    let mut override_lines = Vec::new();
    let mut normal_lines = Vec::new();
    let mut final_lines = Vec::new();
    let mut skipped_final = false;

    // 方案选中的规则按库内顺序、按 section 归类。
    // 目标分组不存在 / 未被本方案选中 / 为空被省略时，跳过该规则并告警，
    // 避免一条失效规则拖垮整份配置（v37）。
    for id in &profile.rules {
        let Some(rule) = store.rule(id) else { continue };
        if !rule.is_raw() {
            let target = rule.target.as_deref().unwrap_or("").trim();
            let invalid = target.is_empty()
                || (!is_builtin_policy(target) && !valid_targets.contains(target));
            if invalid {
                report.warn(format!(
                    "规则「{}」的目标「{}」不存在或不在本方案所选分组中，已从导出中排除",
                    rule.id, target
                ));
                if rule.section == RuleSection::Final {
                    skipped_final = true;
                }
                continue;
            }
        }
        let line = serialize_rule(rule)?;
        match rule.section {
            RuleSection::Override => override_lines.push(line),
            RuleSection::Normal => normal_lines.push(line),
            RuleSection::Final => final_lines.push(line),
        }
    }

    // 最终兜底的目标失效时，补一条显式的 DIRECT，避免 Mihomo 静默回退。
    // 这样配置仍然可加载，且回退行为在预览警告里可见。
    if final_lines.is_empty() && skipped_final {
        report.warn("最终兜底规则的目标分组不存在，已补充 MATCH,DIRECT 作为兜底");
        final_lines.push("MATCH,DIRECT".to_string());
    }

    let has_routing =
        !(override_lines.is_empty() && normal_lines.is_empty() && final_lines.is_empty());

    let mut lines = Vec::new();
    lines.extend(override_lines);
    lines.extend(normal_lines);
    lines.extend(final_lines);

    if has_routing {
        let (errors, warnings) = check_order(&lines);
        for e in errors {
            report.error(e);
        }
        for w in warnings {
            report.warn(w);
        }
    }

    Ok((lines, has_routing))
}

// ---------------------------------------------------------------- 外部依赖

fn validate_external_deps(
    store: &Store,
    config: &Mapping,
    node_names: &[String],
    rule_lines: &[String],
    report: &mut Report,
) -> Result<()> {
    let node_set: HashSet<&str> = node_names.iter().map(|s| s.as_str()).collect();

    // 第 12 条：dialer-proxy 引用的节点必须在最终 proxies 中
    if let Some(Yaml::Sequence(proxies)) = mget(config, "proxies") {
        for p in proxies {
            let Yaml::Mapping(m) = p else { continue };
            let Some(dp) = mstr(m, "dialer-proxy") else {
                continue;
            };
            let node = mstr(m, "name").unwrap_or("<未命名>");
            if !node_set.contains(dp) && !is_builtin_policy(dp) {
                report.error(format!(
                    "节点「{node}」的 dialer-proxy 指向「{dp}」，但该节点不在最终 proxies 中"
                ));
            }
        }
    }

    // 第 13 条：RULE-SET 依赖 rule-providers
    let providers: HashSet<String> = mget(&store.general, "rule-providers")
        .and_then(|v| v.as_mapping())
        .map(|m| {
            m.keys()
                .filter_map(|k| k.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    for line in rule_lines {
        let parts: Vec<&str> = line.split(',').map(|s| s.trim()).collect();
        if parts.len() >= 2 && parts[0].eq_ignore_ascii_case("RULE-SET") {
            let provider = parts[1];
            if !providers.contains(provider) {
                report.error(format!(
                    "规则「{line}」引用了 RULE-SET「{provider}」，但 general.rule-providers 中没有该定义"
                ));
            }
        }
    }

    // 第 11 条（结构校验已覆盖，这里再兜一层，防止 general 被外部注入）
    for key in ["proxies", "proxy-groups", "rules"] {
        if store.general.iter().any(|(k, _)| k.as_str() == Some(key)) {
            report.error(format!("通用设置（general）不得包含「{key}」"));
        }
    }

    // 分组里引用的名字必须存在
    if let Some(Yaml::Sequence(groups)) = mget(config, "proxy-groups") {
        let group_names: HashSet<String> = groups
            .iter()
            .filter_map(|g| g.as_mapping())
            .filter_map(|m| mstr(m, "name").map(|s| s.to_string()))
            .collect();
        for g in groups {
            let Yaml::Mapping(m) = g else { continue };
            let gname = mstr(m, "name").unwrap_or("<未命名>");
            for p in mseq(m, "proxies") {
                if node_set.contains(p) || group_names.contains(p) || is_builtin_policy(p) {
                    continue;
                }
                report.error(format!(
                    "分组「{gname}」引用的「{p}」既不是节点、也不是分组或内置策略"
                ));
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::model::{
        GroupMatch, OnFailure, RegionAlias, RuleDef, default_aliases, ystr as ys,
    };

    fn node(name: &str, regions: &[&str]) -> PreparedNode {
        let mut m = Mapping::new();
        m.insert(ys("name"), ys(name));
        m.insert(ys("type"), ys("ss"));
        m.insert(ys("server"), ys("1.2.3.4"));
        m.insert(ys("port"), Yaml::Number(443.into()));
        PreparedNode {
            name: name.to_string(),
            proxy: m,
            regions: regions.iter().map(|s| s.to_string()).collect(),
            upstream: "u1".to_string(),
        }
    }

    fn store_with_fixture() -> Store {
        let mut store = Store {
            aliases: default_aliases(),
            ..Default::default()
        };
        // 默认分组/规则只是首次运行的种子；本 fixture 需要干净的数据。
        store.groups.clear();
        store.rules.clear();
        store.upstreams.push(crate::store::model::Upstream {
            id: "u1".into(),
            name: "机场A".into(),
            url: "https://example.com/sub".into(),
            note: None,
        });

        let mut auto = Mapping::new();
        auto.insert(ys("type"), ys("url-test"));
        store.groups.push(Group {
            name: "自动选择".into(),
            clash: auto,
            r#match: GroupMatch {
                all: true,
                ..Default::default()
            },
            manual_groups: vec![],
            manual_nodes: vec![],
            fallback_group: None,
        });

        let mut us = Mapping::new();
        us.insert(ys("type"), ys("select"));
        store.groups.push(Group {
            name: "美国节点".into(),
            clash: us,
            r#match: GroupMatch {
                regions: vec!["美国".into()],
                ..Default::default()
            },
            manual_groups: vec!["自动选择".into()],
            manual_nodes: vec![],
            fallback_group: None,
        });

        store.rules.push(RuleDef {
            id: "r-final".into(),
            section: RuleSection::Final,
            r#type: Some("MATCH".into()),
            target: Some("自动选择".into()),
            ..Default::default()
        });

        store
    }

    fn profile() -> Profile {
        Profile {
            slug: "home".into(),
            name: "家庭".into(),
            upstreams: vec!["u1".into()],
            groups: vec!["自动选择".into(), "美国节点".into()],
            rules: vec!["r-final".into()],
            on_failure: OnFailure::Fail,
            token: None,
        }
    }

    #[test]
    fn missing_rule_targets_are_skipped_instead_of_failing_export() {
        let mut store = Store::default();
        store.rules.clear();
        store.rules.push(RuleDef {
            id: "r-missing".into(),
            section: RuleSection::Normal,
            r#type: Some("DOMAIN-SUFFIX".into()),
            payload: Some("missing.example".into()),
            target: Some("不存在的分组".into()),
            ..Default::default()
        });
        store.rules.push(RuleDef {
            id: "r-ok".into(),
            section: RuleSection::Normal,
            r#type: Some("DOMAIN-SUFFIX".into()),
            payload: Some("ok.example".into()),
            target: Some("自动选择".into()),
            ..Default::default()
        });
        store.rules.push(RuleDef {
            id: "r-final-missing".into(),
            section: RuleSection::Final,
            r#type: Some("MATCH".into()),
            target: Some("不存在的分组".into()),
            ..Default::default()
        });

        let profile = Profile {
            slug: "home".into(),
            name: "家庭".into(),
            upstreams: vec![],
            groups: vec!["自动选择".into()],
            rules: vec!["r-missing".into(), "r-ok".into(), "r-final-missing".into()],
            on_failure: OnFailure::Fail,
            token: None,
        };
        let valid: HashSet<String> = ["自动选择".to_string()].into_iter().collect();
        let mut report = Report::default();
        let (lines, has_routing) = build_rules(&store, &profile, &valid, &mut report).unwrap();

        assert!(has_routing);
        assert_eq!(
            lines,
            vec![
                "DOMAIN-SUFFIX,ok.example,自动选择".to_string(),
                "MATCH,DIRECT".to_string(),
            ]
        );
        assert!(report.errors.is_empty(), "缺目标不应阻断导出");
        assert_eq!(report.warnings.len(), 3);
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.contains("r-missing") && w.contains("已从导出中排除"))
        );
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.contains("已补充 MATCH,DIRECT"))
        );
    }

    #[test]
    fn renders_full_config() {
        let store = store_with_fixture();
        let p = profile();
        let input = RenderInput {
            nodes: vec![node("🇺🇸 美国 01", &["美国"]), node("🇯🇵 日本 01", &["日本"])],
            ..Default::default()
        };
        let out = render(&store, &p, &input).unwrap();
        assert!(out.report.ok(), "{:?}", out.report.errors);

        let text = out.yaml;
        assert!(text.contains("proxies:"));
        assert!(text.contains("proxy-groups:"));
        assert!(text.contains("rules:"));
        assert!(text.contains("MATCH,"));
    }

    /// `proxies` 按分组出现顺序排列：先收录节点者在前；未入组节点排最后。
    #[test]
    fn proxies_follow_group_order_not_upstream_order() {
        let mut store = Store {
            aliases: default_aliases(),
            ..Default::default()
        };
        store.groups.clear();
        store.rules.clear();
        // 美国节点在前，且只收录美国节点；自动选择在后，收录全部。
        let mut us = Mapping::new();
        us.insert(ys("type"), ys("select"));
        store.groups.push(Group {
            name: "美国节点".into(),
            clash: us,
            r#match: GroupMatch {
                regions: vec!["美国".into()],
                ..Default::default()
            },
            manual_groups: vec![],
            manual_nodes: vec![],
            fallback_group: None,
        });
        let mut auto = Mapping::new();
        auto.insert(ys("type"), ys("url-test"));
        store.groups.push(Group {
            name: "自动选择".into(),
            clash: auto,
            r#match: GroupMatch {
                all: true,
                ..Default::default()
            },
            manual_groups: vec![],
            manual_nodes: vec![],
            fallback_group: None,
        });
        store.rules.push(RuleDef {
            id: "r-final".into(),
            section: RuleSection::Final,
            r#type: Some("MATCH".into()),
            target: Some("自动选择".into()),
            ..Default::default()
        });

        let profile = Profile {
            slug: "home".into(),
            name: "家庭".into(),
            upstreams: vec![],
            groups: vec!["美国节点".into(), "自动选择".into()],
            rules: vec!["r-final".into()],
            on_failure: OnFailure::Fail,
            token: None,
        };

        // 上游顺序是「日本、美国、未归组」——日本在前。
        let input = RenderInput {
            nodes: vec![
                node("🇯🇵 日本 01", &["日本"]),
                node("🇺🇸 美国 01", &["美国"]),
                node("🇸🇬 新加坡 01", &["新加坡"]),
            ],
            ..Default::default()
        };
        let out = render(&store, &profile, &input).unwrap();
        let text = out.yaml;

        let us = text.find("🇺🇸 美国 01").unwrap();
        let jp = text.find("🇯🇵 日本 01").unwrap();
        let sg = text.find("🇸🇬 新加坡 01").unwrap();
        assert!(us < jp, "美国节点由第一个分组收录，应排在日本之前");
        assert!(jp < sg, "未归组节点应排在已归组节点之后");
    }

    /// `proxy-groups` 按库内顺序输出：容器分组（引用地区组）排在最前，
    /// 与用户在分组页看到的顺序一致；Clash 不要求被引用者先定义。
    #[test]
    fn proxy_groups_follow_store_order() {
        let store = store_with_fixture();
        let p = profile();
        let input = RenderInput {
            nodes: vec![node("🇺🇸 美国 01", &["美国"])],
            ..Default::default()
        };
        let out = render(&store, &p, &input).unwrap();
        let text = out.yaml;
        let auto = text.find("自动选择").unwrap();
        let us = text.find("美国节点").unwrap();
        assert!(auto < us, "库内顺序里自动选择在前，应保持");
    }

    /// 依赖成环仍然必须报错，即使输出顺序不再依赖拓扑排序。
    #[test]
    fn cyclic_group_dependency_still_errors() {
        let mut store = store_with_fixture();
        store.groups.clear();
        for (a, b) in [("A", "B"), ("B", "A")] {
            let mut clash = Mapping::new();
            clash.insert(ys("type"), ys("select"));
            store.groups.push(Group {
                name: a.into(),
                clash,
                r#match: GroupMatch::default(),
                manual_groups: vec![b.into()],
                manual_nodes: vec![],
                fallback_group: None,
            });
        }
        let profile = Profile {
            slug: "home".into(),
            name: "家庭".into(),
            upstreams: vec![],
            groups: vec!["A".into(), "B".into()],
            rules: vec![],
            on_failure: OnFailure::Fail,
            token: None,
        };
        let input = RenderInput {
            nodes: vec![node("🇺🇸 美国 01", &["美国"])],
            ..Default::default()
        };
        let out = render(&store, &profile, &input).unwrap();
        assert!(!out.report.ok(), "应整体失败");
        assert!(
            out.report.errors.iter().any(|e| e.contains("环")),
            "应报依赖成环：{:?}",
            out.report.errors
        );
    }

    #[test]
    fn missing_final_match_is_error_when_routing_present() {
        let mut store = store_with_fixture();
        // 只留一条普通路由规则：有路由，但没有 MATCH 兜底
        store.rules = vec![RuleDef {
            id: "r-geoip".into(),
            section: RuleSection::Normal,
            r#type: Some("GEOIP".into()),
            payload: Some("CN".into()),
            target: Some("DIRECT".into()),
            ..Default::default()
        }];
        let mut p = profile();
        p.rules = vec!["r-geoip".into()];
        let input = RenderInput {
            nodes: vec![node("🇺🇸 美国 01", &["美国"])],
            ..Default::default()
        };
        let out = render(&store, &p, &input).unwrap();
        assert!(!out.report.ok());
        assert!(
            out.report
                .errors
                .iter()
                .any(|e| e.contains("缺少最终兜底规则")),
            "{:?}",
            out.report.errors
        );
    }

    #[test]
    fn no_routing_means_no_rules_key_and_no_match_requirement() {
        let store = store_with_fixture();
        let mut p = profile();
        p.rules.clear();
        let input = RenderInput {
            nodes: vec![node("🇺🇸 美国 01", &["美国"])],
            ..Default::default()
        };
        let out = render(&store, &p, &input).unwrap();
        assert!(out.report.ok(), "{:?}", out.report.errors);
        assert!(!out.yaml.contains("rules:"));
    }

    #[test]
    fn empty_group_without_fallback_is_error() {
        let store = store_with_fixture();
        let p = profile();
        let input = RenderInput {
            nodes: vec![node("🇯🇵 日本 01", &["日本"])], // 没有美国节点
            ..Default::default()
        };
        let out = render(&store, &p, &input).unwrap();
        // 美国节点有 manual_groups，所以非空；这里应通过
        assert!(out.report.ok(), "{:?}", out.report.errors);
    }

    #[test]
    fn top_groups_prune_omitted_region_members() {
        // 默认「自动选择 / 手动切换」把地区组作为可选项；机场缺少某些地区时，
        // 空地区组应从上层成员里移除并警告，而不是让整份配置失败。
        let mut store = Store::default();
        // 本测试自己构造规则，清掉默认 Google 规则。
        store.rules.clear();
        store.upstreams.push(crate::store::model::Upstream {
            id: "u1".into(),
            name: "机场A".into(),
            url: "https://example.com/sub".into(),
            note: None,
        });
        store.rules.push(RuleDef {
            id: "r-final".into(),
            section: RuleSection::Final,
            r#type: Some("MATCH".into()),
            target: Some("手动切换".into()),
            ..Default::default()
        });
        let groups: Vec<String> = store.groups.iter().map(|g| g.name.clone()).collect();
        let p = crate::store::model::Profile {
            slug: "home".into(),
            name: "家庭".into(),
            upstreams: vec!["u1".into()],
            groups,
            rules: vec!["r-final".into()],
            on_failure: OnFailure::Fail,
            token: None,
        };
        let input = RenderInput {
            nodes: vec![node("🇺🇸 美国 01", &["美国"])],
            ..Default::default()
        };
        let out = render(&store, &p, &input).unwrap();
        assert!(out.report.ok(), "{:?}", out.report.errors);
        assert!(
            out.report
                .warnings
                .iter()
                .any(|w| w.contains("引用的空分组")),
            "应提示从上层分组成员中移除了空地区组：{:?}",
            out.report.warnings
        );
        assert!(!out.yaml.contains("香港节点"), "空的地区组不应出现在输出中");
        assert!(out.yaml.contains("美国节点"));
    }

    #[test]
    fn group_name_colliding_with_node_is_error() {
        let mut store = store_with_fixture();
        let mut g = Mapping::new();
        g.insert(ys("type"), ys("select"));
        store.groups.push(Group {
            name: "🇺🇸 美国 01".into(),
            clash: g,
            r#match: GroupMatch {
                all: true,
                ..Default::default()
            },
            manual_groups: vec![],
            manual_nodes: vec![],
            fallback_group: None,
        });
        let mut p = profile();
        p.groups.push("🇺🇸 美国 01".into());
        let input = RenderInput {
            nodes: vec![node("🇺🇸 美国 01", &["美国"])],
            ..Default::default()
        };
        let out = render(&store, &p, &input).unwrap();
        assert!(out.report.errors.iter().any(|e| e.contains("引用歧义")));
    }

    #[test]
    fn dialer_proxy_must_exist() {
        let store = store_with_fixture();
        let p = profile();
        let mut broken = node("🇺🇸 美国 02", &["美国"]);
        broken.proxy.insert(ys("dialer-proxy"), ys("不存在的节点"));
        let input = RenderInput {
            nodes: vec![node("🇺🇸 美国 01", &["美国"]), broken],
            ..Default::default()
        };
        let out = render(&store, &p, &input).unwrap();
        assert!(
            out.report.errors.iter().any(|e| e.contains("dialer-proxy")),
            "{:?}",
            out.report.errors
        );
    }

    #[test]
    fn rule_set_requires_provider() {
        let mut store = store_with_fixture();
        store.rules.insert(
            0,
            RuleDef {
                id: "r-ruleset".into(),
                section: RuleSection::Normal,
                r#type: Some("RULE-SET".into()),
                payload: Some("reject".into()),
                target: Some("DIRECT".into()),
                ..Default::default()
            },
        );
        let mut p = profile();
        p.rules.push("r-ruleset".into());
        let input = RenderInput {
            nodes: vec![node("🇺🇸 美国 01", &["美国"])],
            ..Default::default()
        };
        let out = render(&store, &p, &input).unwrap();
        assert!(out.report.errors.iter().any(|e| e.contains("RULE-SET")));
    }

    #[test]
    fn alias_regions_from_profile_selection() {
        let store = store_with_fixture();
        let p = profile();
        let input = RenderInput {
            nodes: vec![node("🇺🇸 美国 01", &["美国"])],
            region_notes: RegionNotes {
                conflicts: vec!["节点X：名=日本 IP=香港".into()],
                ..Default::default()
            },
            ..Default::default()
        };
        let out = render(&store, &p, &input).unwrap();
        assert!(
            out.report
                .warnings
                .iter()
                .any(|w| w.contains("地区判定冲突"))
        );
    }

    /// 只含地区分组 + 兜底分组（没有 match.all，这样才有「漏网」的节点）。
    fn store_with_catch_all() -> Store {
        let mut store = Store {
            aliases: default_aliases(),
            ..Default::default()
        };
        // 默认分组/规则只是首次运行的种子；本 fixture 需要干净的数据。
        store.groups.clear();
        store.rules.clear();
        store.upstreams.push(crate::store::model::Upstream {
            id: "u1".into(),
            name: "机场A".into(),
            url: "https://example.com/sub".into(),
            note: None,
        });
        let mut sel = Mapping::new();
        sel.insert(ys("type"), ys("select"));
        store.groups.push(Group {
            name: "美国节点".into(),
            clash: sel.clone(),
            r#match: GroupMatch {
                regions: vec!["美国".into()],
                ..Default::default()
            },
            manual_groups: vec![],
            manual_nodes: vec![],
            fallback_group: None,
        });
        store.groups.push(Group {
            name: CATCH_ALL_NAME.into(),
            clash: sel,
            r#match: GroupMatch {
                catch_all: true,
                ..Default::default()
            },
            manual_groups: vec![],
            manual_nodes: vec![],
            fallback_group: None,
        });
        store.rules.push(RuleDef {
            id: "r-final".into(),
            section: RuleSection::Final,
            r#type: Some("MATCH".into()),
            target: Some(CATCH_ALL_NAME.into()),
            ..Default::default()
        });
        store
    }

    const CATCH_ALL_NAME: &str = "漏网之鱼";

    fn catch_all_profile() -> Profile {
        Profile {
            slug: "home".into(),
            name: "家庭".into(),
            upstreams: vec!["u1".into()],
            groups: vec!["美国节点".into(), CATCH_ALL_NAME.into()],
            rules: vec!["r-final".into()],
            on_failure: OnFailure::Fail,
            token: None,
        }
    }

    #[test]
    fn catch_all_collects_nodes_missed_by_other_groups() {
        let store = store_with_catch_all();
        let p = catch_all_profile();
        let input = RenderInput {
            nodes: vec![
                node("🇺🇸 美国 01", &["美国"]),
                node("🇯🇵 日本 01", &["日本"]),
                node("🇸🇬 新加坡 01", &["新加坡"]),
            ],
            ..Default::default()
        };
        let out = render(&store, &p, &input).unwrap();
        assert!(out.report.ok(), "{:?}", out.report.errors);

        let us = out
            .group_stats
            .iter()
            .find(|s| s.name == "美国节点")
            .unwrap();
        assert_eq!(us.member_count, 1);

        let leftover = out
            .group_stats
            .iter()
            .find(|s| s.name == CATCH_ALL_NAME)
            .unwrap();
        assert_eq!(leftover.member_count, 2, "日本与新加坡应落入兜底");
        assert!(out.yaml.contains(CATCH_ALL_NAME));
        assert!(out.yaml.contains("🇯🇵 日本 01"));
    }

    #[test]
    fn empty_catch_all_is_omitted_with_warning() {
        // fixture 里的「自动选择」是 match.all，覆盖全部节点 → 兜底必然为空。
        let mut store = store_with_catch_all();
        let mut auto = Mapping::new();
        auto.insert(ys("type"), ys("url-test"));
        store.groups.insert(
            0,
            Group {
                name: "自动选择".into(),
                clash: auto,
                r#match: GroupMatch {
                    all: true,
                    ..Default::default()
                },
                manual_groups: vec![],
                manual_nodes: vec![],
                fallback_group: None,
            },
        );
        let mut p = catch_all_profile();
        p.groups.insert(0, "自动选择".into());
        // 规则改指自动选择，避免误触「省略后仍被引用」的检查
        if let Some(r) = store.rules.iter_mut().find(|r| r.id == "r-final") {
            r.target = Some("自动选择".into());
        }

        let input = RenderInput {
            nodes: vec![node("🇺🇸 美国 01", &["美国"])],
            ..Default::default()
        };
        let out = render(&store, &p, &input).unwrap();
        assert!(
            out.report.ok(),
            "空兜底不应阻断导出：{:?}",
            out.report.errors
        );
        assert!(
            out.report
                .warnings
                .iter()
                .any(|w| w.contains("兜底分组") && w.contains("省略")),
            "{:?}",
            out.report.warnings
        );
        assert!(
            !out.yaml.contains(CATCH_ALL_NAME),
            "被省略的分组不得出现在输出里"
        );
    }

    #[test]
    fn omitted_catch_all_rule_target_is_skipped() {
        // 兜底为空被省略，规则仍以它为目标 → 跳过该规则，并补 MATCH,DIRECT，
        // 不能产出指向空分组的配置，也不能让整份导出失败。
        let mut store = store_with_catch_all();
        let mut auto = Mapping::new();
        auto.insert(ys("type"), ys("url-test"));
        store.groups.insert(
            0,
            Group {
                name: "自动选择".into(),
                clash: auto,
                r#match: GroupMatch {
                    all: true,
                    ..Default::default()
                },
                manual_groups: vec![],
                manual_nodes: vec![],
                fallback_group: None,
            },
        );
        let mut p = catch_all_profile();
        p.groups.insert(0, "自动选择".into());

        let input = RenderInput {
            nodes: vec![node("🇺🇸 美国 01", &["美国"])],
            ..Default::default()
        };
        let out = render(&store, &p, &input).unwrap();
        assert!(out.report.errors.is_empty(), "{:?}", out.report.errors);
        assert!(
            out.report
                .warnings
                .iter()
                .any(|w| w.contains("已从导出中排除")),
            "{:?}",
            out.report.warnings
        );
        assert!(
            out.report
                .warnings
                .iter()
                .any(|w| w.contains("已补充 MATCH,DIRECT")),
            "{:?}",
            out.report.warnings
        );
        assert!(out.yaml.contains("MATCH,DIRECT"));
        assert!(!out.yaml.contains(CATCH_ALL_NAME));
    }

    #[test]
    fn empty_group_is_error_without_catch_all_but_omitted_with_one() {
        // 机场没有某个地区的节点是常态。加了兜底之后，空分组不该再让整份配置导不出来。
        let mut store = store_with_catch_all();
        let mut sel = Mapping::new();
        sel.insert(ys("type"), ys("select"));
        store.groups.push(Group {
            name: "新加坡节点".into(),
            clash: sel,
            r#match: GroupMatch {
                regions: vec!["新加坡".into()],
                ..Default::default()
            },
            manual_groups: vec![],
            manual_nodes: vec![],
            fallback_group: None,
        });
        let nodes = vec![node("🇺🇸 美国 01", &["美国"])];
        if let Some(r) = store.rules.iter_mut().find(|r| r.id == "r-final") {
            r.target = Some("美国节点".into());
        }

        // ① 方案里没有兜底 → 空分组是错误
        let mut without = catch_all_profile();
        without.groups = vec!["美国节点".into(), "新加坡节点".into()];
        let out = render(
            &store,
            &without,
            &RenderInput {
                nodes: nodes.clone(),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(
            out.report
                .errors
                .iter()
                .any(|e| e.contains("新加坡节点") && e.contains("为空")),
            "无兜底时，空分组必须是错误：{:?}",
            out.report.errors
        );

        // ② 方案里有兜底 → 降级为警告 + 省略，导出成功
        let mut with = without.clone();
        with.groups.push(CATCH_ALL_NAME.into());
        let out = render(
            &store,
            &with,
            &RenderInput {
                nodes,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(
            out.report.ok(),
            "有兜底时不应阻断导出：{:?}",
            out.report.errors
        );
        assert!(
            !out.yaml.contains("新加坡节点"),
            "空的普通分组应从输出中省略"
        );
        assert!(
            out.report
                .warnings
                .iter()
                .any(|w| w.contains("新加坡节点") && w.contains("省略")),
            "{:?}",
            out.report.warnings
        );
    }

    #[test]
    fn catch_all_ignores_match_all_buckets() {
        // 「自动选择」是 match.all 的全收桶，不应把节点算作「已归类」，
        // 否则最常见的配置（自动选择 + 地区分组 + 兜底）里兜底会永远为空。
        let mut store = store_with_catch_all();
        let mut auto = Mapping::new();
        auto.insert(ys("type"), ys("url-test"));
        store.groups.insert(
            0,
            Group {
                name: "自动选择".into(),
                clash: auto,
                r#match: GroupMatch {
                    all: true,
                    ..Default::default()
                },
                manual_groups: vec![],
                manual_nodes: vec![],
                fallback_group: None,
            },
        );
        let mut p = catch_all_profile();
        p.groups.insert(0, "自动选择".into());

        let input = RenderInput {
            nodes: vec![node("🇺🇸 美国 01", &["美国"]), node("🇯🇵 日本 01", &["日本"])],
            ..Default::default()
        };
        let out = render(&store, &p, &input).unwrap();
        assert!(out.report.ok(), "{:?}", out.report.errors);

        let leftover = out
            .group_stats
            .iter()
            .find(|s| s.name == CATCH_ALL_NAME)
            .unwrap();
        assert_eq!(
            leftover.member_count, 1,
            "日本节点虽在「自动选择」里，但没被地区分组选中，仍应落入兜底"
        );
    }

    #[test]
    fn no_nodes_at_all_is_error() {
        let store = store_with_fixture();
        let p = profile();
        let out = render(&store, &p, &RenderInput::default()).unwrap();
        assert!(out.report.errors.iter().any(|e| e.contains("没有可用节点")));
    }

    #[test]
    fn dependency_not_selected_is_error() {
        let store = store_with_fixture();
        let mut p = profile();
        p.groups = vec!["美国节点".into()]; // 缺 自动选择
        let input = RenderInput {
            nodes: vec![node("🇺🇸 美国 01", &["美国"])],
            ..Default::default()
        };
        let out = render(&store, &p, &input).unwrap();
        assert!(
            out.report
                .errors
                .iter()
                .any(|e| e.contains("未被本方案选中")),
            "{:?}",
            out.report.errors
        );
    }

    #[test]
    fn unknown_fields_survive_roundtrip() {
        let store = store_with_fixture();
        let p = profile();
        let mut n = node("🇺🇸 美国 01", &["美国"]);
        n.proxy.insert(
            ys("reality-opts"),
            Yaml::Mapping({
                let mut m = Mapping::new();
                m.insert(ys("public-key"), ys("abc"));
                m
            }),
        );
        let input = RenderInput {
            nodes: vec![n],
            ..Default::default()
        };
        let out = render(&store, &p, &input).unwrap();
        assert!(out.yaml.contains("reality-opts"));
        assert!(out.yaml.contains("public-key"));
    }

    #[test]
    fn region_alias_unused_import_guard() {
        // 确保 RegionAlias 在测试中被引用（避免未使用告警）
        let _ = RegionAlias {
            region: "x".into(),
            aliases: vec![],
        };
    }
}
