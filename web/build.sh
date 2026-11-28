#!/usr/bin/env bash
# Compile FastEngine to wasm and emit the JS bindings into web/pkg.
# The CLI version must match wasm-bindgen in crates/mini-exchange-wasm/Cargo.toml.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

if ! command -v wasm-bindgen >/dev/null 2>&1; then
  echo "wasm-bindgen is not on PATH." >&2
  echo "Install CLI 0.2.130 so it matches the crate:" >&2
  echo "  cargo install wasm-bindgen-cli --version 0.2.130 --locked" >&2
  echo "or download the 0.2.130 release tarball from wasm-bindgen/wasm-bindgen." >&2
  exit 1
fi

cargo +stable build -p mini-exchange-wasm --release --target wasm32-unknown-unknown
rm -rf web/pkg
wasm-bindgen target/wasm32-unknown-unknown/release/mini_exchange_wasm.wasm \
  --out-dir web/pkg \
  --target web

echo "wrote web/pkg"
