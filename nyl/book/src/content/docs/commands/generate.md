---
title: 'generate'
---

Generate auxiliary resources and configurations from Nyl manifests.

## Usage

```bash
nyl generate <subcommand> [options]
```

## Subcommands

### argocd

Generate ArgoCD Application manifests from Nyl releases.

```bash
nyl generate argocd [OPTIONS] <PATH>
```

**Arguments:**
- `<PATH>`: Directory to scan for Nyl release files

**Options:**
- `-o, --output <FILE>`: Output file (default: stdout)
- `--repo-url <URL>`: Git repository URL for generated Applications
- `--target-revision <REV>`: Target revision (branch/tag/commit)
- `--destination-server <URL>`: Kubernetes server URL (default: https://kubernetes.default.svc)
- `--destination-namespace <NS>`: Namespace for Applications (default: argocd)
- `--project <PROJECT>`: ArgoCD project name (default: default)

**Example:**

```bash
# Generate Applications for all releases in clusters/default
nyl generate argocd clusters/default \
  --repo-url https://github.com/myorg/gitops.git \
  --target-revision main \
  -o applications.yaml
```

### schema config

Generate JSON Schema for `nyl.toml` to stdout.

```bash
nyl generate schema config
```

### GitOps schemas

Generate one resource schema, the aggregate resource schema, or every published
schema:

```bash
nyl generate schema resource GitOpsTarget
nyl generate schema gitops
nyl generate schema all --output-dir book/public/reference/schemas
```

## Relation to ApplicationGenerator

The `nyl generate argocd` command is a **manual CLI tool** for one-time generation of ArgoCD Applications from a directory of Nyl releases. It's useful for:

- Initial ArgoCD bootstrap
- One-off Application generation
- CI/CD pipelines that don't use ApplicationGenerator

Rendered manifest GitOps is the recommended approach for ongoing management.
It provides:

- Target-aware rendering and deterministic output ownership
- Ordinary Argo CD directory Applications without a runtime plugin
- Reviewable diffs and protected deployment revisions

**When to use each:**

| Use Case | Tool | Why |
|----------|------|-----|
| Bootstrap ArgoCD | ApplicationGenerator | CMP-compatible bootstrap |
| Ongoing management | `render-tree` | Plain rendered manifests |
| One-time bootstrap | `nyl generate argocd` | Manual control |
| CI/CD generation | `nyl generate argocd` | Explicit generation step |

See [Rendered Manifest GitOps](/nyl/deployment-workflows/rendered-manifests/)
for the recommended workflow. ApplicationGenerator remains available for CMP
installations.

## See Also

- [ApplicationGenerator Resource](/nyl/argocd/application-generator/)
- [ArgoCD Bootstrapping](/nyl/argocd/bootstrapping/)
- [render Command](/nyl/commands/render/)
