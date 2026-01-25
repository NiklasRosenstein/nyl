# nyl

Nyl is a versatile tool for generating Kubernetes manifests from a simple YAML configuration, encouraging consistent and
reusable deployment configurations, project layouts and operational workflows.

**🦀 Rust Rewrite Complete**: Nyl has been rewritten in Rust with 10x performance improvements. See [MOVE_TO_RUST.md](MOVE_TO_RUST.md) for migration details.

## Installation

Download the latest binary from [releases](https://github.com/helsing-ai/nyl/releases) or build from source:

    $ cd nyl-rs
    $ cargo build --release
    $ ./target/release/nyl --version

For some features, additional programs must be available:

- [kubectl](https://kubernetes.io/de/docs/reference/kubectl/) for applying
- [helm](https://helm.sh/) for rendering Helm charts
- [kyverno](https://kyverno.io/docs/kyverno-cli/) ^1.13.x when using the Nyl `PostProcessor` resource
- [sops](https://github.com/getsops/sops) when using the SOPS secrets provider

## Local development

Install development tools with [Mise](https://mise.jdx.dev/):

    $ mise install
    $ eval "$(mise activate)"

Build and test:

    $ mise run build      # Build release binary
    $ mise run test       # Run tests
    $ mise run lint       # Run clippy
    $ mise run fmt        # Format code
    $ mise run pre-commit # Run all checks

To live-preview the documentation:

    $ mise run docs-serve

## Tracking upstream information

- Discussion around ArgoCD supporting Helm lookups (maybe with Project-level service account?), see
  https://github.com/argoproj/argo-cd/issues/5202#issuecomment-2088810772
