#!/usr/bin/env bash
# 打包 Linux 便携版：解压后直接运行，配置/数据都在可执行文件所在目录。
#
# 用法（在仓库根目录执行）：
#   bash scripts/package-linux.sh
#   bash scripts/package-linux.sh --skip-build
#
# 产物：
#   dist/SubForge.rs-<version>-linux-x86_64.tar.gz
#   dist/SHA256SUMS.txt

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SKIP_BUILD=0
if [[ "${1:-}" == "--skip-build" ]]; then
  SKIP_BUILD=1
fi

VERSION="$(grep -m1 -E '^version[[:space:]]*=' "$ROOT/Cargo.toml" | sed -E 's/.*"([^"]+)".*/\1/')"
if [[ -z "$VERSION" ]]; then
  echo "无法从 Cargo.toml 读取 version" >&2
  exit 1
fi

ARCH="$(uname -m)"
case "$ARCH" in
  x86_64|amd64) ARCH="x86_64" ;;
  aarch64|arm64) ARCH="arm64" ;;
esac

NAME="SubForge.rs-${VERSION}-linux-${ARCH}"
DIST="$ROOT/dist"
STAGE="$DIST/$NAME"

if [[ "$SKIP_BUILD" -eq 0 ]]; then
  (cd "$ROOT" && cargo build --release --locked)
fi

rm -rf "$STAGE"
mkdir -p "$STAGE"

if [[ ! -f "$ROOT/target/release/subforge-rs" ]]; then
  echo "找不到 target/release/subforge-rs，请先构建 release" >&2
  exit 1
fi

cp "$ROOT/target/release/subforge-rs" "$STAGE/"
cp -r "$ROOT/static" "$STAGE/"
cp "$ROOT/config.example.toml" "$STAGE/"
cp "$ROOT/README.md" "$STAGE/"
if [[ -f "$ROOT/LICENSE" ]]; then
  cp "$ROOT/LICENSE" "$STAGE/"
fi

# 确保不把密码/数据带进发布包
rm -rf "$STAGE/config.toml" "$STAGE/data" "$STAGE/data-smoke"

TARBALL="$DIST/$NAME.tar.gz"
rm -f "$TARBALL"
tar -C "$DIST" -czf "$TARBALL" "$NAME"

if command -v sha256sum >/dev/null 2>&1; then
  (cd "$DIST" && sha256sum "$NAME.tar.gz" > SHA256SUMS.txt)
else
  (cd "$DIST" && shasum -a 256 "$NAME.tar.gz" > SHA256SUMS.txt)
fi

echo "已生成："
echo "  $TARBALL"
echo "  $DIST/SHA256SUMS.txt"
