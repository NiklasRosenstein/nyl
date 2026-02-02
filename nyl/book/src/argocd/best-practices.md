# Best Practices

This guide covers recommended patterns and best practices for using Nyl with ArgoCD in production environments.

## Directory Structure

### Recommended Layout

Organize your GitOps repository with clear separation of concerns:

```
gitops-repo/
├── bootstrap/
│   └── bootstrap.yaml          # Initial bootstrap manifest
├── argocd/
│   └── argocd.yaml            # ArgoCD installation
├── apps.yaml                   # ApplicationGenerator for apps
├── clusters/
│   ├── production/
│   │   ├── app1.yaml
│   │   ├── app2.yaml
│   │   └── app3.yaml
│   ├── staging/
│   │   ├── app1.yaml
│   │   └── app2.yaml
│   └── development/
│       └── app1.yaml
├── base/                       # Shared base configurations
│   ├── postgres/
│   └── redis/
├── nyl-project.yaml
├── nyl-profiles.yaml
└── nyl-secrets.yaml
```

### Directory Organization Strategies

**By Environment**:
```
clusters/
├── production/
├── staging/
└── development/
```
- Clear separation of environments
- Easy to apply different policies per environment
- Suitable for ApplicationGenerator scanning

**By Team**:
```
teams/
├── platform/
├── data/
└── ml/
```
- Delegation to team-specific directories
- Team-level RBAC with ArgoCD Projects
- Enables team autonomy

**By Application Type**:
```
├── core-services/
├── data-services/
├── monitoring/
└── security/
```
- Logical grouping by function
- Useful for dependency management
- Clear categorization

**Hybrid Approach** (Recommended):
```
├── core/                       # Critical platform services
│   ├── argocd/
│   ├── cert-manager/
│   └── ingress-nginx/
├── clusters/
│   ├── production/
│   │   ├── team-a/
│   │   └── team-b/
│   └── staging/
│       ├── team-a/
│       └── team-b/
└── shared/                     # Shared configurations
    ├── postgres/
    └── redis/
```

## ApplicationGenerator Patterns

### One Generator Per Environment

Create separate ApplicationGenerators for each environment:

```yaml
# apps-production.yaml
apiVersion: argocd.nyl.niklasrosenstein.github.com/v1
kind: ApplicationGenerator
metadata:
  name: production-apps
spec:
  destination:
    server: https://kubernetes.default.svc
    namespace: argocd
  source:
    repoURL: https://github.com/myorg/gitops.git
    targetRevision: main
    path: clusters/production
  project: production
  syncPolicy:
    automated:
      prune: true
      selfHeal: true
  labels:
    environment: production
---
# apps-staging.yaml
apiVersion: argocd.nyl.niklasrosenstein.github.com/v1
kind: ApplicationGenerator
metadata:
  name: staging-apps
spec:
  destination:
    server: https://kubernetes.default.svc
    namespace: argocd
  source:
    repoURL: https://github.com/myorg/gitops.git
    targetRevision: main
    path: clusters/staging
  project: staging
  syncPolicy:
    automated:
      selfHeal: true  # Note: No prune in staging
  labels:
    environment: staging
```

### One App Per File

Each application should have its own YAML file:

```
clusters/production/
├── nginx.yaml          # One app
├── postgres.yaml       # One app
└── redis.yaml          # One app
```

**Benefits**:
- Clear ownership and Git history per app
- Easy to move apps between environments
- Better for code review
- Simpler conflict resolution

**Avoid** putting multiple apps in one file (unless they're tightly coupled).

### Exclude Patterns

Use exclude patterns to prevent accidental Application generation:

```yaml
spec:
  source:
    exclude:
      - ".*"              # Hidden files
      - "_*"              # Underscore prefix (templates, WIP)
      - "*.backup"        # Backup files
      - "test-*"          # Test files
      - "apps.yaml"       # The generator itself
      - "README.md"       # Documentation
```

## Sync Policies

### Production Environments

For production, be conservative:

```yaml
syncPolicy:
  automated:
    prune: true         # Delete removed resources
    selfHeal: true      # Force Git state
  syncOptions:
    - CreateNamespace=true
    - PruneLast=true    # Prune after other operations
```

**Why?**
- `prune: true`: Ensures removed manifests are deleted
- `selfHeal: true`: Prevents manual changes (drift detection)
- `PruneLast`: Safer deletion order

### Staging/Development Environments

For non-production, allow more flexibility:

```yaml
syncPolicy:
  automated:
    prune: false        # Don't auto-delete (allow manual testing)
    selfHeal: true      # Still enforce Git state
```

**Why?**
- `prune: false`: Allows temporary manual resources for testing
- `selfHeal: true`: Still prevents accidental drift

### Manual Sync for Critical Services

For very critical services (databases, auth), consider manual sync:

```yaml
# No automated syncPolicy
# Sync manually via ArgoCD UI or CLI
```

## Secret Management

### Option 1: Sealed Secrets

Use Bitnami Sealed Secrets for encrypted secrets in Git:

```yaml
apiVersion: nyl.niklasrosenstein.github.com/v1
kind: NylRelease
metadata:
  name: app
  namespace: default
---
apiVersion: v1
kind: SealedSecret
metadata:
  name: app-secrets
  namespace: default
spec:
  encryptedData:
    password: AgBjW8X... # Encrypted data safe for Git
```

### Option 2: External Secrets Operator

Sync secrets from external providers (AWS Secrets Manager, Vault, etc.):

```yaml
apiVersion: external-secrets.io/v1beta1
kind: ExternalSecret
metadata:
  name: app-secrets
  namespace: default
spec:
  secretStoreRef:
    name: aws-secrets-manager
  target:
    name: app-secrets
  data:
    - secretKey: password
      remoteRef:
        key: /app/password
```

### Option 3: Nyl Secrets (SOPS)

Use Nyl's built-in SOPS integration:

```yaml
# nyl-secrets.yaml
type: sops
path: ./.secrets.yaml

# .secrets.yaml (encrypted with SOPS)
database:
  password: ENC[AES256_GCM,data:xxx,iv:yyy,tag:zzz]
```

**Never commit plaintext secrets to Git!**

## Multi-Cluster Setup

### Hub-and-Spoke Model

Manage multiple clusters from a central ArgoCD instance:

```yaml
# apps-prod-cluster1.yaml
apiVersion: argocd.nyl.niklasrosenstein.github.com/v1
kind: ApplicationGenerator
metadata:
  name: prod-cluster1-apps
spec:
  destination:
    server: https://cluster1.example.com:6443
    namespace: argocd
  source:
    repoURL: https://github.com/myorg/gitops.git
    path: clusters/cluster1
  labels:
    cluster: cluster1
---
# apps-prod-cluster2.yaml
apiVersion: argocd.nyl.niklasrosenstein.github.com/v1
kind: ApplicationGenerator
metadata:
  name: prod-cluster2-apps
spec:
  destination:
    server: https://cluster2.example.com:6443
    namespace: argocd
  source:
    repoURL: https://github.com/myorg/gitops.git
    path: clusters/cluster2
  labels:
    cluster: cluster2
```

Register clusters in ArgoCD:
```bash
argocd cluster add cluster1-context
argocd cluster add cluster2-context
```

### Per-Cluster ArgoCD

Each cluster has its own ArgoCD instance:

```
gitops-repo/
├── clusters/
│   ├── prod-us-east/
│   │   ├── argocd/
│   │   ├── apps.yaml          # ApplicationGenerator for this cluster
│   │   └── apps/
│   └── prod-eu-west/
│       ├── argocd/
│       ├── apps.yaml
│       └── apps/
```

Each cluster bootstraps independently.

## Monitoring and Observability

### Prometheus Metrics

ArgoCD exports metrics; monitor these key indicators:

- `argocd_app_info`: Application status
- `argocd_app_sync_total`: Sync operations
- `argocd_app_health_status`: Health status

### Alerts

Set up alerts for:

```yaml
# OutOfSync alert
- alert: ArgoCDAppOutOfSync
  expr: argocd_app_sync_status{sync_status="OutOfSync"} == 1
  for: 15m
  labels:
    severity: warning

# Degraded health
- alert: ArgoCDAppUnhealthy
  expr: argocd_app_health_status{health_status!="Healthy"} == 1
  for: 10m
  labels:
    severity: critical
```

### Notifications

Configure ArgoCD notifications for Slack, email, or PagerDuty:

```yaml
apiVersion: v1
kind: ConfigMap
metadata:
  name: argocd-notifications-cm
  namespace: argocd
data:
  service.slack: |
    token: $slack-token
  template.app-sync-status: |
    message: Application {{.app.metadata.name}} sync is {{.app.status.sync.status}}
  trigger.on-sync-failed: |
    - when: app.status.sync.status == 'Failed'
      send: [app-sync-status]
```

## CI/CD Integration

### Validation Pipeline

Add CI checks to validate Nyl manifests before merge:

```yaml
# .github/workflows/validate.yml
name: Validate Manifests
on: [pull_request]
jobs:
  validate:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v3
      - name: Install Nyl
        run: |
          curl -L https://github.com/NiklasRosenstein/nyl/releases/latest/download/nyl-linux-amd64 \
            -o /usr/local/bin/nyl
          chmod +x /usr/local/bin/nyl
      - name: Validate
        run: nyl validate .
      - name: Render
        run: nyl render apps.yaml > /dev/null
```

### Dry-Run Rendering

Test ApplicationGenerator output in CI:

```bash
# Render and check output
nyl render apps.yaml > output.yaml

# Verify Applications were generated
cat output.yaml | grep "kind: Application" | wc -l

# Check specific apps exist
grep "name: nginx" output.yaml
grep "name: postgres" output.yaml
```

### Preview Environments

Create preview environments for PRs:

```yaml
# .github/workflows/preview.yml
name: Preview Environment
on: [pull_request]
jobs:
  preview:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v3
      - name: Deploy Preview
        run: |
          # Create preview namespace
          kubectl create namespace pr-${{ github.event.pull_request.number }}

          # Render manifests with preview profile
          nyl render -e preview apps.yaml | kubectl apply -n pr-${{ github.event.pull_request.number }} -f -
```

## Performance Considerations

### Repository Size

Keep repositories focused:
- Separate infrastructure repo from application repos
- Use Git submodules for shared configurations if needed
- Archive old applications to separate branches

### Sync Frequency

Adjust sync frequency based on needs:

```yaml
# Faster sync for development
metadata:
  annotations:
    argocd.argoproj.io/sync-options: "Timeout=120"

# Slower sync for large apps
metadata:
  annotations:
    argocd.argoproj.io/sync-options: "Timeout=600"
```

### Resource Limits

Set appropriate resource limits for ArgoCD components:

```yaml
spec:
  values:
    repoServer:
      resources:
        limits:
          cpu: "1000m"
          memory: "1Gi"
        requests:
          cpu: "500m"
          memory: "512Mi"
```

## Disaster Recovery

### Backup Strategy

1. **Git is Source of Truth**: All manifests in Git
2. **Backup ArgoCD Configuration**:
   ```bash
   kubectl get applications -n argocd -o yaml > argocd-apps-backup.yaml
   kubectl get appprojects -n argocd -o yaml > argocd-projects-backup.yaml
   ```
3. **Cluster Backups**: Use Velero or similar for cluster state

### Recovery Procedure

1. Restore cluster from backup (if needed)
2. Re-apply bootstrap manifest:
   ```bash
   kubectl apply -f bootstrap.yaml
   ```
3. ArgoCD recreates all Applications from Git
4. Applications sync and restore workloads

**Why this works**: Git is the source of truth; ArgoCD recreates everything from Git automatically.

## Security

### RBAC with Projects

Use ArgoCD Projects for RBAC:

```yaml
apiVersion: argoproj.io/v1alpha1
kind: AppProject
metadata:
  name: team-a
  namespace: argocd
spec:
  description: Team A applications
  sourceRepos:
    - https://github.com/myorg/gitops.git
  destinations:
    - namespace: team-a-*
      server: https://kubernetes.default.svc
  clusterResourceWhitelist:
    - group: ''
      kind: Namespace
  namespaceResourceWhitelist:
    - group: '*'
      kind: '*'
```

### Signed Commits

Require GPG-signed commits for production:

```yaml
spec:
  source:
    repoURL: https://github.com/myorg/gitops.git
    targetRevision: main
  # In ArgoCD settings
  metadata:
    annotations:
      argocd.argoproj.io/verify-signature: "true"
```

### Least Privilege

- ArgoCD service account should have minimal permissions
- Use separate service accounts per ApplicationGenerator/Project
- Audit ArgoCD RBAC regularly

## Troubleshooting

### Common Issues

**Issue**: Applications not syncing
- **Check**: ArgoCD repo-server logs
- **Fix**: Ensure Nyl plugin is installed and accessible

**Issue**: Wrong Applications generated
- **Check**: Render manifests locally: `nyl render apps.yaml`
- **Fix**: Adjust include/exclude patterns

**Issue**: Sync timeouts
- **Check**: Large Helm charts or slow cluster
- **Fix**: Increase timeout annotation

### Debug Commands

```bash
# View ApplicationGenerator output
nyl render apps.yaml

# Check ArgoCD Application
argocd app get <app-name>

# View rendered manifests
argocd app manifests <app-name>

# Force refresh
argocd app get <app-name> --refresh

# View repo-server logs
kubectl logs deployment/argocd-repo-server -n argocd -f
```

## Summary

Key takeaways:
- ✅ Use clear directory structure
- ✅ One ApplicationGenerator per environment
- ✅ One app per file
- ✅ Configure appropriate sync policies
- ✅ Never commit plaintext secrets
- ✅ Monitor and alert on sync status
- ✅ Validate in CI before merge
- ✅ Git is your source of truth

Following these practices ensures reliable, secure, and scalable GitOps with Nyl and ArgoCD.
