# Nyl Repository

This repository contains the Nyl project and related components for Kubernetes manifest generation.

## 📦 Repository Contents

### [`nyl/`](nyl/) - The Nyl Tool
Fast, efficient Kubernetes manifest generator written in Rust. Complete rewrite with 10x performance improvements.

**Key features:**
- 🚀 Blazing fast manifest generation
- 💾 Memory efficient (<50MB RAM)
- 📦 Single static binary (8.5MB)
- 🔧 Helm integration
- 🎨 Jinja2-compatible templating

→ See [nyl/README.md](nyl/README.md) for installation and usage details.

### [`argocd-cmp/`](argocd-cmp/) - ArgoCD Config Management Plugin
Docker image containing Nyl and ArgoCD CMP Server for use as an ArgoCD plugin.

**Includes:**
- Nyl (Rust binary)
- ArgoCD CMP Server
- Helm, SOPS, Kyverno

→ See [argocd-cmp/README.md](argocd-cmp/README.md) for build instructions.

### [`argocd-with-nyl/`](argocd-with-nyl/) - ArgoCD Bootstrap Example
Example Kubernetes manifest to deploy ArgoCD with Nyl as a Config Management Plugin.

→ See [argocd-with-nyl/README.md](argocd-with-nyl/README.md) for deployment guide.

## 🚀 Quick Start

```bash
# Install Nyl from releases
curl -LO https://github.com/helsing-ai/nyl/releases/latest/download/nyl-x86_64-unknown-linux-gnu.tar.gz
tar xzf nyl-x86_64-unknown-linux-gnu.tar.gz
sudo mv nyl /usr/local/bin/

# Create a new project
nyl new project my-app
cd my-app

# Render manifests
nyl render --environment dev
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
- **[Migration Guide](MOVE_TO_RUST.md)** - Python to Rust migration details
- **[Online Docs](https://helsing-ai.github.io/nyl/)** - Complete documentation (mdbook)

## 🦀 Rust Rewrite

Nyl has been completely rewritten in Rust with significant improvements:

| Metric | Improvement |
|--------|-------------|
| Performance | 10x faster |
| Memory | 75% reduction |
| Binary size | 8.5MB (vs ~200MB+ image) |
| Cold start | <50ms |

See [MOVE_TO_RUST.md](MOVE_TO_RUST.md) for complete migration details.

## 📄 License

MIT License - see LICENSE for details.
