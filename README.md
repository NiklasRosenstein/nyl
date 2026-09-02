# Nyl

Nyl is a fast Kubernetes manifest generator built in Rust, with Helm-based components, remote manifest support, cluster-aware rendering, and rendered GitOps output.

## Highlights

- Component-oriented workflow (Helm chart-backed resources)
- `RemoteManifest` resources for HTTPS-hosted YAML/JSON
- Jinja2-compatible templating (MiniJinja)
- Kubernetes-shaped Cluster and DeploymentTarget configuration
- `render`, `diff`, and `apply` commands
- Rendered manifest GitOps workflow for ArgoCD, Flux, or plain `kubectl`
- CI image with Nyl, Helm, Git, SOPS, and Kyverno CLI

## Quick Start

Install with mise:

```toml
[tools."github:NiklasRosenstein/nyl"]
version = "0.4.1"
version_prefix = "v"
```

```bash
mise install
nyl new project platform
cd platform
nyl new gitops cluster local --context kind-kind
nyl new gitops target dev
nyl render --target dev apps.yaml
```

## Feature Examples

### Component

```yaml
apiVersion: components.nyl.niklasrosenstein.github.com/v1
kind: example/v1/Nginx
metadata:
  name: my-nginx
  namespace: default
spec:
  replicas: 3
  image: nginx:1.25
```

Render:

```bash
nyl render examples/components/manifests/nginx.yaml --offline --kube-version 1.30.0 --kube-api-versions v1
```

### RemoteManifest

```yaml
apiVersion: nyl.niklasrosenstein.github.com/v1
kind: RemoteManifest
metadata:
  name: shared-crds
spec:
  url: https://example.com/platform/crds.yaml
```

### CI image

The published image is a shell-friendly rendering environment with no fixed
entrypoint:

```yaml
render:
  image: ghcr.io/niklasrosenstein/nyl:TAG
  script:
    - nyl render-tree --target production --output-dir deploy
```

## Repository Layout

- `nyl/`: main Rust crate and CLI
- `docker/`: CI rendering image
- `examples/`: runnable examples

## Docs

- Full docs: https://niklasrosenstein.github.io/nyl/
- CLI and development details: [nyl/README.md](nyl/README.md)
- Components guide: [nyl/book/src/content/docs/components/overview.md](nyl/book/src/content/docs/components/overview.md)

## License

MIT (see `LICENSE`).
