---
title: 'Getting Started'
---

This guide gets you from an empty GitOps source project to rendered Kubernetes manifests, then points you at the workflow that matches how you deploy.

## Installation

### With mise

Add Nyl to your `mise.toml`:

```toml
[tools."github:NiklasRosenstein/nyl"]
version = "0.4.1"
version_prefix = "v"
```

Then install it:

```bash
mise install
```

### From Release

```bash
curl -LO https://github.com/NiklasRosenstein/nyl/releases/latest/download/nyl-x86_64-unknown-linux-gnu.tar.gz
tar xzf nyl-x86_64-unknown-linux-gnu.tar.gz
sudo mv nyl /usr/local/bin/
```

### From Source

```bash
cd nyl
cargo build --release
```

The binary will be available at `nyl/target/release/nyl` when building from the repository root.

### Install Locally

```bash
cd nyl
cargo install --path .
```

This installs `nyl` to `~/.cargo/bin/nyl`.

## Quick Start

### 1. Create a Project

```bash
nyl new project platform
cd platform
```

This creates:
- `nyl.toml` - Project configuration
- `components/` - Directory for components
- `applications/` - Application sources
- `config/` - Cluster, target, repository, and Argo CD policy resources

### 2. Add a Component

```bash
nyl new component v1.example.io MyApp
```

This creates a new [local component](/nyl/components/authoring-local-components/) at `components/v1.example.io/MyApp/` with:
- `Chart.yaml` - Helm chart metadata
- `values.yaml` - Default values
- `values.schema.json` - JSON schema for validation
- `templates/deployment.yaml` - Kubernetes deployment template

### 3. Validate

```bash
nyl validate
```

Output:
```
✓ Found project config: /path/to/platform/nyl.toml
✓ Components search path exists: /path/to/platform/components
✓ Helm chart search path exists: /path/to/platform

✓ Validation passed
```

For CI, use strict mode to treat warnings as errors:

```bash
nyl validate --strict
```

### 4. Render

Render a manifest file to stdout:

```bash
nyl render apps.yaml
```

For a targetless deterministic render, provide Kubernetes capabilities explicitly:

```bash
nyl render --offline --kube-version 1.31.0 --kube-api-versions v1,apps/v1 apps.yaml
```

For target-aware rendering, scaffold a Cluster and target, update the Cluster's
capabilities, then select the target:

```bash
nyl new gitops cluster local --context kind-kind
nyl new gitops target dev
nyl render --target dev apps.yaml
```

When `kind-kind` exists and the command runs in a terminal, cluster creation
offers to populate its Kubernetes capabilities immediately. Run
`nyl cluster update local` if you decline the prompt.

## Project Structure

```
platform/
├── nyl.toml                  # Project configuration
├── applications/             # Application sources
├── config/
│   ├── clusters/             # Concrete clusters and render capabilities
│   ├── argocd-instances/     # Argo CD control planes and catalog defaults
│   ├── targets/              # Cluster-to-publication bindings
│   ├── repositories/         # Named Git coordinates
│   ├── projects/             # Argo CD project policy
│   └── application-groups/   # Application selection and lifecycle policy
├── components/               # Component definitions
│   └── v1.example.io/
│       └── MyApp/
│           ├── Chart.yaml
│           ├── values.yaml
│           ├── values.schema.json
│           └── templates/
│               └── deployment.yaml
└── charts/                   # Optional: additional Helm chart search path
```

## Next Steps

- Use the [rendered manifest pattern](/nyl/deployment-workflows/rendered-manifests/) when CI should produce plain YAML for ArgoCD, Flux, or another reconciler.
- Use [CLI-first workflows](/nyl/deployment-workflows/cli-workflows/) for debugging, bootstrapping, testing, and direct cluster operations.
- Use [ArgoCD CMP integration](/nyl/argocd/overview/) when ArgoCD should render Nyl inputs directly.
- Read about [Configuration](/nyl/configuration/) and the [Component System](/nyl/components/overview/).
