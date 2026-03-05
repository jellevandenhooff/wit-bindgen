#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
WCJS_DIR="${WCJS_DIR:-$REPO_ROOT/../wcjs}"
WASMTIME_DIR="${WASMTIME_DIR:-$REPO_ROOT/../wasmtime}"
ADAPTER="$WASMTIME_DIR/target/wasm32-unknown-unknown/release/wasi_snapshot_preview1.wasm"
OUT_WASM="${OUT_WASM:-/tmp/wcjs-no-jspi-cli-smoke.wasm}"

cargo build --target=wasm32-wasip1 --manifest-path "$SCRIPT_DIR/Cargo.toml"

wasm-tools component new \
  "$SCRIPT_DIR/target/wasm32-wasip1/debug/wcjs_no_jspi_cli_smoke.wasm" \
  --adapt "wasi_snapshot_preview1=$ADAPTER" \
  -o "$OUT_WASM"

(
  cd "$WCJS_DIR"
  npx @jellevdh/wcjs run --no-jspi "$OUT_WASM"
)
