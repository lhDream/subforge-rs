//! 业务数据模型。
//!
//! 设计原则（AGENTS.md §3.8）：**只有本服务自有字段是强类型，Clash 原生字段一律
//! 用保序 YAML 承载原样透传**，避免解析成强类型后丢失未知字段。

use serde::{Deserialize, Serialize};

pub use serde_yaml_ng::{Mapping, Value as Yaml};

// ---------------------------------------------------------------- 辅助

/// 从映射中按字符串键取值。
pub fn mget<'a>(m: &'a Mapping, key: &str) -> Option<&'a Yaml> {
    m.get(Yaml::String(key.to_string()))
}

/// 取映射中的字符串字段。
pub fn mstr<'a>(m: &'a Mapping, key: &str) -> Option<&'a str> {
    mget(m, key).and_then(|v| v.as_str())
}

/// 取映射中的字符串序列。
pub fn mseq<'a>(m: &'a Mapping, key: &str) -> Vec<&'a str> {
    mget(m, key)
        .and_then(|v| v.as_sequence())
        .map(|s| s.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default()
}

/// 构造字符串 Value。
pub fn ystr(s: impl Into<String>) -> Yaml {
    Yaml::String(s.into())
}

// ---------------------------------------------------------------- 上游

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Upstream {
    /// slug，不可变，用作缓存文件名。
    pub id: String,
    pub name: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

// ---------------------------------------------------------------- 手动节点

/// 用户手工添加的代理节点（分享链接原文）。
///
/// 只保存链接，不保存解析结果：解析逻辑升级后重新解析即可；链接本身可能含
/// 密码/UUID，页面与日志都不得直接回显。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManualNode {
    /// slug，用作删除等操作的稳定标识。
    pub id: String,
    /// 分享链接原文（`ss://` / `vmess://` / ...）。
    pub link: String,
}

// ---------------------------------------------------------------- 分组

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MatchMode {
    /// 地区「或」关键词命中（默认，扩大范围）。
    #[default]
    Any,
    /// 地区「且」关键词命中（缩小范围）。
    All,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct GroupMatch {
    /// 命中**全部**节点。
    ///
    /// 注意与 [`Self::catch_all`] 区分：`all` 是「所有节点都进来」，
    /// `catch_all` 是「别的分组不要的节点才进来」。
    #[serde(default)]
    pub all: bool,
    /// 兜底分组：收录**未被本方案其他分组匹配**的节点（俗称「漏网之鱼」）。
    ///
    /// 该标志会**取代** `all` / `mode` / `regions` / `keywords` 的自动匹配条件，
    /// 只保留 `manual_groups` / `manual_nodes` / `exclude` 的效果。
    #[serde(default)]
    pub catch_all: bool,
    #[serde(default)]
    pub mode: MatchMode,
    #[serde(default)]
    pub regions: Vec<String>,
    #[serde(default)]
    pub keywords: Vec<String>,
    /// 命中即排除，优先级高于一切。
    #[serde(default)]
    pub exclude: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Group {
    /// Clash 的 proxy-group 名，**即主键**（AGENTS.md §3.3）。
    pub name: String,
    /// 其余 Clash 原生字段（type / url / interval / ...），原样透传。
    #[serde(flatten)]
    pub clash: Mapping,
    /// 本服务字段：匹配配方。导出时剥离。
    #[serde(
        default,
        rename = "match",
        skip_serializing_if = "GroupMatch::is_empty"
    )]
    pub r#match: GroupMatch,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub manual_groups: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub manual_nodes: Vec<String>,
    /// 空分组时的备用分组（§6.3 fallback_group）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_group: Option<String>,
}

impl GroupMatch {
    pub fn is_empty(&self) -> bool {
        !self.all
            && !self.catch_all
            && self.mode == MatchMode::Any
            && self.regions.is_empty()
            && self.keywords.is_empty()
            && self.exclude.is_empty()
    }
}

impl Group {
    /// 分组类型（Clash `type` 字段），缺省视为 select。
    pub fn group_type(&self) -> &str {
        mstr(&self.clash, "type").unwrap_or("select")
    }

    /// 导出时使用的 Clash 片段：`name` + 原生字段 + 计算出的 `proxies`。
    pub fn to_clash(&self, proxies: &[String]) -> Mapping {
        let mut out = Mapping::new();
        out.insert(ystr("name"), ystr(&self.name));
        for (k, v) in self.clash.iter() {
            out.insert(k.clone(), v.clone());
        }
        out.insert(
            ystr("proxies"),
            Yaml::Sequence(proxies.iter().map(|p| ystr(p.clone())).collect()),
        );
        out
    }
}

// ---------------------------------------------------------------- 规则

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum RuleSection {
    /// 用户强制覆盖，最高优先级。
    Override,
    /// 普通路由规则。
    #[default]
    Normal,
    /// 最终兜底（MATCH）。
    Final,
}

impl RuleSection {
    pub fn as_str(&self) -> &'static str {
        match self {
            RuleSection::Override => "override",
            RuleSection::Normal => "normal",
            RuleSection::Final => "final",
        }
    }
}

/// 一条规则。
///
/// 两形态（AGENTS.md §3.7）：
/// - **结构化**：`type` / `payload` / `target` / `params`，参与目标校验与级联。
/// - **原始（raw）**：整行原文，**不参与**目标校验与级联，UI 必须显式标注。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RuleDef {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
    #[serde(default)]
    pub section: RuleSection,

    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    pub r#type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub params: Vec<String>,

    /// 原始模式：整行原文。存在时忽略结构化字段。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<String>,
}

impl RuleDef {
    pub fn is_raw(&self) -> bool {
        self.raw.is_some()
    }

    pub fn raw_text(&self) -> &str {
        self.raw.as_deref().unwrap_or("")
    }

    /// 规则类型（结构化取 `type`；raw 取首段）。
    pub fn rule_type(&self) -> String {
        if let Some(raw) = &self.raw {
            return raw.split(',').next().unwrap_or("").trim().to_uppercase();
        }
        self.r#type.as_deref().unwrap_or("").trim().to_uppercase()
    }
}

// ---------------------------------------------------------------- 地区别名

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegionAlias {
    pub region: String,
    #[serde(default)]
    pub aliases: Vec<String>,
}

// ---------------------------------------------------------------- 设置

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RegionSource {
    /// 节点名优先，GeoIP 兜底（默认，AGENTS.md §3.5）。
    #[default]
    NameFirst,
    /// GeoIP 优先。对中转节点会显著降低准确度。
    IpFirst,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppSettings {
    /// 出口代理设置，仅影响本服务访问上游与 GeoIP 下载，绝不进入导出结果。
    /// `configured` 用于区分旧文件/首次初始化的“未设置”与用户主动关闭代理。
    #[serde(default)]
    pub proxy: ProxySettings,
    #[serde(default)]
    pub region_source: RegionSource,
    /// 假节点过滤关键词黑名单（AGENTS.md §4.6）。
    #[serde(default = "default_filter_keywords")]
    pub filter_keywords: Vec<String>,
    #[serde(default = "default_true")]
    pub filter_enabled: bool,
    /// 对外访问前缀（BASEURL），用于拼接完整订阅链接；空表示使用相对路径。
    ///
    /// 例：`https://clash.example.com` → `https://clash.example.com/sub/<slug>`。
    /// **不进入导出结果。**
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub base_url: String,
    /// GeoIP 库的下载与自动更新设置。**只影响本服务，不进入导出结果。**
    #[serde(default)]
    pub geoip: GeoipSettings,
}

/// 网页配置的出口代理。地址只支持 http(s) 与 socks5(h)，由页面保存时规范化。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProxySettings {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub url: String,
    /// 纯新增字段：旧 store.yaml 默认 false，因此继续使用 config.toml。
    #[serde(default)]
    pub configured: bool,
}

/// GeoIP 库自动下载设置（AGENTS.md §3.5）。
///
/// 数据库文件安装到 `data/geoip/geoip-auto.mmdb`，状态记录在
/// `data/geoip/status.yaml`。库缺失时仍按 §3.5 降级为仅用节点名判定。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeoipSettings {
    /// 启动后检查并按间隔定期更新。
    #[serde(default = "default_true")]
    pub auto_update: bool,
    /// 更新间隔（天）。取值会被收敛到 1–365。
    #[serde(default = "default_geoip_interval_days")]
    pub interval_days: u32,
    /// 自定义下载地址；留空使用内置 DB-IP Lite Country（按月份生成）。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub url: String,
}

pub fn default_geoip_interval_days() -> u32 {
    7
}

impl Default for GeoipSettings {
    fn default() -> Self {
        Self {
            auto_update: true,
            interval_days: default_geoip_interval_days(),
            url: String::new(),
        }
    }
}

impl GeoipSettings {
    /// 写入与使用时都把天数收敛到合法区间，避免手改 store.yaml 后出现 0 天或天文数字。
    pub fn interval_days_clamped(&self) -> u32 {
        self.interval_days.clamp(1, 365)
    }

    /// 是否使用自定义下载地址。
    pub fn uses_custom_url(&self) -> bool {
        !self.url.trim().is_empty()
    }
}

fn default_true() -> bool {
    true
}

pub fn default_filter_keywords() -> Vec<String> {
    [
        "剩余流量",
        "套餐到期",
        "到期",
        "过期",
        "官网",
        "订阅",
        "流量重置",
        "重置时间",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            proxy: ProxySettings::default(),
            region_source: RegionSource::default(),
            filter_keywords: default_filter_keywords(),
            filter_enabled: true,
            base_url: String::new(),
            geoip: GeoipSettings::default(),
        }
    }
}

// ---------------------------------------------------------------- 方案

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OnFailure {
    /// 严格失败（默认）。
    #[default]
    Fail,
    /// 返回最近一次成功生成的完整配置。
    LastGood,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    pub slug: String,
    pub name: String,
    #[serde(default)]
    pub upstreams: Vec<String>,
    #[serde(default)]
    pub groups: Vec<String>,
    #[serde(default)]
    pub rules: Vec<String>,
    #[serde(default)]
    pub on_failure: OnFailure,
    /// 订阅令牌（§7.2）：只读本方案，可独立重置。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}

// ---------------------------------------------------------------- 顶层

/// 当前数据格式版本。格式变更时递增，并在 `migrate` 中提供升级路径。
///
/// - v1：初始格式（当时含「应用」模块）。
/// - v2：移除应用模块——`Store.apps`、`Profile.apps`、`Profile.app_overrides` 不再存在。
/// - v3：`settings.geoip`（GeoIP 库自动下载与更新设置）。纯新增，旧文件用默认值补齐。
/// - v4：`settings.base_url`（订阅链接 BASEURL）。纯新增，旧文件默认为空。
/// - v5：新增 `manual_nodes`（手动添加的分享链接节点）。纯新增，旧文件默认为空。
pub const FORMAT_VERSION: u32 = 5;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Store {
    /// 数据格式版本，用于未来迁移。
    #[serde(default = "default_format_version")]
    pub format_version: u32,
    /// 乐观并发版本号（§8.2）。
    #[serde(default)]
    pub revision: u64,
    #[serde(default)]
    pub upstreams: Vec<Upstream>,
    /// 手动添加的分享链接节点；不受上游订阅刷新影响。
    #[serde(default)]
    pub manual_nodes: Vec<ManualNode>,
    #[serde(default)]
    pub groups: Vec<Group>,
    #[serde(default)]
    pub rules: Vec<RuleDef>,
    #[serde(default)]
    pub aliases: Vec<RegionAlias>,
    /// Clash 客户端配置，会进入导出结果。
    #[serde(default)]
    pub general: Mapping,
    #[serde(default)]
    pub profiles: Vec<Profile>,
    /// 本服务行为设置，**绝不进入导出结果**。
    #[serde(default)]
    pub settings: AppSettings,
}

fn default_format_version() -> u32 {
    FORMAT_VERSION
}

impl Default for Store {
    fn default() -> Self {
        Self {
            format_version: FORMAT_VERSION,
            revision: 0,
            upstreams: Vec::new(),
            manual_nodes: Vec::new(),
            groups: default_groups(),
            rules: default_rules(),
            aliases: default_aliases(),
            general: Mapping::new(),
            profiles: Vec::new(),
            settings: AppSettings::default(),
        }
    }
}

/// 首次运行植入的默认分组。
///
/// 只覆盖最常用的地区；其余地区可通过地区别名表 + 手动新建分组扩展。
/// `Google服务` 是一个独立的选择组，默认选中 `美国节点`，规则统一指向它；
/// `漏网之鱼` 是兜底分组，避免机场改节点命名后节点凭空消失。
pub fn default_groups() -> Vec<Group> {
    fn fields(pairs: &[(&str, Yaml)]) -> Mapping {
        let mut out = Mapping::new();
        for (key, value) in pairs {
            out.insert(ystr(*key), value.clone());
        }
        out
    }
    fn group(name: &str, clash: Mapping, matcher: GroupMatch) -> Group {
        Group {
            name: name.to_string(),
            clash,
            r#match: matcher,
            manual_groups: Vec::new(),
            manual_nodes: Vec::new(),
            fallback_group: None,
        }
    }

    let select = || fields(&[("type", ystr("select"))]);
    let auto_test = fields(&[
        ("type", ystr("url-test")),
        ("url", ystr("http://www.gstatic.com/generate_204")),
        ("interval", Yaml::Number(300u64.into())),
        ("tolerance", Yaml::Number(50u64.into())),
    ]);

    // 默认展示顺序：手动切换 → 自动选择 → 主要地区 → 其他地区 → 应用组 →
    // 广告/国内 → 漏网之鱼。分组依赖仍由导出拓扑排序处理。
    let region_groups = [
        ("香港节点", "香港"),
        ("新加坡节点", "新加坡"),
        ("日本节点", "日本"),
        ("美国节点", "美国"),
        ("台湾节点", "台湾"),
        ("韩国节点", "韩国"),
        ("英国节点", "英国"),
        ("德国节点", "德国"),
    ];
    let region_names: Vec<String> = region_groups
        .iter()
        .map(|(name, _)| (*name).to_string())
        .collect();

    // 「自动选择 / 手动切换」不是直接收所有节点，而是把地区组作为可选项：
    // 这样用户新增一个地区组后，只需把它加入这两个组即可参与自动测速/手动切换。
    let mut auto = group("自动选择", auto_test, GroupMatch::default());
    auto.manual_groups = region_names.clone();
    auto.manual_groups.push("漏网之鱼".to_string());

    let mut manual = group("手动切换", select(), GroupMatch::default());
    manual.manual_groups.push("自动选择".to_string());
    manual.manual_groups.extend(region_names.iter().cloned());
    manual.manual_groups.push("漏网之鱼".to_string());

    // 服务组共用同一套地区选项：美国节点第一（默认选择），其余地区随后，
    // 再补自动选择 / 手动切换；DIRECT 走 manual_nodes，作为临时直连选项。
    let mut service_choices = vec!["美国节点".to_string()];
    service_choices.extend(
        region_names
            .iter()
            .filter(|name| name.as_str() != "美国节点")
            .cloned(),
    );
    service_choices.push("自动选择".to_string());
    service_choices.push("手动切换".to_string());
    let service_nodes = vec!["DIRECT".to_string()];

    // Google 服务单独成组：规则指向它，组内按地区分组列出所有地区选项。
    let mut google = group("Google服务", select(), GroupMatch::default());
    google.manual_groups = service_choices.clone();
    google.manual_nodes = service_nodes.clone();

    // 常用功能分组：每个组都是「选地区」的选择器，规则统一指向分组。
    let mut services = vec![google];
    for name in ["OpenAI", "奈飞"] {
        let mut service = group(name, select(), GroupMatch::default());
        service.manual_groups = service_choices.clone();
        service.manual_nodes = service_nodes.clone();
        services.push(service);
    }

    // Steam 拆成商店（走代理）与下载（默认直连）两组，对齐 ACL4SSR 等社区配置。
    let mut steam_store = group("Steam商店", select(), GroupMatch::default());
    steam_store.manual_groups = service_choices.clone();
    steam_store.manual_nodes = service_nodes.clone();
    services.push(steam_store);

    let mut steam_download = group("Steam下载", select(), GroupMatch::default());
    steam_download.manual_nodes = vec!["DIRECT".to_string()];
    services.push(steam_download);

    for name in ["Telegram", "Disney+", "Spotify", "TikTok", "GitHub"] {
        let mut service = group(name, select(), GroupMatch::default());
        service.manual_groups = service_choices.clone();
        service.manual_nodes = service_nodes.clone();
        services.push(service);
    }

    // 广告拦截：默认 REJECT，第二个选项 DIRECT 方便手动关闭；
    // 内置策略通过 manual_nodes 进入 proxies（Clash 允许 select 组包含 REJECT/DIRECT）。
    let mut adblock = group("广告拦截", select(), GroupMatch::default());
    adblock.manual_nodes = vec!["REJECT".to_string(), "DIRECT".to_string()];
    services.push(adblock);

    // 国内网站默认直连；只放 DIRECT，避免默认项被其他分组抢占。
    let mut domestic = group("国内网站", select(), GroupMatch::default());
    domestic.manual_nodes = vec!["DIRECT".to_string()];
    services.push(domestic);

    let mut groups = vec![manual, auto];
    for (name, region) in region_groups {
        groups.push(group(
            name,
            select(),
            GroupMatch {
                regions: vec![region.to_string()],
                ..Default::default()
            },
        ));
    }
    groups.extend(services);
    groups.push(group(
        "漏网之鱼",
        select(),
        GroupMatch {
            catch_all: true,
            ..Default::default()
        },
    ));
    groups
}

/// 首次运行植入的默认路由规则。
///
/// Google 相关域名走「Google服务」分组（默认选中「美国节点」）；
/// 最后一条 `MATCH` 用来自动选择。规则只是资产，仍需要在方案里勾选后才会进入导出。
pub fn default_rules() -> Vec<RuleDef> {
    fn rule(id: &str, tp: &str, payload: &str, target: &str) -> RuleDef {
        RuleDef {
            id: id.to_string(),
            section: RuleSection::Normal,
            r#type: Some(tp.to_string()),
            payload: Some(payload.to_string()),
            target: Some(target.to_string()),
            params: Vec::new(),
            raw: None,
        }
    }

    let mut rules = vec![
        // 一条 KEYWORD 覆盖 google.com / googleapis.com / googleusercontent.com /
        // googlevideo.com 以及各国 google 域名；其余 Google 服务单独列出。
        rule("r-google", "DOMAIN-KEYWORD", "google", "Google服务"),
        rule("r-gstatic", "DOMAIN-SUFFIX", "gstatic.com", "Google服务"),
        rule("r-youtube", "DOMAIN-SUFFIX", "youtube.com", "Google服务"),
        rule("r-youtu-be", "DOMAIN-SUFFIX", "youtu.be", "Google服务"),
        rule("r-ytimg", "DOMAIN-SUFFIX", "ytimg.com", "Google服务"),
        rule("r-gmail", "DOMAIN-SUFFIX", "gmail.com", "Google服务"),
        rule(
            "r-googlemail",
            "DOMAIN-SUFFIX",
            "googlemail.com",
            "Google服务",
        ),
        rule("r-blogspot", "DOMAIN-SUFFIX", "blogspot.com", "Google服务"),
    ];
    // 常用服务域名 → 各自的服务分组；组内默认美国节点，可在客户端切换地区。
    for (id, tp, payload, target) in [
        ("r-openai", "DOMAIN-KEYWORD", "openai", "OpenAI"),
        ("r-chatgpt", "DOMAIN-SUFFIX", "chatgpt.com", "OpenAI"),
        ("r-oaistatic", "DOMAIN-SUFFIX", "oaistatic.com", "OpenAI"),
        (
            "r-oaiusercontent",
            "DOMAIN-SUFFIX",
            "oaiusercontent.com",
            "OpenAI",
        ),
        ("r-sora", "DOMAIN-SUFFIX", "sora.com", "OpenAI"),
        ("r-netflix", "DOMAIN-KEYWORD", "netflix", "奈飞"),
        ("r-nflx", "DOMAIN-KEYWORD", "nflx", "奈飞"),
        ("r-steam", "DOMAIN-KEYWORD", "steam", "Steam商店"),
        ("r-steam-team", "DOMAIN-SUFFIX", "s.team", "Steam商店"),
        (
            "r-steam-store",
            "DOMAIN-SUFFIX",
            "steampowered.com",
            "Steam商店",
        ),
        (
            "r-steam-community",
            "DOMAIN-SUFFIX",
            "steamcommunity.com",
            "Steam商店",
        ),
        ("r-telegram", "DOMAIN-KEYWORD", "telegram", "Telegram"),
        ("r-t-me", "DOMAIN-SUFFIX", "t.me", "Telegram"),
        ("r-tdesktop", "DOMAIN-SUFFIX", "tdesktop.com", "Telegram"),
        ("r-telegra-ph", "DOMAIN-SUFFIX", "telegra.ph", "Telegram"),
        ("r-telesco-pe", "DOMAIN-SUFFIX", "telesco.pe", "Telegram"),
        ("r-disney", "DOMAIN-KEYWORD", "disney", "Disney+"),
        ("r-dssott", "DOMAIN-SUFFIX", "dssott.com", "Disney+"),
        ("r-bamgrid", "DOMAIN-SUFFIX", "bamgrid.com", "Disney+"),
        ("r-spotify", "DOMAIN-KEYWORD", "spotify", "Spotify"),
        ("r-scdn", "DOMAIN-SUFFIX", "scdn.co", "Spotify"),
        ("r-tiktok", "DOMAIN-KEYWORD", "tiktok", "TikTok"),
        ("r-ttlivecdn", "DOMAIN-SUFFIX", "ttlivecdn.com", "TikTok"),
        ("r-musical-ly", "DOMAIN-SUFFIX", "musical.ly", "TikTok"),
        ("r-github", "DOMAIN-KEYWORD", "github", "GitHub"),
    ] {
        rules.push(rule(id, tp, payload, target));
    }
    // 广告拦截规则放在 override 段：优先于普通分流，且目标组可在客户端切到 DIRECT 关闭。
    for (id, tp, payload) in [
        ("r-ads-doubleclick", "DOMAIN-KEYWORD", "doubleclick"),
        (
            "r-ads-googleadservices",
            "DOMAIN-SUFFIX",
            "googleadservices.com",
        ),
        (
            "r-ads-googlesyndication",
            "DOMAIN-SUFFIX",
            "googlesyndication.com",
        ),
        (
            "r-ads-adservice-google",
            "DOMAIN-SUFFIX",
            "adservice.google.com",
        ),
        ("r-ads-adsrvr", "DOMAIN-SUFFIX", "adsrvr.org"),
        ("r-ads-adnxs", "DOMAIN-SUFFIX", "adnxs.com"),
        ("r-ads-criteo", "DOMAIN-SUFFIX", "criteo.com"),
        ("r-ads-taboola", "DOMAIN-SUFFIX", "taboola.com"),
        ("r-ads-outbrain", "DOMAIN-SUFFIX", "outbrain.com"),
        ("r-ads-scorecard", "DOMAIN-SUFFIX", "scorecardresearch.com"),
        ("r-ads-quantserve", "DOMAIN-SUFFIX", "quantserve.com"),
        ("r-ads-moatads", "DOMAIN-SUFFIX", "moatads.com"),
        ("r-ads-adcolony", "DOMAIN-SUFFIX", "adcolony.com"),
        ("r-ads-applovin", "DOMAIN-SUFFIX", "applovin.com"),
        ("r-ads-unityads", "DOMAIN-SUFFIX", "unityads.unity3d.com"),
        ("r-ads-umeng", "DOMAIN-SUFFIX", "umeng.com"),
        ("r-ads-cnzz", "DOMAIN-SUFFIX", "cnzz.com"),
        ("r-ads-mmstat", "DOMAIN-SUFFIX", "mmstat.com"),
        ("r-ads-tanx", "DOMAIN-SUFFIX", "tanx.com"),
        ("r-ads-alimama", "DOMAIN-SUFFIX", "alimama.com"),
        ("r-ads-pos-baidu", "DOMAIN-SUFFIX", "pos.baidu.com"),
        ("r-ads-cpro-baidu", "DOMAIN-SUFFIX", "cpro.baidu.com"),
        ("r-ads-gdt-qq", "DOMAIN-SUFFIX", "gdt.qq.com"),
    ] {
        let mut rule = rule(id, tp, payload, "广告拦截");
        rule.section = RuleSection::Override;
        rules.push(rule);
    }
    // 国内网站走 override：优先直连，避免被普通代理规则抢走。
    for (id, tp, payload) in [
        ("r-cn", "DOMAIN-SUFFIX", "cn"),
        ("r-cn-baidu", "DOMAIN-SUFFIX", "baidu.com"),
        ("r-cn-qq", "DOMAIN-SUFFIX", "qq.com"),
        ("r-cn-tencent", "DOMAIN-SUFFIX", "tencent.com"),
        ("r-cn-wechat", "DOMAIN-SUFFIX", "wechat.com"),
        ("r-cn-taobao", "DOMAIN-SUFFIX", "taobao.com"),
        ("r-cn-tmall", "DOMAIN-SUFFIX", "tmall.com"),
        ("r-cn-alipay", "DOMAIN-SUFFIX", "alipay.com"),
        ("r-cn-aliyun", "DOMAIN-SUFFIX", "aliyun.com"),
        ("r-cn-jd", "DOMAIN-SUFFIX", "jd.com"),
        ("r-cn-bilibili", "DOMAIN-SUFFIX", "bilibili.com"),
        ("r-cn-hdslb", "DOMAIN-SUFFIX", "hdslb.com"),
        ("r-cn-zhihu", "DOMAIN-SUFFIX", "zhihu.com"),
        ("r-cn-zhimg", "DOMAIN-SUFFIX", "zhimg.com"),
        ("r-cn-weibo", "DOMAIN-SUFFIX", "weibo.com"),
        ("r-cn-163", "DOMAIN-SUFFIX", "163.com"),
        ("r-cn-youku", "DOMAIN-SUFFIX", "youku.com"),
        ("r-cn-iqiyi", "DOMAIN-SUFFIX", "iqiyi.com"),
        ("r-cn-douyin", "DOMAIN-SUFFIX", "douyin.com"),
        ("r-cn-toutiao", "DOMAIN-SUFFIX", "toutiao.com"),
        ("r-cn-xiaohongshu", "DOMAIN-SUFFIX", "xiaohongshu.com"),
        ("r-cn-meituan", "DOMAIN-SUFFIX", "meituan.com"),
        ("r-cn-ctrip", "DOMAIN-SUFFIX", "ctrip.com"),
        ("r-cn-mi", "DOMAIN-SUFFIX", "mi.com"),
        ("r-cn-huawei", "DOMAIN-SUFFIX", "huawei.com"),
        ("r-cn-gitee", "DOMAIN-SUFFIX", "gitee.com"),
        ("r-cn-geoip", "GEOIP", "CN"),
    ] {
        let mut rule = rule(id, tp, payload, "国内网站");
        rule.section = RuleSection::Override;
        rules.push(rule);
    }
    // Steam 下载走 override，优先于 Steam商店 的普通规则；命中内容/CDN 域名时直连。
    for (id, tp, payload) in [
        ("r-steam-dl-content", "DOMAIN-SUFFIX", "steamcontent.com"),
        (
            "r-steam-dl-usercontent",
            "DOMAIN-SUFFIX",
            "steamusercontent.com",
        ),
        ("r-steam-dl-china", "DOMAIN-SUFFIX", "steamchina.com"),
        ("r-steam-dl-cm", "DOMAIN-SUFFIX", "cm.steampowered.com"),
        (
            "r-steam-dl-static-cn",
            "DOMAIN-SUFFIX",
            "steamstatic.com.8686c.com",
        ),
    ] {
        let mut rule = rule(id, tp, payload, "Steam下载");
        rule.section = RuleSection::Override;
        rules.push(rule);
    }
    rules.push(RuleDef {
        id: "r-final".to_string(),
        section: RuleSection::Final,
        r#type: Some("MATCH".to_string()),
        payload: None,
        target: Some("自动选择".to_string()),
        params: Vec::new(),
        raw: None,
    });
    rules
}

/// 首次运行植入的默认地区别名表。
pub fn default_aliases() -> Vec<RegionAlias> {
    let pairs: &[(&str, &[&str])] = &[
        ("香港", &["HK", "Hong Kong", "🇭🇰", "港", "沪港"]),
        ("台湾", &["TW", "Taiwan", "🇹🇼", "台北", "台"]),
        ("日本", &["JP", "Japan", "🇯🇵", "东京", "大阪", "日"]),
        ("新加坡", &["SG", "Singapore", "🇸🇬", "狮城", "新加坡"]),
        (
            "美国",
            &["US", "USA", "United States", "🇺🇸", "洛杉矶", "圣何塞", "美"],
        ),
        ("韩国", &["KR", "Korea", "South Korea", "🇰🇷", "首尔", "韩"]),
        (
            "英国",
            &["UK", "GB", "United Kingdom", "Britain", "🇬🇧", "伦敦", "英"],
        ),
        ("德国", &["DE", "Germany", "🇩🇪", "法兰克福", "柏林", "德"]),
        ("澳门", &["MO", "Macau", "Macao", "🇲🇴", "澳门"]),
        ("马来西亚", &["MY", "Malaysia", "🇲🇾", "吉隆坡", "马来"]),
        ("泰国", &["TH", "Thailand", "🇹🇭", "曼谷", "泰"]),
        ("越南", &["VN", "Vietnam", "🇻🇳", "胡志明", "河内", "越南"]),
        ("菲律宾", &["PH", "Philippines", "🇵🇭", "马尼拉", "菲律宾"]),
        (
            "澳大利亚",
            &[
                "AU",
                "Australia",
                "🇦🇺",
                "悉尼",
                "墨尔本",
                "澳大利亚",
                "澳洲",
            ],
        ),
        (
            "加拿大",
            &["CA", "Canada", "🇨🇦", "多伦多", "温哥华", "加拿大"],
        ),
        ("法国", &["FR", "France", "🇫🇷", "巴黎", "法国"]),
        (
            "荷兰",
            &["NL", "Netherlands", "Holland", "🇳🇱", "阿姆斯特丹", "荷兰"],
        ),
        ("俄罗斯", &["RU", "Russia", "🇷🇺", "莫斯科", "俄罗斯"]),
        ("土耳其", &["TR", "Turkey", "🇹🇷", "伊斯坦布尔", "土耳其"]),
        (
            "阿联酋",
            &["AE", "UAE", "United Arab Emirates", "🇦🇪", "迪拜", "阿联酋"],
        ),
        ("巴西", &["BR", "Brazil", "🇧🇷", "圣保罗", "巴西"]),
        (
            "阿根廷",
            &["AR", "Argentina", "🇦🇷", "布宜诺斯艾利斯", "阿根廷"],
        ),
        ("南非", &["ZA", "South Africa", "🇿🇦", "约翰内斯堡", "南非"]),
        ("意大利", &["IT", "Italy", "🇮🇹", "米兰", "意大利"]),
        (
            "西班牙",
            &["ES", "Spain", "🇪🇸", "马德里", "巴塞罗那", "西班牙"],
        ),
        (
            "瑞士",
            &["CH", "Switzerland", "🇨🇭", "苏黎世", "日内瓦", "瑞士"],
        ),
        ("瑞典", &["SE", "Sweden", "🇸🇪", "斯德哥尔摩", "瑞典"]),
        ("波兰", &["PL", "Poland", "🇵🇱", "华沙", "波兰"]),
    ];
    pairs
        .iter()
        .map(|(region, aliases)| RegionAlias {
            region: (*region).to_string(),
            aliases: aliases.iter().map(|s| (*s).to_string()).collect(),
        })
        .collect()
}

impl Store {
    pub fn upstream(&self, id: &str) -> Option<&Upstream> {
        self.upstreams.iter().find(|u| u.id == id)
    }
    pub fn manual_node(&self, id: &str) -> Option<&ManualNode> {
        self.manual_nodes.iter().find(|n| n.id == id)
    }
    pub fn group(&self, name: &str) -> Option<&Group> {
        self.groups.iter().find(|g| g.name == name)
    }
    pub fn rule(&self, id: &str) -> Option<&RuleDef> {
        self.rules.iter().find(|r| r.id == id)
    }
    pub fn profile(&self, slug: &str) -> Option<&Profile> {
        self.profiles.iter().find(|p| p.slug == slug)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_store_has_seed_groups() {
        let store = Store::default();
        let names: Vec<&str> = store.groups.iter().map(|g| g.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "手动切换",
                "自动选择",
                "香港节点",
                "新加坡节点",
                "日本节点",
                "美国节点",
                "台湾节点",
                "韩国节点",
                "英国节点",
                "德国节点",
                "Google服务",
                "OpenAI",
                "奈飞",
                "Steam商店",
                "Steam下载",
                "Telegram",
                "Disney+",
                "Spotify",
                "TikTok",
                "GitHub",
                "广告拦截",
                "国内网站",
                "漏网之鱼",
            ]
        );
        let auto = store.group("自动选择").unwrap();
        assert_eq!(auto.group_type(), "url-test");
        assert_eq!(
            mstr(&auto.clash, "url"),
            Some("http://www.gstatic.com/generate_204")
        );
        // 顶部两个组不是 match.all，而是把地区组作为可选项。
        assert!(!auto.r#match.all);
        assert_eq!(
            auto.manual_groups,
            vec![
                "香港节点",
                "新加坡节点",
                "日本节点",
                "美国节点",
                "台湾节点",
                "韩国节点",
                "英国节点",
                "德国节点",
                "漏网之鱼",
            ]
        );
        let manual = store.group("手动切换").unwrap();
        assert!(!manual.r#match.all);
        assert_eq!(
            manual.manual_groups.first().map(String::as_str),
            Some("自动选择")
        );
        assert!(manual.manual_groups.contains(&"美国节点".to_string()));
        let google = store.group("Google服务").unwrap();
        assert_eq!(google.group_type(), "select");
        assert_eq!(
            google.manual_groups,
            vec![
                "美国节点".to_string(),
                "香港节点".to_string(),
                "新加坡节点".to_string(),
                "日本节点".to_string(),
                "台湾节点".to_string(),
                "韩国节点".to_string(),
                "英国节点".to_string(),
                "德国节点".to_string(),
                "自动选择".to_string(),
                "手动切换".to_string(),
            ],
            "Google 服务组应包含全部地区组，且美国节点排第一作为默认"
        );
        assert_eq!(
            google.manual_nodes,
            vec!["DIRECT".to_string()],
            "服务组应提供 DIRECT 直连选项"
        );
        let openai = store.group("OpenAI").unwrap();
        assert_eq!(openai.group_type(), "select");
        assert_eq!(
            openai.manual_groups, google.manual_groups,
            "所有服务组应共用同一套地区选项"
        );
        assert_eq!(
            openai.manual_nodes, google.manual_nodes,
            "所有服务组应共用 DIRECT 选项"
        );
        for name in [
            "OpenAI", "奈飞", "Telegram", "Disney+", "Spotify", "TikTok", "GitHub",
        ] {
            assert!(store.group(name).is_some(), "缺少默认服务分组：{name}");
        }
        let steam_store = store.group("Steam商店").unwrap();
        assert_eq!(
            steam_store.manual_groups, google.manual_groups,
            "Steam商店应与其他服务组共用地区选项"
        );
        let steam_download = store.group("Steam下载").unwrap();
        assert_eq!(
            steam_download.manual_nodes,
            vec!["DIRECT".to_string()],
            "Steam下载默认直连"
        );
        let adblock = store.group("广告拦截").unwrap();
        assert_eq!(adblock.group_type(), "select");
        assert_eq!(
            adblock.manual_nodes,
            vec!["REJECT".to_string(), "DIRECT".to_string()],
            "广告拦截默认 REJECT，DIRECT 用于关闭"
        );
        let domestic = store.group("国内网站").unwrap();
        assert_eq!(domestic.group_type(), "select");
        assert_eq!(
            domestic.manual_nodes,
            vec!["DIRECT".to_string()],
            "国内网站默认 DIRECT"
        );
        assert!(store.group("漏网之鱼").unwrap().r#match.catch_all);
        // 兜底组只能有一个；地区组必须引用别名表里存在的地区。
        assert_eq!(
            store.groups.iter().filter(|g| g.r#match.catch_all).count(),
            1
        );
    }

    #[test]
    fn default_store_has_google_and_service_rules() {
        let store = Store::default();
        let normal: Vec<&RuleDef> = store
            .rules
            .iter()
            .filter(|r| r.section == RuleSection::Normal)
            .collect();
        assert_eq!(normal.len(), 33);
        assert_eq!(
            normal
                .iter()
                .filter(|r| r.target.as_deref() == Some("Google服务"))
                .count(),
            8
        );
        let mut targets: Vec<&str> = normal.iter().filter_map(|r| r.target.as_deref()).collect();
        targets.sort_unstable();
        targets.dedup();
        assert_eq!(
            targets,
            vec![
                "Disney+",
                "GitHub",
                "Google服务",
                "OpenAI",
                "Spotify",
                "Steam商店",
                "Telegram",
                "TikTok",
                "奈飞",
            ]
        );
        assert!(
            normal.iter().any(
                |r| r.rule_type() == "DOMAIN-KEYWORD" && r.payload.as_deref() == Some("google")
            ),
            "应有覆盖 Google 各域名的 DOMAIN-KEYWORD 规则"
        );
        let ads: Vec<&RuleDef> = store
            .rules
            .iter()
            .filter(|r| r.section == RuleSection::Override)
            .collect();
        assert_eq!(ads.len(), 55);
        assert_eq!(
            ads.iter()
                .filter(|r| r.target.as_deref() == Some("广告拦截"))
                .count(),
            23
        );
        assert_eq!(
            ads.iter()
                .filter(|r| r.target.as_deref() == Some("国内网站"))
                .count(),
            27
        );
        assert_eq!(
            ads.iter()
                .filter(|r| r.target.as_deref() == Some("Steam下载"))
                .count(),
            5
        );
        assert_eq!(store.rules.len(), 89);
        let finals: Vec<&RuleDef> = store
            .rules
            .iter()
            .filter(|r| r.section == RuleSection::Final)
            .collect();
        assert_eq!(finals.len(), 1);
        assert_eq!(finals[0].rule_type(), "MATCH");
        assert_eq!(finals[0].target.as_deref(), Some("自动选择"));
        // id 必须唯一。
        let mut ids: Vec<&str> = store.rules.iter().map(|r| r.id.as_str()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), store.rules.len());
    }

    #[test]
    fn default_aliases_cover_common_regions() {
        let aliases = default_aliases();
        let regions: Vec<&str> = aliases.iter().map(|a| a.region.as_str()).collect();
        for region in [
            "香港",
            "台湾",
            "日本",
            "新加坡",
            "美国",
            "韩国",
            "英国",
            "德国",
            "澳门",
            "马来西亚",
            "泰国",
            "越南",
            "菲律宾",
            "澳大利亚",
            "加拿大",
            "法国",
            "荷兰",
            "俄罗斯",
            "土耳其",
            "阿联酋",
            "巴西",
            "阿根廷",
            "南非",
            "意大利",
            "西班牙",
            "瑞士",
            "瑞典",
            "波兰",
        ] {
            assert!(regions.contains(&region), "缺少默认地区：{region}");
        }
        assert!(
            crate::region::find_alias_conflicts(&aliases).is_empty(),
            "默认别名不得跨地区重复"
        );
    }
}
