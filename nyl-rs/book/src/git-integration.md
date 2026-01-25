# Git Integration

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
apiVersion: v1.nyl.io
kind: HelmChart
metadata:
  name: nginx
spec:
  chart:
    git: https://github.com/bitnami/charts.git
    git_ref: main
    path: bitnami/nginx
  release:
    name: nginx
    namespace: default
```

### Parameters

- **`git`**: Git repository URL (required)
- **`git_ref`**: Branch, tag, or commit (optional, defaults to HEAD)
- **`path`**: Subdirectory within repository (optional)

### Supported Ref Types

You can reference different types of Git refs:

**Branch:**
```yaml
spec:
  chart:
    git: https://github.com/example/charts.git
    git_ref: main
```

**Tag:**
```yaml
spec:
  chart:
    git: https://github.com/example/charts.git
    git_ref: v2.1.0
```

**Commit SHA:**
```yaml
spec:
  chart:
    git: https://github.com/example/charts.git
    git_ref: abc123def456
```

**HEAD (default):**
```yaml
spec:
  chart:
    git: https://github.com/example/charts.git
    # git_ref defaults to HEAD
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
1. Clone the Git repository to cache
2. Check out the `main` branch
3. Navigate to the `apps/` directory
4. Scan for YAML files matching the include/exclude patterns
5. Generate ArgoCD Application manifests for each NylRelease found

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

**Phase 2a (Current)**: Only public repositories are supported.

**Phase 2b (Planned)**: Will support:
- SSH key authentication
- HTTPS credentials
- ArgoCD repository secrets integration
- Credential helpers

## Limitations

1. **Shallow clones not supported**: libgit2 (the underlying library) doesn't support shallow clones. Full repository history is fetched.

2. **Public repositories only**: Authentication is not yet implemented (Phase 2a).

3. **Force checkout**: When reusing worktrees, local changes are discarded. Worktrees are treated as read-only checkouts.

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
    git: https://github.com/large/repo.git
    git_ref: v1.2.3  # Specific tag, not a branch
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

### Git command not found

**Problem**: Git operations fail with "command not found"

**Solution**: Install Git:
```bash
# Ubuntu/Debian
apt-get install git

# macOS
brew install git

# Fedora/RHEL
dnf install git
```

## Examples

### Multi-environment Chart from Git

```yaml
apiVersion: v1.nyl.io
kind: HelmChart
metadata:
  name: app-production
spec:
  chart:
    git: https://github.com/company/charts.git
    git_ref: stable
    path: applications/myapp
  release:
    name: myapp
    namespace: production
  values:
    environment: production
    replicas: 5
```

### Development Branch

```yaml
apiVersion: v1.nyl.io
kind: HelmChart
metadata:
  name: app-development
spec:
  chart:
    git: https://github.com/company/charts.git
    git_ref: develop
    path: applications/myapp
  release:
    name: myapp
    namespace: development
  values:
    environment: development
    replicas: 1
```

### Specific Version

```yaml
apiVersion: v1.nyl.io
kind: HelmChart
metadata:
  name: app-stable
spec:
  chart:
    git: https://github.com/company/charts.git
    git_ref: v2.1.0
    path: applications/myapp
  release:
    name: myapp
    namespace: staging
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
   git_ref: v1.2.3  # Tag
   # or
   git_ref: abc123  # Commit SHA
   ```

2. **Use branches for development**: Use branch refs for development environments:
   ```yaml
   git_ref: develop  # Branch name
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
       git: https://github.com/company/charts.git
       path: charts/applications/myapp
   ```
