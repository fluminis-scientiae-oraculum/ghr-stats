#!/usr/bin/env bash
# Build the distributable ghr-stats binary: a fully static x86-64-v2 musl build.
#
# Why this shape:
#   - static (musl + crt-static, on by default for the musl target) → one file,
#     no glibc/version coupling, drops onto any x86-64 Linux box.
#   - target-cpu=x86-64-v2 (SSE4.2/POPCNT/AVX… baseline, ~2013+ CPUs) for a
#     faster binary, set HERE via RUSTFLAGS — NEVER pinned in Cargo.toml, which
#     would silently break `cargo install` for anyone on an older/other CPU.
#   - rusqlite(bundled) compiles SQLite's C with the musl C compiler; ureq's TLS
#     (rustls + ring) and mimalloc are all musl-clean, so the link is static.
#
# For a native install instead, use glibc: `cargo install --path .` (no target,
# no RUSTFLAGS) — that path deliberately stays generic.
#
# Usage: scripts/release.sh [target-triple]   (default x86_64-unknown-linux-musl)
set -euo pipefail
cd "$(dirname "$0")/.."

TARGET="${1:-x86_64-unknown-linux-musl}"
TARGET_CPU="${TARGET_CPU:-x86-64-v2}"

echo "==> target=$TARGET  target-cpu=$TARGET_CPU"

# The musl std component (idempotent).
rustup target add "$TARGET" >/dev/null 2>&1 || true

# A musl C compiler for the bundled SQLite + ring C sources. Prefer an explicit
# one; fall back to the conventional names.
if [[ -z "${CC_x86_64_unknown_linux_musl:-}" ]]; then
  if command -v x86_64-linux-musl-gcc >/dev/null 2>&1; then
    export CC_x86_64_unknown_linux_musl=x86_64-linux-musl-gcc
  elif command -v musl-gcc >/dev/null 2>&1; then
    export CC_x86_64_unknown_linux_musl=musl-gcc
  fi
fi
echo "==> musl CC=${CC_x86_64_unknown_linux_musl:-<rust default>}"

# target-cpu via RUSTFLAGS, appended so an existing RUSTFLAGS is preserved.
export RUSTFLAGS="-C target-cpu=${TARGET_CPU} ${RUSTFLAGS:-}"

cargo build --release --target "$TARGET"

BIN="target/$TARGET/release/ghr-stats"
echo "==> built $BIN"

# Prove it is static. A musl build is "static-pie linked" (file) and ldd reports
# "statically linked" — match either spelling of "static".
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
