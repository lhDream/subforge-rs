# SubForge.rs 发布方案设计

本文设计两种发布形态：

1. **便携压缩包**：解压后直接运行，配置、数据、日志都落在 exe 所在目录。
2. **Docker 部署**：镜像不携带任何密码，数据通过 volume 持久化。

两种形态共享同一套**首次运行密码策略**。

---

## 1. 目标与非目标

### 目标

- 发布包**默认不携带密码**，仓库、镜像、压缩包里都不出现真实密码。
- 首次运行可以选择：
  - 自动生成一个随机管理密码；或
  - 通过环境变量 / secret 文件 / 配置文件由用户指定。
- 便携包解压即可运行，不要求用户先手工创建 `config.toml`。
- Docker 镜像无状态，业务数据全部在 `/data` volume。
- 不破坏现有数据格式和路由。

### 非目标

- 不内置 HTTPS/TLS；公网仍需反向代理。
- 不做多用户、注册、权限分级。
- 不把密码写进镜像层、zip 或 Git。

---

## 2. 共用：首次运行密码策略

### 2.1 优先级

从高到低：

1. 环境变量 `SUBFORGE_PASSWORD`
2. 环境变量 `SUBFORGE_PASSWORD_FILE` 指向的文件内容
3. 已有配置文件里的 `password`
4. 都没有 → 自动生成随机密码，写入配置文件，并在控制台打印一次

> 自动生成只在「没有可用密码」时发生。已经有有效密码时，重启不会重新生成，也不会重复打印。

### 2.2 什么算「没有可用密码」

- 配置文件不存在；或
- `password` 为空；或
- `password` 仍是示例占位符 `changeme`。

出现以上情况时：

- 若设置了环境变量 / secret 文件 → 使用环境值，不打印密码。
- 否则 → 生成 32 字符随机密码（base64url，无填充），写入配置文件，并打印首次启动横幅。

### 2.3 密码生成

- 随机源：`rand` + OS entropy。
- 长度：24 字节 → base64url 32 字符。
- 字符集：`A-Z a-z 0-9 - _`，避免 shell / URL 转义问题。
- 不生成易混淆的短密码，不使用时间戳。

### 2.4 打印格式

只在首次生成时打印一次，写到 stdout（容器里就是 `docker logs`）：

```text
============================================================
 SubForge.rs 首次启动
 已自动生成管理密码：

   xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx

 请立即保存。此密码不会再次显示。
 配置文件：/path/to/config.toml
 如需自行指定，请设置 SUBFORGE_PASSWORD 后删除该配置文件。
============================================================
```

后续启动只记录普通日志，不打印密码。

### 2.5 配置文件写入

- 首次运行创建 `config.toml`。
- 已有文件但密码为空 / 占位符时：
  - 尽量只替换 `password = ...` 这一行，保留注释与其它字段；
  - 找不到该行则在文件末尾追加；
  - 写入使用原子替换。
- 文件权限：
  - Unix：`0600`；
  - Windows：尽力设置当前用户可读写，并在文档中提示不要放在共享目录。
- 如果写文件失败：
  - 不静默继续；
  - 打印错误并提示改用 `SUBFORGE_PASSWORD`。

### 2.6 环境变量清单

| 变量 | 作用 | 默认 |
| --- | --- | --- |
| `SUBFORGE_PASSWORD` | 直接指定管理密码 | 无 |
| `SUBFORGE_PASSWORD_FILE` | 从文件读取密码（Docker secret） | 无 |
| `SUBFORGE_CONFIG` | 配置文件路径 | exe 目录 / `/data/config.toml` |
| `SUBFORGE_DATA_DIR` | 数据目录 | exe 目录 / `/data` |
| `SUBFORGE_STATIC_DIR` | 静态资源目录 | exe 目录下的 `static/` |
| `SUBFORGE_LISTEN` | 监听地址，可覆盖配置文件 | 配置文件或 `127.0.0.1:8080` |
| `RUST_LOG` | 日志级别 | `info` |

### 2.7 命令行

最小集合：

```text
subforge-rs [--config PATH]
subforge-rs --init [--config PATH]     # 只生成配置和密码，打印后退出
subforge-rs --version
subforge-rs --help
```

- 双击 exe 时不带参数，走默认流程。
- `--init` 方便用户先生成配置再启动服务。

---

## 3. 方案 A：便携压缩包

### 3.1 发布物

```text
SubForge.rs-0.1.0-windows-x86_64/
  subforge-rs.exe
  static/
    style.css
    ui-tokens.css
    app.js
  config.example.toml      # 占位符，不含真实密码
  README.txt
  LICENSE.txt              # 如有
```

**不包含**：

- `config.toml`
- `data/`
- 任何真实密码、token、订阅地址

### 3.2 首次运行行为

1. 以 exe 所在目录为基准目录。
2. 创建 `data/`。
3. 创建 `config.toml`：
   - `listen = "127.0.0.1:8080"`
   - `data_dir = "data"`（相对配置文件所在目录解析）
   - `refresh_interval = "6h"`
   - `fetch_timeout = "30s"`
   - `fetch_concurrency = 4`
   - `password = "<自动生成>"`
4. 打印首次启动横幅。
5. 启动 HTTP 服务。

### 3.3 运行目录约束

关键改造：**不再依赖当前工作目录。**

- 默认配置路径：`<exe 目录>/config.toml`
- 默认数据目录：`<exe 目录>/data`
- 静态资源：`<exe 目录>/static`
- 配置文件里的相对 `data_dir` 相对**配置文件所在目录**解析，而不是 CWD。

这样即使用户：

```powershell
cd C:\
D:\tools\SubForge.rs\subforge-rs.exe
```

也能正常找到 `static/`、写入 `data/`。

### 3.4 打包脚本

`scripts/package-windows.ps1`：

1. `cargo build --release --locked`
2. 创建 staging 目录
3. 复制：
   - `target/release/subforge-rs.exe`
   - `static/`
   - `config.example.toml`
   - `README.txt`
4. 压缩为 `SubForge.rs-<version>-windows-x86_64.zip`
5. 生成 `SHA256SUMS.txt`

可选：同时产出 Linux `tar.gz`，复用同一脚本结构。

### 3.5 验收

- 解压到全新目录，双击 exe：
  - 生成 `config.toml`、`data/`
  - 控制台打印一次密码
  - 浏览器可登录
- 重启：不再打印密码，旧密码仍有效。
- 删除 `config.toml` 再启动：生成新密码。
- 从其他 CWD 启动：静态资源与数据目录仍正确。
- 压缩包内**没有** `config.toml`、`data/`、真实密码。

---

## 4. 方案 B：Docker 部署

### 4.1 镜像设计

多阶段构建：

```dockerfile
# 构建阶段
FROM rust:1.90-bookworm AS builder
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY templates ./templates
COPY static ./static
RUN cargo build --release --locked

# 运行阶段
FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl \
 && rm -rf /var/lib/apt/lists/*
RUN useradd --system --uid 10001 --home /data subforge
WORKDIR /app
COPY --from=builder /src/target/release/subforge-rs /app/subforge-rs
COPY static /app/static
ENV SUBFORGE_CONFIG=/data/config.toml \
    SUBFORGE_DATA_DIR=/data \
    SUBFORGE_STATIC_DIR=/app/static \
    SUBFORGE_LISTEN=0.0.0.0:8080 \
    RUST_LOG=info
VOLUME ["/data"]
EXPOSE 8080
USER subforge
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s \
  CMD curl -fsS http://127.0.0.1:8080/healthz || exit 1
ENTRYPOINT ["/app/subforge-rs"]
```

要点：

- 镜像内**没有** `config.toml`、`data/`、密码。
- 运行用户非 root（UID 10001）。
- 需要 `ca-certificates` 访问上游订阅与 GeoIP 下载。
- 用 `curl` 做 healthcheck；若不想装 curl，可增加 `subforge-rs --healthcheck` 子命令。
- 数据目录 `/data` 是唯一需要持久化的位置。

### 4.2 启动方式

自动生成密码：

```bash
docker run -d \
  --name subforge-rs \
  -p 127.0.0.1:8080:8080 \
  -v subforge-data:/data \
  subforge-rs:0.1.0

docker logs subforge-rs   # 首次启动横幅里有生成的密码
```

用户指定密码：

```bash
docker run -d \
  --name subforge-rs \
  -p 127.0.0.1:8080:8080 \
  -v subforge-data:/data \
  -e SUBFORGE_PASSWORD='<自己设置的高强度密码>' \
  subforge-rs:0.1.0
```

Docker secret：

```bash
printf '%s' '<密码>' > ./subforge_password
docker run -d \
  --name subforge-rs \
  -p 127.0.0.1:8080:8080 \
  -v subforge-data:/data \
  -e SUBFORGE_PASSWORD_FILE=/run/secrets/subforge_password \
  -v "$PWD/subforge_password:/run/secrets/subforge_password:ro" \
  subforge-rs:0.1.0
```

### 4.3 Compose 示例

`docker-compose.example.yml`：

```yaml
services:
  subforge-rs:
    image: subforge-rs:0.1.0
    container_name: subforge-rs
    restart: unless-stopped
    ports:
      - "127.0.0.1:8080:8080"
    volumes:
      - subforge-data:/data
    environment:
      # 推荐：从 .env 读取，不要把密码写进 compose 文件
      SUBFORGE_PASSWORD: ${SUBFORGE_PASSWORD:?请在 .env 中设置 SUBFORGE_PASSWORD}
      RUST_LOG: info

volumes:
  subforge-data:
```

如果希望首次自动生成：

- 去掉 `SUBFORGE_PASSWORD`；
- 启动后执行 `docker logs subforge-rs` 获取一次性密码。

### 4.4 数据与权限

- `/data` 包含：`store.yaml`、上游缓存、GeoIP、备份、`config.toml`。
- 这些内容含上游 token，**不要**提交到 Git，也不要用匿名 volume 后忘记备份。
- 如果使用 bind mount，需要保证 UID 10001 可写：

```bash
mkdir -p ./subforge-data
sudo chown -R 10001:10001 ./subforge-data
docker run ... -v "$PWD/subforge-data:/data" ...
```

- 建议只监听 `127.0.0.1:8080`，HTTPS 交给 Caddy/Nginx。

### 4.5 优雅停机

Docker `stop` 会发 `SIGTERM`。当前服务未处理信号，设计上补充：

- `tokio::signal::ctrl_c` + Unix `SIGTERM` 监听；
- `axum::serve(...).with_graceful_shutdown(...)`；
- 关闭时等待进行中的请求结束，再退出。

这样 `docker stop` 不会留下半写入文件（原子写已保证一致性，但优雅停机更干净）。

### 4.6 验收

- `docker run` 不带任何密码：
  - 容器启动成功；
  - `/data/config.toml` 生成；
  - `docker logs` 有一次性密码；
  - `/healthz` 返回 ok。
- 设置 `SUBFORGE_PASSWORD` 后：不再生成、不再打印密码，使用指定密码。
- 删除 volume 重建：生成新密码。
- 保留 volume 重启：密码不变。
- 镜像内不存在 `config.toml` / `data/` / 明文密码。
- `docker stop` 后进程正常退出，无 panic。

---

## 5. 共用代码改造清单

| # | 改造 | 文件 |
| --- | --- | --- |
| 1 | 从 exe 目录解析默认配置/数据/静态路径 | `src/main.rs`、`src/config.rs` |
| 2 | 环境变量覆盖：password / config / data / static / listen | `src/config.rs` |
| 3 | 首次运行生成密码、写配置、打印横幅 | `src/config.rs` 或新增 `src/bootstrap.rs` |
| 4 | 相对 `data_dir` 相对配置文件目录解析 | `src/config.rs` |
| 5 | `static` 目录可配置 | `src/web/mod.rs` |
| 6 | `--init` / `--version` / `--help` | `src/main.rs` |
| 7 | 优雅停机 | `src/main.rs` |
| 8 | 密码生成工具 | `src/auth.rs` |
| 9 | 单测：首次生成、优先级、路径解析、权限 | `src/config.rs` 测试 |
| 10 | Dockerfile、.dockerignore、compose 示例 | 仓库根目录 |
| 11 | Windows 打包脚本、校验和 | `scripts/` |
| 12 | README 部署章节、RELEASE.md | `README.md`、`docs/` |

---

## 6. 版本与产物命名

统一版本来自 `Cargo.toml` 的 `version`：

```text
SubForge.rs-0.1.0-windows-x86_64.zip
SubForge.rs-0.1.0-linux-x86_64.tar.gz   # 可选
subforge-rs:0.1.0
subforge-rs:latest
```

每个发布产物附带：

```text
SHA256SUMS.txt
```

Docker 镜像可同时构建 `linux/amd64` 与 `linux/arm64`。

---

## 7. 安全注意事项

- 密码只在首次生成时打印一次；之后不打印。
- 不把密码写入镜像层、zip、Git、日志文件。
- `config.toml` 权限 0600；`data/` 目录尽量 0700。
- Docker 推荐用 `SUBFORGE_PASSWORD_FILE` / Docker secret，而不是 `-e` 明文。
- 默认只监听 `127.0.0.1`；对外必须 HTTPS 反代。
- 上游 token、订阅地址、GeoIP 库文件不进入发布包。

---

## 8. 已确认决策

1. 便携包同时做 **Windows x64** 与 **Linux x86_64 / arm64**（脚本按架构命名）。
2. 自动生成的密码**只打印到控制台**，不额外写入 `initial-password.txt`。
3. Docker 运行基础镜像使用 **`debian:bookworm-slim`**。
4. 本文设计已进入实现：首次运行引导、`--init`、环境变量、静态资源路径、优雅停机、
   `Dockerfile`、compose 示例与打包脚本均已落地。
