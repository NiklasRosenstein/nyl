# Nyl

Nyl is a fast Kubernetes manifest generator built in Rust, with Helm-based components, remote manifest support, profile-aware rendering, and ArgoCD integration.

## Highlights

- Component-oriented workflow (Helm chart-backed resources)
- `RemoteManifest` resources for HTTPS-hosted YAML/JSON
- Jinja2-compatible templating (MiniJinja)
- Profile-based environment config (for example: `dev`, `staging`, `prod`)
- `render`, `diff`, and `apply` commands
- ArgoCD integration via CMP container, Helm chart, and `ApplicationGenerator` resource

## Quick Start

```bash
# install from release (Linux x86_64)
curl -LO https://github.com/NiklasRosenstein/nyl/releases/latest/download/nyl-x86_64-unknown-linux-gnu.tar.gz
tar xzf nyl-x86_64-unknown-linux-gnu.tar.gz
sudo mv nyl /usr/local/bin/

nyl new project my-app
cd my-app
nyl render --profile dev  # profile name is project-defined; "dev" is only an example
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

### ArgoCD Bootstrap

Nyl ships ArgoCD integration assets in this repo:

- `docker/`: Nyl CMP container image
- `chart/`: Helm chart to deploy ArgoCD with Nyl
- `examples/argocd-bootstrap/`: self-managing ArgoCD bootstrap using `ApplicationGenerator`

```bash
export NYL_REPO_URL="https://github.com/NiklasRosenstein/nyl-rs.git"
nyl apply examples/argocd-bootstrap/bootstrap.yaml
```

## Repository Layout

- `nyl/`: main Rust crate and CLI
- `docker/`: ArgoCD CMP image
- `chart/`: ArgoCD Helm chart
- `examples/`: runnable examples

## Docs

- Full docs: https://niklasrosenstein.github.io/nyl/
- CLI and development details: [nyl/README.md](nyl/README.md)
- Components guide: [nyl/book/src/components/overview.md](nyl/book/src/components/overview.md)

## License

MIT (see `LICENSE`).
