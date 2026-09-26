#!/usr/bin/env bash
# Static x86-64-v2 musl build of ghr-stats for distribution. target-cpu is set
# here, never in Cargo.toml: a manifest pin would break `cargo install` on CPUs
# below the baseline.
#
# Usage: scripts/release.sh [target-triple]   (default x86_64-unknown-linux-musl)
set -euo pipefail
cd "$(dirname "$0")/.."

TARGET="${1:-x86_64-unknown-linux-musl}"
TARGET_CPU="${TARGET_CPU:-x86-64-v2}"

echo "==> target=$TARGET  target-cpu=$TARGET_CPU"

rustup target add "$TARGET" >/dev/null 2>&1 || true

# The bundled SQLite and ring's C sources need a musl C compiler.
if [[ -z "${CC_x86_64_unknown_linux_musl:-}" ]]; then
  if command -v x86_64-linux-musl-gcc >/dev/null 2>&1; then
    export CC_x86_64_unknown_linux_musl=x86_64-linux-musl-gcc
  elif command -v musl-gcc >/dev/null 2>&1; then
    export CC_x86_64_unknown_linux_musl=musl-gcc
  fi
fi
echo "==> musl CC=${CC_x86_64_unknown_linux_musl:-<rust default>}"

export RUSTFLAGS="-C target-cpu=${TARGET_CPU} ${RUSTFLAGS:-}"

cargo build --release --target "$TARGET"

BIN="target/$TARGET/release/ghr-stats"
echo "==> built $BIN"

# `file` says "static-pie linked", `ldd` says "statically linked".
echo "--- file ---";  file "$BIN" || true
echo "--- ldd  ---";  ldd "$BIN" 2>&1 || true
if ldd "$BIN" 2>&1 | grep -q "statically linked" || file "$BIN" | grep -q "static"; then
  echo "==> OK: static binary"
else
  echo "!! WARNING: binary appears dynamically linked" >&2
  exit 1
fi

echo "--- size --- "; ls -lh "$BIN" | awk '{print $5, $NF}'
echo "--- sha256 ---"; sha256sum "$BIN"
