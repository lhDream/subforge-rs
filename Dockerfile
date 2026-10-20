# syntax=docker/dockerfile:1

# ---------------------------------------------------------------- 构建阶段
FROM rust:1.90-bookworm AS builder
WORKDIR /src

# 先复制清单，尽量复用依赖缓存
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY templates ./templates
RUN cargo build --release --locked

# ---------------------------------------------------------------- 运行阶段
FROM debian:bookworm-slim

LABEL org.opencontainers.image.title="SubForge.rs" \
      org.opencontainers.image.description="Clash 订阅合并与配置编排服务" \
      org.opencontainers.image.licenses="AGPL-3.0-only"

RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 --home /data --shell /usr/sbin/nologin subforge \
 && mkdir -p /data \
 && chown -R subforge:subforge /data

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

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
  CMD curl -fsS http://127.0.0.1:8080/healthz || exit 1

ENTRYPOINT ["/app/subforge-rs"]
