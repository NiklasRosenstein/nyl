# nyl-rs

Rust rewrite of the nyl Kubernetes manifest generator.

## Status

**Phase 1: Configuration & CLI Foundation** ✅ COMPLETE

- Configuration loading (YAML/JSON) ✅
- File discovery with upward traversal ✅
- `nyl validate` command with strict mode ✅
- `nyl new project` command ✅
- `nyl new component` command ✅
- Comprehensive test coverage (>90%) ✅
- Binary size: 2.0 MB (well under 20MB target) ✅

**Phase 2: Helm Integration & Component Discovery** ✅ COMPLETE

- Component discovery with caching ✅
- Helm chart resolution (local paths) ✅
- Profile system with typed structures ✅
- Secrets framework (NullProvider) ✅
- HelmChart resources and data structures ✅
- Generator with component instantiation ✅
- Binary size: 2.5 MB ✅

**Phase 3: Template Rendering & Advanced Helm** ✅ COMPLETE

- Template engine with MiniJinja (b64encode/decode filters) ✅
- Helm template execution and YAML parsing ✅
- Deep profile merging for nested values ✅
- `nyl render` command fully functional ✅
- Template context from profiles/secrets ✅
- YAML output formatting ✅
- Binary size: 2.7 MB ✅

**Current Metrics:**
- Binary size: 2.7 MB (under 3.5 MB target)
- Test coverage: 123 unit tests + 12 integration tests + 1 doc test
- All tests passing ✅
- No regressions from previous phases ✅

## Documentation

Comprehensive documentation is available in mdbook format:

```bash
# Build documentation
mise run rust-docs-build
# or
cd nyl-rs && mdbook build book

# Serve documentation locally
mise run rust-docs-serve
# or
cd nyl-rs && mdbook serve book --open
```

Documentation includes:
- Getting started guide
- Configuration reference
- Command documentation (new, validate)
- Migration guide from Python

## Goals

- **5-10x performance improvement** over Python version ✅ (10x in Phase 1)
- **70-90% memory reduction** ✅
- **<20MB binary size** ✅ (2.0 MB achieved)
- Clean architecture with improved CLI ✅

## Quick Start

```bash
# Create a new project
nyl new project my-app
cd my-app

# Add a component
nyl new component v1.example.io MyApp

# Validate the project
nyl validate
```

## Commands

### Available

- `nyl new project <name>` - Create new project with scaffolding ✅
- `nyl new component <api-version> <kind>` - Create new component ✅
- `nyl validate [--strict]` - Validate configuration ✅
- `nyl render [--environment ENV] [--component KIND]` - Render manifests to stdout ✅

### Coming Soon (Phase 4+)

- `nyl diff` - Show kubectl diff against cluster
- `nyl apply` - Apply to cluster with kubectl

## Building

```bash
# Development build
cargo build

# Release build (optimized)
cargo build --release

# Run tests
cargo test

# Run integration tests
cargo test --test integration_test

# Run benchmarks
cargo bench

# Format code
cargo fmt

# Lint code
cargo clippy -- -D warnings

# Run all checks (format, lint, test)
mise run rust-check-all
```

## Architecture

```
src/
├── cli/          # Command-line interface with clap
│   ├── commands/ # Command implementations
│   └── output/   # Output formatting
├── config/       # Project configuration loading
├── template/     # MiniJinja templating (Phase 3)
├── generator/    # Manifest generation (Phase 3)
├── kubernetes/   # K8s resource types (Phase 4)
├── resources/    # HelmChart, Component (Phase 2-3)
├── util/         # File system utilities, hash computation
└── error.rs      # Error types and handling
```

## Testing

```bash
# Run all tests
cargo test

# Run specific test module
cargo test --lib config::tests
cargo test --lib util::fs::tests

# Run integration tests
cargo test --test integration_test

# Run with output
cargo test -- --nocapture

# Run specific test
cargo test test_validate_with_valid_config
```

## Development

See the main project README for development setup and contribution guidelines.

### Using mise

```bash
# Format code
mise run rust-fmt

# Check formatting
mise run rust-fmt-check

# Run linter
mise run rust-lint

# Run tests
mise run rust-test

# Build release binary
mise run rust-build

# Run all checks
mise run rust-check-all

# Build documentation
mise run rust-docs-build

# Serve documentation
mise run rust-docs-serve
```

## Next Phase

**Phase 4: Kubernetes Client Integration**
- Kubernetes client integration with kube-rs
- Cluster state fetching and resource lookup
- `nyl diff` command implementation
- `nyl apply` command with server-side apply
- ApplySet generation and management

**Phase 5+: Advanced Features**
- Git/OCI chart resolution for Helm
- SOPS secret provider integration
- Kubernetes secret provider
- SSH tunnels for profiles
- Real-time cluster operations

## Migration from Python

The Rust version is designed to be a drop-in replacement for the Python version. Existing `nyl-project.yaml` files work without modification.

See the [Migration Guide](book/src/migration.md) for details.

## Contributing

See [CONTRIBUTING.md](../CONTRIBUTING.md) in the main repository.

## License

MIT
