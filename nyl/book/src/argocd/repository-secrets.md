# Repository Secrets

Nyl automatically discovers credentials from ArgoCD repository secrets to authenticate with private Git repositories. This enables seamless integration with private Helm charts and GitOps repositories.

## Overview

When Nyl encounters a Git URL in a `HelmChart` or `ApplicationGenerator` resource, it:

1. Queries ArgoCD repository secrets from the `argocd` namespace
2. Matches the Git URL to an appropriate secret
3. Uses the discovered credentials for authentication
4. Falls back to SSH agent authentication if no secret matches

This approach provides:
- **Zero configuration**: Credentials are discovered automatically from existing ArgoCD secrets
- **Security**: Credentials remain in Kubernetes secrets, never in configuration files
- **Consistency**: Same credentials used by ArgoCD and Nyl
- **Flexibility**: Supports both SSH and HTTPS authentication

## Secret Types

ArgoCD supports two types of repository credential secrets, both of which are supported by Nyl:

### Repository Secrets (Fine-Grained)

Repository secrets (`argocd.argoproj.io/secret-type=repository`) provide credentials for **specific repositories** using exact URL matching.

**Example:**

```yaml
apiVersion: v1
kind: Secret
metadata:
  name: myorg-charts-repo
  namespace: argocd
  labels:
    argocd.argoproj.io/secret-type: repository
type: Opaque
stringData:
  type: git  # Only "git" type secrets are used (defaults to "git" if omitted)
  url: git@github.com:myorg/charts.git
  sshPrivateKey: |
    -----BEGIN OPENSSH PRIVATE KEY-----
    ...
    -----END OPENSSH PRIVATE KEY-----
```

**Use when:**
- Managing credentials for individual repositories
- Need fine-grained access control
- Different credentials for different repos on same host

### Repository Credential Templates (Scalable)

Repository credential templates (`argocd.argoproj.io/secret-type=repo-creds`) provide credentials for **multiple repositories** matching a URL pattern.

**Example:**

```yaml
apiVersion: v1
kind: Secret
metadata:
  name: github-myorg-all
  namespace: argocd
  labels:
    argocd.argoproj.io/secret-type: repo-creds
type: Opaque
stringData:
  type: git  # Only "git" type secrets are used (defaults to "git" if omitted)
  url: https://github.com/myorg/*
  username: myusername
  password: ghp_token123
```

**Pattern Examples:**
- `https://github.com/myorg/*` - All repos in organization
- `https://github.com/*` - All GitHub repos
- `git@gitlab.com:*/*` - All GitLab repos with org and repo

**Use when:**
- Managing many repositories from same provider
- Centralized credential management
- Single credential applies to multiple repos

**Important:** Both secret types must have `type: git` (or omit the `type` field, which defaults to `git`). Secrets with other types (e.g., `helm`, `oci`) are ignored.

## Authentication Methods

### SSH Key Authentication

SSH key authentication is the recommended method for private repositories.

**Creating an SSH repository secret:**

```bash
# Create secret with SSH private key
kubectl create secret generic my-private-repo \
  -n argocd \
  --from-literal=url=git@github.com:myorg/charts.git \
  --from-file=sshPrivateKey=$HOME/.ssh/id_rsa

# Label the secret for ArgoCD
kubectl label secret my-private-repo \
  -n argocd \
  argocd.argoproj.io/secret-type=repository
```

**Secret structure:**

```yaml
apiVersion: v1
kind: Secret
metadata:
  name: my-private-repo
  namespace: argocd
  labels:
    argocd.argoproj.io/secret-type: repository
type: Opaque
data:
  url: Z2l0QGdpdGh1Yi5jb206bXlvcmcvY2hhcnRzLmdpdA==  # base64: git@github.com:myorg/charts.git
  sshPrivateKey: LS0tLS1CRUdJTi...                    # base64 encoded SSH private key
```

### HTTPS Token Authentication

HTTPS authentication uses personal access tokens or passwords.

**Creating an HTTPS repository secret:**

```bash
# Create secret with username and token
kubectl create secret generic my-private-repo-https \
  -n argocd \
  --from-literal=url=https://github.com/myorg/charts.git \
  --from-literal=username=myusername \
  --from-literal=password=ghp_your_token_here

# Label the secret for ArgoCD
kubectl label secret my-private-repo-https \
  -n argocd \
  argocd.argoproj.io/secret-type=repository
```

**Secret structure:**

```yaml
apiVersion: v1
kind: Secret
metadata:
  name: my-private-repo-https
  namespace: argocd
  labels:
    argocd.argoproj.io/secret-type: repository
type: Opaque
data:
  url: aHR0cHM6Ly9naXRodWIuY29tL215b3JnL2NoYXJ0cy5naXQ=  # base64: https://github.com/myorg/charts.git
  username: bXl1c2VybmFtZQ==                              # base64: myusername
  password: Z2hwX3lvdXJfdG9rZW5faGVyZQ==                  # base64: ghp_your_token_here
```

### SSH Agent Fallback

If no ArgoCD secret matches a Git URL, Nyl automatically falls back to SSH agent authentication for SSH URLs:

```bash
# Start SSH agent
eval "$(ssh-agent -s)"

# Add your SSH key
ssh-add ~/.ssh/id_rsa

# Nyl will now use the SSH agent for authentication
nyl render my-chart.yaml
```

## URL Matching

Nyl matches Git URLs to repository secrets using multiple strategies with specific precedence rules.

### Credential Selection Precedence

When multiple secrets could apply to a repository, Nyl uses this precedence:

1. **Exact match** (repository secret with exact URL match)
2. **Pattern match** (repo-creds secret with matching pattern)
3. **Hostname fallback** (repository secret with same hostname)

**Example:**

Given these secrets:

```yaml
# 1. Exact match (highest precedence)
apiVersion: v1
kind: Secret
metadata:
  name: charts-exact
  namespace: argocd
  labels:
    argocd.argoproj.io/secret-type: repository
stringData:
  url: https://github.com/myorg/charts.git
  sshPrivateKey: |
    -----BEGIN OPENSSH PRIVATE KEY-----
    ...specific key for charts repo...

# 2. Pattern match (medium precedence)
apiVersion: v1
kind: Secret
metadata:
  name: myorg-pattern
  namespace: argocd
  labels:
    argocd.argoproj.io/secret-type: repo-creds
stringData:
  url: https://github.com/myorg/*
  username: myuser
  password: ghp_token_for_myorg

# 3. Hostname fallback (lowest precedence)
apiVersion: v1
kind: Secret
metadata:
  name: github-fallback
  namespace: argocd
  labels:
    argocd.argoproj.io/secret-type: repository
stringData:
  url: https://github.com/other/repo.git
  username: myuser
  password: ghp_generic_token
```

For URL `https://github.com/myorg/charts.git`:
- Uses **exact match** (#1) ✅

For URL `https://github.com/myorg/other-repo.git`:
- No exact match
- Uses **pattern match** (#2) ✅

For URL `https://github.com/different-org/repo.git`:
- No exact match
- No pattern match
- Uses **hostname fallback** (#3) ✅

### Exact Match

The secret's `url` field exactly matches the requested URL (after normalization):

```yaml
# Secret URL (repository type)
url: https://github.com/myorg/charts.git

# Matches
- https://github.com/myorg/charts.git
- https://github.com/myorg/charts      # .git suffix optional
```

### Pattern Match

The secret's `url` field contains wildcards (`*`) that match the requested URL:

```yaml
# Secret URL (repo-creds type)
url: https://github.com/myorg/*

# Matches
- https://github.com/myorg/charts.git
- https://github.com/myorg/other-repo.git
- https://github.com/myorg/any-repo

# Does NOT match
- https://github.com/otherorg/repo.git  # Different organization
- https://gitlab.com/myorg/repo.git     # Different host
```

**Pattern Examples:**

```yaml
# Organization-level access
url: https://github.com/myorg/*
# Matches: https://github.com/myorg/repo1, https://github.com/myorg/repo2

# Host-level access (all repos on GitHub)
url: https://github.com/*
# Matches: https://github.com/anyorg/anyrepo

# SSH with pattern
url: git@github.com:myorg/*
# Matches: git@github.com:myorg/charts, git@github.com:myorg/other

# Multi-level wildcard
url: https://gitlab.com/*/*
# Matches: https://gitlab.com/group/project, https://gitlab.com/org/repo
```

### Hostname Fallback

If no exact or pattern match is found, Nyl matches by hostname. This allows a single repository secret to authenticate all repositories on the same host:

```yaml
# Secret URL (repository type)
url: https://github.com/myorg/charts.git

# Also matches (same hostname, no exact/pattern match)
- https://github.com/myorg/other-repo.git
- https://github.com/another-org/repo.git
```

### URL Normalization

URLs are normalized before matching:

- Case-insensitive comparison
- `.git` suffix is optional
- SSH shorthand converted to full URL:
  - `git@github.com:org/repo` → `ssh://git@github.com/org/repo`
- Trailing slashes removed

This means these URLs are considered equivalent:
- `https://GitHub.com/MyOrg/Repo.git`
- `https://github.com/myorg/repo`
- `git@github.com:myorg/repo` (when matching SSH patterns)

## Examples

### Private Helm Chart

```yaml
apiVersion: nyl.niklasrosenstein.github.com/v1
kind: HelmChart
metadata:
  name: my-app
spec:
  chart:
    repository: git+git@github.com:myorg/private-charts.git
    version: main
    name: charts/my-app
  release:
    name: my-app
    namespace: default
```

Nyl will:
1. Look for ArgoCD secret with URL `git@github.com:myorg/private-charts.git`
2. If not found, look for secret with hostname `github.com`
3. If not found, try SSH agent
4. Clone repository and extract chart from `charts/my-app`

### Private ApplicationGenerator

```yaml
apiVersion: argocd.nyl.niklasrosenstein.github.com/v1
kind: ApplicationGenerator
metadata:
  name: microservices
spec:
  destination:
    server: https://kubernetes.default.svc
    namespace: argocd
  source:
    repoURL: https://github.com/myorg/gitops-repo.git
    targetRevision: main
    path: apps
```

Nyl will:
1. Look for ArgoCD secret with URL `https://github.com/myorg/gitops-repo.git`
2. If found, use credentials to clone repository
3. Scan `apps/` directory for application definitions
4. Generate ArgoCD `Application` resources

## Troubleshooting

### Authentication Failures

**Error**: `Authentication failed for git@github.com:myorg/repo.git: Failed to use SSH agent`

**Solutions**:
1. Create an ArgoCD repository secret with SSH key
2. Start SSH agent and add your key: `ssh-add ~/.ssh/id_rsa`
3. Verify SSH key has access to the repository

**Error**: `No credentials found for repository: https://github.com/myorg/repo.git`

**Solutions**:
1. Create an ArgoCD repository secret for this URL
2. Verify secret has label `argocd.argoproj.io/secret-type=repository`
3. Check secret is in `argocd` namespace

### Permission Issues

**Error**: `Failed to query ArgoCD secrets: Forbidden`

**Solutions**:
1. Verify Nyl has RBAC permissions to read secrets in `argocd` namespace
2. Add ClusterRole/Role with `get`, `list` permissions on secrets
3. Bind role to Nyl service account

### URL Matching Issues

If credentials aren't being discovered:

1. Check URL format matches between secret and chart
2. Verify secret URL is normalized (lowercase, no trailing slash)
3. Test with exact URL match before relying on hostname fallback
4. Check secret has the correct label

### Debugging

Enable debug logging to see credential discovery:

```bash
RUST_LOG=debug nyl render my-chart.yaml
```

Look for log messages:
- `Discovering credentials from ArgoCD secrets`
- `Found credential for URL: ...`
- `No credential found, falling back to SSH agent`

## Best Practices

1. **Use SSH keys**: Preferred over HTTPS tokens for better security
2. **Choose the right secret type**:
   - Use `repo-creds` for organization-wide access (scalable)
   - Use `repository` for specific repos needing different credentials
   - Combine both types for flexibility with proper precedence
3. **Pattern-based credentials**: Use `repo-creds` with patterns like `https://github.com/myorg/*` to centrally manage credentials for multiple repositories
4. **Leverage precedence**: Use exact match `repository` secrets to override broader `repo-creds` patterns for specific repos
5. **Rotate credentials**: Update secrets regularly and test after rotation
6. **Least privilege**: Ensure SSH keys/tokens have minimal required permissions
7. **Monitor access**: Review ArgoCD secret access logs periodically
8. **Document secrets**: Maintain inventory of repository secrets, their patterns, and their purpose

**Example Setup:**

```yaml
# 1. Organization-wide access (repo-creds)
apiVersion: v1
kind: Secret
metadata:
  name: github-myorg-default
  labels:
    argocd.argoproj.io/secret-type: repo-creds
stringData:
  type: git
  url: https://github.com/myorg/*
  username: ci-bot
  password: ghp_org_token

---
# 2. Specific repo with different credentials (repository)
apiVersion: v1
kind: Secret
metadata:
  name: sensitive-repo-override
  labels:
    argocd.argoproj.io/secret-type: repository
stringData:
  type: git
  url: https://github.com/myorg/sensitive-repo.git
  sshPrivateKey: |
    -----BEGIN OPENSSH PRIVATE KEY-----
    ...dedicated SSH key for sensitive repo...
```

This setup provides:
- Default credentials for all `myorg` repositories via pattern match
- Override with dedicated SSH key for `sensitive-repo` via exact match
- Separation of concerns and least privilege access

## Kubernetes RBAC

Nyl requires these permissions to discover repository secrets:

```yaml
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata:
  name: nyl-secret-reader
rules:
- apiGroups: [""]
  resources: ["secrets"]
  verbs: ["get", "list"]
  # Optionally scope to argocd namespace
---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRoleBinding
metadata:
  name: nyl-secret-reader
subjects:
- kind: ServiceAccount
  name: nyl
  namespace: argocd
roleRef:
  kind: ClusterRole
  name: nyl-secret-reader
  apiGroup: rbac.authorization.k8s.io
```

## Security Considerations

- **Credential storage**: Secrets are stored in Kubernetes, encrypted at rest
- **Access control**: RBAC limits who can read repository secrets
- **Credential scope**: Each secret applies only to specific repositories
- **No plaintext**: Credentials never appear in logs or configuration files
- **SSH keys**: Use dedicated deploy keys with read-only access
- **Token rotation**: Update tokens/keys regularly and monitor for leaks
