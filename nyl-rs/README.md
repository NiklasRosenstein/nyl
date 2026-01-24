# nyl-rs

Rust rewrite of the nyl Kubernetes manifest generator.

## Status

**Phase 0: Project Setup** ✅

This is the initial scaffolding with command stubs. All commands are functional but display "not yet implemented" messages.

## Goals

- **5-10x performance improvement** over Python version
- **70-90% memory reduction**
- **<20MB binary size**
- Clean architecture with improved CLI

## Commands

- `nyl render` - Output manifests to stdout
- `nyl diff` - Show kubectl diff against cluster
- `nyl apply` - Apply to cluster with kubectl
- `nyl new` - Create new project
- `nyl validate` - Validate configuration

## Building

```bash
# Development build
cargo build

# Release build (optimized)
cargo build --release

# Run tests
cargo test

# Run benchmarks
cargo bench

# Format code
cargo fmt

# Lint code
cargo clippy
```

## Architecture

```
src/
├── cli/          # Command-line interface
├── config/       # Project configuration
├── template/     # MiniJinja templating
├── generator/    # Manifest generation
├── kubernetes/   # K8s resource types
├── resources/    # HelmChart, Component
└── util/         # Utilities
```

## Development

See the main project README for development setup and contribution guidelines.

## Next Phase

**Phase 1: Configuration & CLI Foundation**
- Implement ProjectConfig loading
- YAML/JSON parsing with serde-norway
- File discovery and validation
- Implement `nyl new` and `nyl validate` commands
