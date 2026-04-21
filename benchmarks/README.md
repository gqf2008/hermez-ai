# Hermes Agent Benchmarks

This directory contains benchmarks for evaluating Hermes Agent performance across key dimensions.

## Benchmark Categories

### 1. Startup Performance
- **Target**: Time from binary invocation to first prompt
- **Command**: `hyperfine './target/release/hermes --version'`
- **Goal**: < 200ms cold start

### 2. Session Throughput
- **Target**: Sessions created per second
- **Command**: `cargo bench -p hermes-state`
- **Goal**: > 100 sessions/sec

### 3. LLM Provider Latency
- **Target**: End-to-end request latency (mock server)
- **Command**: `cargo test -p hermes-llm -- --ignored`
- **Goal**: < 50ms overhead vs raw HTTP

### 4. Tool Execution Overhead
- **Target**: Tool dispatch + execution latency
- **Command**: `cargo bench -p hermes-tools`
- **Goal**: < 10ms per tool call

### 5. Context Compression
- **Target**: Compression ratio and speed
- **Command**: `cargo bench -p hermes-compress`
- **Goal**: 4-stage pipeline < 100ms

### 6. Gateway Message Throughput
- **Target**: Messages processed per second per platform
- **Command**: `cargo bench -p hermes-gateway`
- **Goal**: > 1000 msg/sec (telegram mock)

## Running Benchmarks

```bash
# Install hyperfine for shell benchmarks
cargo install hyperfine

# Run all Rust benchmarks
cargo bench --workspace

# Run shell-level benchmarks
bash benchmarks/run.sh
```

## Adding New Benchmarks

1. Add criterion benchmark in the relevant crate's `benches/` directory
2. Register the benchmark in `benchmarks/run.sh`
3. Update this README with target and goal
