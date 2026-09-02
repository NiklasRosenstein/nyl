---
title: 'Git Integration'
---

Nyl provides built-in Git repository management for fetching Helm charts and scanning application manifests from Git repositories. This enables declarative infrastructure management without requiring manual repository cloning.

## Features

- **Bare repositories**: Minimal disk usage with shared object store
- **Worktrees**: Isolated checkouts for different refs (branches, tags, commits)
- **Lazy fetching**: Refs fetched first, objects downloaded on-demand
- **Automatic caching**: Repositories cached locally for fast subsequent access
- **Concurrent access**: Multiple refs from the same repository can be checked out simultaneously

## Cache Directory

Nyl stores Git repositories in a cache directory to avoid redundant clones and improve performance.

### Configuration

The cache directory is determined by:

1. **`NYL_CACHE_DIR` environment variable** (preferred)
2. **`.nyl/cache/` in the current directory** (fallback)

Example:
```bash
export NYL_CACHE_DIR=/var/cache/nyl
nyl render app.yaml
```

### Cache Structure

```text
$NYL_CACHE_DIR/git/
├── bare/
│   ├── {url_hash}-{repo_name}/          # Bare repository
│   │   ├── objects/
│   │   ├── refs/
│   │   └── ...
│   └── {url_hash2}-{repo_name2}/
└── worktrees/
    ├── {url_hash}-{ref_hash}/            # Worktree checkout
    ├── {url_hash}-{ref_hash2}/           # Another ref from same repo
    └── {url_hash2}-{ref_hash}/           # Different repo
```

**Key points:**
- One **bare repository** per Git URL (shared object store)
- One **worktree** per unique URL + ref combination
- Worktrees share objects from bare repo (disk-efficient)
- URL and ref hashes ensure uniqueness and avoid conflicts

## Using Git with HelmChart

HelmChart resources can reference Helm charts stored in Git repositories.

### Basic Example

```yaml
apiVersion: nyl.niklasrosenstein.github.com/v1
kind: HelmChart
metadata:
  name: nginx
  namespace: default
spec:
  chart:
    repository: git+https://github.com/bitnami/charts.git
    version: main
    name: bitnami/nginx
```

### Parameters

- **`repository`**: Git repository URL with `git+` prefix (required)
- **`version`**: Branch, tag, or commit (optional, defaults to HEAD)
- **`name`**: Subdirectory within repository (optional)

### Helm Dependencies

Nyl automatically detects and builds Helm chart dependencies for charts from Git repositories. When a chart contains a `Chart.yaml` with dependencies or a `Chart.lock` file, Nyl will:

1. Detect the presence of dependencies
2. Automatically run `helm dependency build` to fetch and build them
3. Continue with normal chart rendering

This means you can use charts with dependencies from Git repositories without any additional configuration:

```yaml
# Chart.yaml in Git repository
apiVersion: v2
name: my-app
version: 1.0.0
dependencies:
  - name: common
    version: "^1.0"
    repository: "oci://registry-1.docker.io/bitnamicharts"
  - name: postgresql
    version: "~12.0"
    repository: "https://charts.bitnami.com/bitnami"
```

The dependencies will be automatically resolved when you reference this chart:

```yaml
apiVersion: nyl.niklasrosenstein.github.com/v1
kind: HelmChart
metadata:
  name: my-app
spec:
  chart:
    repository: git+https://github.com/example/charts.git
    version: main
    name: charts/my-app  # Chart with dependencies
  release:
    name: my-app
    namespace: default
```

### Supported Ref Types

You can reference different types of Git refs:

**Branch:**
```yaml
spec:
  chart:
    repository: git+https://github.com/example/charts.git
    version: main
```

**Tag:**
```yaml
spec:
  chart:
    repository: git+https://github.com/example/charts.git
    version: v2.1.0
```

**Commit SHA:**
```yaml
spec:
  chart:
    repository: git+https://github.com/example/charts.git
    version: abc123def456
```

**HEAD (default):**
```yaml
spec:
  chart:
    repository: git+https://github.com/example/charts.git
    # version defaults to HEAD
```

## Using Git with ApplicationGenerator

ApplicationGenerator resources scan Git repositories for Nyl manifests and automatically generate ArgoCD Applications.

### Example

```yaml
apiVersion: argocd.nyl.niklasrosenstein.github.com/v1
kind: ApplicationGenerator
metadata:
  name: cluster-apps
  namespace: argocd
spec:
  destination:
    server: https://kubernetes.default.svc
    namespace: argocd
  source:
    repoURL: https://github.com/example/gitops-demo.git
    targetRevision: main
    path: apps
    include:
      - "*.yaml"
      - "*.yml"
    exclude:
      - ".*"
      - "_*"
  project: default
```

When rendered, Nyl will:
1. Resolve repository source location (override, local checkout reuse, ArgoCD checkout reuse, or cached Git worktree)
2. Use the `main` revision
3. Navigate to the `apps/` directory
4. Scan for YAML files matching the include/exclude patterns
5. Generate ArgoCD Application manifests for each Release found

### ApplicationGenerator Resolution Order

For `ApplicationGenerator`, Nyl resolves the source repository in this order:

1. `NYL_APPGEN_REPO_PATH_OVERRIDE`, if set
2. The current local Git checkout, if the current `PWD` is inside a repository whose remote matches `spec.source.repoURL`, and `spec.source.targetRevision` is `HEAD` or the current branch name
3. ArgoCD's local checkout via `ARGOCD_APP_SOURCE_*`
4. The normal Git cache/worktree flow

If one step does not match, Nyl falls through to the next step automatically.

### Current Local Checkout Reuse

When you run Nyl from inside the same repository referenced by an `ApplicationGenerator`, Nyl can skip clone/worktree operations and reuse the current local checkout.

Reuse is enabled only when all of these conditions match:
- The current `PWD` is inside a Git repository
- At least one local remote matches `spec.source.repoURL` after normalization
- `spec.source.targetRevision` is `HEAD`, or exactly matches the current checked-out branch name

Notes:
- Detached `HEAD` only matches `targetRevision: HEAD`
- If the remote or revision does not match, Nyl falls back to ArgoCD checkout reuse or the normal Git cache/worktree flow

### ArgoCD Local Checkout Reuse

When running in ArgoCD plugin context, Nyl can also reuse the local checkout that ArgoCD already prepared.

Reuse is enabled only when all of these conditions match exactly:
- `spec.source.repoURL` equals `ARGOCD_APP_SOURCE_REPO_URL` (normalized URL comparison)
- `spec.source.targetRevision` equals `ARGOCD_APP_SOURCE_TARGET_REVISION` (exact string match)
- `ARGOCD_APP_SOURCE_PATH` can be resolved relative to the current working directory

If any condition does not match, Nyl falls back to the normal Git cache/worktree resolution.

### Local Worktree Override (Testing)

For local testing, you can bypass Git clone/worktree resolution and force all `ApplicationGenerator` resources to read from a local repository root:

```bash
export NYL_APPGEN_REPO_PATH_OVERRIDE=/path/to/local/repo
nyl render apps.yaml
```

```bash
export NYL_APPGEN_REPO_PATH_OVERRIDE=@git
nyl render apps.yaml
```

Behavior:
- If `NYL_APPGEN_REPO_PATH_OVERRIDE` is set to a local path, Nyl resolves `spec.source.path`/`spec.source.paths` selectors under that path.
- If `NYL_APPGEN_REPO_PATH_OVERRIDE=@git`, Nyl discovers the Git repository root from the current `PWD` and resolves selectors under that root.
- If unset, Nyl tries current local checkout reuse first, then ArgoCD checkout reuse, then normal Git clone/cache/worktree resolution from `spec.source.repoURL` and `spec.source.targetRevision`.
- If the override path is invalid, or `@git` is used outside a Git repository, rendering fails immediately.

Selector behavior:
- `source.path` scans the selected directory non-recursively by default.
- Use glob selectors (for example `**/*.yaml`) or `source.paths` for multi-selector/recursive workflows.
- Include/exclude patterns are matched against paths relative to the repository root.

## Performance Characteristics

### Initial Clone
- **Lazy fetching**: Only refs are fetched initially (~KB), not objects (~MB/GB)
- **On-demand objects**: Commit objects fetched only when needed
- **Bandwidth efficient**: Minimal initial download

### Subsequent Access
- **Cache hit**: Near-instant if ref already checked out
- **Ref update**: Only fetch new refs if branch updated
- **Object reuse**: Worktrees share objects from bare repo

### Disk Usage
- **Bare repo**: One copy of objects for all refs
- **Worktrees**: Only working directory files (no .git directory)
- **Efficient**: Much smaller than full clones for each ref

Example disk usage for a 100MB repository with 3 refs:
- Traditional approach: 3 × 100MB = 300MB
- Nyl approach: 100MB (bare) + 3 × ~10MB (worktrees) = ~130MB

## Authentication

Nyl supports both public and private Git repositories through multiple authentication methods.

### Automatic Credential Discovery

When running inside a Kubernetes cluster with access to ArgoCD secrets, Nyl automatically discovers repository credentials from ArgoCD repository secrets. No additional configuration required!

**Example**: Using a private Helm chart

```yaml
apiVersion: nyl.niklasrosenstein.github.com/v1
kind: HelmChart
metadata:
  name: private-app
  namespace: default
spec:
  chart:
    repository: git+git@github.com:myorg/private-charts.git
    version: main
    name: charts/app
```

If an ArgoCD repository secret exists for `github.com`, Nyl will automatically use those credentials.

### Authentication Methods

1. **ArgoCD Repository Secrets** (Recommended)
   - Credentials automatically discovered from `argocd` namespace
   - Supports both SSH keys and HTTPS tokens
   - Zero configuration required
   - See [Repository Secrets](/nyl/argocd/repository-secrets/) for details

2. **SSH Agent**
   - Fallback for SSH URLs when no secret found
   - Works with local development workflows
   - Requires SSH agent running with key loaded

3. **Public Repositories**
   - No authentication needed
   - Works out of the box

### Supported Credential Types

**SSH Key Authentication:**
- Private key stored in ArgoCD secret
- Recommended for production use
- Better security than HTTPS tokens

**HTTPS Token Authentication:**
- Personal access tokens or passwords
- Useful for HTTPS-only repositories
- Stored in ArgoCD secret

### Example: Creating Repository Secret

```bash
# SSH authentication
kubectl create secret generic github-private \
  -n argocd \
  --from-literal=url=git@github.com:myorg/charts.git \
  --from-file=sshPrivateKey=$HOME/.ssh/id_rsa

kubectl label secret github-private \
  -n argocd \
  argocd.argoproj.io/secret-type=repository

# HTTPS authentication
kubectl create secret generic github-https \
  -n argocd \
  --from-literal=url=https://github.com/myorg/charts.git \
  --from-literal=username=myuser \
  --from-literal=password=ghp_token123

kubectl label secret github-https \
  -n argocd \
  argocd.argoproj.io/secret-type=repository
```

For complete documentation on authentication, see [Repository Secrets](/nyl/argocd/repository-secrets/).

## Limitations

1. **Shallow clones not supported**: libgit2 (the underlying library) doesn't support shallow clones. Full repository history is fetched.

2. **Force checkout**: When reusing worktrees, local changes are discarded. Worktrees are treated as read-only checkouts.

## Troubleshooting

### Cache directory permissions

**Problem**: Permission denied when creating cache directory

**Solution**: Set `NYL_CACHE_DIR` to a writable location:
```bash
export NYL_CACHE_DIR=$HOME/.cache/nyl
```

### Large repository performance

**Problem**: Initial clone is slow for large repositories

**Explanation**: Nyl fetches the full repository (no shallow clone support)

**Workaround**: Use a specific tag/commit to avoid fetching all branches:
```yaml
spec:
  chart:
    repository: git+https://github.com/large/repo.git
    version: v1.2.3  # Specific tag, not a branch
```

### Stale cache

**Problem**: Git repository not updating with latest changes

**Solution**: Clear the cache directory:
```bash
rm -rf $NYL_CACHE_DIR/git/
```

Or for a specific repository:
```bash
# Find the cached repo
ls $NYL_CACHE_DIR/git/bare/
# Remove it
rm -rf $NYL_CACHE_DIR/git/bare/{hash}-{repo-name}
rm -rf $NYL_CACHE_DIR/git/worktrees/{hash}-*
```

### Authentication failures

**Problem**: Cannot access private repository

**Solution**: See [Repository Secrets](/nyl/argocd/repository-secrets/) for authentication setup

## Examples

### Multi-environment Chart from Git

```yaml
apiVersion: nyl.niklasrosenstein.github.com/v1
kind: HelmChart
metadata:
  name: myapp
  namespace: production
spec:
  chart:
    repository: git+https://github.com/company/charts.git
    version: stable
    name: applications/myapp
  values:
    environment: production
    replicas: 5
```

### Development Branch

```yaml
apiVersion: nyl.niklasrosenstein.github.com/v1
kind: HelmChart
metadata:
  name: myapp
  namespace: development
spec:
  chart:
    repository: git+https://github.com/company/charts.git
    version: develop
    name: applications/myapp
  values:
    environment: development
    replicas: 1
```

### Specific Version

```yaml
apiVersion: nyl.niklasrosenstein.github.com/v1
kind: HelmChart
metadata:
  name: myapp
  namespace: staging
spec:
  chart:
    repository: git+https://github.com/company/charts.git
    version: v2.1.0
    name: applications/myapp
```

### ApplicationGenerator with Filtering

```yaml
apiVersion: argocd.nyl.niklasrosenstein.github.com/v1
kind: ApplicationGenerator
metadata:
  name: monitoring-apps
  namespace: argocd
spec:
  destination:
    server: https://kubernetes.default.svc
    namespace: argocd
  source:
    repoURL: https://github.com/company/infrastructure.git
    targetRevision: main
    path: monitoring
    include:
      - "*.yaml"
      - "*.yml"
    exclude:
      - ".*"              # Hidden files
      - "_*"              # Files starting with underscore
      - "test_*"          # Test files
  project: monitoring
  labels:
    team: platform
    category: monitoring
```

## Best Practices

1. **Pin versions in production**: Use tags or commit SHAs for production deployments:
   ```yaml
   version: v1.2.3  # Tag
   # or
   version: abc123  # Commit SHA
   ```

2. **Use branches for development**: Use branch refs for development environments:
   ```yaml
   version: develop  # Branch name
   ```

3. **Set cache directory**: Configure `NYL_CACHE_DIR` in CI/CD environments:
   ```bash
   export NYL_CACHE_DIR=/cache/nyl
   ```

4. **Monitor cache size**: Periodically clean up old worktrees if disk space is limited:
   ```bash
   find $NYL_CACHE_DIR/git/worktrees -mtime +30 -delete
   ```

5. **Use subpaths**: Keep charts in subdirectories for better organization:
   ```yaml
   spec:
     chart:
       repository: git+https://github.com/company/charts.git
       name: charts/applications/myapp
   ```
