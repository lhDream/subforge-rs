# SubForge.rs

**SubForge.rs** 是一个用 Rust 编写的 Clash 订阅合并与配置编排服务。把多个上游 Clash 订阅的**代理节点**汇总去重，按**地区判定 + 匹配规则**归入分组，叠加**路由规则**，按**多套方案**导出可直接使用的 Clash 配置。

单人自用、本地部署、前后端不分离（服务端渲染）。

## 界面截图

| 方案列表 | 分组管理 |
| --- | --- |
| ![方案列表](docs/ui-shots/profiles-1440.png) | ![分组管理](docs/ui-shots/groups-1440.png) |

| 方案编辑 | 上游订阅 |
| --- | --- |
| ![方案编辑](docs/ui-shots/profile-1440.png) | ![上游订阅 320px](docs/ui-shots/upstreams-320.png) |

> 截图存于 `docs/ui-shots/`；设计规范见 [docs/ui-design-spec.md](docs/ui-design-spec.md)。

![License](https://img.shields.io/badge/license-AGPL--3.0-blue)
![Rust](https://img.shields.io/badge/rust-edition%202024-orange)
![Docker](https://img.shields.io/badge/ghcr.io-subforge--rs-informational)

> 架构约束、设计取舍与开发规则见仓库中的 `AGENTS.md`（内部开发文档，不随仓库分发）。

## 目录

- [功能特性](#功能特性)
- [快速开始](#快速开始)
- [安装](#安装)
  - [Docker（推荐）](#docker推荐)
  - [便携版（Windows / Linux）](#便携版windows--linux)
  - [从源码构建](#从源码构建)
- [使用](#使用)
  - [核心概念与页面](#核心概念与页面)
  - [订阅链接与下载](#订阅链接与下载)
  - [手动节点](#手动节点)
  - [上游刷新](#上游刷新)
  - [预览、编辑与手动填入上游配置](#预览编辑与手动填入上游配置)
  - [可选：GeoIP](#可选geoip)
- [配置](#配置)
- [开发](#开发)
- [部署](#部署)
- [常见问题](#常见问题)
- [安全须知](#安全须知)
- [贡献指南](#贡献指南)
- [许可](#许可)

## 功能特性

- **多机场合并成一份配置**：多个上游订阅的节点汇总去重，不用在客户端里来回切订阅。
- **分组自动归类**：按地区判定（节点名 + 可选 GeoIP）与关键词自动吸纳节点，机场改节点名时不必手工维护列表。
- **规则按目标绑定分组**：每条路由规则显式写目标，分 `override → normal → final` 三段，`final` 段必须有且仅有一条 `MATCH` 兜底。
- **多套方案并存**：一组「上游 + 分组 + 规则」的选择可保存为命名方案，每个方案有独立订阅 URL。
- **缓存优先、失败降级**：上游数据落盘缓存并定期刷新；拉取失败继续用旧数据，**HTTP 200 不等于内容可用**（新响应必须通过解析与结构校验才能替换缓存）。
- **应用分组自带出口选项**：Google服务 / AI / 奈飞 / Steam商店 / Telegram / Disney+ / Spotify / TikTok / GitHub / Reddit / Apple / 游戏加速 组内包含各地区的 `自动选择`、`手动切换` 与 `DIRECT`，可在客户端直接切换出口或临时直连。
- **开箱即用的规则库**：默认 27 个分组、198 条路由规则（来源 ACL4SSR `Clash/Ruleset`），按 AI / 流媒体 / 社交 / 游戏 / 广告拦截 / 国内直连分好组；已有 `store.yaml` 不会被覆盖。
- **Steam 分流**：商店/社区走代理（`Steam商店`），下载 CDN 走直连（`Steam下载`），下载规则在 override 段优先于商店规则。
- **兜底分组默认直连**：机场新增地区导致节点无组可归时，由 `catch_all` 分组（预设名「漏网之鱼」）接住，节点不会凭空消失；该组**默认选项是 `DIRECT`**，用途不明的流量不会悄悄走代理。
- **方案依赖自动补全**：勾选分组时自动带上其依赖闭包并高亮；保存时服务端仍会校验，避免「保存成功但导出失败」。
- **规则目标失效不拖垮整份配置**：目标分组不存在/未选中/被省略的规则会被跳过并告警；`final` 的 `MATCH` 失效时显式补 `MATCH,DIRECT`。
- **订阅输出可缓存**：`/sub/<slug>` 返回 `ETag`，客户端带 `If-None-Match` 时未变化直接 304。
- **备份可预览差异**：恢复前先看当前数据与备份的差异再确认覆盖；恢复前仍会自动备份当前状态。
- **信息节点过滤**：机场塞的「剩余流量 / 套餐到期」信息节点不会进配置，但其流量与到期信息会被提取到上游管理页。
- **前后端不分离**：askama 服务端渲染 + 原生 JS 渐进增强，没有前端构建链；所有操作都有无 JS 降级。
- **编辑效率**：分组卡片可拖动排序（无 JS 时用上移/下移按钮）；方案页的上游/分组/规则与分组页的「手动子分组」都有全选/反选；分组的「手动节点」可用弹窗勾选手动节点 / 上游节点 / 内置策略，无 JS 时仍可手填。

## 快速开始

```bash
cp config.example.toml config.toml   # 至少改掉 password
cargo run -- config.toml
```

也可以不创建 `config.toml`：首次运行会自动生成配置和随机密码，并只在控制台打印一次。

打开 http://127.0.0.1:8080/ ，用 Basic Auth 登录（用户名固定为 `admin`，密码是 `config.toml` 里的 `password`）。

首次进入后：

0. **首次运行会自动初始化**：`data/store.yaml` 里植入 **27 个默认分组**
   （手动切换、自动选择、香港/新加坡/日本/美国/台湾/韩国/英国/德国节点、
   Google服务、AI、奈飞、Steam商店、Steam下载、Telegram、Disney+、Spotify、TikTok、GitHub、
   Reddit、Apple、游戏加速、广告拦截、国内网站、漏网之鱼）、
   **28 个常见地区别名**，以及 **198 条默认路由规则**
   （69 条 override + 128 条 normal + 1 条 final）：
   Google 相关域名走 **Google服务**（组内包含 8 个地区组 + 自动选择，**默认选中美国节点**），
   **AI**（48 条，覆盖 OpenAI / Claude / Gemini / Copilot / Cursor / Grok 等）、
   奈飞 / Telegram / Disney+ / Spotify / TikTok / GitHub / Reddit（4 条）/
   **Apple**（19 条，只取域名类规则）/
   **游戏加速**（29 条，暴雪 / Epic / Origin / Xbox / 任天堂 / Sony）各自走对应服务分组；
   **Steam商店**走代理（与其他服务组共用地区选项），**Steam下载**默认 `DIRECT`，
   下载 CDN（`steamcontent.com` / `steamusercontent.com` / `steamchina.com` 等）在 override 段优先直连，
   常见广告域名走 **广告拦截**（默认 `REJECT`，第二个选项 `DIRECT` 可关闭），
   国内常见域名与 `GEOIP,CN` 走 **国内网站**（默认 `DIRECT`），
   最后 `MATCH → 自动选择`。已有 `data/store.yaml` 不会被改写。
   **漏网之鱼**是兜底分组，默认选项 `DIRECT` 排在最前（**默认直连**），
   需要时可在客户端里切成具体节点。
   `自动选择` / `手动切换` 默认把 8 个地区组和 `漏网之鱼` 作为**可选项**（嵌套分组）；
   在「分组」页编辑它们时，可用复选框继续勾选或取消其他分组。
   默认规则只是资产，需要新建/编辑方案时勾选才会进入导出。
1. **上游** —— 添加订阅地址；没有订阅时也可以在「手动节点」页粘贴几条分享链接
2. **分组** —— 定义分组与匹配配方（地区 / 关键词）
3. **路由规则** —— 至少加一条 `MATCH` 兜底；各类应用的域名规则也在这里维护
4. **方案** —— 勾选上述内容，保存后得到订阅地址
5. **通用设置 → 订阅链接** —— 配置 BASEURL（如 `https://clash.example.com`），
   让方案页显示完整订阅地址；留空则显示相对路径 `/sub/<slug>`

## 安装

环境要求：

| 形态 | 要求 |
| --- | --- |
| 源码 / 便携版 | Rust 1.85+（edition 2024）；Windows / Linux / macOS |
| Docker | 任意 OCI 运行时；数据挂在 `/data` |
| Cloudflare Workers | Rust 1.91+、Node 20+、`cargo install worker-build`、已启用 R2 的账号 |

### Docker（推荐）

镜像不携带任何密码，数据通过 `/data` volume 持久化。

已发布镜像（GitHub Container Registry，linux/amd64）：

```bash
docker pull ghcr.io/lhdream/subforge-rs:0.1.0
# 或跟随最新：
docker pull ghcr.io/lhdream/subforge-rs:latest
```

自动生成密码：

```bash
docker run -d --name subforge-rs \
  -p 127.0.0.1:8080:8080 \
  -v subforge-data:/data \
  ghcr.io/lhdream/subforge-rs:0.1.0

docker logs subforge-rs   # 首次启动横幅里有一次性密码
```

用户指定密码：

```bash
docker run -d --name subforge-rs \
  -p 127.0.0.1:8080:8080 \
  -v subforge-data:/data \
  -e SUBFORGE_PASSWORD='<高强度密码>' \
  ghcr.io/lhdream/subforge-rs:0.1.0
```

Docker secret：

```bash
printf '%s' '<高强度密码>' > ./subforge_password
docker run -d --name subforge-rs \
  -p 127.0.0.1:8080:8080 \
  -v subforge-data:/data \
  -e SUBFORGE_PASSWORD_FILE=/run/secrets/subforge_password \
  -v "$PWD/subforge_password:/run/secrets/subforge_password:ro" \
  ghcr.io/lhdream/subforge-rs:0.1.0
```

本地构建与 Compose 示例见 [docker-compose.example.yml](docker-compose.example.yml)：

```bash
docker build -t subforge-rs:0.1.0 .
```

### 便携版（Windows / Linux）

解压后直接运行，配置与数据都落在可执行文件所在目录：

```text
SubForge.rs-<version>-windows-x86_64/
  subforge-rs.exe
  static/
  config.example.toml
  README.md
```

首次运行：

- 自动创建 `config.toml` 和 `data/`
- 自动生成随机管理密码，并**只在控制台打印一次**
- 已有 `config.toml` 时不会重新生成

常用命令：

```powershell
.\subforge-rs.exe                 # 启动（首次自动初始化）
.\subforge-rs.exe --init          # 只生成配置和密码，然后退出
.\subforge-rs.exe --version
.\subforge-rs.exe --help
```

Linux 便携版同理：

```bash
./subforge-rs --init
./subforge-rs
```

自己打便携包（产物在 `dist/`，附带 `SHA256SUMS.txt`）：

```powershell
pwsh -File scripts/package-windows.ps1
```

```bash
bash scripts/package-linux.sh
```

### 从源码构建

```bash
git clone https://github.com/lhDream/subforge-rs.git
cd subforge-rs
cp config.example.toml config.toml   # 修改 password
cargo build --release
./target/release/subforge-rs config.toml
```

## 使用

### 核心概念与页面

各类资产**相互独立、独立复用**，方案只记录「选了哪些」：

| 资产 | 页面 | 说明 |
| --- | --- | --- |
| 上游订阅 | `/upstreams` | 机场订阅地址；数据落盘缓存，失败降级用旧数据 |
| 手动节点 | `/nodes` | 直接粘贴分享链接，不参与订阅刷新 |
| 分组 | `/groups` | 存的是**配方**（地区 / 关键词 / 手动项），不是节点清单 |
| 路由规则 | `/rules` | 每条规则显式写目标分组，分 `override / normal / final` 段 |
| 地区别名 | `/aliases` | 节点名命中哪些关键词算哪个地区 |
| 通用设置 | `/general` | 进入导出结果的 Clash 客户端配置 + 不进入导出的服务行为设置 |
| 方案 | `/profiles/{slug}` | 勾选上游 + 分组 + 规则，生成订阅地址 `/sub/<slug>` |

导出管线：`引用完整性校验 → 读缓存 → 解析去重 → 地区判定 → 分组求值 → 拓扑排序 → 规则分段 → 拼装输出`。
输出键顺序固定为 `general → proxies → proxy-groups → rules`，可直接投入 Mihomo / Clash 使用。

| 主题 | 说明 |
| --- | --- |
| 数据存储 | 全部业务数据集中在单一 `data/store.yaml`，一次原子替换提交，避免多文件半途崩溃导致新旧混杂 |
| 缓存 | 单文件信封（元数据 + 原始正文），正文与到期信息同属一次刷新，不会错配 |
| 规则拼装 | 严格对齐 Mihomo `ParseRulePayload`：目标是第 3 个字段，`no-resolve` 等参数在目标**之后** |
| 规则分段 | 导出顺序固定为 `override → normal → final` |
| 兜底 | 缺少 `MATCH` 不会让配置非法，Mihomo 会**静默回退 DIRECT**；因此本服务把它当错误拦截 |
| 地区判定 | 默认「节点名优先，GeoIP 兜底」——机场用中转时 GeoIP 判的是入口 IP，不是落地地区 |
| 引用完整性 | 分组重命名级联改写全部引用（含方案的 groups 列表）；删除被引用的分组必须指定替代目标 |
| 认证 | Basic Auth + CSRF token + 来源检查；订阅可另用只读令牌 |

规则多了以后用 **`/rules?target=<分组名>`** 按目标分组筛选（纯 GET 表单，地址可收藏、无 JS 也能用）。
筛选只影响显示，导出的仍是方案勾选的全部规则；序号保持全局序号，上移/下移与导出顺序仍对得上。

### 订阅链接与下载

在方案列表（首页）和方案编辑页，每个方案都有两个操作：

- **复制链接** —— 复制完整的订阅 URL（含 BASEURL 与可选令牌），填入 Clash 客户端。
  没有 JS 时按钮不显示，但完整 URL 已直接列出，可手动选择复制。
- **下载配置** —— 以 `.yaml` 文件下载当前方案生成的完整配置（`/sub/<slug>?download=1`）。
  **文件名使用方案名称**（如「base64方案」→ `base64方案.yaml`），中文名通过
  `Content-Disposition` 的 `filename*` 传递，而不是用 slug。

直接访问 `/sub/<slug>` 会在浏览器内联展示 YAML；只有带 `download=1` 时才以附件下载。
客户端拉取订阅不受影响。

### 外部客户端提示 401？

`/sub/<slug>` 需要 **Basic Auth 或该方案的订阅令牌**。Clash/Mihomo 等外部客户端不会携带
浏览器的 Basic Auth，所以复制出去的链接必须带令牌：

1. 进入方案的编辑页 → 「订阅令牌」→ 点「生成并保存令牌」；
2. 回到首页重新「复制链接」，链接应包含 `?token=...`；
3. 如果列表里出现「未设订阅令牌，外部客户端拉取会返回 401」，按上面两步处理即可。

令牌只能读取该方案，不能访问管理功能，可随时在方案页重新生成（旧令牌立即失效）。
不建议把管理密码直接写进客户端 URL——那会泄露全部管理权限。

### 手动节点

如果只有几条代理地址、不想为它们单独搭订阅，可以在 **手动节点**（`/nodes`）页直接粘贴分享链接：

- 每行一条，支持 `ss` / `vmess` / `vless` / `trojan` / `hysteria2` / `tuic` / `socks5`
  （也接受 `socks5h://`，映射为 Clash 的 `socks5`）
- 保存前会逐条解析；有格式错误或重复节点名时整批拒绝，并提示行号
- 手动节点不参与订阅刷新，导出时与上游节点合并
- 同名节点以**手动节点优先**（先去重）
- 页面只展示节点名、协议、服务器地址，不回显完整链接（链接里可能含密码/UUID）
- 删除前会检查是否仍被分组的「手动节点」引用；有引用时先阻止删除

### 上游刷新

启动应用时**不会无脑重拉所有上游**，只刷新真正到期的项：

- 没有成功缓存、或缓存正文为空 → 刷新
- `last_success_at + refresh_interval` 已过 → 刷新
- 订阅 `expire` 已过期 → 跳过，避免对失效订阅反复请求
- 其余情况直接跳过，缓存文件的 `last_success_at` / mtime 不变

之后后台按 `refresh_interval` 定期检查，同样只刷新到期项。上游页的「立即刷新 / 全部刷新」是手动操作，仍会强制拉取。

### 上游需要代理才能访问？

有些机场只在特定网络或地区可访问，直连会超时。在网页 **通用设置 → 上游访问代理** 里勾选「拉取上游时使用代理」并填地址即可，**保存后立即生效，不用重启**；**拉取上游和下载 GeoIP 库都会走它**。

也可以写在 `config.toml` 里，作为还没在网页设置过时的默认值（网页设置优先）：

```toml
# 留空 = 直连（默认，行为与以前完全一致）
proxy = "http://127.0.0.1:7890"       # 本机 Mihomo / Clash 的 mixed-port
# proxy = "socks5://127.0.0.1:7891"
# proxy = "socks5://user:pass@127.0.0.1:1080"
```

- 只支持 `http(s)://` 与 `socks5(h)://`，并且**必须写端口**。
- 拉下来的节点（`ss://` / `vmess://` 等）**不能直接当代理用**：那是 Clash 节点的协议，需要客户端内核负责转发。正确做法是让 Mihomo/Clash 用本项目导出的订阅跑起来，再把它的 `mixed-port` / `socks-port` 填进代理地址。
- 代理写错不会导致启动失败：日志会记录一条告警，并按直连运行；网页里填错则直接回填报错，不会保存。
- 代理不可用时，按既有降级规则继续使用旧缓存，不会清空已有节点；代理地址中的账号密码在日志与页面上都会脱敏。

### 预览、编辑与手动填入上游配置

在「上游订阅」列表点 **预览 / 编辑配置**，可以查看当前缓存的原始正文，并直接编辑或粘贴替换内容。页面不会为了预览而请求上游；首次同步失败、没有缓存时，也可以手动填入配置后使用。

- 支持 Clash YAML，以及现有解析器支持的明文 / 整体 base64 分享链接列表。
- 点「校验并保存到缓存」后，只有解析成功且过滤后仍有有效节点的内容才能替换缓存；失败时保留输入和旧缓存。
- **这是替换当前缓存，不是永久覆盖上游。** 后续自动或手动同步成功会覆盖手动内容；同步失败仍保留可用缓存。
- 手动保存清除旧的 HTTP ETag / Last-Modified，避免同步返回 304 而错误沿用手动内容；流量与到期信息仅从本次正文提取，不沿用旧响应头。
- 页面保存使用版本检查；编辑期间缓存或业务设置发生变化时，会拒绝覆盖，需重新打开页面核对最新内容。
- 上游正文可能包含代理密码 / UUID，请勿向他人分享页面内容。**导出仅提取节点**：原文中的通用设置、分组、路由规则不会直接进入方案输出。

### 可选：GeoIP

GeoIP 用于在节点名没有命中地区别名时，按节点 `server` 的 IP 归属地兜底判定。

- **自动下载**：在「通用设置 → GeoIP 数据库」点「立即下载 / 更新」即可安装，
  默认来源 **DB-IP Lite Country**（免注册，按月发布）。
- **定期自动更新**：默认开启，每 **7 天**检查一次（可在页面上关闭或改为 1–365 天）。
  源站内容未变时用条件请求（304）跳过下载。
- **自定义来源**：可填任意 http(s) 地址，支持 `.mmdb` 与 `.mmdb.gz`；下载后先校验再
  原子替换，失败不会破坏现有库。
- 也可以把任意 `.mmdb` 手工放进 `data/geoip/`（目录中选 mtime 最新者）。

库缺失时会自动降级为「仅按节点名判定地区」并在首页提示，**不影响导出**。

> ⚠️ 默认策略是「节点名优先，GeoIP 兜底」。机场大量使用中转时，GeoIP 判的是入口 IP、
> 不是实际落地地区；在通用设置页切换为「IP 优先」前请确认你的机场是否使用中转。

请遵守所选数据库的许可条款（多数要求署名），并在部署时注明来源与版本。
DB-IP 的许可说明见 <https://db-ip.com/db/lite.php>。

## 配置

服务设置与业务数据**严格分离**：

- `config.toml` —— 服务自身设置（监听地址、管理密码、刷新间隔等），**手工编辑，网页不管理**。
- `data/store.yaml` —— 全部业务数据（上游 / 节点 / 分组 / 规则 / 别名 / 方案 / 客户端配置），网页管理。
- `data/cache/` —— 上游缓存信封；`data/backups/` —— `store.yaml` 历史快照。

### config.toml

复制 [config.example.toml](config.example.toml) 后修改密码即可，其余项保持默认：

| 键 | 默认值 | 说明 |
| --- | --- | --- |
| `listen` | `127.0.0.1:8080` | 监听地址；默认仅本机 |
| `password` | `changeme` | 管理密码（Basic Auth 用户名固定 `admin`）；**必须修改** |
| `refresh_interval` | `6h` | 上游定期刷新间隔（humantime：`30s` / `10m` / `6h` / `1d`） |
| `fetch_timeout` | `30s` | 单个上游拉取超时 |
| `fetch_concurrency` | `4` | 拉取并发上限，避免被限流 |
| `proxy` | `""` | 出口代理（留空直连）；网页设置优先 |
| `data_dir` | `data` | 数据与缓存目录，相对路径相对配置文件所在目录解析 |

### 环境变量

| 变量 | 作用 |
| --- | --- |
| `SUBFORGE_PASSWORD` | 管理密码（优先级最高） |
| `SUBFORGE_PASSWORD_FILE` | 从文件读取管理密码（适合 Docker secret） |
| `SUBFORGE_CONFIG` | 配置文件路径 |
| `SUBFORGE_DATA_DIR` | 数据目录 |
| `SUBFORGE_STATIC_DIR` | 静态资源目录 |
| `SUBFORGE_LISTEN` | 监听地址，覆盖配置文件 |
| `RUST_LOG` | 日志级别，默认 `info` |

## 开发

架构说明、领域模型与开发约定见 `AGENTS.md`（内部开发文档，不随仓库分发）；UI 规范见
[docs/ui-design-spec.md](docs/ui-design-spec.md)。

```bash
cargo build
cargo run -- config.smoke.toml
cargo test
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

单元测试覆盖别名边界（`US` 不误命中 `Russia`，但命中 `US01`）、匹配语义（`any` / `all` / `exclude`）、
规则参数位置、逻辑规则 raw 模式、引用级联与依赖闭包、缓存内容/状态字段分离、响应校验分类、
终局规则检查、`revision` 冲突等。

本地手工冒烟：

```bash
# 终端 1：起一个假订阅服务
python -m http.server 18081 --bind 127.0.0.1 --directory fixtures
# 终端 2：
cargo run -- config.smoke.toml     # 监听 127.0.0.1:18080
```

### 目录结构

```
src/          源码（模块职责见 src/lib.rs 的模块声明与各文件头注释）
templates/    askama 模板
static/       样式与渐进增强脚本（无构建链）
fixtures/     冒烟测试用的假订阅
docs/         发布设计、UI 规范、Workers 迁移
Dockerfile    Docker 镜像
scripts/      便携包打包脚本
data/         运行时生成：store.yaml、cache/、backups/、geoip/
```

## 部署

完整的发布设计见 [docs/release-design.md](docs/release-design.md)。三种部署形态（安装方式见上文）：

- 便携版 / 源码 —— 直接跑在机器上，默认 `127.0.0.1:8080`
- Docker —— `/data` volume 持久化，已发布到 GHCR
- Cloudflare Workers —— 无服务器，Durable Object + R2

### Cloudflare Workers

与本地版共用同一份业务代码；存储换成 Durable Object + R2。迁移细节见
[docs/workers-migration.md](docs/workers-migration.md)。

前置：Node ≥ 20、Rust ≥ 1.91、`cargo install worker-build`、Cloudflare 账号并已启用 R2。

```bash
# 1. 创建 R2 桶（一次）
npx wrangler r2 bucket create subforge-cache

# 2. 管理密码（Basic Auth 用户名固定为 admin）
npx wrangler secret put SUBFORGE_PASSWORD

# 3. 部署（wrangler 会调用 worker-build --release）
npx wrangler deploy
```

把本地 `data/store.yaml` 迁移上去（整库导入，含上游 token）：

```bash
# 导出当前线上数据（备份用）
curl -u admin:<密码> https://<worker>.workers.dev/admin/store -o backup.yaml

# 导入本地数据
curl -u admin:<密码> --data-binary @data/store.yaml \
  https://<worker>.workers.dev/admin/store
```

- 定时刷新：`wrangler.jsonc` 的 `triggers.crons`（默认 `15 */6 * * *`），只刷到期上游；
  GeoIP 按通用设置里的间隔判断是否更新。
- 管理页、表单、CSRF、备份恢复与本地版完全一致。
- Workers 侧限制：出口代理设置不生效（`worker::Fetch` 不支持代理）；GeoIP 无 DNS，
  域名节点只按节点名判定；`wrangler r2 object` 要看线上对象需加 `--remote`。

### 反向代理与 HTTPS

服务本身不提供 HTTPS。对外暴露时请在前面加 Caddy / Nginx：

```caddyfile
clash.example.com {
    reverse_proxy 127.0.0.1:8080
}
```

**Basic Auth 在明文 HTTP 下会泄露凭据**，对外暴露必须置于 HTTPS 之后。

### 备份与恢复

每次写 `store.yaml` 前会把旧文件复制到 `data/backups/`，保留最近 20 份（文件名带时间戳）。
恢复流程：列出备份 → **预览与当前状态的差异** → 确认后先备份当前状态再原子替换 → 可回滚。

`data/` 含 token 不入库，这是唯一的版本历史来源。

## 常见问题

**导出的配置客户端加载失败？** 先看方案预览页的错误与警告：节点名重复、分组最终成员为空、
规则目标不存在、`dialer-proxy` 引用缺失、`RULE-SET` 未在 `rule-providers` 中定义都会导致失败。
验收标准是 **Mihomo 实际加载成功**，而不是「看起来是合法 YAML」。

**上游一直显示 stale？** 说明最近一次刷新没有通过校验（网络失败 / 非 2xx / 解析失败 / 零节点）。
旧缓存仍会参与导出；上游页可看到具体的 `last_error_kind`。

**节点地区判错了？** 默认「节点名优先，GeoIP 兜底」。中转节点的 `server` 是入口 IP，
GeoIP 会判成入口地区；在「地区别名」页补关键词，或确认后在通用设置里切换 `region_source`。

**分组一个节点都没有？** 检查匹配配方：`any` 是「地区**或**关键词」，`all` 是「地区**且**关键词」，
`exclude` 优先级最高。机场没有该地区节点时，交给 `catch_all` 兜底分组（「漏网之鱼」）接住。

**改了配置但客户端没变？** `/sub/<slug>` 带 `ETag`，客户端可能拿到 304。
直接用 `?download=1` 拉一次，或改动方案触发新内容。

**规则该写在哪一段？** `override` 段放强制覆盖（广告拦截、国内直连白名单），`normal` 放普通路由，
`final` 放且仅放一条 `MATCH` 兜底。分段由你显式指定，不按书写顺序。

## 安全须知

- `config.toml` 与 `data/` 含密码与上游 token，**已在 `.gitignore` 中排除**，不要提交。
- 默认只监听 `127.0.0.1`。**对外暴露必须置于 HTTPS 之后**——Basic Auth 在明文 HTTP 下会泄露凭据。
- 修改类请求校验 CSRF token 与 `Origin` / `Referer`；订阅令牌只能读取指定方案，不能访问管理功能，可独立重置。
- 上游 URL 中的 token 在日志与页面里脱敏；订阅正文不会写入日志。

## 贡献指南

- 提 Issue 请附上：**现象 / 复现步骤 / 期望行为**，以及版本、脱敏后的 `config.toml` 与相关日志。
- 改动代码前请先读 `AGENTS.md`（内部开发文档）：它写明了架构硬约束（前后端不分离、纯函数边界、
  节点原样透传、引用完整性）与明确的非目标。
- PR 前请通过 `cargo test`、`cargo fmt --check`、`cargo clippy --all-targets -- -D warnings`，
  并说明验证方式；新增逻辑请补单测（尤其是纯函数层）。
- 变更请在 `AGENTS.md` 末尾的变更记录追加一条（版本号 + 改动 + 验证）；
  数据格式变动时同步更新 `FORMAT_VERSION` 与迁移逻辑。
- 不要提交密码、真实订阅地址、GeoIP 库文件或 `data/` 下的任何内容。

## 许可

本项目采用 **GNU Affero General Public License v3.0**（`AGPL-3.0-only`），完整文本见 [LICENSE](LICENSE)。

- 你可以自由使用、修改和分发，但修改后的版本必须以同样的 AGPL 条款开源。
- **通过网络提供服务时**，必须向使用者提供对应源码的获取方式（AGPL 第 13 条）。
- 本项目不提供任何担保，详见许可证文本。
