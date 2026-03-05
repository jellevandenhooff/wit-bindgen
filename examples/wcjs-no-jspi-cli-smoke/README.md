# wcjs no-JSPI CLI smoke

Minimal Rust component for testing `wcjs run --no-jspi` against the current
`wit-bindgen` async guest runtime prototype.

Build:

```sh
cargo build --target=wasm32-wasip1 --manifest-path examples/wcjs-no-jspi-cli-smoke/Cargo.toml
wasm-tools component new \
  examples/wcjs-no-jspi-cli-smoke/target/wasm32-wasip1/debug/wcjs_no_jspi_cli_smoke.wasm \
  --adapt wasi_snapshot_preview1=../wasmtime/target/wasm32-unknown-unknown/release/wasi_snapshot_preview1.wasm \
  -o /tmp/wcjs-no-jspi-cli-smoke.wasm
```

Run:

```sh
cd ../wcjs
npx @jellevdh/wcjs run --no-jspi /tmp/wcjs-no-jspi-cli-smoke.wasm
```
