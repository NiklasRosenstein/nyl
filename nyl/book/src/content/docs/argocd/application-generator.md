---
title: 'ApplicationGenerator Reference'
---

The ApplicationGenerator resource enables automatic discovery and generation of ArgoCD Applications from NylRelease files in a Git repository directory.

> **Note**: Repository sources are resolved automatically by Nyl. Depending on context, Nyl may reuse an existing local checkout or clone into its Git cache. See the [Git Integration](/nyl/git-integration/) guide for cache management and resolution details.

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
    path: string            # Single selector to scan (mutually exclusive with paths)
    paths: [string]         # Multiple selectors to scan (mutually exclusive with path)
    include: [string]       # Include patterns (default: ["*.yaml", "*.yml"])
    exclude: [string]       # Exclude patterns (default: [".*", "_*", ".nyl/**"])
  project: string           # ArgoCD project (default: "default")
  syncPolicy:               # Optional sync policy for generated Applications
    automated:
      prune: bool
      selfHeal: bool
    syncOptions: [string]
  applicationNameTemplate: string  # Naming template (default: "{{ .release.name }}")
  labels: {string: string}         # Labels for generated Applications
  annotations: {string: string}    # Annotations for generated Applications
  releaseCustomization:            # Optional NylRelease override policy
    allowedPaths: [string]
    deniedPaths: [string]
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

- **path** (required if `paths` is not set): Single selector to scan
  - Relative to repository root
  - Can be a file, directory, or glob selector
  - Directory selectors are scanned non-recursively by default

- **paths** (required if `path` is not set): Multiple selectors to scan
  - Relative to repository root
  - Can include glob selectors (for example `clusters/*/apps` or `**/*.yaml`)
  - Mutually exclusive with `path`

- **include** (optional, default: `["*.yaml", "*.yml"]`): Glob patterns for files to include
  - Patterns without `/` match basename
  - Patterns with `/` match relative path from repository root
  - Multiple patterns are OR'd together

- **exclude** (optional, default: `[".*", "_*", ".nyl/**"]`): Glob patterns for files to exclude
  - Takes precedence over include patterns
  - Default excludes hidden files (`.`), underscore-prefixed files (`_`), and Nyl cache directories
  - Example: `["test_*", ".*", "backup*"]`

### spec.project

- **project** (optional, default: `"default"`): ArgoCD project name for generated Applications
  - Must be an existing ArgoCD AppProject
  - Used for RBAC and resource restrictions

### spec.syncPolicy

Optional sync policy applied to all generated Applications. `ServerSideApply=true` is always added to
`syncOptions` by default (even when `syncPolicy` is not set), ensuring that large resources such as CRDs
do not exceed the Kubernetes API client-side apply size limit.

- **automated** (optional): Enable automated sync
  - **prune** (bool): Delete resources no longer defined in Git
  - **selfHeal** (bool): Force resource state to match Git

- **syncOptions** (optional): List of sync options (merged with the `ServerSideApply=true` default)
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

### spec.releaseCustomization

Controls project-level `NylRelease.spec.argocd.applicationOverride` customization of generated Applications.

- `NylRelease` overrides are always evaluated against effective `allowedPaths`/`deniedPaths`.
- `+<field>` append overrides are matched against the canonical field path without the `+` prefix.
  - Example: `spec.syncPolicy.+syncOptions` is checked as `spec.syncPolicy.syncOptions`.
- If `releaseCustomization` is omitted, defaults are used.
- `allowedPaths` and `deniedPaths` use dotted field globs:
  - `*` matches one segment (does not cross dots)
  - `**` matches multiple segments (crosses dots)
- If both allow and deny match, deny takes precedence.
- If `allowedPaths` is omitted or null, defaults are used:
  - `metadata.annotations."pref.argocd.argoproj.io/*"`
  - `spec.info.**`
  - `spec.ignoreDifferences.**`
  - `spec.syncPolicy.**`
- If `allowedPaths` is an empty list, no fields are allowed.

Ignored fields (unsupported/disallowed/invalid) are not applied and are reported in generated `Application.spec.info`.

## File Filtering

The ApplicationGenerator scans the configured directory and applies include/exclude patterns:

### Pattern Matching

Patterns use standard glob syntax:

- `*.yaml` - Matches files ending with `.yaml`
- `*.yml` - Matches files ending with `.yml`
- `app*` - Matches files starting with `app`
- `.*` - Matches hidden files (starting with `.`)
- `_*` - Matches files starting with underscore
- `test_*.yaml` - Matches `test_*.yaml` files
- `**/*.yaml` - Recursive match for YAML files
- Exact match: `apps.yaml` - Matches only `apps.yaml`

### Filtering Logic

1. Expand selectors from `path`/`paths` into candidate files
2. File must match at least one `include` pattern
3. File must NOT match any `exclude` pattern
4. Nyl parses the file and looks for a NylRelease resource
5. If NylRelease found, an Application is generated

### Default Patterns

By default:
- **Include**: `["*.yaml", "*.yml"]` - All YAML files
- **Exclude**: `[".*", "_*", ".nyl/**"]` - Hidden files, underscore-prefixed files, and Nyl cache paths

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

### With Release-Level `+syncOptions`

Generator defaults can be extended from a discovered `NylRelease` instead of replaced.

ApplicationGenerator:

```yaml
apiVersion: argocd.nyl.niklasrosenstein.github.com/v1
kind: ApplicationGenerator
metadata:
  name: home-lab
  namespace: argocd
spec:
  destination:
    server: https://kubernetes.default.svc
    namespace: argocd
  source:
    repoURL: git@gitlab.com:NiklasRosenstein/config.git
    targetRevision: HEAD
    path: kasoku.netbird.selfhosted/gitops/home-lab
  project: home-lab
  syncPolicy:
    syncOptions:
      - ServerSideApply=true
      - ApplyOutOfSyncOnly=true
```

NylRelease:

```yaml
apiVersion: nyl.niklasrosenstein.github.com/v1
kind: NylRelease
metadata:
  name: service-proxies
  namespace: home-proxy
spec:
  argocd:
    applicationOverride:
      spec:
        syncPolicy:
          +syncOptions:
            - RespectIgnoreDifferences=false
```

Generated Application excerpt:

```yaml
spec:
  syncPolicy:
    syncOptions:
      - ServerSideApply=true
      - ApplyOutOfSyncOnly=true
      - RespectIgnoreDifferences=false
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
      name: nyl-v2
      env:
        - name: NYL_CMP_TEMPLATE_INPUT
          value: nginx.yaml
        - name: NYL_RELEASE_NAME
          value: nginx
        - name: NYL_RELEASE_NAMESPACE
          value: web
  destination:
    server: https://kubernetes.default.svc  # From generator.spec.destination.server
    namespace: web               # From NylRelease.metadata.namespace
  syncPolicy:                    # ServerSideApply=true always included; rest from generator.spec.syncPolicy
    automated:
      prune: true
      selfHeal: true
    syncOptions:
      - ServerSideApply=true
      - CreateNamespace=true
```

## Behavior

### Processing Flow

1. When `nyl render` encounters an ApplicationGenerator resource:
   - The ApplicationGenerator is extracted and NOT included in output
   - The source path is resolved (from Git by default, or from local override path if configured)
   - The resolved source path is scanned for YAML files
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

By default, `source.path` / `source.paths` are resolved using the following precedence:

1. `NYL_APPGEN_REPO_PATH_OVERRIDE`, if set
2. The current local Git checkout, if the current `PWD` is inside a Git repository and:
   - one of the local repository remotes matches `source.repoURL` after normalization
   - `source.targetRevision` is `HEAD`, or exactly matches the current local branch name
3. ArgoCD's local checkout, when the ArgoCD plugin environment variables match
4. Nyl's normal Git cache/worktree resolution from `source.repoURL` + `source.targetRevision`

If a higher-priority resolution strategy does not match, Nyl falls back to the next one automatically.

### Current Local Repository Reuse

When you run `nyl render`, `nyl diff`, or `nyl apply` from inside the same Git repository referenced by an `ApplicationGenerator`, Nyl can reuse the current local checkout instead of creating a separate clone/worktree.

Reuse is enabled only when all of these conditions are satisfied:
- The current `PWD` is inside a Git repository
- One of that repository's remotes matches `source.repoURL` (normalized URL comparison)
- `source.targetRevision` is `HEAD`, or exactly matches the current checked-out branch name

Notes:
- A detached `HEAD` only matches `targetRevision: HEAD`
- Branch names must match exactly
- If these checks fail, Nyl falls back to ArgoCD checkout reuse or normal Git resolution

This is useful for local development because `ApplicationGenerator` sees the files currently checked out in your working tree.

### ArgoCD Local Checkout Reuse

When Nyl runs inside ArgoCD plugin context, it can also reuse ArgoCD's local checkout instead of cloning:
- `source.repoURL` must match `ARGOCD_APP_SOURCE_REPO_URL` (normalized URL comparison)
- `source.targetRevision` must exactly match `ARGOCD_APP_SOURCE_TARGET_REVISION`
- `ARGOCD_APP_SOURCE_PATH` must be resolvable to a local source directory

If these checks fail, Nyl falls back to normal Git cache/worktree resolution.

For local testing, set:

```bash
export NYL_APPGEN_REPO_PATH_OVERRIDE=/path/to/local/repo
```

Or use:

```bash
export NYL_APPGEN_REPO_PATH_OVERRIDE=@git
```

to resolve the repository root from the current `PWD`.

When this env var is set, selectors from `source.path`/`source.paths` are resolved under that local directory and no local checkout detection, ArgoCD checkout reuse, clone, or worktree checkout is performed for ApplicationGenerator processing.

If the override path is missing/invalid, `@git` is used outside a Git repository, or the resulting source path does not exist, `nyl render` fails with a configuration error.

For generated Applications:
- The `path` field points to the directory containing the NylRelease file
- Relative to the repository root

### Validation

ApplicationGenerator is validated when parsed:

- `spec.destination.server` must not be empty
- `spec.destination.namespace` must not be empty
- `spec.source.repoURL` must not be empty
- Exactly one of `spec.source.path` or `spec.source.paths` must be set
- `spec.source.path` / `spec.source.paths` selectors must not be empty

Invalid ApplicationGenerator resources cause `nyl render` to fail with a clear error message.

## Limitations (Phase 1)

Current limitations:

- **No templating**: `applicationNameTemplate` only supports basic substitution
- **Single repository**: Cannot scan multiple Git repositories in one generator

These limitations are addressed in Phase 2 (future enhancement).

## Next Steps

- [Bootstrapping Guide](/nyl/argocd/bootstrapping/) - Step-by-step setup
- [Best Practices](/nyl/argocd/best-practices/) - Production recommendations
