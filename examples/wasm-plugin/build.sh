#!/bin/bash
set -e

echo "Building example WASM plugin..."

cd "$(dirname "$0")"

# Ensure rustup-managed tools are used (bypass Homebrew override)
export PATH="${HOME}/.cargo/bin:${PATH}"

# Build the WASM module
cargo build --target wasm32-wasip1 --release

# Copy output to plugin directory
mkdir -p "../../plugins/example-wasm-plugin"
cp target/wasm32-wasip1/release/hermes_example_wasm_plugin.wasm \
   ../../plugins/example-wasm-plugin/plugin.wasm
cp plugin.yaml ../../plugins/example-wasm-plugin/

echo "Built: plugins/example-wasm-plugin/plugin.wasm"
