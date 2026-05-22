#!/usr/bin/env bash
# Local build pipeline: produces dist/sidecar/ + dist/bin/<platform>/ from
# this checkout. Used by `npm run build` and when developing against a
# checked-out package (no postinstall download required).

set -euo pipefail

PKG_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PKG_ROOT"

echo "==> Building TS sidecar (with devDeps for tsc)"
(
  cd sidecar
  npm ci
  npx tsc -p tsconfig.json
)

echo "==> Pruning sidecar to production deps only"
(
  cd sidecar
  npm prune --omit=dev
)

echo "==> Building Rust binary"
cargo build --release -p claude-app-server

echo "==> Staging dist/"

# Sidecar artifacts (rebuilt from scratch so stale files do not bloat).
rm -rf dist/sidecar
mkdir -p dist/sidecar
cp -R sidecar/dist/. dist/sidecar/
mkdir -p dist/sidecar/node_modules
rsync -a --delete sidecar/node_modules/ dist/sidecar/node_modules/

# Strip platform-specific binaries we never ship to npm.
# (Win32 is not in package.json os whitelist; ripgrep ships per-platform
# in the SDK and the postinstall pass on the user's machine prunes the
# non-matching ones. We pre-strip win32 to shrink the published tarball.)
rm -rf "dist/sidecar/node_modules/@anthropic-ai/claude-agent-sdk/vendor/ripgrep/x64-win32" || true

# Rust binary for the current platform
UNAME_OS="$(uname -s)"
UNAME_ARCH="$(uname -m)"
case "$UNAME_OS" in
  Darwin) PLATFORM_OS="darwin" ;;
  Linux)  PLATFORM_OS="linux" ;;
  *) echo "unsupported OS $UNAME_OS" >&2; exit 1 ;;
esac
case "$UNAME_ARCH" in
  x86_64|amd64) PLATFORM_ARCH="x64" ;;
  arm64|aarch64) PLATFORM_ARCH="arm64" ;;
  *) echo "unsupported arch $UNAME_ARCH" >&2; exit 1 ;;
esac

TRIPLE="${PLATFORM_OS}-${PLATFORM_ARCH}"
TARGET_DIR="dist/bin/$TRIPLE"
mkdir -p "$TARGET_DIR"
cp target/release/claude-app-server "$TARGET_DIR/claude-app-server"
chmod +x "$TARGET_DIR/claude-app-server"

# Patch the sidecar launcher import path so it resolves vendored modules
# relative to dist/sidecar instead of sidecar/node_modules.
# (No-op today; the launcher passes through to node which honours
# node_modules sibling resolution.)

echo "==> Done"
echo "    sidecar:  dist/sidecar/index.js"
echo "    binary:   $TARGET_DIR/claude-app-server"
