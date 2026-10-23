# SubForge.rs

**SubForge.rs** 是一个用 Rust 编写的 Clash 订阅合并与配置编排服务。把多个上游 Clash 订阅的**代理节点**汇总去重，按**地区判定 + 匹配规则**归入分组，叠加**路由规则**，按**多套方案**导出可直接使用的 Clash 配置。

单人自用、本地部署、前后端不分离（服务端渲染）。

> 架构约束、设计取舍与开发规则详见 [AGENTS.md](AGENTS.md)。**改动代码前请先读它。**

## 快速开始

```bash
cp config.example.toml config.toml   # 至少改掉 password
cargo run -- config.toml
```

也可以不创建 `config.toml`：首次运行会自动生成配置和随机密码，并只在控制台打印一次。

打开 http://127.0.0.1:8080/ ，用 Basic Auth 登录（用户名任意，密码是 `config.toml` 里的 `password`）。

首次进入后：

0. **首次运行会自动初始化**：`data/store.yaml` 里植入 **23 个默认分组**
   （手动切换、自动选择、香港/新加坡/日本/美国/台湾/韩国/英国/德国节点、
   Google服务、OpenAI、奈飞、Steam商店、Steam下载、Telegram、Disney+、Spotify、TikTok、GitHub、
   广告拦截、国内网站、漏网之鱼）、
   **28 个常见地区别名**，以及 **89 条默认路由规则**：
   Google 相关域名走 **Google服务**（组内包含 8 个地区组 + 自动选择，**默认选中美国节点**），
   OpenAI / 奈飞 / Telegram / Disney+ / Spotify / TikTok / GitHub 的常见域名走各自的服务分组；
   **Steam商店**走代理（与其他服务组共用地区选项），**Steam下载**默认 `DIRECT`，
   下载 CDN（`steamcontent.com` / `steamusercontent.com` / `steamchina.com` 等）在 override 段优先直连，
   常见广告域名走 **广告拦截**（默认 `REJECT`，第二个选项 `DIRECT` 可关闭），
   国内常见域名与 `GEOIP,CN` 走 **国内网站**（默认 `DIRECT`），
   最后 `MATCH → 自动选择`。已有 `data/store.yaml` 不会被改写。
   `自动选择` / `手动切换` 默认把 8 个地区组和 `漏网之鱼` 作为**可选项**（嵌套分组）；
   在「分组」页编辑它们时，可用复选框继续勾选或取消其他分组。
   默认规则只是资产，需要新建/编辑方案时勾选才会进入导出。
1. **上游** —— 添加订阅地址；没有订阅时也可以在「手动节点」页粘贴几条分享链接
2. **分组** —— 定义分组与匹配配方（地区 / 关键词）
3. **路由规则** —— 至少加一条 `MATCH` 兜底；各类应用的域名规则也在这里维护
4. **方案** —— 勾选上述内容，保存后得到订阅地址
5. **通用设置 → 订阅链接** —— 配置 BASEURL（如 `https://clash.example.com`），
   让方案页显示完整订阅地址；留空则显示相对路径 `/sub/<slug>`

## 订阅链接与下载

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

## 手动节点

如果只有几条代理地址、不想为它们单独搭订阅，可以在 **手动节点**（`/nodes`）页直接粘贴分享链接：

- 每行一条，支持 `ss` / `vmess` / `vless` / `trojan` / `hysteria2` / `tuic` / `socks5`
  （也接受 `socks5h://`，映射为 Clash 的 `socks5`）
- 保存前会逐条解析；有格式错误或重复节点名时整批拒绝，并提示行号
- 手动节点不参与订阅刷新，导出时与上游节点合并
- 同名节点以**手动节点优先**（先去重）
- 页面只展示节点名、协议、服务器地址，不回显完整链接（链接里可能含密码/UUID）
- 删除前会检查是否仍被分组的「手动节点」引用；有引用时先阻止删除

## 上游刷新

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

## 它解决什么问题

- **多个机场的节点合并成一份配置**，不用在客户端里来回切订阅。
- **分组自动归类**：按地区判定（节点名 + 可选 GeoIP）与关键词自动吸纳节点，机场换节点名时不必手工维护列表。
- **一次网络抖动不会毁掉配置**：上游数据落盘缓存，拉取失败继续用旧数据。
- **应用分组自带出口选项**：Google服务 / OpenAI / 奈飞 / Steam商店 / Telegram / Disney+ / Spotify / TikTok / GitHub 组内默认包含各地区的 `自动选择`、`手动切换` 与 `DIRECT`，可在客户端直接切换出口或临时直连。
- **方案依赖自动补全**：勾选分组时自动带上其依赖闭包并高亮；保存时服务端仍会校验，避免保存成功但导出失败。
- **备份可预览差异**：恢复前先看当前数据与备份的差异，再确认覆盖；恢复前仍会自动备份当前状态。
- **订阅输出可缓存**：`/sub/<slug>` 返回 `ETag`，客户端带 `If-None-Match` 时未变化直接 304，不重复传输。
- **Steam 分流**：商店/社区走代理（`Steam商店`），下载 CDN 走直连（`Steam下载`），下载规则在 override 段优先于商店规则。
- **分组可排序**：分组卡片左侧有拖动手柄，拖动即可调整库内顺序；卡片内保留上移/下移按钮作为无 JS 替代。导出仍按依赖拓扑排序，该顺序只影响同层分组。
- **规则目标失效不会拖垮整份配置**：导出时目标分组不存在/未选中/被省略的规则会被跳过并告警；`final` 的 `MATCH` 失效时会显式补 `MATCH,DIRECT`。
- **手动节点可勾选**：分组编辑页的「手动节点」输入框旁有「选择节点」，弹窗内用复选框勾选手动节点 / 上游节点 / 内置策略；无 JS 时仍可直接手填。
- **勾选更省事**：方案页的上游/分组/规则、分组页的「手动子分组」都提供全选/反选按钮（无 JS 时按钮隐藏，仍可逐个勾选）。
- **HTTP 200 不等于内容可用**：新响应必须通过 YAML 解析与结构校验才能替换有效缓存，否则保留旧数据。
- **机场塞的「剩余流量 / 套餐到期」信息节点不会进配置**，但其信息会被提取到上游管理页。

## 设计要点

| 主题 | 说明 |
| --- | --- |
| 数据存储 | 全部业务数据集中在单一 `data/store.yaml`，一次原子替换提交，避免多文件半途崩溃导致新旧混杂 |
| 缓存 | 单文件信封（元数据 + 原始正文），正文与到期信息同属一次刷新，不会错配 |
| 规则拼装 | 严格对齐 Mihomo `ParseRulePayload`：目标是第 3 个字段，`no-resolve` 等参数在目标**之后** |
| 规则分段 | 导出顺序固定为 `override → app → normal → final` |
| 兜底 | 缺少 `MATCH` 不会让配置非法，Mihomo 会**静默回退 DIRECT**；因此本服务把它当错误拦截 |
| 地区判定 | 默认「节点名优先，GeoIP 兜底」——机场用中转时 GeoIP 判的是入口 IP，不是落地地区 |
| 引用完整性 | 分组重命名级联改写全部引用（含方案的 groups 列表）；删除被引用的分组必须指定替代目标 |
| 认证 | Basic Auth + CSRF token + 来源检查；订阅可另用只读令牌 |

## 目录

```
config.example.toml   配置示例（占位符）
src/                  源码（结构见 AGENTS.md §9）
templates/            askama 模板
static/               样式
fixtures/sub.yaml     冒烟测试用的假订阅
data/                 运行时生成：store.yaml、cache/、backups/、geoip/
Dockerfile            Docker 镜像
docker-compose.example.yml
scripts/              便携包打包脚本
docs/release-design.md 发布方案设计
```

## 可选：GeoIP

GeoIP 用于在节点名没有命中地区别名时，按节点 `server` 的 IP 归属地兜底判定。

- **自动下载**：在「通用设置 → GeoIP 数据库」点「立即下载 / 更新」即可安装，
  默认来源 **DB-IP Lite Country**（免注册，按月发布）。
- **定期自动更新**：默认开启，每 **7 天**检查一次（可在页面上关闭或改为 1–365 天）。
  源站内容未变时用条件请求（304）跳过下载。
- **自定义来源**：可填任意 http(s) 地址，支持 `.mmdb` 与 `.mmdb.gz`；下载后先校验再
  原子替换，失败不会破坏现有库。
- 也可以把任意 `.mmdb` 手工放进 `data/geoip/`（目录中选 mtime 最新者）。

库缺失时会自动降级为「仅按节点名判定地区」并在首页提示，**不影响导出**。

请遵守所选数据库的许可条款（多数要求署名），并在部署时注明来源与版本。
DB-IP 的许可说明见 <https://db-ip.com/db/lite.php>。

## 测试

```bash
cargo test
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

单元测试覆盖别名边界（`US` 不误命中 `Russia`，但命中 `US01`）、匹配语义、
规则参数位置、正则回退、引用级联、缓存字段分离、响应校验分类、终局规则检查等。

本地手工冒烟：

```bash
# 终端 1：起一个假订阅服务
python -m http.server 18081 --bind 127.0.0.1 --directory fixtures
# 终端 2：
cargo run -- config.smoke.toml     # 监听 127.0.0.1:18080
```

## 部署

完整的发布设计见 [docs/release-design.md](docs/release-design.md)。两种发布形态：

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

打便携包：

```powershell
pwsh -File scripts/package-windows.ps1
```

```bash
bash scripts/package-linux.sh
```

产物在 `dist/`，并附带 `SHA256SUMS.txt`。

### Docker

镜像不携带任何密码，数据通过 `/data` volume 持久化。

已发布镜像（GitHub Container Registry，linux/amd64）：

```bash
docker pull ghcr.io/lhdream/subforge-rs:0.1.0
# 或跟随最新：
docker pull ghcr.io/lhdream/subforge-rs:latest
```

本地构建：

```bash
docker build -t subforge-rs:0.1.0 .
```

自动生成密码：

```bash
docker build -t subforge-rs:0.1.0 .
docker run -d --name subforge-rs \
  -p 127.0.0.1:8080:8080 \
  -v subforge-data:/data \
  subforge-rs:0.1.0

docker logs subforge-rs   # 首次启动横幅里有一次性密码
```

用户指定密码：

```bash
docker run -d --name subforge-rs \
  -p 127.0.0.1:8080:8080 \
  -v subforge-data:/data \
  -e SUBFORGE_PASSWORD='<高强度密码>' \
  subforge-rs:0.1.0
```

Docker secret：

```bash
printf '%s' '<高强度密码>' > ./subforge_password
docker run -d --name subforge-rs \
  -p 127.0.0.1:8080:8080 \
  -v subforge-data:/data \
  -e SUBFORGE_PASSWORD_FILE=/run/secrets/subforge_password \
  -v "$PWD/subforge_password:/run/secrets/subforge_password:ro" \
  subforge-rs:0.1.0
```

Compose 示例见 [docker-compose.example.yml](docker-compose.example.yml)。

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

### 反向代理

服务本身不提供 HTTPS。对外暴露时请在前面加 Caddy / Nginx：

```caddyfile
clash.example.com {
    reverse_proxy 127.0.0.1:8080
}
```

## 安全须知

- `config.toml` 与 `data/` 含密码与上游 token，**已在 `.gitignore` 中排除**，不要提交。
- 默认只监听 `127.0.0.1`。**对外暴露必须置于 HTTPS 之后**——Basic Auth 在明文 HTTP 下会泄露凭据。
- 订阅令牌只能读取指定方案，不能访问管理功能，可独立重置。

## 许可

本项目采用 **GNU Affero General Public License v3.0**（`AGPL-3.0-only`），完整文本见 [LICENSE](LICENSE)。

- 你可以自由使用、修改和分发，但修改后的版本必须以同样的 AGPL 条款开源。
- **通过网络提供服务时**，必须向使用者提供对应源码的获取方式（AGPL 第 13 条）。
- 本项目不提供任何担保，详见许可证文本。
