#!/usr/bin/env bash
# Hermes Agent benchmark runner
# Usage: bash benchmarks/run.sh [--release]

set -euo pipefail

PROFILE="${1:---release}"
BIN_DIR="target/release"

if [ "$PROFILE" = "--debug" ]; then
  BIN_DIR="target/debug"
fi

echo "=== Hermes Agent Benchmarks ==="
echo "Profile: $PROFILE"
echo ""

# Build binaries first
echo "Building binaries..."
cargo build $PROFILE 2>/dev/null

echo ""
echo "--- 1. Startup Performance ---"
if command -v hyperfine >/dev/null 2>&1; then
  hyperfine --warmup 3 "$BIN_DIR/hermes --version"
else
  echo "hyperfine not installed. Skipping. Install: cargo install hyperfine"
fi

echo ""
echo "--- 2. Binary Size ---"
ls -lh "$BIN_DIR"/hermes "$BIN_DIR"/hermez-agent "$BIN_DIR"/hermez-acp 2>/dev/null | awk '{print $5, $9}'

echo ""
echo "--- 3. Workspace Test Duration ---"
cargo test --workspace --quiet 2>&1 | tail -3

echo ""
echo "--- 4. Rust Benchmarks (if available) ---"
for crate in hermez-core hermez-state hermez-llm hermez-tools hermez-compress; do
  if [ -d "crates/$crate/benches" ]; then
    echo "Running $crate benchmarks..."
    cargo bench -p $crate 2>/dev/null || echo "  (no benchmarks or failed)"
  fi
done

echo ""
echo "=== Benchmarks Complete ==="
