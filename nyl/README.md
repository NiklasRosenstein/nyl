# Nyl

> **Kubernetes manifest generator with Helm integration**

[![Release](https://img.shields.io/github/v/release/NiklasRosenstein/nyl)](https://github.com/NiklasRosenstein/nyl/releases)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)

Fast, efficient Kubernetes manifest generation with a small operational footprint.

## ✨ Features

- 🚀 **Blazing Fast**: Optimized manifest generation
- 📦 **Simple Distribution**: Single-binary CLI install flow
- 🧩 **Component-Oriented**: Local component charts, aliases, and remote chart shortcuts
- 🔧 **Helm Integration**: First-class Helm chart support with value customization
- 🌍 **Multi-Environment**: Profile-based configurations (dev, staging, prod)
- 🔐 **Git Support**: Private repository access with SSH/HTTPS authentication
- 🎨 **Templating**: Jinja2-compatible templates with custom filters
- ☸️ **Kubernetes Native**: kubectl-style diff and apply commands

## 📥 Installation

### From Release (Recommended)

```bash
# Download latest release for your platform
# Linux (x86_64)
curl -LO https://github.com/NiklasRosenstein/nyl/releases/latest/download/nyl-x86_64-unknown-linux-gnu.tar.gz
tar xzf nyl-x86_64-unknown-linux-gnu.tar.gz
sudo mv nyl /usr/local/bin/

# macOS (Apple Silicon)
curl -LO https://github.com/NiklasRosenstein/nyl/releases/latest/download/nyl-aarch64-apple-darwin.tar.gz
tar xzf nyl-aarch64-apple-darwin.tar.gz
sudo mv nyl /usr/local/bin/

# macOS (Intel)
curl -LO https://github.com/NiklasRosenstein/nyl/releases/latest/download/nyl-x86_64-apple-darwin.tar.gz
tar xzf nyl-x86_64-apple-darwin.tar.gz
sudo mv nyl /usr/local/bin/
```

### From Source

```bash
# Clone repository
git clone https://github.com/NiklasRosenstein/nyl.git
cd nyl/nyl

# Build and install
cargo install --path .

# Or just build
cargo build --release
# Binary will be at target/release/nyl
```

### Using Cargo

```bash
cargo install --git https://github.com/NiklasRosenstein/nyl nyl
```

## 🚀 Quick Start

```bash
# Create a new project
nyl new project my-app
cd my-app

# Validate configuration
nyl validate

# Render manifests for development
nyl render --profile dev

# See diff against cluster
nyl diff --profile dev

# Apply to cluster
nyl apply --profile dev
```

## 📚 Commands

| Command | Description |
|---------|-------------|
| `nyl new project <name>` | Create new project with scaffolding |
| `nyl new component <api-version> <kind>` | Create component definition |
| `nyl validate [--strict]` | Validate project configuration |
| `nyl render [--profile NAME]` | Render manifests to stdout |
| `nyl diff [--profile NAME]` | Show kubectl diff against cluster |
| `nyl apply [--profile NAME]` | Apply manifests to cluster |
| `nyl generate argocd` | Generate ArgoCD Applications |
| `nyl cluster-info` | Display cluster version information |

## 📖 Documentation

Comprehensive documentation is available in mdbook format:

```bash
# Serve documentation locally
mdbook serve book --open

# Or view online
# https://niklasrosenstein.github.io/nyl/
```

**Documentation includes:**
- Getting started guide
- Component system guide
- Configuration reference
- Command documentation
- API reference (rustdoc)
- Example projects

Key entry points:
- [Component System](./book/src/components/overview.md)
- [Component Reference](./book/src/reference/resources/component.md)

## 🎯 Examples

Check out the [`examples/`](./examples/) directory for practical usage:

- **[simple-app](./examples/simple-app/)** - Basic web application with profiles
- **More examples coming soon!**

## 🏗️ Architecture

```
src/
├── cli/          # Command-line interface with clap
│   ├── commands/ # Command implementations (render, diff, apply, etc.)
│   └── output/   # Output formatting with colored diffs
├── config/       # Project configuration loading
├── template/     # MiniJinja templating engine
├── generator/    # Manifest generation pipeline
├── kubernetes/   # Kubernetes client integration (kube-rs)
├── resources/    # HelmChart, Component resources
├── git/          # Git repository management with authentication
├── helm/         # Helm chart processing
├── components/   # Component discovery and registry
├── profiles/     # Profile management
├── secrets/      # Secrets provider framework
└── util/         # Utilities (fs, hashing, etc.)
```

## ⚙️ Development

### Prerequisites

- Rust 1.83 or newer
- kubectl (for diff/apply commands)
- Helm (for Helm chart rendering)

### Building

```bash
# Development build
cargo build

# Release build (optimized)
cargo build --release

# Run tests
cargo test

# Format and lint
cargo fmt
cargo clippy -- -D warnings
```

### Using mise

The project includes a `.mise.toml` for tool management:

```bash
# Install tools
mise install

# Run tasks
mise run fmt          # Format code
mise run lint         # Run clippy
mise run test         # Run tests
mise run docs-serve   # Serve documentation
```

## 🧪 Testing

```bash
# Run all tests
cargo test

# Run with coverage
cargo tarpaulin --out Html

# Test specific module
cargo test --lib config::tests

# Integration tests
cargo test --test integration_test
```

**Test Coverage:**
- 221 unit tests
- 34 integration tests
- 90%+ code coverage

## 🗺️ Roadmap

### ✅ Completed (v0.1.0)

- [x] Configuration & CLI foundation
- [x] Template engine (Jinja2-compatible)
- [x] Helm integration
- [x] Component system
- [x] Git repository support with authentication
- [x] Kubernetes client integration
- [x] Diff & apply commands
- [x] ArgoCD integration

### 🔮 Future Releases

- [ ] **v0.2.0**: SOPS secrets integration
- [ ] **v0.3.0**: SSH tunnel & profile enhancements
- [ ] **v0.4.0**: Advanced post-processing
- [ ] **v0.5.0**: Performance optimizations & caching improvements

## 📄 License

MIT License - see [LICENSE](../LICENSE) for details

## 🙏 Contributing

Contributions are welcome! Please read [CONTRIBUTING.md](../CONTRIBUTING.md) for guidelines.

### Contributors

- Niklas Rosenstein ([@NiklasRosenstein](https://github.com/NiklasRosenstein))

## 📝 Changelog

See [CHANGELOG.md](./CHANGELOG.md) for a detailed history of changes.

## 🔗 Links

- [Repository](https://github.com/NiklasRosenstein/nyl)
- [Issues](https://github.com/NiklasRosenstein/nyl/issues)
- [Releases](https://github.com/NiklasRosenstein/nyl/releases)
- [Documentation](https://niklasrosenstein.github.io/nyl/)

---

**Made with ❤️ in Rust**
