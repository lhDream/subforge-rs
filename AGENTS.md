# AGENTS.md

本文件面向在此仓库中工作的 AI 编码代理（以及人类协作者），说明项目定位、架构约束与开发约定。**开始改动前请先读完本文件。本文件与既有代码冲突时，以本文件为准，并同步修正代码或明确提出质疑。**

## 1. 项目是什么

`SubForge.rs`（仓库名 `subforge-rs`）是一个用 Rust 编写的 **Clash 订阅合并与配置编排服务**。它缓存并合并多个上游 Clash 订阅的**代理节点**，按**地区判定 + 匹配规则**把节点归入用户定义的分组，叠加路由规则，导出一份可直接投入使用的 Clash 配置文件。

- 技术栈：Rust（edition 2024）、`axum` 0.8、`tokio`。
- 形态：**单体、前后端不分离**的 HTTP 服务（硬约束见 §2）。
- 使用者：**仅单人自用**，不面向多租户、公开注册或团队协作场景。

### 1.1 目标客户端（验收基准）

**以 Mihomo 为主要支持内核。** 所有输出必须以目标内核**实际加载成功**为验收标准，而不是"看起来是合法 YAML"。

- 支持的协议、规则类型、分组能力在 Clash 各分支间并不一致（Clash Premium / Clash.Meta / Mihomo / Clash Verge 内置内核各有差异）。
- 规则解析语义以 Mihomo 源码 `rules/common/base.go` 的 `ParseRulePayload` 为准（§3.7）。
- 未命中规则的处理以 `tunnel/tunnel.go` 的 `match()` 为准（§6.1 第 10 条）。
- **最低支持版本待确定**，确定后写入 README，并在验收时使用该版本实测。

### 核心目标

1. **资产化管理**：上游、分组、路由规则、通用设置各自独立维护、独立复用。
2. **节点自动归类**：分组通过地区判定与关键词匹配自动吸纳节点，而非手工枚举节点名。
3. **规则按目标绑定分组**：每条规则显式写出目标分组，不依赖节点名里的应用标注。
4. **缓存优先**：上游数据落盘缓存并定期刷新；拉取失败时继续使用旧数据。
5. **导出时组合**：勾选「哪些上游 + 哪些分组 + 哪些规则」，合并为一份配置。
6. **多套方案并存**：一组选择可保存为命名方案，每套方案有独立订阅 URL。

### 非目标（不要实现）

- ❌ 不做多用户体系、账号注册、权限分级、计费、审计日志。
- ❌ 不做节点测速、延迟排序（分组类型 `url-test` 等由客户端自行测速）。
- ❌ 不做 Clash 内核的功能（不转发流量、不实现任何代理协议）。
- ❌ 不引入 SPA 或前端构建工具链（见 §2）。
- ❌ 不做多格式导出（Base64 订阅、`ss://` 分享链接等）——**输出**只有 Clash YAML 一种格式。
  （注意区分：作为**输入**的分享链接列表是支持的，见 §4.3。）确有需要时先与用户确认。

## 2. 前后端不分离（架构硬约束）

本项目是**服务端渲染的单体应用**，没有独立前端工程。以下为硬约束：

- 页面由 axum handler 渲染 HTML 返回，模板引擎默认 **askama**；更换模板引擎需先说明理由。
- 静态资源放 `static/`，由 axum 直接托管。**允许原生 JS 做渐进增强，但不得引入框架或打包器。**
- 交互模型：HTML 表单 `POST` → 服务端处理 → `303 See Other` 重定向回页面（PRG 模式）。**不要**为页面另写 JSON API + `fetch`。
- 仓库中**不得出现** `package.json`、`node_modules`、`vite.config.*` 等前端工具链产物。
- 不做 CORS 配置；管理页面与订阅输出端点同源、同进程。
- 依赖 JS 才可完成的操作必须提供无 JS 降级。

## 3. 领域模型

### 3.1 资产一览

| 资产 | 管理页面 | 存储位置 | 身份键 | 有序 |
| --- | --- | --- | --- | --- |
| 上游订阅 Upstream | `/upstreams` | `data/store.yaml` | `id`（slug，不可变） | 是 |
| 手动节点 ManualNode | `/nodes` | `data/store.yaml` | `id`（slug） | 是 |
| 分组 Group | `/groups` | `data/store.yaml` | Clash `name`（§3.3） | 输出拓扑序 |
| 路由规则 Rule | `/rules` | `data/store.yaml` | `id` | **是（分段内有序）** |
| 通用设置 General | `/general` | `data/store.yaml` | 单例 | — |
| 地区别名表 Alias | `/aliases` | `data/store.yaml` | 地区名 | 否 |
| 导出方案 Profile | `/profiles` | `data/store.yaml` | `slug` | — |
| 上游缓存 | — | `data/cache/<id>.yaml` | 上游 id | — |

> 全部业务数据集中在**单一 `data/store.yaml`**，理由见 §8.1。缓存因体积大且按上游独立更新，单独存放。

**关键原则：各类资产之间没有从属关系。** 方案只保存"选了哪些 id"。任何把分组塞进上游、或把规则塞进分组的建模都是错的。

### 3.2 引用关系

```
Profile ──引用──> Upstream.id
        ──引用──> Group.name     (依赖闭包须完整，见 §3.3.2)
        ──引用──> Rule.id

Group.match         ──匹配──> 节点
Group.manual_groups ──引用──> 其他 Group.name
Group.manual_nodes  ──引用──> 字面节点名
Rule.target         ──引用──> Group.name / DIRECT / REJECT / 内置策略
Proxy.dialer-proxy  ──引用──> 其他节点名（见 §6.1 第 12 条）
```

### 3.3 分组的身份与引用完整性

Clash 语义决定：`proxy-groups[].name` 既被 `rules` 的最后一列引用，也被其他分组的 `proxies` 列表引用。

- 分组的 `name` **即主键**，不额外引入内部 id。
- 上游、规则使用内部 `id`，展示名可自由修改，不产生断链。

#### 3.3.1 引用处理总原则

> **检查所有直接与间接依赖，确认修改后的完整状态有效，再提交。**

**不允许静默断链，也不允许"清理掉引用"了事。**

| 操作 | 行为 |
| --- | --- |
| 重命名分组 | 级联改写**全部**引用处：其他分组的 `manual_groups`、规则的 `target`、**方案的 `groups` 列表**。先展示影响范围，确认后提交 |
| 删除分组 | **优先要求指定替代目标**（将引用重定向到另一个分组）。无法提供替代目标时**阻止删除**，不要直接清空引用 |
| 删除上游 | 列出引用它的方案；从这些方案的 `upstreams` 中移除；删除其缓存文件 |
| 删除规则 | 列出引用它的方案；从这些方案的 `rules` 中移除 |

- **判断"是否被引用"必须检查全部直接与间接引用**，不能只看方案。一个分组可能没有被任何方案直接选中，但被其他分组或规则引用。
- 仅当**任何位置都没有引用**时，才允许无确认直接删除。
- 确认页必须列出：受影响对象、将发生的具体改动、以及提交后的最终状态摘要。

#### 3.3.2 方案的依赖闭包

方案勾选分组 A，而 A 的 `manual_groups` 引用了未被勾选的 B 时：

- **UI 层**：勾选 A 时**自动补全并勾选其依赖闭包**，被自动加入的项以"依赖自动加入"样式高亮，可手动取消。
- **提交时**：校验闭包完整。缺失依赖 → **拒绝保存/导出**，并列出缺失项，**不静默自动包含**。
- 依赖闭包计算需检测环（A→B→A 直接报错）。

### 3.4 分组的节点来源：匹配 + 手动

**分组不直接存储节点列表**，存储的是配方：

```
Group.members = manual_groups(子分组名) ⊕ manual_nodes(字面节点名) ⊕ auto_matched(自动匹配)
```

顺序固定为 **手动子分组 → 手动节点 → 自动匹配节点**，段内保持各自顺序，整体去重。

#### 匹配配置

```yaml
- name: 美国节点
  type: select
  url: http://www.gstatic.com/generate_204
  interval: 300
  match:                          # ↓ 本服务字段，导出时剥离
    mode: any                     # any（默认，OR）| all（AND）
    regions: [美国]
    keywords: [GPT, 解锁]
    exclude: [香港]
  manual_groups: [自动选择]
  manual_nodes: []
```

**`mode` 决定地区与关键词的组合方式**（这是必须显式选择的，不要依赖直觉）：

| mode | 语义 | 典型用途 |
| --- | --- | --- |
| `any`（默认） | `regions` **或** `keywords` 任一命中 | 扩大范围：所有美国节点，加上任何标了 GPT 的节点 |
| `all` | `regions` **且** `keywords` 都命中 | 缩小范围：只要"美国且 GPT"的节点 |

> ⚠️ `any` 模式下，`regions: [美国]` + `keywords: [GPT]` 会把一个叫「日本 GPT」的节点收进「美国节点」分组。这符合语义但违背直觉，**预览页必须展示每个节点的入组原因**（§5），而不只是数量。

**匹配语义**：

1. `match.catch_all: true` → **兜底分组**：收录未被本方案其他分组匹配的节点（俗称「漏网之鱼」）。
   它会**取代**下面第 2、3 条的条件，只保留 `exclude` 与手动项。详见下文。
2. `match.all: true` → 命中**全部**节点。
3. 否则按 `mode` 组合 `regions` 与 `keywords`；两者都为空 → 不自动匹配任何节点。
4. `exclude` 命中则排除，**优先级高于一切**（包括 `all` 与 `catch_all`）。
5. `keywords`：不区分大小写子串；`re/` 前缀启用正则。

#### 兜底分组（`catch_all`）

机场常新增地区、或改节点命名，导致某个分组**一个节点都匹配不到**；这些节点如果没人收，
就会从配置里凭空消失。兜底分组给它们一个固定去处。

- **语义**：一个节点进兜底分组，当且仅当它**没有被本方案其他分组按匹配条件选中**。
  - ⚠️ `match.all` 的分组（如「自动选择」）是**全收桶**，**不算**「选中」。否则任何带
    「自动选择」的方案里兜底都会永远为空——而那恰恰是最常见的配置形态。
  - 其他分组的 `manual_nodes` 算「已明确归置」，不再落进兜底。
- **求值顺序**：兜底分组必须在其他分组**全部求值之后**再算，否则「已选中」集合不完整。
- **不依赖名字**：分组名可任意改。预设名是「漏网之鱼」，但那只是预设；
  语义由 `catch_all` 标志表达，**代码里没有任何按名字特判的逻辑**。
- **取代关系**：勾了 `catch_all` 后 `all` / `mode` / `regions` / `keywords` 全部失效，
  只保留 `exclude` 与 `manual_groups` / `manual_nodes`。
- **允许为空**：所有节点都被别的分组收走时，兜底为空是**正常状态**。
  - 普通分组为空 → **错误**（Clash 拒绝空 `proxies`）。
  - 兜底分组为空 → **警告**，并把该分组**从输出中省略**。
  - 若被省略的兜底分组仍作为**规则目标** → **跳过该规则并警告**；若被其他分组的 `manual_groups`
    引用 → 从成员中移除并**警告**（详见「空分组判定」）。
- 多个兜底分组会得到**完全相同**的成员列表 → 仅警告。

> ⚠️ 别把 `match.all` 和 `match.catch_all` 搞混：`all` 是「所有节点都进来」，
> `catch_all` 是「别的分组不要的才进来」。两者语义相反。

#### 空分组判定（按最终成员）

**判定依据是最终成员列表，不是自动匹配结果。**

- 最终成员为空且**方案里没有兜底分组** → **错误**，导出失败并指明分组
  （Clash 拒绝空 `proxies`，且此时没有任何地方承接这些节点）。
- 最终成员为空但**方案里有兜底分组** → **警告**，并把该分组**从输出中省略**。
  理由：机场经常没有某个地区的节点，没被匹配到的节点已由兜底收纳，
  为此让整份配置导不出来是本末倒置。
- 最终成员非空、但**自动匹配为空**（成员全部来自手动项）→ **警告**（信息性）。
- 只要 `manual_groups` 引用的分组有效存在，最终成员就已非空——**不存在"有 manual_groups 却展开为空"的情况**；若出现，说明引用已被破坏，属 §6.1 第 2 条错误。
- 任何被省略的分组：
  - 若仍作为**规则目标** → **跳过该规则并警告**（v37；不能让一条失效规则拖垮整份配置）；
  - 若仍被其他分组的 `manual_groups` 引用 → 从该分组成员中移除并**警告**
    （`分组「X」引用的空分组 Y 已从成员中省略`）。这样「自动选择 / 手动切换」可以把
    地区组作为可选项，而某个地区没有节点时不会让整份配置失败；若上层分组因此变空，
    有兜底时省略并警告，无兜底时仍为错误。

### 3.5 地区判定

#### 来源 A：节点名字符串匹配（默认优先）

```yaml
# data/aliases.yaml
香港: [HK, Hong Kong, 🇭🇰, 港]
日本: [JP, Japan, 🇯🇵, 东京, 大阪, 日]
美国: [US, United States, 🇺🇸, 洛杉矶, 圣何塞, 美]
```

**⚠️ 短 ASCII 别名的边界规则（修正版）**

单纯用子串会误伤；但用 `\b` 词边界又匹配不到常见写法 `US01`（`0` 是单词字符，`\b` 不成立）。正确规则是：

> **含 CJK 的别名 → 子串匹配。**
> **纯 ASCII 且长度 ≤ 3 的别名 → 前后不得是 ASCII 字母**（正则 `(?<![A-Za-z])US(?![A-Za-z])`，不区分大小写）。数字、符号、空格、字符串首尾均允许。

| 节点名 | 子串 | `\b` | 前后非字母（采用） |
| --- | --- | --- | --- |
| `Russia` | ❌ 误命中 | ✅ 不命中 | ✅ 不命中（前有 `r`） |
| `Australia` | ❌ 误命中 | ✅ 不命中 | ✅ 不命中（前有 `A`） |
| `US01` | ✅ | ❌ **漏命中** | ✅ 命中（后为数字） |
| `🇺🇸 美国 01` | ✅ | ✅ | ✅ |
| `US-01` | ✅ | ✅ | ✅ |

此规则写死，不开放配置。

#### 来源 B：GeoIP 查询（兜底）

- 库格式 **mmdb**，用 `maxminddb` crate；推荐 **DB-IP Lite Country**（免注册）或 MaxMind GeoLite2。
- 库放 `data/geoip/`，**不提交版本库**。注意许可条款（多数要求署名），README 注明来源与版本。
- `server` 为 IP → 直接查；为域名 → DNS 解析后查，解析结果**缓存**（TTL 与 `refresh_interval` 对齐）。
- 私有地址段与解析失败 → 跳过。

**⚠️ 库缺失时的降级**：`data/geoip/` 为空时，GeoIP 来源整体禁用，**仅用来源 A**，并在首页与预览页提示"GeoIP 库未安装"。不得因此拒绝导出。

#### 自动下载与更新（v16）

通用设置页提供 GeoIP 库的**手动下载**与**定期自动更新**（默认开启，间隔 7 天，可在页面关闭或改为 1–365 天）：

- **默认来源**：DB-IP Lite Country，免注册，地址按 UTC 年月生成
  `https://download.db-ip.com/free/dbip-country-lite-YYYY-MM.mmdb.gz`；月初文件尚未发布时回退上一个月。
- **自定义来源**：可填 http(s) 地址，内容可为 `.mmdb` 或 `.mmdb.gz`。地址中的 token 只存本机，
  日志与页面展示**必须脱敏**。
- **校验后提交**：下载内容先过 MMDB 校验（`inspect_mmdb`），再原子替换
  `data/geoip/geoip-auto.mmdb` 并**热加载**；校验失败绝不动现有库。
- **条件请求**：定期更新带 ETag / Last-Modified，源站未变时走 304，不重复下载。
- **失败降级**：下载/校验失败只记录状态并告警，继续使用现有库；没有库时仍按上面的降级规则。
- 目录中存在多个 `.mmdb` 时选 **mtime 最新**者：自动下载的文件因此会在重启后继续生效。
- 更新状态记录在 `data/geoip/status.yaml`（不是 `store.yaml`，避免每次更新制造业务数据备份）。
  设置本身在 `settings.geoip`，**不进入导出结果**。

#### 多结果与歧义处理

一个节点名可能同时命中多个地区（如「🇭🇰香港中转→日本」）；一个域名可能解析到多个国家的 IP。规则：

- 地区判定返回**地区集合**，不是单值。
- 分组的 `regions` 匹配：与集合**有交集**即命中。
- 集合大小 > 1 → 记入**「地区歧义」清单**，预览页展示，供人工核对或改用 `exclude`。
- 域名解析出多国 IP → 取出现次数最多者；并列时按 IP 排序取首个，并记入歧义清单。

#### ⚠️ 中转与 CDN 的失真问题（必读）

**GeoIP 判的是 `server` 入口 IP 的归属地，不是实际落地地区。** 机场大量使用中转：`server` 是香港入口、实际落地日本，GeoIP 会确定地判成香港，且因为"已判出结果"不会再回退字符串匹配。

因此默认 **`region_source = "name_first"`**（节点名优先，GeoIP 兜底）：

| 情形 | 结果 |
| --- | --- |
| 节点名命中 | 采用节点名结果 |
| 节点名未命中、GeoIP 命中 | 采用 GeoIP 结果 |
| 两者一致 | 采用，无提示 |
| 两者冲突 | 按 `region_source` 取值，并记入**「地区判定冲突」清单** |
| 都未命中 | 无地区归属，仅能被 `match.all` 或 `keywords` 命中 |

`region_source` 可在通用设置页切换为 `ip_first`。**切换前请确认机场是否使用中转**——对中转节点，`ip_first` 会显著降低准确度。

#### 纯函数边界（架构要求）

地区判定第 ④ 步被要求为不依赖网络的纯函数，但 GeoIP 需要 DNS 与 MMDB 查询。因此**分层**：

- **IO 层（外层）**：解析域名、查 MMDB，产出 `RegionInputs { name, server, resolved_ips, geo_countries }`。
- **纯函数层**：`decide_region(inputs, aliases, region_source) -> RegionDecision { regions, conflicts, reasons }`，不碰网络与文件。

单测只覆盖纯函数层；IO 层用固定 fixture 驱动。

### 3.6 应用（v15 已移除）

**本服务不再有「应用」这一层抽象。** 原先的 `/apps` 把一组域名规则绑定到一个目标分组；
它与「路由规则」能力完全重合——在 `/rules` 里给每条规则直接写 `target` 即可。

- `Store.apps`、`Profile.apps`、`Profile.app_overrides` 均不存在（数据格式 v2）；
- 导出管线不再有 `app` 分段，规则分段固定为 `override → normal → final`（§6.2）；
- 旧 `store.yaml` 里的 `apps` 键会被忽略，启动时打印告警，提示把域名规则补到「路由规则」页。

> **为什么可以移除**：应用的 `target_group` + 规则列表，等价于「把每条规则的 target 直接写成
> 分组名」。保留两套写法会让「这条域名到底走哪个分组」有两个权威来源，而收益只是少写几次目标。
> 注意：**仍然不要用节点名关键词去判断应用归属**——归属由规则的 `target` 决定。

### 3.7 规则语法与拼装（关键）

#### 规则格式（以 Mihomo `ParseRulePayload` 为准）

源码注释即格式定义：**`tp,payload,target(,params...)`**

```go
case "MATCH":
    // MATCH doesn't contain payload and params
    target = item[1]
case "NOT", "OR", "AND", "SUB-RULE", "DOMAIN-REGEX",
     "PROCESS-NAME-REGEX", "PROCESS-PATH-REGEX":
    // payload 含逗号，无 params，target 在末尾
    target = item[l-1]
    payload = strings.Join(item[1:], ",")
default:
    payload = item[1]
    target  = item[2]      // ← 目标在第 3 位，不是最后一列
    params  = item[3:]     // ← no-resolve 等参数在目标之后
```

**⚠️ 因此「把目标追加到末尾」是错的。** 反例：

| 写法 | 解析结果 | 是否正确 |
| --- | --- | --- |
| `IP-CIDR,1.2.3.0/24,no-resolve,美国节点` | `target = no-resolve` | ❌ **错误** |
| `IP-CIDR,1.2.3.0/24,美国节点,no-resolve` | `target = 美国节点`, `params = [no-resolve]` | ✅ 正确 |

**必须按规则类型识别条件、目标与附加参数，再在正确位置插入目标。**

#### 规则的两形态

| 形态 | 存储 | 参与目标校验 | 参与级联重命名/删除 |
| --- | --- | --- | --- |
| **结构化** | `type` / `payload` / `target` / `params` | ✅ | ✅ |
| **原始（raw）** | 整行原文 | ❌ | ❌ |

```yaml
rules:
  - id: r1                       # 结构化
    type: IP-CIDR
    payload: 1.2.3.0/24
    target: 美国节点
    params: [no-resolve]
  - id: r2                       # 原始：逻辑规则无法安全结构化
    raw: "AND,((NETWORK,tcp),(DOMAIN-SUFFIX,example.com)),美国节点"
```

序列化：
- 结构化默认 → `type,payload,target[,params...]`
- `MATCH` → `MATCH,target`
- 逻辑类（`AND`/`OR`/`NOT`/`SUB-RULE`/`*-REGEX`）→ `type,payload,target`（target 在末尾）
- raw → 原样输出

#### raw 规则的明确限制

**无法安全解析的规则，不得当作"已校验通过"。** raw 规则：

- **不参与**分组重命名与删除的引用完整性检查；
- **不参与**目标存在性校验；
- UI 中必须显式标注「此规则为原始模式，不参与分组引用检查」；
- 若 raw 规则文本中出现某个分组名，重命名该分组时**无法自动改写**，系统必须在重命名确认页**提示存在可能受影响的 raw 规则**，由用户自行核对。

> 这是有意的取舍：宁可让限制可见，也不要假装校验覆盖了无法解析的内容。

### 3.8 节点必须原样透传

Clash 节点有十几种类型，字段各不相同且随内核演进。

- **禁止**把节点解析成强类型 Rust 结构再序列化——会静默丢失未知字段（`reality-opts`、`smux`、`client-fingerprint` 等）。
- 统一用保序的 YAML 值类型（`serde_yaml_ng::Value` / `IndexMap<String, Value>`）承载，仅读取 `name`、`server`、`port`、`dialer-proxy` 用于匹配、去重与校验。

> **透传承诺的准确范围**：保留**未知字段及其值**。**不承诺**保留注释、缩进风格、键顺序之外的格式信息。YAML 解析后再序列化无法保留注释，这是既定限制，不要承诺做不到的事。

### 3.9 去重约定

- 主键为节点 `name`，保留**先出现**者（顺序 = `upstreams` 声明顺序）。
- 导出前必须校验节点名唯一，否则客户端会拒绝加载。
- 同名跨上游时取先者，并在预览中提示冲突。

## 4. 上游订阅：缓存、刷新与订阅信息

### 4.1 缓存布局：单文件信封

```
data/cache/<upstream-id>.yaml
```

```yaml
meta:
  last_attempt_at: 2025-01-01T12:00:00Z   # 状态字段：每次刷新都更新
  last_success_at: 2025-01-01T12:00:00Z   # 内容字段：仅校验通过时更新
  status: ok                 # ok | stale | empty | failed
  last_error: null
  last_error_kind: null      # network | http_status | parse | structure | empty
  etag: "abc123"
  expire: 1735689600
  total: 107374182400
  upload: 1234
  download: 5678
  node_count: 42
  forced_at: null
body: |
  <上游原始响应文本>
```

**为什么必须单文件**：正文与元数据分成两个文件，就存在「正文写成功、元数据写失败」的窗口，会出现**内容与到期信息错配**。同一文件后，**一次 `write temp → rename` 同时提交两者**。

- 缓存存**原始文本**而非解析结果：解析逻辑升级后可重新解析，格式变化时便于排查。
- 代价：`body` 作为 block scalar 保存会重新缩进，非字节级原样。若需字节级比对，另出调试副本，**调试副本不得作为数据源**。

#### 网页预览与手动替换缓存

- 管理页面可通过 `/upstreams/{id}/config` 查看与编辑缓存原文；仅查看不触发上游请求，无成功缓存时允许粘贴配置使用。
- 手动内容复用同步时的解析与假节点过滤校验，过滤后必须有有效节点；校验失败保留旧缓存与表单输入。
- 手动保存**替换当前缓存**，不是独立覆盖层；后续新同步成功可以覆盖，失败继续使用已有缓存。
- 手动保存清除 HTTP ETag / Last-Modified、旧强制接受标记；流量与到期仅从新正文提取。`meta.manual_saved_at` 标识手动来源，网络同步成功清除，失败保留。
- 表单带业务 revision 与缓存内容指纹，提交时重校验；在途同步若发现缓存已变则丢弃旧结果，防止覆盖手动保存。
- 配置正文含代理密钥，页面仅管理认证可见、HTML 自动转义、响应 `Cache-Control: no-store`；不将正文写入日志。导出仍只提取节点，不继承上游分组、规则或通用设置。

### 4.2 刷新时机与并发控制

| 时机 | 行为 |
| --- | --- |
| 进程启动 | **只刷新到期项**：无缓存、正文为空，或 `last_success_at + refresh_interval` 已过；未到期不发起请求 |
| 定期 | 按 `refresh_interval` 周期检查，同样只刷新到期项 |
| 手动 | 上游页「立即刷新」，支持单个/全部（强制，不看是否到期） |
| 导出时 | 缓存缺失才同步拉取；已有缓存直接用 |

> 订阅 `expire` 已过时也跳过自动刷新，避免对已失效订阅反复请求；手动刷新仍可强制。

- 带 `etag` / `last-modified` 条件请求。**收到 304 时正文沿用，但派生数据（`node_count`、
  `source_format`、`skipped_links`、过滤统计）必须用当前解析器重新计算**——否则解析逻辑升级后，
  只要上游内容不变就一直是 304，这些统计会永远停在旧值。
- **并发上限默认 4**，避免被限流。
- **请求必须带 Clash 兼容的 User-Agent**（如 `mihomo/1.x`）。机场常按 UA 返回不同格式，UA 不对会拿到 base64 分享链接列表，直接判为解析失败。
- 刷新失败**不得清除**已有缓存。

#### 并发与竞态防护（必做）

- **同一上游的重复刷新必须合并**：已有 in-flight 请求时，后续请求复用同一 future，不重复发起。
- **写缓存前必须重新校验上游仍存在且 URL 未变**。否则：
  - 用户改了上游地址，旧请求返回后会用**旧地址的数据**覆盖新缓存；
  - 用户删除了上游，旧请求返回后会**重新生成已删除的缓存**。
  - 校验不通过 → 丢弃本次结果，不写盘。

### 4.3 响应校验与提交（validate-then-commit）

**HTTP 200 不代表内容可用。** 上游可能返回登录页、错误提示、损坏 YAML，或只剩流量信息的节点列表。**只有通过全部校验的响应才能替换有效缓存。**

| # | 检查 | 不通过归类 |
| --- | --- | --- |
| 1 | HTTP 状态为 2xx | `http_status` |
| 2 | 响应体非空 | `parse` |
| 3 | 能被 YAML 解析且顶层为 mapping | `parse` |
| 4 | 含 `proxies` 键且为 sequence | `structure` |
| 5 | 每个节点至少含字符串 `name` | `structure` |
| 6 | 过滤假节点后有效节点数 > 0 | `empty`（见 §4.4） |

**响应格式与回退路径**：先按 Clash YAML 解析（检查 3–5）；失败则回退到**分享链接列表**——
机场在 UA 不被识别时最常返回这种格式，可能是明文，也可能整体 base64 编码。

- 支持 `ss`（含 SIP002 与旧式 base64、obfs / v2ray-plugin）/ `vmess` / `vless`（含 reality）/
  `trojan` / `hysteria2`（`hy2`）/ `tuic`。
- 其余协议（`ssr`、`hysteria` v1、未知 scheme）**跳过并计数**，不生成字段缺失的节点。
  - 理由：生成一个连不上的节点，用户要到客户端里连接失败才发现；跳过并明确报告更安全。
- 两条路径都失败才判 `structure`，错误信息同时说明两次尝试的原因。
- 识别到的格式与跳过条数记入缓存的 `source_format` / `skipped_links`，在上游页展示。

> 分享链接来源是**合成**节点（`links.rs` 构造 Clash 字段），与 §3.8 的「原样透传」是两条
> 不同路径：YAML 来源的节点原样透传，链接来源的节点由本服务转换生成。

**内容字段与状态字段必须分开处理**：

| 类别 | 字段 | 更新时机 |
| --- | --- | --- |
| **内容字段** | `body`、`last_success_at`、`expire`、`total`/`upload`/`download`、`etag`、`node_count` | **全有或全无**，仅在全部校验通过时一起更新 |
| **状态字段** | `last_attempt_at`、`last_error`、`last_error_kind`、`status` | 每次刷新都更新 |

- 校验失败时，**即使响应头的 `subscription-userinfo` 可信，也不写入** `expire`/流量。否则会出现「正文是旧的、到期信息是新的」错配。

### 4.4 失败与降级模型

| 情形 | 内容字段 | 状态 | 导出行为 |
| --- | --- | --- | --- |
| 校验通过 | 全部更新 | `ok` | 使用新数据 |
| 网络失败 / 超时 | 保留 | `stale`，kind=`network` | 使用旧缓存 |
| HTTP 非 2xx | 保留 | `stale`，kind=`http_status` | 使用旧缓存 |
| 解析或结构校验失败 | 保留 | `stale`，kind=`parse`/`structure` | 使用旧缓存 |
| 合法但**零节点**，旧缓存非空 | **保留** | `empty` | 使用旧缓存 + 强告警 |
| 合法但**零节点**，旧缓存为空 | 写入 | `empty` | 使用新数据（空）+ 告警 |
| 无任何旧缓存 | — | `failed` | 贡献 0 节点，告警 |
| 全部上游无可用数据 | — | — | 见 §6.3 |

**零节点策略**：旧缓存有节点而新响应零节点时**不替换**。更可能是 token 失效、机场故障或订阅被清空。上游页提供「**强制接受本次响应**」按钮（二次确认，记录 `forced_at`）。

### 4.5 导出时的解析失败

即使写入时校验过，导出时仍可能解析失败（解析逻辑升级、缓存被改坏、旧格式遗留）。

**管线第③步的解析失败必须纳入 §4.4 的同一降级模型**：

- 某上游缓存解析失败 → 该上游**贡献 0 节点**，标记失败并告警，**不中断**其余上游。
- 缓存信封损坏、完全无法读取 → 同样处理。

### 4.6 订阅信息与假节点过滤

机场常在节点列表里塞信息条目（`剩余流量：123.45 GB`、`套餐到期：2025-12-31`、`官网：...`）。它们是合法节点结构，**透传设计会把它们当正常节点收进来**。

1. **提取信息**：优先解析响应头 `subscription-userinfo`（`upload`/`download`/`total`/`expire`）；回退解析信息节点名称文本。
2. **过滤**：这些节点**不得进入导出的 `proxies`**。

识别采用**关键词黑名单**，默认预置：`剩余流量`、`套餐到期`、`到期`、`过期`、`官网`、`订阅`、`流量重置`、`重置时间`。可在通用设置页编辑。

- 过滤**只针对导出**，不影响上游页的信息展示。
- ⚠️ 可能误杀真实节点（名字里恰好含"订阅"）。预览页需显示**被过滤的节点清单**。

### 4.7 上游状态的四个维度（必须分开展示）

不要用单一状态混合表达四件不同的事：

| 维度 | 取值 | 含义 |
| --- | --- | --- |
| **刷新状态** | `ok` / `stale` / `empty` / `failed` | 最近一次刷新是否成功 |
| **数据年龄** | `last_success_at` 距今 | 缓存内容有多旧 |
| **套餐到期** | `expire` 与剩余天数 | 订阅是否到期 |
| **流量状态** | `upload` / `download` / `total` | 用量与剩余 |

**必须解决的矛盾**：原表述中「已过期仍可导出」与「到期时间决定能否使用」自相矛盾。正确定义为：

- **刷新状态**决定数据**是否可用**（有缓存即可用，无论新鲜度）。
- **套餐到期**与**数据年龄**只影响**告警级别**，不阻断导出。
- **刷新成功但订阅已过期**（如机场返回了最后一版节点）：状态为 `ok`，同时显示「套餐已过期」标记，两者独立呈现。

## 5. 页面与路由

| 方法 | 路径 | 说明 | 需认证 |
| --- | --- | --- | --- |
| GET | `/` | 方案列表，展示订阅 URL、上游健康摘要、全部告警 | 是 |
| GET/POST | `/upstreams` | 上游管理：增删改、四维状态、流量与到期、手动刷新 | 是 |
| GET/POST | `/nodes` | 手动节点：粘贴分享链接、查看解析结果、删除 | 是 |
| GET/POST | `/upstreams/{id}/config` | 预览缓存原文、编辑或手动填入并校验保存 | 是 |
| POST | `/upstreams/{id}/refresh` | 立即刷新单个上游 | 是 |
| POST | `/upstreams/refresh-all` | 立即刷新全部上游 | 是 |
| POST | `/upstreams/{id}/accept` | 强制接受零节点响应（二次确认） | 是 |
| GET/POST | `/groups` | 分组管理：增删改、匹配规则（含 `mode`）、手动项 | 是 |
| GET/POST | `/rules` | 规则管理：分段、增删改、排序、批量粘贴导入 | 是 |
| GET/POST | `/aliases` | 地区别名表 | 是 |
| GET/POST | `/general` | 通用设置：Clash 客户端配置 + `region_source` + 过滤黑名单 | 是 |
| POST | `/general/geoip/settings` | 保存 GeoIP 自动更新设置（开关 / 间隔 / 自定义地址） | 是 |
| POST | `/general/geoip/download` | 手动下载并安装一次 GeoIP 库（失败保留旧库） | 是 |
| GET/POST | `/profiles/new` | 新建方案 | 是 |
| GET/POST | `/profiles/{slug}` | 导出配置界面：勾选上游 + 分组 + 规则，预览并保存 | 是 |
| POST | `/profiles/{slug}/preview` | 渲染预览（不落盘） | 是 |
| POST | `/profiles/{slug}/delete` | 删除方案 | 是 |
| GET | `/sub/{slug}` | 输出该方案的实际配置文件；`?download=1` 以附件下载，否则内联展示 | 是 |
| GET | `/healthz` | 健康检查 | 否 |

- 每个管理页**独立、互不嵌套**。
- **新增类表单一律用弹窗**：入口是普通链接（如 `?new=1`），服务端按需带 `open` 渲染
  `<dialog class="modal">`，无 JS 时退化为页内卡片（§12 v13/v17）。不要把它们恢复成列表上方的常驻卡片。
- `/sub/{slug}` 响应为合法 Clash YAML，带 `Content-Type: text/yaml` 与合适的 `Content-Disposition`。
- 分组/规则未配置时**省略**对应键，不要输出 `proxy-groups: []` 或 `rules: []`。

**导出预览必须展示**：
- 每个分组的成员数与**每个节点的入组原因**（命中哪个地区/关键词/手动项）
- 地区判定**冲突清单**与**歧义清单**
- 被过滤的假节点清单
- 陈旧/已过期/空响应上游清单
- 节点名冲突、分组名冲突
- 规则分段预览与顺序检查结果
- 全部校验错误与警告

### 5.1 通用设置页：服务设置与客户端配置必须分开

- **服务设置**（`config.toml`，网页不管理）：`listen`、`password`、`refresh_interval`、`fetch_concurrency`、`proxy`。
- **客户端配置**（`data/store.yaml` 的 `general`）：Clash 的 `mixed-port`、`mode`、`dns` 等，会**进入导出结果**。
- **服务行为设置**（`data/store.yaml` 的 `settings`）：`region_source`、假节点过滤、`geoip`
  （自动更新开关 / 间隔 / 自定义地址）、`base_url`（订阅链接前缀）。可在网页修改，但**绝不进入导出结果**。

**二者不得混存**，否则服务自身配置可能被写进 Clash 输出。

**表单与高级 YAML 的优先级**：高级 YAML 编辑框**不得包含**已在结构化表单中暴露的键；出现重复键 → **报错**，不做静默覆盖。这样同一份配置只有一处权威来源。

## 6. 导出管线

```
读取快照（§8.2）→ 读取方案
  → ① 引用完整性校验（§6.1）
  → ② 取所选上游的缓存（缺失才同步拉取；失败按 §4.4 降级）
  → ③ 解析各上游缓存 → 过滤假节点（§4.6）→ 按 name 去重
       单个上游解析失败 → 贡献 0 节点并告警，不中断（§4.5）
  → ④ 地区判定（§3.5）：IO 层准备数据 → 纯函数判定
  → ⑤ 对每个分组求值：manual_groups ⊕ manual_nodes ⊕ auto_matched
  → ⑥ 分组拓扑排序（被引用者先输出，同层保持库内顺序）
  → ⑦ 规则分段拼装（§6.2）
  → ⑧ 拼装 general + proxies + proxy-groups + rules
  → ⑨ 校验最终配置的外部依赖（§6.1 第 12、13 条）
  → ⑩ 序列化为 YAML 输出
```

- 输出键顺序固定（`general` 键 → `proxies` → `proxy-groups` → `rules`）。
- 步骤 ①⑤⑥⑦⑨ 失败：**整体失败**并指明具体对象，不输出半成品。
- 步骤 ②③ 失败：按 §4.4 / §4.5 降级，不算导出失败（除非触发 §6.3）。
- 步骤 ④⑤⑦ 的核心逻辑必须是**纯函数**（§3.5 分层要求）。
- **一次导出全程使用同一份数据快照**（§8.2），不得在中途重新读取。

### 6.1 校验清单

| # | 检查项 | 级别 |
| --- | --- | --- |
| 1 | 节点名全局唯一（同名跨上游取先者） | 冲突提示 |
| 2 | 分组引用的其他分组名存在，且依赖闭包无环且完整（§3.3.2） | **错误** |
| 3 | 分组**最终成员**为空 | **错误** |
| 4 | 分组最终成员非空但自动匹配为空 | 警告 |
| 5 | 分组名与内置策略重名（`DIRECT`/`REJECT`/`PASS`/`GLOBAL`） | **错误** |
| 6 | 分组名与某个节点名重名（`proxies` 引用歧义） | **错误** |
| 7 | （v15 起不再适用）原「应用的 `target_group` 必须在所选分组内」——应用模块已移除；**编号保留**以稳定交叉引用 | — |
| 8 | 地区别名无冲突（同一别名不得挂在两个地区下） | **错误** |
| 9 | 规则目标存在（分组名 / `DIRECT` / `REJECT` / 内置策略）。**仅对结构化规则生效** | 目标不存在 / 未选中 / 被省略：**跳过该规则并警告**（v37） |
| 10 | 存在明确的最终兜底策略（`final` 段有且仅有一条 `MATCH`） | **错误**（产品要求，见下） |
| 11 | `general` 不含 `proxies` / `proxy-groups` / `rules` 键 | **错误** |
| 12 | 所有 `dialer-proxy` 引用的节点都在最终 `proxies` 中 | **错误** |
| 13 | 规则中出现的 `RULE-SET` 名称在 `general.rule-providers` 中有定义 | **错误** |
| 14 | 规则分段合法：非 `final` 段无 `MATCH`；`final` 之后无规则 | **错误** |
| 15 | 存在多个兜底规则或明显不可达的后续规则 | 警告 |
| 16 | 分组为空 | 方案**有兜底**时：警告 + 从输出省略；**无兜底**时：**错误** |
| 17 | 被省略的分组仍作为规则目标 / 仍被其他分组 `manual_groups` 引用 | 规则目标：**跳过规则并警告**；分组引用：移除并**警告** |

> **第 10 条的性质（已由源码核实修正）**：缺少 `MATCH` **不会**导致配置非法。Mihomo 的 `match()` 在所有规则未命中时 `return proxies["DIRECT"], nil, nil` —— 即**静默回退到 DIRECT**。
>
> 因此这是**本产品的防误配置要求**，不是内核要求。默认 DIRECT 会绕过用户的隐私与路由意图，属于必须拦截的静默行为变更。级别定为**错误**；如将来要放宽，必须由用户显式确认。
>
> **v37 例外**：如果 `final` 段的 `MATCH` 目标分组不存在/未选中/被省略，该规则会被跳过，
> 系统**显式补一条 `MATCH,DIRECT`** 并在预览中警告。这是唯一允许的自动兜底，
> 目的是让其余配置仍可导出使用；不会把普通规则静默改成 DIRECT。

> **第 12 条的原因**：节点可含 `dialer-proxy` 字段，按名字引用另一个节点。只抽取 `proxies` 而引用对象不在最终列表中时，配置会加载失败或行为异常。必须做引用闭合检查。

> **第 13 条的原因**：`RULE-SET` 依赖 `general.rule-providers` 中的定义。支持该规则类型就必须校验依赖完整。

### 6.2 规则顺序与分段

**「越具体的规则越该排在前面」不能靠默认顺序保证。** 一条宽泛的 `GEOSITE` / `RULE-SET` 规则可能抢先覆盖用户明确设置的拒绝或直连规则，所以分段由用户显式指定，而不是按书写顺序。

规则分为三段，导出顺序固定：

```
① override  → 用户强制覆盖（明确的 REJECT / 直连白名单）
② normal    → 普通路由规则（按库内顺序）
③ final     → 最终兜底（有且仅有一条 MATCH）
```

- 每条规则有 `section: override | normal | final` 字段。
- **必须检查**：提前出现的 `MATCH`、多个兜底、`final` 之后的规则、明显不可达的后续规则（如一条全匹配规则之后的任何规则）。

### 6.3 空分组导致整体失败时的策略

**「单个上游失败不影响整体」存在例外。** 例如：美国上游首次拉取失败 → 美国分组为空 → 目标为「美国节点」的规则与引用它的分组全部落空 → **整份配置导出失败，即使日本上游完全正常**。

保留旧缓存只能解决一部分故障。方案级提供 `on_failure` 策略：

| 取值 | 行为 |
| --- | --- |
| `fail`（默认） | 严格失败，错误信息指明「上游 X 无数据 → 分组 Y 为空 → 规则 / 分组 Z 依赖 Y」 |
| `fallback_group` | 分组级配置用户**显式指定**的备用分组 |
| `last_good` | 返回最近一次成功生成的完整配置 |

- **绝对不要自动降级到 `DIRECT`**——会静默改变用户的隐私与路由意图。
- **v37 例外**：规则目标分组失效导致 `final` 的 `MATCH` 被跳过时，补显式 `MATCH,DIRECT` 并警告；
  普通规则只跳过，不改目标。
- 采用 `last_good` 时，**必须在响应头与页面显示生成时间与回退原因**，避免用户误以为最新修改已生效。缓存于 `data/cache/rendered/<slug>.yaml` + meta。

## 7. 认证与密钥

- 单人使用，**访问密码从配置文件读取**，通过 HTTP Basic Auth 校验。
- Basic Auth 的用户名**固定为 `admin`**（`auth::ADMIN_USER`，大小写敏感），不开放配置；
  用户名与密码**都**参与校验，任一不匹配即拒绝。
- 密码**禁止硬编码**在源码、测试、模板或示例中；示例一律用占位符（`changeme`）。
- `config.toml`、`data/` 属本地私密数据，**不得提交版本库**。
- 日志与错误信息不得回显密码、上游 URL 中的 token；输出 URL 时脱敏。
- 默认仅监听 `127.0.0.1`；**Basic Auth 在明文 HTTP 下会泄露凭据**，对外暴露必须置于 HTTPS 之后。

### 7.1 CSRF 防护（修正）

**原表述有误**：在纯 Basic Auth 下，浏览器会自动附带凭据，`SameSite=Strict` cookie **无法**阻止伪造请求——服务端只认 `Authorization` 头，缺少 cookie 并不影响操作，设置该 cookie 提供不了所声称的保护。

正确要求：

- 所有**修改类操作**（POST）必须校验 **CSRF token**：服务端签发、绑定会话、表单隐藏字段回传、提交时比对。
- 叠加 **`Origin` / `Referer` 来源检查**，不匹配则拒绝。
- 管理会话使用 `SameSite=Strict` cookie 作为**纵深防御**，而非唯一手段。

### 7.2 订阅令牌

实现前必须确定，因为它直接决定客户端能否拉取。若采用，严格限定：

- **只能读取指定方案的订阅**，不能访问任何管理功能。
- **可独立撤销与重置**，不影响管理密码。
- 在日志、错误页、访问记录中**脱敏**。
- 与 `config.toml` 的 `password` **分开存储**（password 是密钥，令牌是业务数据，放 `data/store.yaml`）。

## 8. 配置、存储与一致性

### 8.1 单一数据文件

```
config.toml                    # 服务设置（含密码，手工编辑，网页不管理）
data/
  store.yaml                   # 全部业务数据：upstreams/manual_nodes/groups/rules/aliases/general/profiles
  cache/<id>.yaml              # 上游缓存信封
  cache/rendered/<slug>.yaml   # last_good 渲染结果
  geoip/*.mmdb                 # 离线 GeoIP 库（不提交版本库）
  geoip/geoip-auto.mmdb        # 自动下载安装的库（固定文件名）
  geoip/status.yaml            # 下载状态：最近尝试/成功、错误、ETag、来源
  backups/                     # store.yaml 历史快照
```

**为什么合并为单一 `store.yaml`**：分组重命名会同时改动分组、规则和多个方案。若分散在多个文件，即使每个文件都原子写，写到一半进程退出仍会留下**新旧名称混杂**的数据，且备份也会跨文件不一致。

单一文件后：**一次 `write temp → rename` = 全部业务数据的一致提交**，备份天然是一致快照，恢复也不会出现互不兼容的版本组合。

### 8.2 快照、版本与并发

- **内存快照**：`store.yaml` 加载后成为不可变 `Arc<Snapshot>`；所有读操作（含导出）持有快照引用，全程一致。
- **写入**：所有修改走单一写锁，在锁内完成「读当前快照 → 计算新状态 → 校验新状态有效 → 原子写盘 → 替换快照」。
- **乐观版本校验**：快照带 `revision`；表单提交时回传 `revision`，不匹配则**拒绝并提示"数据已被其他操作修改，请刷新后重试"**。这防止丢失更新（两个操作基于同一旧版本分别计算，后者覆盖前者）。
- **锁粒度**：网络请求与 GeoIP 查询必须在**锁外**执行，只在提交写入的瞬间持锁。写成「持写锁 → 发 HTTP → 写文件」会让一次慢请求阻塞所有导出。
- **首次运行**：`data/` 不存在时自动创建，植入默认地区别名表（28 个常见地区）、
  23 个默认分组（手动切换 / 自动选择 / 香港 / 新加坡 / 日本 / 美国 / 台湾 / 韩国 / 英国 / 德国 /
  Google服务 / OpenAI / 奈飞 / **Steam商店 / Steam下载** / Telegram / Disney+ / Spotify / TikTok /
  GitHub / 广告拦截 / 国内网站 / 漏网之鱼）、
  89 条默认路由规则（55 条 override：23 广告 + 27 国内 + 5 Steam下载；33 条服务 normal；
  `MATCH → 自动选择`）与默认过滤黑名单，
  不要让进程直接报错退出。**已存在 `store.yaml` 时不注入、不覆盖**默认分组与规则。
- `slug` / `id` 用作 URL 片段与文件名，**必须校验 `^[a-z0-9-]+$`**，防止 `../` 路径穿越。

### 8.3 备份与恢复

- 每次写 `store.yaml` 前，将旧文件复制到 `data/backups/store.<时间戳>.yaml`，保留最近 N 份（默认 20）。
- **必须提供恢复流程**（不能只备份不恢复）：
  1. 列出备份（时间、大小、摘要）；
  2. **恢复前预览**：展示与当前状态的差异（新增/删除/修改的对象）；
  3. 恢复前先备份当前状态，再原子替换 `store.yaml`；
  4. 提供回滚（即恢复上一个自动备份）。
- 因 `data/` 含 token 不得入库，这是唯一的版本历史来源。

## 9. 仓库结构（已实现）

> 与 v7 的规划略有出入，以下为**实际结构**；`src/match.rs` 通过 `#[path]` 声明为
> `matcher` 模块（`match` 是 Rust 关键字）。`rules/*` 合并为单文件 `rules.rs`。

```
Cargo.toml
src/main.rs             # 入口：日志、配置、后台刷新任务、HTTP 服务
src/config.rs           # 服务设置（config.toml）
src/store/mod.rs        # 单一 store.yaml：快照 / revision / 原子写 / 备份恢复
src/store/model.rs      # 业务数据模型
src/store/migrate.rs    # 数据格式版本迁移
src/validate.rs         # 结构校验 + 导出期 Report（§6.1）
src/cascade.rs          # 引用闭包、级联重命名/删除、分组 upsert
src/rules.rs            # 规则解析/拼装/分段（对齐 Mihomo ParseRulePayload）
src/region.rs           # 纯函数：地区判定、歧义与冲突
src/match.rs            # 纯函数：分组匹配求值（声明为 matcher 模块）
src/subinfo.rs          # subscription-userinfo 解析、假节点过滤
src/links.rs            # 分享链接（ss/vmess/vless/trojan/hysteria2/tuic/socks5）→ Clash 节点
src/fetch.rs            # 拉取与响应校验（§4.3 六项检查）
src/cache.rs            # 缓存信封：单文件、内容字段与状态字段分离
src/refresh.rs          # 定期刷新、in-flight 合并、写前校验
src/geoip.rs            # IO 层：DNS 解析 + MMDB 查询 + 热加载
src/geoip_update.rs     # GeoIP 库下载 / 校验 / 原子安装 / 定期更新
src/render.rs           # 导出管线：校验 / 拓扑 / 分段 / 外部依赖
src/lastgood.rs         # last_good 渲染缓存
src/pipeline.rs         # AppState 与导出粘合
src/auth.rs             # Basic Auth / CSRF / 订阅令牌
src/web/mod.rs          # 路由、认证中间件、表单解析
src/web/pages.rs        # 各管理页 handler
src/storage.rs          # 存储抽象：get/put/delete/list（native 文件 / wasm DO+R2）
src/storage/fs_impl.rs  # native 实现：临时文件 + rename 原子提交
src/storage/wasm_impl.rs# wasm 实现：DO「Core」/obj/* + R2
src/config_wasm.rs      # Workers 配置：bindings → Config
src/refresh_wasm.rs     # Workers 刷新：worker::Fetch + 共享缓存信封
src/geoip_wasm.rs       # Workers GeoIP：R2 加载 mmdb，无 DNS 降级
src/geoip_update_wasm.rs# Workers GeoIP 下载：Fetch + R2
src/proxy_wasm.rs       # Workers 代理设置：仅校验/脱敏（Fetch 不支持代理）
src/workers/            # Worker 入口 + DO（store.rs）+ 兼容层（cache/upstream/export/auth）
wrangler.jsonc          # Cloudflare Workers 部署配置（DO/R2 绑定）
docs/workers-migration.md # Workers 迁移设计、阶段与验证记录
templates/              # askama 模板（base.html 为页面骨架，error.html 为统一错误页）
static/style.css        # 样式入口：只引用设计变量，不写字面色值
static/ui-tokens.css    # 设计变量唯一来源（颜色/字号/间距/圆角/阴影/层级/动效）
static/app.js           # 渐进增强：确认弹窗、表单弹窗、提交中的按钮状态、错误页返回
docs/ui-design-spec.md  # UI 设计规范（含落地情况与已知偏差）
docs/release-design.md  # 便携包 / Docker 发布设计
Dockerfile              # Docker 镜像（debian-slim、非 root、/data volume、healthcheck）
.dockerignore
docker-compose.example.yml
scripts/package-windows.ps1  # Windows 便携包
scripts/package-linux.sh     # Linux 便携包
config.example.toml     # 配置示例（占位符，可提交）
config.smoke.toml       # 本地冒烟测试用（含测试密码，非真实密钥）
fixtures/sub.yaml       # 冒烟测试的假订阅（含信息节点，用于验证过滤）
```

### 9.1 本地运行

```bash
cp config.example.toml config.toml   # 修改 password
cargo run -- config.toml             # 默认监听 127.0.0.1:8080
```

GeoIP 库是可选的：在通用设置页点「立即下载 / 更新」即可自动安装
（默认 DB-IP Lite Country），也可以把任意 `.mmdb` 放进 `data/geoip/`。缺失时自动降级为
「仅按节点名判定地区」并在首页提示，不影响导出。

## 10. 开发约定

- 依赖精简，新增 crate 前说明理由。推荐：`serde` + `toml` + `serde_yaml_ng` + `reqwest` + `askama` + `tower-http` + `maxminddb`。
  - ⚠️ **不要选用 `serde_yaml`**（已停止维护）。
  - `flate2`：DB-IP 以 `.mmdb.gz` 分发，reqwest 的 gzip 特性只处理 Content-Encoding，
    不处理 `.gz` 文件体，因此需要显式解压（纯 Rust 后端，不引入 C 依赖）。
- 异步优先：网络与文件 IO 走 `tokio`，不在 async 上下文做阻塞调用。
- 拉取多个上游**并发执行**（上限 `fetch_concurrency`）。
- **纯函数边界**：`region.rs`（判定）、`match.rs`、`rules/*`、`render.rs` 不碰网络与文件。
- 单测必须覆盖的边界：
  - 空匹配、`mode` 的 any/all、`exclude` 优先级
  - `US` / `Russia` / `US01` 的别名边界
  - 别名冲突、地区多命中歧义、地区判定冲突
  - 依赖闭包缺失与环引用
  - 空分组（最终成员口径）
  - **规则 params 位置**：`IP-CIDR,1.2.3.0/24,TARGET,no-resolve` 往返
  - 逻辑规则 raw 模式
  - 假节点误杀
  - 响应校验的六种失败归类
  - `revision` 冲突
- 错误处理：`Result` 向上传播，请求处理路径禁止 `unwrap()` / `expect()`。
- 模板默认自动转义，禁止用 `|safe` 渲染用户输入。
- `cargo fmt` 与 `cargo clippy --all-targets -- -D warnings` 应通过。

```bash
cargo build
cargo run
cargo test
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

- 提交前自查：是否违反 §1 非目标或 §2 硬约束？是否把密码、token、真实订阅地址或 GeoIP 库文件写进了仓库？

## 11. 代理工作守则

- 需求含糊时**先提问再动手**，不要自行扩大范围。
- 只改与任务相关的文件；不要顺手重构无关代码。
- 新增页面必须遵守 §5 的"一页一职责"划分。
- **不要重新引入「应用」这一层抽象**（v15 已移除，见 §3.6）——域名规则直接写在路由规则里，并在规则上显式指定目标分组。
- **不要把规则目标追加到末尾**——必须按 §3.7 的位置拼装。
- **不要自动降级到 DIRECT**（§6.3）。唯一例外是 v37：`final` 的 `MATCH` 目标分组失效时，
  跳过该规则并**显式补 `MATCH,DIRECT`**，同时在预览中警告；普通规则不会被改成 DIRECT。
- **不要给分组加"手工节点列表"这种退化设计**——手工项只是补充（§3.4）。
- 用户以中文交流，文档与注释默认中文；标识符与日志字段用英文。
- 修改本文件时保持章节编号稳定。

## 12. 变更记录

- **v48**：新增 **Cloudflare Workers** 部署目标，与 native 共用同一份业务代码。**未改 FORMAT_VERSION。**
  - 存储抽象 `src/storage.rs`（`get/put/delete/list`）：native = 文件系统（临时文件 + rename 原子提交）；
    wasm = DO「Core」的 `/obj/*`（小对象，单写者串行化）+ R2（缓存信封与 mmdb）。
    `store`/`cache`/`lastgood` 改为异步接口，`StoreHandle` 持 `Arc<Storage>` 而非 `PathBuf`。
  - 平台模块按 `cfg` 同名替换：`config_wasm`（bindings/secrets）、`refresh_wasm`（`worker::Fetch`）、
    `geoip_wasm`（R2 加载 mmdb；Workers 无 DNS，域名节点只按名字判定）、
    `geoip_update_wasm`（Fetch 下载 + R2 安装）、`proxy_wasm`（仅校验/脱敏，Fetch 不支持出口代理）。
  - `pipeline`/`web`/`lastgood` 解除 native 门控：管理页、表单、CSRF、备份恢复全部跑在 Workers 上。
  - `src/workers/`：`#[event(fetch)]` 入口 + DO「Core」+ 兼容层；`wrangler.jsonc` 配置 DO/R2 绑定。
  - 关键约束：Storage 的 wasm 方法用 `SendFuture` 包装（worker 绑定 `JsFuture` 是 `!Send`，
    而 axum handler 要求 `Send`）；CSRF 密钥必须稳定（优先 `SUBFORGE_CSRF_SECRET`，
    否则由管理密码派生），否则多 isolate 下表单校验会随机失败。
  - 验证：**205 项 Rust 测试**、双目标 clippy/fmt 通过；线上
    `https://subforge-rs.luhuadream.workers.dev` 九页 200、表单写入 303、`/sub/{slug}` 导出正常。
  - 已知限制（Workers）：出口代理设置不生效（`worker::Fetch` 不支持）、GeoIP 无 DNS 解析
    （域名节点只按名字判定）、`wrangler r2 object` CLI 需加 `--remote` 才能看到线上对象。
  - 定时任务：`#[event(scheduled)]` + `wrangler.jsonc` 的 `triggers.crons`（`15 */6 * * *`），
    只刷新到期上游；GeoIP 按 `settings.geoip.interval_days` 判断是否更新。
  - 整库迁移：Workers 专属 `GET/POST /admin/store`（Basic Auth），用于导入 native 的
    `store.yaml`；共用 web 路由不提供该能力。`src/workers/` 只保留 DO 定义与 Fetch 拉取，
    早期另写的导出/缓存/认证兼容层已删除。

- **v47**：出口代理改为**网页可配**（通用设置页），并修掉一个复选框回显缺陷。**未改 FORMAT_VERSION。**
  - `AppSettings.proxy`（`ProxySettings { enabled, url, configured }`）：纯新增可选字段。
    `configured` 区分「旧数据/首次未设置」与「用户在网页里主动关闭」，后者不应回退到 `config.toml`。
  - 通用设置页新增「上游访问代理」卡片：勾选启用 + 地址；启用时地址为空或非法会回填报错，
    未启用时允许预填。保存后调用 `Refresher::set_proxy` / `GeoipUpdater::set_proxy`
    重建 HTTP 客户端，**下一次拉取立即生效**，无需重启。
  - `config.toml` 的 `proxy` 降级为「网页没设置过时的默认值」；启动日志注明来源。
  - 修复 `FormEcho::checked`：GET 打开页面时 `FormEcho` 为空，原实现因作用域相等直接返回
    `false`，把「已启用」的复选框渲染成未勾选（保存前看到的状态是错的）。现在没有提交值时
    返回 fallback，只有真正提交过且缺该键才表示用户取消勾选。
  - 验证：203 项 Rust 测试、Clippy、fmt 通过；实际 HTTP 验证启用/取消/非法地址回填、
    即时生效与页面回显，并复测上游刷新写入正常。未做真实代理链路联调（本机无代理端口）。

- **v46**：新增可选的出口代理 `config.toml` 的 `proxy`（默认直连）。**未改数据格式。**
  - 作用范围：上游拉取与 GeoIP 库下载。只支持 `http(s)://` 与 `socks5(h)://`，必须写端口。
  - 不实现任何 Clash 节点协议（§1 非目标）：拉取的 `ss://` / `vmess://` 等节点**不能**直接当代理，
    需由客户端内核（Mihomo 等）跑起来后，把其 `mixed-port` / `socks-port` 填进 `proxy`。
  - 代理配置非法或写错时**只告警并回落直连**，不阻止启动；代理地址中的账号密码在日志脱敏。
  - 代理不可用时沿用既有降级：失败保留旧缓存，不清空已有节点。
  - 验证：202 项 Rust 测试、Clippy、fmt 通过；新增单测覆盖空值直连、常见代理 URL、非法协议、
    缺端口与凭据脱敏；未执行真实代理链路联调。

- **v45**：新增上游配置原文预览与手动替换缓存。业务 store 格式不变，缓存元数据新增向后兼容的可选 `manual_saved_at`。
  - 上游列表提供「预览 / 编辑配置」，无成功缓存时也可粘贴配置；校验失败保留输入与旧缓存。
  - 手动保存替换当前缓存，清除旧 HTTP 条件请求标记与响应头订阅信息，后续同步成功允许覆盖。
  - 配置页面沿用管理认证、CSRF 与来源检查；原文自动转义且禁止浏览器缓存。
  - 验证：197 项 Rust 测试、Clippy、fmt、构建通过；隔离 HTTP 验证手动填入/导出、失败保留、后续成功覆盖、旧表单重试拒绝及认证/CSRF/来源检查；Chrome 无 JS、320px 检查通过。测试使用进程级 `CARGO_PROFILE_DEV_DEBUG=0` 与工作区临时目录，未修改真实数据。未执行 Mihomo 内核加载验收。

- **v44**：新增 GitHub Actions 构建并发布 Docker 镜像到 GHCR。**未改数据格式。**
  - 新增 `.github/workflows/docker.yml`：在 `v*` tag 或手动触发时构建并推送
    `ghcr.io/lhdream/subforge-rs`（`0.1.0` / `0.1` / `latest`，linux/amd64）。
  - 镜像 digest：`sha256:2c8375793e91e43d0f56aade252f73277a184cfc58a072f5c9ae8e9a4644f6c3`。
  - README 增加 GHCR 拉取方式。
  - 说明：arm64 暂未构建，后续可用原生 arm runner 追加，避免 QEMU 下 Rust 构建过慢。

- **v43**：项目采用 **AGPL-3.0-only** 开源协议。**未改数据格式。**
  - 新增根目录 `LICENSE`（GNU Affero General Public License v3.0 完整文本）；
  - `Cargo.toml` 增加 `license = "AGPL-3.0-only"`；`Dockerfile` 的
    `org.opencontainers.image.licenses` 同步为 `AGPL-3.0-only`；
  - `README.md` 增加「许可」章节，说明网络提供服务时须提供对应源码（AGPL 第 13 条）；
  - `src/main.rs` 增加 SPDX 头。
  - **注意**：AGPL 具有传染性，修改后再分发或作为网络服务提供时必须同样开源。

- **v42**：新增便携包与 Docker 两种发布形态，发布包不携带密码，首次运行自动生成或由环境变量指定。**未改数据格式。**
  - **首次运行**：配置/数据/静态资源默认相对可执行文件目录解析；`SUBFORGE_PASSWORD` /
    `SUBFORGE_PASSWORD_FILE` 优先；缺失时生成 32 字符随机密码，写入 `config.toml`（0600）并
    只打印一次；`--init` 可只初始化后退出。
  - **环境变量**：`SUBFORGE_CONFIG` / `SUBFORGE_DATA_DIR` / `SUBFORGE_STATIC_DIR` /
    `SUBFORGE_LISTEN`；`data_dir` 相对路径相对配置文件所在目录解析。
  - **运维**：新增 `--version` / `--help` 与优雅停机（Ctrl-C / SIGTERM）。
  - **发布**：`Dockerfile`（debian-slim、非 root、`/data` volume、healthcheck）、
    `.dockerignore`、`docker-compose.example.yml`、`scripts/package-windows.ps1`、
    `scripts/package-linux.sh`；设计见 `docs/release-design.md`。
  - **验证**：`cargo test`（**187 通过**）/ clippy / fmt 通过；实际验证首次生成密码、二次启动复用、
    环境变量优先、相对 data_dir 解析、便携目录运行。

- **v41**：项目更名为 **SubForge.rs**（仓库名 `subforge-rs`）。**未改数据格式。**
  - **原因**：`SubForge` 在 GitHub 上已有多个同名项目，其中 `bitxwave/subforge` 也是订阅转换类工具，
    因此按约定加 `rs` 后缀，避免重名。
  - **改动**：`Cargo.toml` 包名、`Cargo.lock`、README 标题、AGENTS 项目名、GeoIP 下载 User-Agent、
    Basic Auth realm、页面标题与品牌名。
  - **不变**：数据格式、`data/` 布局、`/sub/{slug}` 等路由、配置项名称。
  - **验证**：`cargo test`（**180 通过**）/ clippy / fmt / `node --check` 通过；服务以
    `subforge-rs.exe` 启动并返回 200。

- **v40**：补齐方案依赖、备份恢复、订阅输出缓存与分组页性能四项。**未改数据格式。**
  - **方案依赖**：方案页分组勾选带 `data-group-deps`，脚本自动补全并高亮「依赖自动加入」；
    服务端保存/创建时用 `required_dependencies` 拒绝缺失闭包；分组页自动勾选缺失依赖并提示。
  - **备份恢复**：`/backups?preview=<name>` 展示当前与备份的差异（上游/分组/规则/别名/手动节点/方案/
    设置），确认后才恢复；恢复仍会先自动备份当前状态。
  - **订阅输出**：`/sub/{slug}` 增加 `ETag` 与 `Cache-Control: no-cache`，支持 `If-None-Match` 304；
    `AppState` 按 store revision + 上游缓存元数据 + GeoIP 库修改时间缓存渲染结果；`last_good`
    只在 `on_failure = last_good` 时写盘。
  - **分组页性能**：节点名列表按 revision + 缓存元数据缓存，并在 `spawn_blocking` 中解析，
    避免每次打开分组页同步解析所有上游缓存。
  - **验证**：`cargo test`（**180 通过**）/ clippy / fmt / `node --check` 通过；实际验证 ETag 200/304、
    备份预览、依赖缺失保存被拒绝。静态资源版本串升至 `20261020-4`。
  - **文档同步**：README「它解决什么问题」。

- **v39**：默认**应用/服务分组**内部统一增加 `自动选择`、`手动切换` 与 `DIRECT`。**未改数据格式。**
  - **范围**：`Google服务`、`OpenAI`、`奈飞`、`Steam商店`、`Telegram`、`Disney+`、`Spotify`、
    `TikTok`、`GitHub`；`Steam下载` 仍保持 `DIRECT` 单选项，广告拦截/国内网站不变。
  - **实现**：`manual_groups` 末尾追加 `自动选择`、`手动切换`；`manual_nodes` 追加 `DIRECT`
    （内置策略必须走 `manual_nodes`，不能当分组引用，否则依赖闭包会报错）。
  - **现有数据**：9 个服务分组已通过接口更新；两个方案无需改动（已包含 `手动切换` 及其依赖）。
  - **验证**：`cargo test`（**179 通过**）/ clippy / fmt 通过；`/sub/b64`、`/sub/item-3a7f33`
    均 200，服务组 `proxies` 末尾包含 `自动选择`、`手动切换`、`DIRECT`。
  - **文档同步**：README 快速开始。

- **v38**：默认 **Steam 分组拆成「Steam商店」（代理）与「Steam下载」（直连）**，对齐 ACL4SSR 等社区配置。
  **未改数据格式。**
  - **分组**：新增 `Steam商店`（与其他服务组共用地区选项，默认美国节点）与 `Steam下载`
    （`select` + 唯一选项 `DIRECT`）；默认分组 22 → **23**，顺序为 … 奈飞、Steam商店、Steam下载、Telegram …
  - **规则**：Steam 商店域名（`steampowered.com` / `steamcommunity.com` / `s.team` /
    `DOMAIN-KEYWORD,steam`）走 `Steam商店`；下载 CDN（`steamcontent.com` / `steamusercontent.com` /
    `steamchina.com` / `cm.steampowered.com` / `steamstatic.com.8686c.com`）在 **override** 段走
    `Steam下载`，优先于普通 Steam 规则。override 50 → **55**，normal 31 → **33**，总计 82 → **89**。
  - **现有数据**：已通过接口把 `Steam` 重命名为 `Steam商店`、新增 `Steam下载` 与 7 条规则，
    并更新两个方案的 groups/rules（保留 token）；分组顺序同步调整为「Steam商店 → Steam下载」。
  - **验证**：`cargo test`（**179 通过**）/ clippy / fmt 通过；两个方案 `/sub/*` 均返回 200，
    导出中包含 `Steam下载` 组（`proxies: [DIRECT]`）与 5 条下载 override 规则。
  - **来源参考**：ACL4SSR 的 `SteamCN.yaml` / `GameDownload.yaml` 与 `ACL4SSR_Online_Full.ini`。
  - **文档同步**：README 快速开始、本文件 §8.2。

- **v37**：导出时**跳过目标分组不存在/未选中的结构化规则**，保证其余配置可正常导出。**未改数据格式。**
  - **判定**：目标必须是内置策略，或最终 `proxy-groups` 中实际保留的分组；目标不存在、
    未被本方案选中、或因空成员被省略，都会跳过该规则并写入 `report.warn`。
  - **最终兜底**：若 `final` 段的 `MATCH` 因目标失效被跳过，补一条显式 `MATCH,DIRECT` 并警告；
    普通规则不会自动改目标，也不会静默降级。
  - **raw 规则**：仍不解析、不参与目标校验，原样输出（既有明确限制）。
  - **验证**：`cargo test`（**179 通过**）/ clippy / fmt 通过；新增单测覆盖普通规则跳过、
    最终兜底补 `MATCH,DIRECT`、正常规则保留。实际导出见交付说明。
  - **文档同步**：README、本文件 §6.1 第 9/17 条与第 10 条说明。

- **v36**：分组编辑页的「手动节点」增加**复选框选择器**。**未改数据格式。**
  - **UI**：输入框旁新增「选择节点」按钮（默认 `hidden`），点击打开共享弹窗；弹窗列出
    内置策略（`DIRECT`/`REJECT`/`PASS`/`GLOBAL`）+ 全部手动节点 + 上游缓存节点，支持搜索、
    全选/反选/清空，应用后把所选名字写回 `manual_nodes` 输入框。
  - **保留手填**：无 JS 时按钮不出现，原文本输入仍可用；已有但不在列表中的名称在应用时保留。
  - **数据来源**：`available_node_names` 读取 `snapshot.manual_nodes` 与各上游缓存；缓存解析失败
    或缺失的上游跳过，不阻止分组页打开。节点名去重后按字典序排列。
  - **验证**：`cargo test`（**178 通过**）/ clippy / fmt / `node --check` 通过；headless Chrome
    确认 24 个选择按钮显示、弹窗 199 个节点复选框可用。静态资源版本串升至 `20261020-3`。
  - **文档同步**：README「它解决什么问题」。

- **v35**：调整**默认分组顺序**（只影响首次初始化；已有数据由页面排序）。**未改数据格式。**
  - **顺序**：手动切换 → 自动选择 → 香港 → 新加坡 → 日本 → 美国 → 台湾 → 韩国 → 英国 → 德国 →
    Google服务 → OpenAI / 奈飞 / Steam / Telegram / Disney+ / Spotify / TikTok / GitHub →
    广告拦截 → 国内网站 → 漏网之鱼。
  - **成员顺序**：`自动选择` / `手动切换` 的地区选项随地区顺序调整；服务组仍以美国节点为默认，
    其余地区按 香港 / 新加坡 / 日本 / 台湾 / 韩国 / 英国 / 德国 排列，`自动选择` 兜底。
  - **现有数据**：通过 `/groups/reorder` 调整为同一顺序，自定义分组保留并排在应用组之后、
    `漏网之鱼` 之前。分组成员与规则不变。
  - **验证**：`cargo test` / clippy / fmt；实际页面顺序与导出见交付说明。

- **v34**：分组页新增**拖动排序**，与分组列表统一处理。**未改数据格式。**
  - **UI**：每个分组卡片（`<details>`）标题左侧有拖动手柄；拖动时按目标卡片的上/下半区插入。
    卡片展开后仍保留「↑ 上移 / ↓ 下移」按钮，作为无 JS / 键盘/精确微调的替代。
    不再单独维护一张排序表。
  - **保存**：拖动结束后由脚本把当前完整顺序写入隐藏表单 `POST /groups/reorder`（重复键 `order`），
    服务端 `cascade::reorder_groups` 校验「不重不漏」后再替换 `Store.groups`。
    上移/下移仍走 `POST /groups/move`。
  - **语义**：只调整库内顺序；导出仍先做依赖拓扑排序，因此该顺序只影响**同层分组**与页面展示。
  - **渐进增强**：拖动手柄默认 `hidden`，由 `initGroupDrag` 显示；无 JS 时手柄不出现，
    卡片内的上移/下移表单仍可用。静态资源版本串升至 `20261020-2`。
  - **验证**：`cargo test`（**178 通过**）/ clippy / fmt / `node --check` 通过；新增单测覆盖重排、
    少/多/重复分组拒绝。实际 HTTP 验证见交付说明。
  - **文档同步**：README「它解决什么问题」。

- **v33**：所有多选列表增加**全选 / 反选**按钮（渐进增强）。**未改数据格式。**
  - **范围**：方案编辑页的上游 / 分组 / 规则三个列表；分组页创建弹窗与每行编辑表单的「手动子分组」列表。
  - **实现**：列表与按钮包在 `[data-checkbox-group]` 中，按钮默认 `hidden`，由 `initCheckboxGroups`
    解除 hidden 并绑定点击；无 JS 时按钮不出现，复选框仍可逐个勾选。静态资源版本串升至 `20261020-1`。
  - **验证**：`cargo test`（**176 通过**）/ clippy / fmt / `node --check` 通过；实际页面检查按钮渲染与勾选计数。
  - **文档同步**：README「它解决什么问题」。

- **v32**：重启/定期刷新改为**只刷新到期上游**，不再每次启动全量重拉。**未改数据格式。**
  - **到期判定**（`refresh::is_due`）：无成功缓存或正文为空 → 到期；订阅 `expire` 已过 → 不刷新；
    `last_success_at + refresh_interval <= now` → 到期；否则跳过。
  - **启动**：`spawn_periodic` 首轮调用 `refresh_due`，未到期直接跳过并发日志；定期 tick 同样只刷到期项。
    「立即刷新 / 全部刷新」仍走 `refresh_all`，保持强制语义。
  - **验证**：`cargo test`（**176 通过**）/ clippy / fmt 通过；新增 4 个纯函数单测覆盖无缓存、未到期、
    已到期、订阅已过期。实际重启验证见交付说明。
  - **文档同步**：README「上游刷新」、本文件 §4.2。

- **v31**：手动节点新增 **SOCKS5** 支持，可直接把另一台电脑的 Clash `socks-port`/`mixed-port`
  作为手动节点接入。**数据格式不变（v5）。**
  - **链接**：`socks5://[user:pass@]host:port#name`，也接受 `socks5h://`（映射为 Clash `socks5`）；
    默认端口 1080，`udp: true`，userinfo 中的特殊字符会百分号解码。
  - **语义**：生成 Clash `type: socks5` 节点，参与地区判定/分组/导出，与其它手动节点一致。
  - **验证**：`cargo test`（**172 通过**）/ clippy / fmt 通过；单测覆盖带认证、无认证与 `socks5h`。
    实际手动节点页提示与导出验证见交付说明。
  - **文档同步**：README「手动节点」、`templates/nodes.html` 提示。

- **v30**：新增**手动节点**功能（`/nodes`），支持直接粘贴代理分享链接，不必为几条地址单独搭订阅。**数据格式升到 v5。**
  - **数据**：`Store.manual_nodes: Vec<ManualNode { id, link }>`；只保存分享链接原文，导出时用 `links.rs`
    重新解析，解析逻辑升级后无需迁移。`FORMAT_VERSION` 4 → 5（纯新增，旧文件默认为空）。
  - **UI**：导航新增「手动节点」。弹窗内每行一条链接；保存前逐条解析，格式错误/重复节点名整批拒绝并提示行号；
    列表只展示节点名/协议/服务器地址，不回显完整链接；删除前检查分组 `manual_nodes` 引用并阻止断链。
  - **导出**：手动节点先于上游节点入池，同名时手动优先；参与地区判定、分组匹配、预览与最终校验；
    解析失败只告警、不中断导出。
  - **兼容**：无上游、只有手动节点时也能导出；手动节点不参与订阅刷新与缓存。
  - **验证**：`cargo test`（**170 通过**）/ clippy / fmt 通过；新增校验单测覆盖合法链接、重复节点名、
    非法链接。实际 HTTP 验证添加/删除/引用阻止、导出包含手动节点（见交付说明）。
  - **文档同步**：README「手动节点」、本文件 §3.1/§5/§8.1。

- **v29**：新增默认 **`国内网站` 分组**与 27 条 override 国内直连规则，默认 `DIRECT`。**未改数据格式。**
  - **分组**：`国内网站`（`select`）的 `manual_nodes` 为 `[DIRECT]`，第一项即默认直连；
    只放 DIRECT，避免默认项被其他分组抢占，也不引入隐藏的分组依赖。
  - **规则**：27 条 `override` 段规则指向 `国内网站`：
    `DOMAIN-SUFFIX,cn` + 26 个常见国内域名（百度/腾讯/微信/淘宝/天猫/支付宝/阿里云/京东/B站/
    知乎/微博/网易/优酷/爱奇艺/抖音/头条/小红书/美团/携程/小米/华为/Gitee 等）+ `GEOIP,CN`。
  - **顺序**：广告拦截 23 条 override → 国内网站 27 条 override → 31 条服务 normal → final MATCH。
  - **默认分组/规则总数**：21 → **22** 组；55 → **82** 条（50 override + 31 normal + 1 final）。
  - **验证**：`cargo test`（**169 通过**）/ clippy / fmt 通过；单测检查国内组 `[DIRECT]`、
    50 条 override 的目标分布（广告 23 / 国内 27）、normal 31、final 唯一、id 唯一。
  - **文档同步**：README 快速开始、本文件 §8.2。

- **v28**：新增默认 **`广告拦截` 分组**与 23 条 override 广告规则，默认 `REJECT`。**未改数据格式。**
  - **分组**：`广告拦截`（`select`）的 `manual_nodes` 为 `[REJECT, DIRECT]`，第一项即默认拦截；
    客户端切到 `DIRECT` 即可临时关闭。内置策略走 `manual_nodes` 进入 `proxies`，
    这是当前模型下不新增字段的最小实现（`render` 的最终校验允许内置策略）。
  - **规则**：23 条 `override` 段规则指向 `广告拦截`，优先于普通分流：
    doubleclick/googleadservices/googlesyndication/adservice.google、
    adsrvr/adnxs/criteo/taboola/outbrain/scorecardresearch/quantserve/moatads、
    adcolony/applovin/unityads、umeng/cnzz/mmstat/tanx/alimama、pos/cpro.baidu、gdt.qq。
  - **默认分组/规则总数**：20 → **21** 组；32 → **55** 条（23 override + 31 normal + 1 final）。
  - **验证**：`cargo test`（**169 通过**）/ clippy / fmt 通过；单测检查广告组
    `[REJECT, DIRECT]` 顺序、23 条 override 规则目标、normal 31、final 唯一、id 唯一。
  - **文档同步**：README 快速开始、本文件 §8.2。

- **v27**：新增 8 个**常用服务分组**与对应默认规则：OpenAI、奈飞、Steam、Telegram、Disney+、Spotify、
  TikTok、GitHub。**未改数据格式。**
  - **分组**：每个服务组都是 `select`，选项与 `Google服务` 相同（美国第一 → 8 个地区组 → 自动选择），
    默认美国，可在客户端切换。默认分组总数 12 → **20**。
  - **规则**：新增 23 条 `normal` 规则（Google 8 条不变），总计 **31 条 normal + 1 条 final**：
    OpenAI（openai/chatgpt/oaistatic/oaiusercontent/sora）、奈飞（netflix/nflx）、
    Steam（steam/s.team）、Telegram（telegram/t.me/tdesktop/telegra.ph/telesco.pe）、
    Disney+（disney/dssott/bamgrid）、Spotify（spotify/scdn.co）、
    TikTok（tiktok/ttlivecdn/musical.ly）、GitHub（github）。
  - **语义**：服务规则只指向服务组；换出口地区只改服务组内选项，无需逐条改规则。
  - **验证**：`cargo test`（**169 通过**）/ clippy / fmt 通过；单测检查 20 个默认分组、
    各服务组共享地区选项、31 条 normal 规则覆盖 9 个服务目标、final MATCH 唯一、id 唯一。
  - **文档同步**：README 快速开始、本文件 §8.2。

- **v26**：`Google服务` 组内从「美国节点 + 自动选择」扩展为**全部 8 个地区组 + 自动选择**，
  美国节点仍固定排第一作为默认选择。**未改数据格式。**
  - **顺序**：美国、香港、台湾、日本、新加坡、韩国、英国、德国、自动选择。Clash 的 `select`
    在无持久化选择时使用第一个选项，因此 Google 默认仍走美国，但用户可在客户端里切到其他地区。
  - **规则**：8 条 Google 规则目标保持 `Google服务` 不变；改地区只需切组内选项。
  - **验证**：`cargo test`（**169 通过**）/ clippy / fmt 通过；单测检查 `Google服务` 的完整顺序；
    当前数据与导出实际检查 `Google服务.proxies` 含全部地区组且首项为美国。
  - **文档同步**：README 快速开始、v25 记录。

- **v25**：新增默认 **`Google服务` 分组**，Google 规则改为指向该分组，而不是直接指向地区组。**未改数据格式。**
  - **分组**：`Google服务`（`select`）的 `manual_groups` 为 `[美国节点, 自动选择]`，顺序即默认选择：
    Clash 的 `select` 在无持久化选择时使用第一个选项，因此默认走 **美国节点**；美国节点为空时
    仍可落到 `自动选择`。默认分组总数 11 → **12**。**v26 起扩展为全部地区组 + 自动选择。**
  - **规则**：v24 的 8 条 Google `normal` 规则目标由「美国节点」改为「Google服务」；
    final `MATCH,自动选择` 不变。以后要整体切换 Google 出口，只改 `Google服务` 组即可，不必改 8 条规则。
  - **初始化语义**：以上只影响首次创建 `store.yaml`；已有数据不会自动补组/改规则。
  - **验证**：`cargo test`（**169 通过**）/ clippy / fmt 通过；单测检查默认分组顺序、
    `Google服务` 的 `[美国节点, 自动选择]` 顺序与 8 条规则目标；`validate_structure` 通过。
    独立临时目录首次初始化确认 `Google服务` 组与规则目标写入 `store.yaml`。
  - **文档同步**：README 快速开始、本文件 §8.2。

- **v24**：首次初始化新增**默认路由规则**，Google 相关服务走「美国节点」。**只影响首次创建 store.yaml，不改已有数据。**
  - **规则**：`DOMAIN-KEYWORD,google`、`gstatic.com`、`youtube.com`、`youtu.be`、`ytimg.com`、
    `gmail.com`、`googlemail.com`、`blogspot.com` → `美国节点`（normal 段，8 条），
    最后 `MATCH,自动选择`（final 段）。这样默认规则集本身就满足 §6.1 第 10 条。
  - **语义**：规则仍是独立资产，新建/编辑方案时需勾选才会进入导出；已有 `store.yaml` 不会自动
    补规则，也不会自动勾选进现有方案。
  - **验证**：`cargo test`（**169 通过**）/ clippy / fmt 通过；单测检查 8 条 normal 规则目标均为
    「美国节点」、唯一 final MATCH 指向「自动选择」、id 唯一；`validate_structure` 通过。
    独立临时目录首次初始化确认默认规则写入 `store.yaml`。
  - **文档同步**：README 快速开始、本文件 §8.2。

- **v23**：「自动选择 / 手动切换」改为**嵌套分组**，分组编辑器支持勾选其他分组作为成员。**未改数据格式。**
  - **默认分组**：`自动选择`（url-test）和 `手动切换`（select）不再使用 `match.all`，而是把
    8 个地区组 + `漏网之鱼` 写进 `manual_groups`；`手动切换` 还包含 `自动选择`。
    新增地区组后，在分组编辑器里勾选即可加入自动测速/手动切换。
  - **编辑器**：原「手动子分组」逗号文本框改为**复选框列表**（已排除自身），创建与编辑表单一致；
    提交用重复键 `manual_groups`，服务端改用 `FormData::get_list` 收集。无 JS 时复选框照常工作。
  - **空分组引用语义**：地区组为空被省略时，上层分组的 `manual_groups` 引用会**从成员中移除并警告**，
    不再让整份配置失败；若上层分组因此变空，有兜底省略、无兜底仍错误。**规则目标**引用被省略分组
    仍然报错。这使「自动选择」可安全地包含所有可能为空的地区组。
  - **验证**：`cargo test`（**168 通过**）/ clippy / fmt 通过。单测覆盖默认顶层组的 `manual_groups`
    与「空地区组剪枝」；实际 HTTP 验证分组页复选框渲染、重复键保存两个子分组、删除临时组；
    独立临时目录首次初始化确认 `自动选择` / `手动切换` 的 YAML 嵌套分组正确。
    已有 `store.yaml` 不会自动改写。
  - **文档同步**：README 默认分组说明、本文件 §3.4 / §6.1 第 17 条。

- **v22**：首次初始化植入**默认分组**并把默认地区别名扩到 **28 个常见地区**。**只影响首次创建 store.yaml，不改已有数据。**
  - **默认分组（11 个）**：自动选择（`url-test` + `match.all`）、手动切换（`select` + `match.all`）、
    香港 / 台湾 / 日本 / 新加坡 / 美国 / 韩国 / 英国 / 德国节点（`select` + `regions`）、
    漏网之鱼（`select` + `catch_all`）。
  - **默认别名（28 个）**：原 8 个 + 澳门、马来西亚、泰国、越南、菲律宾、澳大利亚、加拿大、
    法国、荷兰、俄罗斯、土耳其、阿联酋、巴西、阿根廷、南非、意大利、西班牙、瑞士、瑞典、波兰。
  - **暂不收录印度 / 印度尼西亚**：中文别名「印度」是「印度尼西亚」的子串，当前子串匹配会让
    每个印尼节点同时命中印度。需要时应使用 ISO 码/英文名/城市名，或先实现「最长别名优先」。
  - **初始化语义**：`Store::default()` 只在 `store.yaml` 缺失时使用；已有文件（含用户删改过默认组）
    不会补回或覆盖。默认分组不引用彼此、不创建规则/方案；地区组为空时仍需靠 `漏网之鱼` 兜底。
  - **验证**：新增单测检查 11 个默认分组的结构（含唯一 catch_all）与 28 个地区别名及冲突；
    用独立临时数据目录实际启动服务，确认首次生成 `format_version: 4`、11 个分组、28 个别名；
    `cargo test`（**167 通过**）/ clippy / fmt 通过。
  - **文档同步**：README 快速开始、本文件 §8.2。

- **v21**：导出配置的**下载文件名由 slug 改为方案名称**。**只改 `/sub/{slug}` 的响应头与文件名工具函数。**
  - `/sub/{slug}`（无论内联还是 `?download=1`）的 `Content-Disposition` 现为：
    `filename="<slug>.yaml"; filename*=UTF-8''<方案名>.yaml`。`filename*` 使用 RFC 5987
    UTF-8 百分号编码，浏览器保存为中文方案名；`filename` 保留 slug 作为不支持该字段的客户端回退。
  - 文件名清理：`/ \ : * ? " < > |` 与控制字符替换为 `_`，去掉首尾空白与点；名称本身以
    `.yaml` / `.yml` 结尾时不重复追加；清理后为空回退 slug，再为空回退 `config.yaml`；最多 80 字符。
  - **验证**：单测覆盖中文名、路径字符、空名、超长名与 ASCII header；实际响应头为
    `attachment; filename="b64.yaml"; filename*=UTF-8''base64%E6%96%B9%E6%A1%88.yaml`；
    headless Chrome 实际下载得到 **`base64方案.yaml`**。`cargo test`（**165 通过**）/ clippy / fmt 通过。
  - **文档同步**：README「订阅链接与下载」。

- **v20**：解决**外部客户端拉取订阅 401** 的问题，并补齐令牌入口的可用性。**未改鉴权语义与路由。**
  - **原因**：`/sub/{slug}` 只接受 Basic Auth 或方案令牌；外部客户端不携带浏览器的 Basic Auth，
    而此前方案可以不设令牌，复制出来的链接因此必然 401。
  - **产品解法不变**：继续用 §7.2 的**方案级只读令牌**，不要把管理密码写进客户端 URL。
  - **UI**：方案编辑页新增「生成并保存令牌」（客户端 `crypto.getRandomValues` 生成 32 位十六进制，
    填入输入框后随现有表单一起提交，不新增写路由）；首页/方案页/预览页在**未设令牌**时显示
    「外部客户端会返回 401」提示，复制按钮的提示改为「已复制，但未设订阅令牌，外部客户端会返回 401」。
    无 JS 时生成按钮隐藏，仍可手动填写令牌后保存。
  - **验证**：headless Chrome 实际点击「生成并保存令牌」→ 页面重载后令牌已保存，首页/方案页
    复制链接均包含 `?token=...`；不带 Authorization 请求该链接返回 **200**，不带 token 仍返回 **401**；
    `cargo test`（163 通过）/ clippy / fmt / `node --check` 通过。
  - **文档同步**：README「外部客户端提示 401？」、`docs/ui-design-spec.md` §13.2 第 11 条。

- **v19**：修复「复制链接」在部分浏览器里**按钮可见但点击无效、且没有成功提示**。**只改静态资源、样式与 JS，未改数据模型或路由语义。**
  - **根因（两个叠加）**：① 全局 `button { display: inline-flex }` 覆盖了浏览器对 `hidden` 属性的默认
    `display:none`，所以没有 JS 时按钮也可见；② `static/app.js` 被浏览器缓存后，旧脚本没有绑定复制事件，
    于是出现「可见的死按钮」。
  - **修复**：新增全局 `[hidden] { display: none !important; }`；静态资源统一
    `Cache-Control: no-cache` 并在模板里给 `app.js` / `style.css` 带版本查询串，强制重新验证；
    `README` 与规范同步说明。
  - **复制体验**：复制成功后按钮短暂显示「已复制」，并在页面底部弹出 2.6 秒的
    「订阅链接已复制」提示（`role=status` + `aria-live`）；失败时提示「复制失败，请手动选择链接」。
    未配置 BASEURL 时 JS 会把相对路径补成 `window.location.origin` 的完整地址；Clipboard API
    不可用的局域网 HTTP 仍走 `execCommand` 回退。
  - **验证**（headless Chrome + CDP）：`127.0.0.1`（安全上下文，Clipboard API）与
    `192.168.10.220`（局域网 HTTP，legacy 路径）点击后剪贴板均得到完整订阅 URL，按钮显示
    「已复制」且出现成功提示；禁用脚本时按钮 `display:none`（不再是可见死控件），完整 URL 文本仍可见；
    `app.js` 响应带 `Cache-Control: no-cache`。`cargo test`（163 通过）/ clippy / fmt / `node --check` 通过。
  - **文档同步**：本文件、`docs/ui-design-spec.md` §13.2 第 10 条。

- **v18**：新增 **BASEURL 配置**与**方案链接复制 / 配置下载**。**数据格式升到 v4。**
  - **BASEURL**：`settings.base_url`，可在「通用设置 → 订阅链接」编辑（网页管理，**不进入导出结果**）。
    规范化时去掉末尾斜杠，只接受 http(s)、拒绝查询串/锚点/凭据。为空时页面显示相对路径
    `/sub/<slug>`；反向代理暴露 HTTPS 时必须填外部地址。
  - **会话内展示**：方案列表、方案编辑页、预览页都展示完整订阅链接（含可选令牌），并提供
    「复制链接」。令牌在 URL 中做百分号编码，含 `+ & /` 等字符也不会截断查询串。
  - **复制**：按钮默认 `hidden`，由 `static/app.js` 启用；Clipboard API 不可用（如局域网 HTTP）时
    退回 `execCommand('copy')`。无 JS 时按钮不出现，但完整 URL 是可选文本，复制不依赖脚本。
  - **下载**：`/sub/{slug}` 默认 `Content-Disposition: inline` 在浏览器内联展示；
    `?download=1` 改为 `attachment; filename="<slug>.yaml"`。方案列表/编辑页/预览页的
    「下载配置」链接直接指向后者。客户端拉取订阅不受 Content-Disposition 影响。
  - **纯函数**：`web::normalize_base_url` 与 `web::sub_url`（含编码）带单测；数据格式
    `FORMAT_VERSION` 3 → 4（纯新增，旧文件默认为空）。
  - **验证**：`cargo test`（**163 通过**）/ `cargo clippy --all-targets -- -D warnings` /
    `cargo fmt --check` 通过。实际 HTTP 验证：配置 `https://clash.example.com/` 后自动规范化为
    无尾斜杠，首页/方案页/预览页出现完整链接与复制/下载按钮；`/sub/b64` 为 `inline`、
    `?download=1` 为 `attachment` 且文件名为 `b64.yaml`；非法 scheme 保存被拒并回填；
    测试后已把 BASEURL 恢复为空，避免把示例域名写进真实数据。
  - **文档同步**：README「订阅链接与下载」、本文件 §5/§5.1、`docs/ui-design-spec.md` §13.2。

- **v17**：**所有新增类输入改用弹窗**，不再默认以页内卡片显示在列表上方。**只改表现层与服务端只读查询参数，未改任何写路径或数据模型。**
  - **范围**：新增上游（`/upstreams?new=1`）、手动新增规则（`/rules?new=1`）、批量粘贴导入
    （`/rules?import=1`）、新增地区（`/aliases?new=1`）。分组新建原本就是弹窗，保持不变。
  - **实现**：沿用 v13 的同一套模式——入口是普通链接，服务端按查询参数或校验失败的作用域给
    `<dialog class="modal">` 带 `open` 渲染；无 JS 时由 CSS 退化为页内卡片，有 JS 时由
    `static/app.js` 升级为模态（焦点移入出错字段 / 首个输入框）。
  - **错误回填**：新增表单的错误摘要移进弹窗内部，页面级摘要只显示行内编辑等其他作用域的错误；
    校验失败时服务端自动把对应弹窗展开，已填内容不丢。
  - **未纳入**：方案新建（`/profiles/new`）仍是独立整页——它是带多选与预览的完整流程，
    不是「在列表上方填一条」的短表单；改为弹窗会牺牲可用性。
  - **验证**：`cargo test`（161 通过）/ `cargo clippy --all-targets -- -D warnings` /
    `cargo fmt --check` 通过。实际 HTTP 验证：四个页面默认均不展开弹窗、无内联新增卡片；
    `?new=1` / `?import=1` 深链只展开对应弹窗；四类新增表单校验失败时对应弹窗带 `open`
    返回且错误与回填在弹窗内；每页表单嵌套深度均为 1。
  - **文档同步**：`docs/ui-design-spec.md` §6 弹窗、§13.2 第 9 条。

- **v16**：新增 **GeoIP 库自动下载 / 安装 / 定期更新**，入口在通用设置页。**数据格式升到 v3。**
  - **功能**：手动「立即下载 / 更新」；默认开启的定期更新（默认 7 天，可改 1–365 天或关闭）；
    可选自定义 http(s) 来源。默认来源 **DB-IP Lite Country**（免注册，按 UTC 年月生成地址，
    月初未发布时回退上一个月）。
  - **安全与一致性**：下载内容先校验 MMDB 元数据，再原子替换
    `data/geoip/geoip-auto.mmdb` 并**热加载**；校验失败、HTTP 失败、网络失败都**不动现有库**。
    定期更新带 ETag / Last-Modified，304 时只刷新状态；失败后 1 小时内不重试，避免锤下载源。
    手动与定期任务共用一把锁，在途时第二次触发直接返回「进行中」。
  - **状态与展示**：状态记录在 `data/geoip/status.yaml`（与 `store.yaml` 分开，避免每次更新
    制造业务数据备份）。页面展示库类型、构建日期、大小、mtime、最近尝试/成功、错误、来源（脱敏）、
    下次检查时间；首页 GeoIP 提示同步显示类型与自动更新状态。
  - **设置**：`settings.geoip { auto_update, interval_days, url }`，随 `store.yaml` 原子提交并走
    `revision` 乐观校验；**不进入导出结果**。空的自定义地址不写盘。
  - **数据/路径**：`FORMAT_VERSION` 2 → 3（纯新增，旧文件用默认值补齐）；目录中有多个 `.mmdb`
    时改为选 **mtime 最新**者，保证自动下载的库重启后继续生效。
  - **新依赖**：`flate2`（DB-IP 以 `.mmdb.gz` 分发；reqwest 的 gzip 特性不处理文件体）。
  - **路由**：`POST /general/geoip/settings`、`POST /general/geoip/download`；均需 Basic Auth +
    CSRF，来源检查沿用现有中间件。
  - **验证**（本机真实执行）：`cargo build` / `cargo test`（**161 通过**）/
    `cargo clippy --all-targets -- -D warnings` / `cargo fmt --check` 全部通过。端到端实测：
    ① 启动后 10 秒自动下载并安装 DBIP-Country-Lite（构建于 2026-10-01，8.0 MB）；
    ② 手动下载成功并立即热加载（页面元数据即时变化）；
    ③ 设置保存 / 关闭再开启自动更新 / 间隔回填均正确；
    ④ 非法 scheme 保存被拒且回填，合法但 404 的自定义地址下载失败后**文件哈希与 mtime 不变**、
    页面显示错误与「已安装」并存，恢复内置来源后再次成功；
    ⑤ 把状态与库 mtime 改旧后重启，后台走条件请求得到 **304**，不重新下载、状态成功时间更新；
    ⑥ 缺少 / 错误 CSRF 的新端点均返回 400；`/sub/{slug}` 仍为 200。
  - **文档同步**：本文件 §3.5/§5/§5.1/§8.1/§9/§9.1/§10 与 README「可选：GeoIP」。

- **v15**：按用户要求**移除「应用」模块**——`/apps` 页面、`src/apps.rs` 模板库、`Store.apps`、
  `Profile.apps` / `Profile.app_overrides`、导出管线的 `app` 分段。**数据格式升到 v2。**
  - **理由**：应用的「规则列表 + 目标分组」与路由规则完全重合——在 `/rules` 里给每条规则直接写
    `target` 就能表达同一件事。两套写法并存会让「这条域名走哪个分组」有两个权威来源。
  - **影响面**：路由表去掉 `/apps` 与 `/apps/{id}/delete`；导航去掉「应用」；`ProfileTpl` 去掉
    `all_apps` / `app_overrides`；规则分段由 `override → app → normal → final` 收敛为
    `override → normal → final`；§6.1 第 7 条不再适用——**编号保留**，以免打乱文档与代码注释里
    对第 10/12/13/16/17 条的交叉引用。
  - **迁移**：`FORMAT_VERSION` 1 → 2。旧文件里的 `apps` 键在反序列化阶段被忽略，
    `StoreHandle::new` 会显式告警（不静默丢数据），提示把域名规则改到「路由规则」页维护。
  - **验证（受环境限制）**：本机沙箱下子进程处于 Low 完整性，只能写工作区根目录，无法在
    `target/`、`src/` 下写文件，且无法链接可执行文件——`cargo build/test/clippy/fmt` 与测试二进制
    **均未能在本环境执行**。改用同源手段：`rustc --emit=metadata`（`bin` 与 `--test` 两种形态，
    含 askama 模板展开）**0 错误 0 警告**；`clippy-driver -D warnings`（同样覆盖 `--test`）**干净**；
    `rustfmt --check` **通过**；并逐文件 `grep` 核对残留引用（无 `apps` / `AppRule` /
    `app_overrides` / `target_for`）。**单元测试未实际运行**，这是本次唯一未完成的验收项。
  - **文档同步**：README 快速开始与简介、本文件 §1/§3.1/§3.2/§3.3/§3.6/§5/§6.1/§6.2/§6.3/§8/§9/§11、
    `docs/ui-design-spec.md` 的页面清单，以及全部模板文案。

- **v14**：解除环境阻塞后完成**真实验收**，并补上 v12 未覆盖的一处校验。**只改文档与一处保存期校验，未改数据模型或导出语义。**
  - **环境**：v13 记录的沙箱限制（孙进程一律 `EPERM` / os error 5）在本机已不复现——工作区根目录缺少 `WRITE_OWNER` 是另一处独立问题，补齐后 `cargo` 与浏览器均可正常运行。因此 v13 的「改用同源检查」不再是唯一手段。
  - **门禁**：`cargo build` / `cargo test`（**144 通过**）/ `cargo clippy --all-targets -- -D warnings` / `cargo fmt --check` 全部通过。
  - **浏览器实测**（headless Chrome + CDP，精确视口；注意 `--window-size` 会被钳到最小 500px，320px 必须走 CDP 模拟）：逐条提交真实表单并断言页面状态。
    - 校验失败回填：上游重复 id、规则导入无有效行、通用设置高级 YAML 重复键、方案 slug 非法、方案覆盖项格式错误——**五处均**出现顶部错误摘要 + 字段旁 `field-error` + `aria-invalid`，且**已填内容全部保留**（含下拉框与复选框）。
    - 分组弹窗：`/groups?new=1` 服务端带 `open` 渲染，脚本接管后 `dialog.matches(':modal')` 为真，标题与底部操作就位。
    - 响应式：320 / 1280 / 1440 下 `documentElement.scrollWidth == innerWidth`，**无横向溢出**。
  - **新增保存期校验**：`/aliases` 保存时复用导出期的 `region::find_alias_conflicts`，把「同一别名挂在两个地区下」拦在保存这一步并落到 `aliases` 字段。此前该检查只在导出期生效，而页面文案写的是「否则保存会被拒绝」——**现在文案与行为一致**，判据与导出期同源，不会出现两套标准。
  - **文档同步**：`docs/ui-shots/` 截图全部重出（含弹窗与错误回填两张新图），移除规范 §13.1 里「截图已过期、勿作验收证据」的警告。

- **v13**：分组页的「新增分组」改为**弹窗填参数创建**。**只改表现层与一个只读查询参数，未改数据模型、写路径或导出语义。**
  - **无 JS 降级不靠 `<noscript>`**：触发入口是普通链接 `/groups?new=1`，服务端按 `GroupsTpl.create_open` 把**同一个** `<dialog>` 带 `open` 渲染出来，样式表用 `dialog.modal[open]:not([data-modal])` 把它还原成页内卡片——表单、标签、回填全部照旧。`static/app.js` 只做增强：拦截点击 → `showModal()`；服务端已 `open`（深链或校验失败回填）时升级为模态。于是「JS 被禁用 / 脚本没加载 / 浏览器不支持 `showModal`」三条路径都落在同一份服务端渲染结果上，**不会出现点不动的死控件**，也不依赖 `:modal` 伪类。
  - **校验失败仍然回填**：作用域为空串的错误（即新增表单）直接把弹窗展开，摘要放进弹窗内部（用户不必先关掉弹窗才知道原因）；页面级摘要只渲染其他作用域的错误。
  - **弹窗结构**：`dialog.modal` 分 `.modal-head` / `.modal-body` / `.modal-foot` 三段，主体独立滚动（`min-height: 0` 让 flex 子项真能收缩，否则底部操作会被内容顶出弹窗）、底部操作常驻；新增变量 `--ui-dialog-width-wide`、`--ui-dialog-max-height(-dvh)`（`dvh` 用于移动端地址栏 / 软键盘弹出时仍完整可见）。
  - **页头职责**：「添加『漏网之鱼』兜底分组」由独立卡片移到页头次操作，说明文字改为常驻 hint，功能与文案语义未变。
  - **验证（受环境限制）**：本机沙箱禁止**孙进程**创建——`cargo → rustc`、`rustc → link.exe`、`node → cmd` 一律 `EPERM` / os error 5（提权后同样如此，进程处于 Low 完整性），因此 `cargo build/test/clippy/fmt` 与浏览器截图**在本环境无法执行**。改用同源检查：以 `rustc --edition 2024 --emit=metadata` 加 `target/debug/deps` 里已构建的 176 个依赖对整 crate 做类型检查（含 askama 模板展开）**0 错误 0 警告**，并用「模板引用不存在的字段 → 必须报 E0609」的阴性对照证明该检查确实生效；另加 `node --check static/app.js` 与一组静态检查（变量引用闭合、花括号平衡、style.css 无字面色值、模板标签闭合、弹窗结构与创建表单字段齐全）全部通过。**渲染效果与视觉截图未验收。**
- **v12**：修复 UI 设计规范落地复核中发现的**静默失败**与若干可访问性偏差。**只改表现层与表单错误反馈，未改任何业务语义、路由或数据模型。**
  - **静默失败（本轮最重要）**：`/general` 与 `/rules` 的校验失败此前会**丢弃用户输入且不显示任何错误**——服务端已算出 `FormEcho`，但模板既没引入 `partials/form_errors.html` 也没有 `field-error` 标记，错误"算了但没人渲染"。现已接线：两页都有错误摘要（`role="alert"`）+ 字段旁错误 + 回填。
  - **`/profiles/*` 改走页内回填**：方案新建/编辑的 `bail!`（名称为空、slug 非法、覆盖项格式、slug 重复、版本冲突）此前跳到统一错误页，整页勾选丢失；现改为与其余 5 页一致的页内回填。`parse_overrides` 改为返回文案而非 `AppError`。
  - **规则校验失败也回填**：`rules_save` / `rules_import` 在 `store.update` 校验失败时（缺 target、id 重复等）保留用户输入并把原因显示出来，不再跳错误页。
  - **`FormEcho` 回显改为保序 `Vec`**：原来用 `BTreeMap` 存回显值，**重复键会被折叠成最后一个**，复选框组（`upstreams`/`groups`/`apps`/`rules`）无法回填。新增 `values()` 取全部取值；「一个都没勾」与「本次没有回显」用 `is_empty()` 区分。补单测。
  - **控件类型**：通用设置的 `mixed-port`/`socks-port`/`port` 改 `type="number"` + `min/max`（服务端同步校验 1–65535）；`allow-lan`/`ipv6` 从文本框改下拉（含「不写入该键」），历史值 `yes`/`on` 归一化为 `true`，避免保存时被静默丢弃。
  - **可访问性**：卡片内分区标题由 `h3` 提升为 `h2`（消除 h1→h3 跳级），视觉仍按 §3.1「卡片标题」16px 呈现（`.card > h2` 与 `h3` 共用一条规则）；`.field` 内的常驻帮助文字补 `id` 并由控件的 `aria-describedby` 引用（与错误提示并列）。
  - **启用 `.dense`**：§4.2 的 36px 紧凑规格此前定义了却从未被任何模板使用；现用于三张带行内控件的宽表（上游/应用/地区别名），窄屏自动回到 44px。
  - **验证**：`cargo build` / `cargo test`（144 通过）/ `cargo clippy --all-targets -- -D warnings` / `cargo fmt --check` 全部通过；并对 7 条写路径逐条实测校验失败时的页面表现。
  - **未完成**：视觉截图未重出（本机浏览器无法在沙箱内启动），`docs/ui-shots/` 的图已标注为过期。
- **v11**：按 `docs/ui-design-spec.md`（UI 设计规范 v1.0）改造全部管理页面的视觉与交互。
  **只改表现层，未改任何业务语义、路由或数据模型。**
  - **变量唯一来源**：新增 `static/ui-tokens.css`；`static/style.css` 重写为纯变量引用，
    删除了原先的 `--fg`/`--muted`/`--accent` 一套。控件 44px 最小高度、圆角 8/12/16px、
    卡片以边框为主无阴影、4px 间距尺度。
  - **可访问性**：`base.html` 增加跳过链接、`main`/`nav` 地标与 `lang`；导航选中态用
    `aria-current` 并同时改变颜色/背景/字重/标识线；**此前所有表单控件都没有与标签关联**
    （标签是控件的兄弟节点，无 `for`/`id`），现已全部建立关联；表格数值列右对齐并启用
    `tabular-nums`；键盘焦点统一 2px 外描边 + 2px 间距。
  - **Tpl 结构体新增 `nav: &'static str`**（10 个），仅用于模板高亮当前导航项。
  - **`groups.html` 从 14 字段宽的表格行改为「卡片 + `<details>` 编辑器」**：原布局一个
    单元格塞 6 个无标签控件，既无法关联标签，窄屏也会逐字换行。
  - **不可逆操作改为 `<dialog>` 确认**（`static/app.js`），文案说明对象与后果，默认焦点在
    「取消」；原先的 `onclick="return confirm(...)"` 已全部移除。无 JS 时照常提交。
  - **错误页套用页面骨架**：`AppError` 由裸 HTML 改为渲染 `templates/error.html`，
    用户不会掉出导航上下文，并能用「返回上一页」拿回已填内容。
  - **新增 `templates/error.html`、`static/ui-tokens.css`、`static/app.js`、
    `docs/ui-design-spec.md`**；`.gitignore` 补 `/data-smoke/`（冒烟数据同样含订阅 URL）。
  - **已知未完成项**：表单字段级错误与「提交失败保留已填内容」未实现，服务端校验失败仍返回
    统一错误页；详见规范 §13.2。这是本轮唯一未满足的验收项。
    → **该缺口已在 v12 修复**（且当时遗漏了通用设置页与规则页的静默失败），此处保留原文作为历史记录。
- **v10**：新增**兜底分组**（`match.catch_all`）——收录未被本方案其他分组匹配的节点，
  常用名「漏网之鱼」。分组页提供一键预设（`POST /groups/preset-catch-all`），已存在同名分组时
  只补标志、不覆盖用户配置。
  - **不按名字特判**：语义由 `catch_all` 标志表达，分组可随意改名。
  - **两遍求值**：普通分组先算并汇总「已选中」集合，兜底分组最后算，因此管线 ⑤ 步被拆成两遍。
  - ⚠️ **`match.all` 不算「选中」**：这是实现中发现的关键修正。最初的语义是「不在任何其他分组
    的成员列表里」，但最常见的配置是「自动选择（`match.all`）+ 地区分组 + 兜底」——此时所有节点
    都在自动选择里，兜底会**永远为空**，功能完全失效。改为「未被其他分组**按匹配条件**选中」，
    并显式跳过 `match.all` 的全收桶。
  - **空值语义差异**：无兜底时普通分组为空是错误；**有兜底时降级为警告 + 从输出省略**
    （节点已由兜底承接），这样机场没有某地区节点时不会让整份配置导不出来。
    被省略的分组若仍被引用则报错。
  - **实测验证**（赔钱机场 68 个节点）：`自动选择` 68、`美国节点` 30、`漏网之鱼` 39
    （日本 19 + 新加坡 17 + 台湾 3）；该机场**没有任何香港节点**，`香港节点` 被正确省略。
  - 新增 §6.1 第 16、17 条与此处的语义说明。
- **v9**：新增**分享链接解析**（`src/links.rs`）。此前上游返回 base64 分享链接列表时会被判为
  `structure` 错误而无法使用；现在 `validate_and_parse` 先试 Clash YAML，失败再回退到链接列表
  （自动识别整体 base64），支持 `ss`（SIP002 与旧式 base64、obfs / v2ray-plugin）/ `vmess` /
  `vless`（含 reality）/ `trojan` / `hysteria2` / `tuic`，不支持的协议跳过并计数。缓存的
  `source_format`/`skipped_links` 在上游页展示。
  - **折行处理**：机场常按 76 字符折行，出现 `vmess://` 单独一行、或链接中途断开两种形态。
    逐行解析会整条丢失，因此在解析前用 `join_folded_lines` 拼回。
  - **304 必须重算派生数据**：304 时正文沿用是正确的，但 `node_count`/`source_format` 等
    **派生结果必须用当前解析器重算**。否则解析逻辑升级后，只要上游内容不变（一直 304），
    这些统计就会永远停在旧值。这是实测中发现的真实缺陷。
  - 另一处实测缺口：SIP002 的 base64 userinfo 若被百分号编码（`=` → `%3D`）会导致解析失败，已补解码。
  - 版本号仍是 `v9`：这三处都属于同一次「支持分享链接输入」的改动。
- **v8（实现）**：按 v7 完成全部功能编码。105 个单元测试通过，`cargo clippy --all-targets -- -D warnings` 干净。端到端冒烟测试覆盖：认证（匿名 401 / 带认证 200 / 订阅令牌）、缓存校验与提交（HTTP 200 但内容无效时不覆盖旧缓存）、假节点过滤（6 个节点 → 有效 4 个）、订阅信息提取（从信息节点回退解析到期时间）、分组匹配与拓扑排序、应用规则展开、规则分段顺序（override → app → final）、未知字段透传（`reality-opts` 保留）、`/sub` 输出。
  - **实现期间修复的真实缺陷**：`groups_save` 的新建分支只做了重名检查却漏了写入，导致新建分组静默丢失（已提取为 `cascade::upsert_group` 并补 5 个单测）；`/sub/{不存在的 slug}` 在未认证时返回 404 而非 401，可被用来枚举方案是否存在。
- **v7**：按需求评审修正 8 类问题。**规则拼装**改为按类型识别条件/目标/参数，修正「目标追加到末尾」的错误（§3.7，已核对 Mihomo `ParseRulePayload` 源码），并引入结构化/raw 双形态与 raw 的明确限制；**`MATCH` 级别**从"待验证"改为产品级**错误**，依据是 `match()` 未命中回退 DIRECT（§6.1 第 10 条，已核对源码）；新增**规则分段**（override/app/normal/final）与顺序风险检查（§6.2）；**引用完整性**补 `Profile.groups`、依赖闭包、替代目标优先于清理、间接引用检查（§3.3）；**存储合并为单一 `store.yaml`** + 快照 + `revision` 乐观校验 + 备份恢复流程（§8）；**CSRF** 修正为 token + 来源检查（§7.1）；新增上游刷新 in-flight 合并与写前校验（§4.2）、状态四维分离（§4.7）、`dialer-proxy` 与 `rule-providers` 依赖校验（§6.1 第 12、13 条）、空分组失败策略与 `last_good`（§6.3）、`match.mode` 的 any/all（§3.4）、别名边界改为"前后非 ASCII 字母"以兼顾 `US01`（§3.5）、地区多命中歧义处理与 GeoIP 缺失降级、纯函数分层（§3.5）、应用目标方案级覆盖（§3.6）、透传承诺范围限定（§3.8）、目标客户端与验收基准（§1.1）。
- **v6**：确立校验后提交（§4.3）；缓存改单文件信封杜绝内容与到期信息错配（§4.1）；区分内容字段与状态字段；零节点响应策略与「强制接受」逃生口（§4.4）；导出解析失败纳入同一降级模型（§4.5）；明确 Clash 兼容 User-Agent。
- **v5**：假节点过滤与订阅信息提取；GeoIP 地区判定与冲突清单；分组级联删除/重命名；分组名冲突校验；陈旧判定改用订阅到期时间；应用重构为规则绑定目标分组；自动备份与锁粒度。
- **v4**：分组改为匹配规则驱动，废弃占位符；地区别名表；上游缓存与定期刷新；分组拓扑排序。
- **v3**：分组、规则、通用设置提升为独立可复用资产；方案退化为"引用集合"。
- **v2**：分组与规则改为本项目内配置；确立前后端不分离约束；多方案与多配置导出。
- **v1**：初版。
