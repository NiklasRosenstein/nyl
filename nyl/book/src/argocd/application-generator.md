# ApplicationGenerator Reference

The ApplicationGenerator resource enables automatic discovery and generation of ArgoCD Applications from NylRelease files in a Git repository directory.

> **Note**: Git repositories are cloned automatically by Nyl. You don't need to manually clone repositories. See the [Git Integration](../git-integration.md) guide for cache management and configuration details.

## Resource Definition

```yaml
apiVersion: argocd.nyl.niklasrosenstein.github.com/v1
kind: ApplicationGenerator
metadata:
  name: string              # Generator name (optional)
  namespace: string         # Namespace (optional, typically 'argocd')
spec:
  destination:              # Required
    server: string          # Kubernetes API server URL
    namespace: string       # Namespace for generated Applications
  source:                   # Required
    repoURL: string         # Git repository URL
    targetRevision: string  # Branch, tag, or commit (default: "HEAD")
    path: string            # Directory to scan
    include: [string]       # Include patterns (default: ["*.yaml", "*.yml"])
    exclude: [string]       # Exclude patterns (default: [".*", "_*"])
  project: string           # ArgoCD project (default: "default")
  syncPolicy:               # Optional sync policy for generated Applications
    automated:
      prune: bool
      selfHeal: bool
    syncOptions: [string]
  applicationNameTemplate: string  # Naming template (default: "{{ .release.name }}")
  labels: {string: string}         # Labels for generated Applications
  annotations: {string: string}    # Annotations for generated Applications
```

## Field Reference

### metadata

Standard Kubernetes metadata for the ApplicationGenerator resource itself.

- **name** (optional): Identifier for this generator
- **namespace** (optional): Namespace where this resource lives (typically `argocd`)

### spec.destination

Defines where generated Applications should be created and what cluster they target.

- **server** (required): Kubernetes API server URL for the target cluster
  - Example: `https://kubernetes.default.svc` (in-cluster)
  - Example: `https://my-cluster.example.com:6443` (external cluster)

- **namespace** (required): Namespace where generated Applications are created
  - Typically `argocd`
  - Must match where ArgoCD is installed

### spec.source

Configures the Git repository and directory scanning behavior.

- **repoURL** (required): Git repository URL
  - HTTPS: `https://github.com/org/repo.git`
  - SSH: `git@github.com:org/repo.git`

- **targetRevision** (optional, default: `"HEAD"`): Git reference to use
  - Branch: `main`, `develop`
  - Tag: `v1.0.0`
  - Commit: `abc123def456`

- **path** (required): Directory path to scan for YAML files
  - Relative to repository root
  - Example: `clusters/production`
  - Example: `apps`

- **include** (optional, default: `["*.yaml", "*.yml"]`): Glob patterns for files to include
  - Supports simple glob syntax: `*.yaml`, `*.yml`, `app*.yaml`
  - Multiple patterns are OR'd together

- **exclude** (optional, default: `[".*", "_*"]`): Glob patterns for files to exclude
  - Takes precedence over include patterns
  - Default excludes hidden files (`.`) and underscore-prefixed files (`_`)
  - Example: `["test_*", ".*", "backup*"]`

### spec.project

- **project** (optional, default: `"default"`): ArgoCD project name for generated Applications
  - Must be an existing ArgoCD AppProject
  - Used for RBAC and resource restrictions

### spec.syncPolicy

Optional default sync policy applied to all generated Applications.

- **automated** (optional): Enable automated sync
  - **prune** (bool): Delete resources no longer defined in Git
  - **selfHeal** (bool): Force resource state to match Git

- **syncOptions** (optional): List of sync options
  - `CreateNamespace=true`: Create destination namespace if missing
  - `PruneLast=true`: Prune resources after other operations
  - `RespectIgnoreDifferences=true`: Respect ignore differences
  - `ApplyOutOfSyncOnly=true`: Only apply resources that are out of sync

### spec.applicationNameTemplate

- **applicationNameTemplate** (optional, default: `"{{ .release.name }}"`): Template for Application names
  - Currently supports: `{{ .release.name }}`, `{{ .release.namespace }}`
  - Future: Full Handlebars/Tera template support

### spec.labels

Key-value map of labels to add to all generated Applications.

Example:
```yaml
labels:
  managed-by: nyl
  team: platform
  environment: production
```

### spec.annotations

Key-value map of annotations to add to all generated Applications.

Example:
```yaml
annotations:
  docs-url: https://wiki.example.com/apps
  team-slack: "#platform-team"
```

## File Filtering

The ApplicationGenerator scans the configured directory and applies include/exclude patterns:

### Pattern Matching

Patterns use simple glob syntax:

- `*.yaml` - Matches files ending with `.yaml`
- `*.yml` - Matches files ending with `.yml`
- `app*` - Matches files starting with `app`
- `.*` - Matches hidden files (starting with `.`)
- `_*` - Matches files starting with underscore
- `test_*.yaml` - Matches `test_*.yaml` files
- Exact match: `apps.yaml` - Matches only `apps.yaml`

### Filtering Logic

1. File must be a regular file (not directory)
2. File must match at least one `include` pattern
3. File must NOT match any `exclude` pattern
4. Nyl parses the file and looks for a NylRelease resource
5. If NylRelease found, an Application is generated

### Default Patterns

By default:
- **Include**: `["*.yaml", "*.yml"]` - All YAML files
- **Exclude**: `[".*", "_*"]` - Hidden files and underscore-prefixed files

This prevents accidental inclusion of:
- Hidden files like `.secrets.yaml`, `.git/`
- Backup files like `_backup.yaml`
- Test files like `_test_app.yaml`

## Examples

### Basic Usage

Simplest ApplicationGenerator for scanning a directory:

```yaml
apiVersion: argocd.nyl.niklasrosenstein.github.com/v1
kind: ApplicationGenerator
metadata:
  name: my-apps
  namespace: argocd
spec:
  destination:
    server: https://kubernetes.default.svc
    namespace: argocd
  source:
    repoURL: https://github.com/myorg/gitops.git
    path: apps
```

This scans `apps/` for `*.yaml` and `*.yml` files, generates an Application for each NylRelease found.

### With Automated Sync

Enable automatic synchronization and pruning:

```yaml
apiVersion: argocd.nyl.niklasrosenstein.github.com/v1
kind: ApplicationGenerator
metadata:
  name: production-apps
  namespace: argocd
spec:
  destination:
    server: https://kubernetes.default.svc
    namespace: argocd
  source:
    repoURL: https://github.com/myorg/gitops.git
    targetRevision: production
    path: clusters/production
  project: production
  syncPolicy:
    automated:
      prune: true
      selfHeal: true
    syncOptions:
      - CreateNamespace=true
      - PruneLast=true
```

### Custom File Filtering

Include only specific files and exclude test files:

```yaml
apiVersion: argocd.nyl.niklasrosenstein.github.com/v1
kind: ApplicationGenerator
metadata:
  name: filtered-apps
  namespace: argocd
spec:
  destination:
    server: https://kubernetes.default.svc
    namespace: argocd
  source:
    repoURL: https://github.com/myorg/gitops.git
    path: apps
    include:
      - "prod-*.yaml"
      - "core-*.yaml"
    exclude:
      - ".*"
      - "_*"
      - "test-*"
      - "*-backup.yaml"
```

### With Labels and Annotations

Add metadata to generated Applications:

```yaml
apiVersion: argocd.nyl.niklasrosenstein.github.com/v1
kind: ApplicationGenerator
metadata:
  name: team-apps
  namespace: argocd
spec:
  destination:
    server: https://kubernetes.default.svc
    namespace: argocd
  source:
    repoURL: https://github.com/myorg/gitops.git
    path: teams/platform/apps
  labels:
    team: platform
    managed-by: nyl
    environment: production
  annotations:
    team-slack: "#platform-team"
    oncall-pagerduty: "P123ABC"
```

### Multi-Cluster Setup

Generate Applications for an external cluster:

```yaml
apiVersion: argocd.nyl.niklasrosenstein.github.com/v1
kind: ApplicationGenerator
metadata:
  name: staging-cluster-apps
  namespace: argocd
spec:
  destination:
    server: https://staging-cluster.example.com:6443
    namespace: argocd
  source:
    repoURL: https://github.com/myorg/gitops.git
    targetRevision: main
    path: clusters/staging
  project: staging
  syncPolicy:
    automated:
      selfHeal: true
```

Note: The cluster must be registered in ArgoCD first.

### Multiple Generators

You can have multiple ApplicationGenerators in the same file:

```yaml
apiVersion: argocd.nyl.niklasrosenstein.github.com/v1
kind: ApplicationGenerator
metadata:
  name: core-apps
spec:
  destination:
    server: https://kubernetes.default.svc
    namespace: argocd
  source:
    repoURL: https://github.com/myorg/gitops.git
    path: core
  project: core
---
apiVersion: argocd.nyl.niklasrosenstein.github.com/v1
kind: ApplicationGenerator
metadata:
  name: addon-apps
spec:
  destination:
    server: https://kubernetes.default.svc
    namespace: argocd
  source:
    repoURL: https://github.com/myorg/gitops.git
    path: addons
  project: addons
```

## Generated Application Structure

For a NylRelease file like this:

```yaml
# clusters/default/nginx.yaml
apiVersion: nyl.niklasrosenstein.github.com/v1
kind: NylRelease
metadata:
  name: nginx
  namespace: web
---
# ... other resources ...
```

The ApplicationGenerator produces:

```yaml
apiVersion: argoproj.io/v1alpha1
kind: Application
metadata:
  name: nginx                    # From NylRelease.metadata.name
  namespace: argocd              # From generator.spec.destination.namespace
  labels:                        # From generator.spec.labels
    managed-by: nyl
  annotations:                   # From generator.spec.annotations
    docs-url: https://wiki.example.com/apps
spec:
  project: default               # From generator.spec.project
  source:
    repoURL: https://github.com/myorg/gitops.git  # From generator.spec.source.repoURL
    path: clusters/default       # Directory of the file
    targetRevision: HEAD         # From generator.spec.source.targetRevision
    plugin:
      name: nyl
      env:
        - name: NYL_RELEASE_NAME
          value: nginx
        - name: NYL_RELEASE_NAMESPACE
          value: web
  destination:
    server: https://kubernetes.default.svc  # From generator.spec.destination.server
    namespace: web               # From NylRelease.metadata.namespace
  syncPolicy:                    # From generator.spec.syncPolicy
    automated:
      prune: true
      selfHeal: true
    syncOptions:
      - CreateNamespace=true
```

## Behavior

### Processing Flow

1. When `nyl render` encounters an ApplicationGenerator resource:
   - The ApplicationGenerator is extracted and NOT included in output
   - The source path is scanned for YAML files
   - Files are filtered by include/exclude patterns
   - Each file is parsed for a NylRelease resource
   - An ArgoCD Application is generated for each NylRelease
   - Generated Applications are added to the output

2. The ApplicationGenerator acts as an "offline controller" pattern:
   - Processing happens during `nyl render`
   - No runtime controller or operator needed
   - Deterministic output (same input always produces same output)
   - Works in ArgoCD plugin context

### Path Resolution

The `source.path` in ApplicationGenerator is resolved relative to the base directory:

- If rendering a file: `/path/to/apps.yaml` → base is `/path/to`
- If rendering a directory: `/path/to/project` → base is `/path/to/project`
- Source path `clusters/default` → scans `/path/to/clusters/default`

For generated Applications:
- The `path` field points to the directory containing the NylRelease file
- Relative to the repository root

### Validation

ApplicationGenerator is validated when parsed:

- `spec.destination.server` must not be empty
- `spec.destination.namespace` must not be empty
- `spec.source.repoURL` must not be empty
- `spec.source.path` must not be empty

Invalid ApplicationGenerator resources cause `nyl render` to fail with a clear error message.

## Limitations (Phase 1)

Current limitations:

- **No Git cloning**: Repositories must be pre-cloned (suitable for ArgoCD plugin use)
- **Simple glob patterns**: No full glob library support (e.g., `**/*.yaml` not supported)
- **No templating**: `applicationNameTemplate` only supports basic substitution
- **Single repository**: Cannot scan multiple Git repositories in one generator

These limitations are addressed in Phase 2 (future enhancement).

## Next Steps

- [Bootstrapping Guide](./bootstrapping.md) - Step-by-step setup
- [Best Practices](./best-practices.md) - Production recommendations
