# Technology Stack

## Overview

This document outlines the Rust crates and tools selected for the Nyl rewrite, with rationale for each choice.

## Core Dependencies

### CLI Framework

**Selected: `clap` v4.x**
- **Why**: Most popular Rust CLI framework, excellent derive macros
- **Alternatives**: `structopt` (merged into clap), `argh` (less features)
- **Usage**: Command parsing, argument validation, help generation

```toml
[dependencies]
clap = { version = "4.5", features = ["derive", "env", "wrap_help"] }
```

**Example**:
```rust
#[derive(Parser)]
#[command(name = "nyl", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Render templates and generate manifests
    Template(TemplateArgs),
    /// Create a new Nyl project
    New(NewArgs),
}
```

### Template Engine

**Selected: `minijinja` v2.x**
- **Why**: Jinja2-compatible, pure Rust, excellent performance
- **Alternatives**:
  - `tera` (different syntax, less compatible)
  - `handlebars` (different template language)
  - `askama` (compile-time, less dynamic)
- **Usage**: Template parsing and rendering

```toml
[dependencies]
minijinja = { version = "2.5", features = ["builtins", "custom_syntax"] }
```

**Key Features**:
- Jinja2 syntax compatibility
- Custom functions and filters
- Template inheritance and includes
- Excellent error messages
- 10-100x faster than Python Jinja2

### Async Runtime

**Selected: `tokio` v1.x**
- **Why**: Industry standard, mature, excellent ecosystem
- **Alternatives**: `async-std` (smaller ecosystem), `smol` (minimal)
- **Usage**: Async I/O, parallel task execution

```toml
[dependencies]
tokio = { version = "1.42", features = ["full"] }
```

### YAML Processing

**Selected: `serde-norway` v0.1**
- **Why**: Modern YAML 1.2 library, actively maintained (serde_yaml is deprecated)
- **Alternatives**: `serde_yaml` (deprecated), `yaml-rust` (older)
- **Usage**: YAML parsing and serialization
- **Benefits**: YAML 1.2 spec compliance, better error messages, maintained

```toml
[dependencies]
serde-norway = "0.1"
serde = { version = "1.0", features = ["derive"] }
```

**Note**: serde-norway is the recommended replacement for serde_yaml, which is no longer maintained.

### Kubernetes Client

**Selected: `kube` v0.95 (kube-rs)**
- **Why**: Official Rust Kubernetes client, well-maintained
- **Alternatives**: Custom types (too much work), `k8s-openapi` directly (lower level)
- **Usage**: Kubernetes resource types, API interaction (future)

```toml
[dependencies]
kube = { version = "0.95", features = ["runtime", "derive"] }
k8s-openapi = { version = "0.23", features = ["v1_31"] }
```

**Initially**: Just use types, not runtime features (defer cluster interaction)

### JSON Processing

**Selected: `serde_json` v1.x**
- **Why**: Standard JSON library, fast, well-integrated
- **Usage**: JSON config parsing, OCI registry metadata

```toml
[dependencies]
serde_json = "1.0"
```

### TOML Processing

**Selected: `toml` v0.8**
- **Why**: Standard TOML library for Rust
- **Usage**: TOML config file parsing (optional format)

```toml
[dependencies]
toml = { version = "0.8", optional = true }
```

### HTTP Client

**Selected: `reqwest` v0.12**
- **Why**: High-level, async HTTP client with good ergonomics
- **Alternatives**: `hyper` (lower level), `ureq` (blocking)
- **Usage**: Chart repository downloads (HTTP/OCI)

```toml
[dependencies]
reqwest = { version = "0.12", features = ["json", "stream"] }
```

### Error Handling

**Selected: `thiserror` v2.x + `anyhow` v1.x**
- **Why**:
  - `thiserror`: Library error types with derive macros
  - `anyhow`: Application error handling with context
- **Alternatives**: `eyre` (similar to anyhow), manual impl
- **Usage**: Error type definitions and propagation

```toml
[dependencies]
thiserror = "2.0"
anyhow = "1.0"
```

### Logging

**Selected: `tracing` v0.1 + `tracing-subscriber` v0.3**
- **Why**: Structured logging, excellent async support, widely adopted
- **Alternatives**: `log` + `env_logger` (simpler but less features)
- **Usage**: Logging, diagnostics, performance tracing

```toml
[dependencies]
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter", "json"] }
```

### File System Operations

**Selected: `tokio::fs` (from tokio) + `walkdir` v2.x**
- **Why**: Async file I/O from tokio, efficient directory traversal from walkdir
- **Usage**: Reading manifests, searching for files

```toml
[dependencies]
# tokio already includes fs
walkdir = "2.5"
```

### Process Execution

**Selected: `tokio::process` (from tokio)**
- **Why**: Async process spawning, integrated with tokio
- **Usage**: Calling `helm template`, `git clone`, etc.

```rust
use tokio::process::Command;

let output = Command::new("helm")
    .args(&["template", "myrelease", "mychart"])
    .output()
    .await?;
```

### Hashing

**Selected: `sha2` v0.10 + `hex` v0.4**
- **Why**: Standard cryptographic hashing, needed for stable hashing
- **Usage**: Resource deduplication via stable hashing

```toml
[dependencies]
sha2 = "0.10"
hex = "0.4"
```

### Git Operations

**Selected: `git2` v0.19**
- **Why**: libgit2 bindings, mature, feature-complete
- **Alternatives**: Call `git` CLI (simpler but less control)
- **Usage**: Cloning Helm chart repositories from Git

```toml
[dependencies]
git2 = { version = "0.19", optional = true }
```

**Note**: May defer Git support to later phase and use CLI instead initially.

### Base64 Encoding

**Selected: `base64` v0.22**
- **Why**: Standard base64 library, fast
- **Usage**: Template functions (b64encode/b64decode)

```toml
[dependencies]
base64 = "0.22"
```

### Random Number Generation

**Selected: `rand` v0.8**
- **Why**: Standard RNG library for Rust
- **Usage**: random_password, randhex template functions

```toml
[dependencies]
rand = "0.8"
```

### Testing Framework

**Selected: Built-in `cargo test` + `tokio::test` + `assert_cmd`**
- **Why**: Standard testing tools, good ecosystem
- **Additional**: `assert_cmd` for CLI testing, `tempfile` for temp directories

```toml
[dev-dependencies]
tokio-test = "0.4"
assert_cmd = "2.0"
tempfile = "3.13"
predicates = "3.1"
```

### Benchmarking

**Selected: `criterion` v0.5**
- **Why**: Statistical benchmarking framework
- **Usage**: Performance comparisons with Python version

```toml
[dev-dependencies]
criterion = { version = "0.5", features = ["async_tokio"] }
```

## Development Tools

### Code Formatting
- **`rustfmt`**: Standard Rust formatter
- **Config**: Use default settings, maintain consistency

### Linting
- **`clippy`**: Official Rust linter
- **Config**: Enable pedantic lints, deny warnings in CI

### Documentation
- **`rustdoc`**: Built-in documentation generator
- **Usage**: Generate API docs from doc comments

### CI/CD
- **GitHub Actions**: Automated testing and releases
- **Cross-compilation**: `cross` or GitHub Actions matrix builds
- **Release**: `cargo-dist` for binary distribution

## Optional Features

Use Cargo features for optional functionality:

```toml
[features]
default = ["toml-config"]
toml-config = ["dep:toml"]
git-support = ["dep:git2"]
secrets-sops = []  # Future: SOPS support
secrets-k8s = []   # Future: K8s secret provider
```

## Version Pinning Strategy

### Development
- Use `^` (caret) for most dependencies (semver compatible)
- Pin exact versions for critical dependencies (kube, serde_yaml)

### Production
- Generate `Cargo.lock` and commit it
- Use Dependabot for automated dependency updates

## Binary Optimization

### Cargo.toml Profile Settings

```toml
[profile.release]
opt-level = 3           # Maximum optimization
lto = "thin"            # Link-time optimization
codegen-units = 1       # Better optimization, slower compile
strip = true            # Strip symbols
panic = "abort"         # Smaller binary, no unwinding
```

### Additional Optimizations
- **UPX compression**: Optional post-processing for even smaller binaries
- **Feature flags**: Only compile needed features
- **Lazy static**: Use `once_cell` for lazy initialization

```toml
[dependencies]
once_cell = "1.20"
```

## Dependency Summary

### Required (MVP)
| Crate | Version | Purpose |
|-------|---------|---------|
| clap | 4.5 | CLI framework |
| minijinja | 2.5 | Template engine |
| tokio | 1.42 | Async runtime |
| serde | 1.0 | Serialization |
| serde-norway | 0.1 | YAML parsing |
| serde_json | 1.0 | JSON parsing |
| kube | 0.95 | Kubernetes types |
| reqwest | 0.12 | HTTP client |
| thiserror | 2.0 | Error types |
| anyhow | 1.0 | Error handling |
| tracing | 0.1 | Logging |
| walkdir | 2.5 | Directory traversal |
| sha2 | 0.10 | Hashing |
| hex | 0.4 | Hex encoding |
| base64 | 0.22 | Base64 encoding |
| rand | 0.8 | Random generation |

### Optional (Future)
| Crate | Version | Purpose |
|-------|---------|---------|
| toml | 0.8 | TOML config |
| git2 | 0.19 | Git operations |
| bcrypt | 0.15 | Password hashing |

### Development Only
| Crate | Version | Purpose |
|-------|---------|---------|
| tokio-test | 0.4 | Async testing |
| assert_cmd | 2.0 | CLI testing |
| tempfile | 3.13 | Temp files |
| criterion | 0.5 | Benchmarking |

## Comparison with Python Stack

| Python Library | Rust Equivalent | Notes |
|----------------|-----------------|-------|
| Typer | clap | Similar ergonomics with derive |
| Jinja2 | minijinja | Compatible syntax, faster |
| PyYAML | serde-norway | More strict parsing, YAML 1.2 |
| kubernetes | kube-rs | Similar functionality |
| requests | reqwest | Async-first design |
| loguru | tracing | Structured logging |
| databind | serde | Built into ecosystem |
| bcrypt | bcrypt (future) | Same algorithm |
| filelock | fs2 crate (future) | Cross-platform locking |

## License Compatibility

All selected crates use permissive licenses:
- **MIT/Apache-2.0**: Most Rust crates (dual licensed)
- **Compatible with Nyl's current license**

No GPL or copyleft dependencies.
