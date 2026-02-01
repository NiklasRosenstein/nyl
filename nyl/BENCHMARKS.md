# Nyl Rust Benchmarks

This document describes the benchmark suite for the Rust rewrite of Nyl and how to run performance comparisons.

## Running Benchmarks

```bash
# Run all benchmarks
cargo bench

# Run specific benchmark
cargo bench template_rendering
cargo bench config_loading

# Generate HTML reports (saved to target/criterion/)
cargo bench -- --save-baseline main
```

## Benchmark Suites

### Template Rendering (`benches/template_rendering.rs`)

Tests template engine performance across various scenarios:

- **template_engine_new**: Template engine creation overhead
- **simple_template_render**: Basic variable substitution (`{{ name }}`)
- **complex_template_render**: Full Kubernetes Deployment manifest with loops
- **template_with_b64encode**: Template with custom filter (base64 encoding)
- **yaml_parse_single_doc**: Single YAML document parsing
- **yaml_parse_multi_doc**: Multi-document YAML parsing (3 docs with `---`)
- **json_to_yaml_serialization**: JSON to YAML conversion
- **template_scaling**: Scaling test with 10, 50, and 100 variables

### Configuration Loading (`benches/config_loading.rs`)

Tests configuration discovery and loading performance:

- **config_discovery**: Finding config file via upward directory traversal
- **config_parsing_simple**: YAML config parsing with profiles
- **config_load_full**: Full config load including directory creation

## Performance Targets

Based on the rewrite goals:

- **5-10x faster** than Python version
- **<1s for 100 Helm charts**
- **<50ms cold start**
- **<50MB RAM** for typical projects

## Comparing with Python

To compare with the Python version:

```bash
# Rust version
time target/release/nyl render --environment prod

# Python version (if available)
time python -m nyl render --environment prod
```

## Baseline Results

Initial benchmark results (will be updated as optimizations are made):

```
# Template Rendering
template_engine_new:           ~XXX ns
simple_template_render:        ~XXX ns
complex_template_render:       ~XXX ns
template_with_b64encode:       ~XXX ns

# Configuration Loading
config_discovery:              ~XXX ns
config_parsing_simple:         ~XXX ns
config_load_full:              ~XXX μs
```

## Performance Profiling

To profile hot paths:

```bash
# Install flamegraph
cargo install flamegraph

# Generate flamegraph
cargo flamegraph --bench template_rendering

# The flamegraph will be saved to flamegraph.svg
```

## CI Integration

Benchmarks run in CI on every commit to track performance regressions. Results are compared against the `main` baseline.

## Memory Profiling

To analyze memory usage:

```bash
# Using heaptrack (Linux)
heaptrack target/release/nyl render

# Using Instruments (macOS)
instruments -t Allocations target/release/nyl render
```
