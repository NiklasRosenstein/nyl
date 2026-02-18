# generate

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

## Relation to ApplicationGenerator

The `nyl generate argocd` command is a **manual CLI tool** for one-time generation of ArgoCD Applications from a directory of Nyl releases. It's useful for:

- Initial ArgoCD bootstrap
- One-off Application generation
- CI/CD pipelines that don't use ApplicationGenerator

The **ApplicationGenerator resource** is the **recommended approach** for ongoing management. It provides:

- Automatic discovery and generation during `nyl render`
- Integration with ArgoCD plugin for GitOps
- Declarative configuration
- Self-hosting bootstrap pattern

**When to use each:**

| Use Case | Tool | Why |
|----------|------|-----|
| Bootstrap ArgoCD | ApplicationGenerator | Declarative, self-hosting |
| Ongoing management | ApplicationGenerator | Automatic, GitOps-native |
| One-time bootstrap | `nyl generate argocd` | Manual control |
| CI/CD generation | `nyl generate argocd` | Explicit generation step |

**Recommendation**: Use ApplicationGenerator for most use cases. It's processed during `nyl render` and integrates seamlessly with ArgoCD.

## See Also

- [ApplicationGenerator Resource](../argocd/application-generator.md)
- [ArgoCD Bootstrapping](../argocd/bootstrapping.md)
- [render Command](./render.md)
