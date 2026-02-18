# Nyl Repository

This repository contains the Nyl project and related components for Kubernetes manifest generation.

## 📦 Repository Contents

### [`nyl/`](nyl/) - The Nyl Tool
Fast, efficient Kubernetes manifest generator.

**Key features:**
- 🚀 Blazing fast manifest generation
- 🔧 Helm integration
- 🎨 Jinja2-compatible templating

→ See [nyl/README.md](nyl/README.md) for installation and usage details.

### [`docker/`](docker/) - ArgoCD Config Management Plugin
Docker image containing Nyl and ArgoCD CMP Server for use as an ArgoCD plugin.

**Includes:**
- Nyl (Rust binary)
- ArgoCD CMP Server
- Helm, SOPS, Kyverno

→ See [docker/README.md](docker/README.md) for build instructions.

### [`chart/`](chart/) - ArgoCD Helm Chart
Helm chart to deploy ArgoCD with Nyl as a Config Management Plugin.

→ See [chart/README.md](chart/README.md) for installation instructions.

## 🚀 Quick Start

```bash
# Install Nyl from releases
curl -LO https://github.com/NiklasRosenstein/nyl/releases/latest/download/nyl-x86_64-unknown-linux-gnu.tar.gz
tar xzf nyl-x86_64-unknown-linux-gnu.tar.gz
sudo mv nyl /usr/local/bin/

# Create a new project
nyl new project my-app
cd my-app

# Render manifests
nyl render --profile dev
```

## 🔧 Development

Install development tools with [mise](https://mise.jdx.dev/):

```bash
mise install
eval "$(mise activate)"

# Build and test
mise run build       # Build release binary
mise run test        # Run tests
mise run lint        # Run clippy
mise run fmt         # Format code
mise run pre-commit  # Run all checks

# Documentation
mise run docs-serve  # Serve mdbook documentation
```

## 📚 Documentation

- **[Nyl Tool Documentation](nyl/README.md)** - Installation, commands, examples
- **[Component System Guide](nyl/book/src/components/overview.md)** - Component authoring, resolution, shortcuts, aliases
- **[Online Docs](https://niklasrosenstein.github.io/nyl/)** - Complete documentation (mdbook)

## 📄 License

MIT License - see LICENSE for details.
