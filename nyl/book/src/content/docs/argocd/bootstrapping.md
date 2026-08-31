---
title: 'Bootstrapping ArgoCD with Nyl'
---

This guide demonstrates how to bootstrap ArgoCD using Nyl with a self-hosting pattern, where ArgoCD manages itself and all applications through the ApplicationGenerator resource.

## Overview

The bootstrap pattern works as follows:

1. Manually apply a bootstrap manifest to install ArgoCD
2. The bootstrap includes an ArgoCD Application pointing to `apps.yaml`
3. ArgoCD syncs the "apps" Application, running `nyl render apps.yaml`
4. Nyl sees ApplicationGenerator in `apps.yaml` and scans for Release files
5. Nyl generates ArgoCD Applications for each found Release
6. ArgoCD receives and creates all child Applications
7. ArgoCD now manages all applications (including itself) declaratively

## Prerequisites

- `kubectl` configured with cluster access
- Git repository for storing manifests
- Basic understanding of ArgoCD concepts

## Directory Structure

Organize your repository as follows:

```
gitops-repo/
├── bootstrap.yaml              # Initial bootstrap (apply once manually)
├── apps.yaml                   # ApplicationGenerator definition
├── argocd/                     # ArgoCD installation manifests
│   └── argocd.yaml            # HelmChart for ArgoCD
├── clusters/
│   └── default/               # Applications for default cluster
│       ├── app1.yaml          # Application 1 with Release
│       ├── app2.yaml          # Application 2 with Release
│       └── app3.yaml          # Application 3 with Release
├── nyl.toml                   # Nyl project configuration
└── nyl-secrets.yaml           # Secrets configuration
```

## Step 1: Create Configuration Files

### nyl.toml

```toml
[project]
components_search_paths = ["components"]
helm_chart_search_paths = ["."]
```

### nyl-secrets.yaml

```yaml
type: null
```

## Step 2: Create ArgoCD Installation Manifest

Create `argocd/argocd.yaml`:

```yaml
apiVersion: gitops.nyl/v1
kind: Release
metadata:
  name: argocd
  namespace: argocd
---
apiVersion: v1
kind: Namespace
metadata:
  name: argocd
---
apiVersion: nyl.niklasrosenstein.github.com/v1
kind: HelmChart
metadata:
  name: argocd
  namespace: argocd
spec:
  chart:
    repository: https://argoproj.github.io/argo-helm
    name: argo-cd
    version: "5.51.6"
  values:
    # ArgoCD configuration
    server:
      extraArgs:
        - --insecure  # For testing; use TLS in production

    # Configure the Nyl plugin
    repoServer:
      volumes:
        - name: plugins
          emptyDir: {}

      initContainers:
        - name: install-nyl
          image: alpine:3.18
          command: [sh, -c]
          args:
            - |
              apk add --no-cache curl
              curl -L https://github.com/NiklasRosenstein/nyl/releases/latest/download/nyl-linux-amd64 \
                -o /plugins/nyl
              chmod +x /plugins/nyl
          volumeMounts:
            - name: plugins
              mountPath: /plugins

      volumeMounts:
        - name: plugins
          mountPath: /usr/local/bin/nyl
          subPath: nyl

    # Plugin configuration
    configs:
      cm:
        configManagementPlugins: |
          - name: nyl-v2
            generate:
              command: ["/bin/sh", "-c"]
              args:
                - |
                  TEMPLATE_INPUT="${ARGOCD_ENV_NYL_CMP_TEMPLATE_INPUT:-${NYL_CMP_TEMPLATE_INPUT:-}}"
                  test -n "$TEMPLATE_INPUT" || { echo "NYL_CMP_TEMPLATE_INPUT is required" >&2; exit 1; }
                  nyl render "$TEMPLATE_INPUT"
```

`NYL_CMP_TEMPLATE_INPUT` should usually be source-path-relative (for example `apps.yaml` when `spec.source.path` points at that directory). A leading `/` marks a repository-root-relative path.

## Step 3: Create ApplicationGenerator

Create `apps.yaml` with the ApplicationGenerator resource:

```yaml
apiVersion: argocd.nyl.niklasrosenstein.github.com/v1
kind: ApplicationGenerator
metadata:
  name: cluster-apps
  namespace: argocd
spec:
  # Where to create generated Applications
  destination:
    server: https://kubernetes.default.svc
    namespace: argocd

  # Source repository configuration
  source:
    repoURL: https://github.com/your-org/gitops-repo.git
    targetRevision: HEAD
    path: clusters/default

    # Optional: file filtering
    include:
      - "*.yaml"
      - "*.yml"
    exclude:
      - ".*"           # Skip hidden files
      - "_*"           # Skip files starting with underscore
      - "apps.yaml"    # Avoid recursion

  # ArgoCD project for generated Applications
  project: default

  # Default sync policy for all generated Applications
  syncPolicy:
    automated:
      prune: true
      selfHeal: true
    syncOptions:
      - CreateNamespace=true

  # Labels added to all generated Applications
  labels:
    managed-by: nyl
    generator: cluster-apps

  # Annotations added to all generated Applications
  annotations:
    docs-url: https://wiki.example.com/cluster-apps
```

## Step 4: Create Sample Applications

Create `clusters/default/nginx.yaml`:

```yaml
apiVersion: gitops.nyl/v1
kind: Release
metadata:
  name: nginx
  namespace: default
---
apiVersion: nyl.niklasrosenstein.github.com/v1
kind: HelmChart
metadata:
  name: nginx
  namespace: default
spec:
  chart:
    repository: https://charts.bitnami.com/bitnami
    name: nginx
    version: "15.4.4"
  values:
    replicaCount: 2
    service:
      type: ClusterIP
```

Create `clusters/default/redis.yaml`:

```yaml
apiVersion: gitops.nyl/v1
kind: Release
metadata:
  name: redis
  namespace: default
---
apiVersion: nyl.niklasrosenstein.github.com/v1
kind: HelmChart
metadata:
  name: redis
  namespace: default
spec:
  chart:
    repository: https://charts.bitnami.com/bitnami
    name: redis
    version: "18.4.0"
  values:
    architecture: standalone
    auth:
      enabled: false
```

## Step 5: Create Bootstrap Manifest

Create `bootstrap.yaml`:

```yaml
# Namespace for ArgoCD
apiVersion: v1
kind: Namespace
metadata:
  name: argocd
---
# Application to install ArgoCD itself
apiVersion: argoproj.io/v1alpha1
kind: Application
metadata:
  name: argocd
  namespace: argocd
  finalizers:
    - resources-finalizer.argocd.argoproj.io
spec:
  project: default
  source:
    repoURL: https://github.com/your-org/gitops-repo.git
    targetRevision: HEAD
    path: argocd
    plugin:
      name: nyl-v2
  destination:
    server: https://kubernetes.default.svc
    namespace: argocd
  syncPolicy:
    automated:
      prune: true
      selfHeal: true
    syncOptions:
      - CreateNamespace=true
---
# Application generator that creates all other Applications
apiVersion: argoproj.io/v1alpha1
kind: Application
metadata:
  name: apps
  namespace: argocd
  finalizers:
    - resources-finalizer.argocd.argoproj.io
spec:
  project: default
  source:
    repoURL: https://github.com/your-org/gitops-repo.git
    targetRevision: HEAD
    path: .
    plugin:
      name: nyl-v2
  destination:
    server: https://kubernetes.default.svc
    namespace: argocd
  syncPolicy:
    automated:
      prune: true
      selfHeal: true
```

**Important**: In the `apps` Application, the `path` is `.` (repository root) because `apps.yaml` contains the ApplicationGenerator which will scan `clusters/default`.

## Step 6: Bootstrap the Cluster

1. Commit all files to your Git repository:

```bash
git add .
git commit -m "Initial Nyl + ArgoCD bootstrap"
git push origin main
```

2. Apply the bootstrap manifest to your cluster:

```bash
kubectl apply -f bootstrap.yaml
```

3. Wait for ArgoCD to install:

```bash
kubectl wait --for=condition=available --timeout=5m \
  deployment/argocd-server -n argocd
```

4. Access ArgoCD UI:

```bash
# Port forward to access UI
kubectl port-forward svc/argocd-server -n argocd 8080:443

# Get admin password
kubectl get secret argocd-initial-admin-secret -n argocd \
  -o jsonpath='{.data.password}' | base64 -d
```

5. Verify Applications were created:

```bash
kubectl get applications -n argocd
```

Expected output:
```
NAME     SYNC STATUS   HEALTH STATUS
argocd   Synced        Healthy
apps     Synced        Healthy
nginx    Synced        Healthy
redis    Synced        Healthy
```

## How It Works

When you apply `bootstrap.yaml`:

1. **ArgoCD Namespace Created**: The `argocd` namespace is created
2. **ArgoCD Application Syncs**: ArgoCD installs itself via the Nyl plugin
3. **Apps Application Syncs**: The "apps" Application runs `nyl render "$NYL_CMP_TEMPLATE_INPUT"` (for example `apps.yaml`)
4. **ApplicationGenerator Processes**: Nyl finds `apps.yaml`, sees ApplicationGenerator
5. **Directory Scanned**: Nyl scans `clusters/default/` for YAML files
6. **Applications Generated**: For each Release found (nginx, redis), Nyl generates an ArgoCD Application
7. **Applications Created**: ArgoCD receives the generated Applications and creates them
8. **Child Applications Sync**: Each generated Application syncs its resources to the cluster

## Verification

### Check Application Status

```bash
# View all applications
argocd app list

# Get details of a specific application
argocd app get nginx

# View application tree
argocd app get nginx --show-operation

# View rendered manifests
argocd app manifests nginx
```

### Verify Nyl Plugin

```bash
# Check if Nyl is available in repo-server
kubectl exec -it deployment/argocd-repo-server -n argocd -- nyl --version

# View plugin configuration
kubectl get configmap argocd-cm -n argocd -o yaml | grep -A10 configManagementPlugins
```

### View Generated Applications

```bash
# See what ApplicationGenerator produced
kubectl exec -it deployment/argocd-repo-server -n argocd -- \
  sh -c 'cd /tmp && git clone https://github.com/your-org/gitops-repo.git && \
  cd gitops-repo && nyl render apps.yaml'
```

## Adding New Applications

To add a new application to be managed by ArgoCD:

1. Create a new YAML file in `clusters/default/`:

```yaml
# clusters/default/postgres.yaml
apiVersion: gitops.nyl/v1
kind: Release
metadata:
  name: postgres
  namespace: database
---
apiVersion: v1
kind: Namespace
metadata:
  name: database
---
apiVersion: nyl.niklasrosenstein.github.com/v1
kind: HelmChart
metadata:
  name: postgres
  namespace: database
spec:
  chart:
    repository: https://charts.bitnami.com/bitnami
    name: postgresql
    version: "13.2.24"
  values:
    auth:
      username: myuser
      database: mydb
```

2. Commit and push to Git:

```bash
git add clusters/default/postgres.yaml
git commit -m "Add PostgreSQL application"
git push origin main
```

3. ArgoCD automatically detects the change and creates the `postgres` Application

No manual intervention needed! The ApplicationGenerator pattern discovers new applications automatically.

## Troubleshooting

### Applications Not Created

If generated Applications don't appear:

1. Check the "apps" Application status:
   ```bash
   argocd app get apps
   ```

2. View the rendered manifests:
   ```bash
   argocd app manifests apps
   ```

3. Check if ApplicationGenerator is processing correctly:
   ```bash
   # Should show ArgoCD Applications, not the ApplicationGenerator
   argocd app manifests apps | grep "kind: Application"
   ```

### Plugin Failures

If the Nyl plugin fails:

1. Check repo-server logs:
   ```bash
   kubectl logs deployment/argocd-repo-server -n argocd -f
   ```

2. Verify Nyl installation:
   ```bash
   kubectl exec deployment/argocd-repo-server -n argocd -- which nyl
   kubectl exec deployment/argocd-repo-server -n argocd -- nyl --version
   ```

3. Test Nyl render manually:
   ```bash
   kubectl exec -it deployment/argocd-repo-server -n argocd -- sh
   # Inside the pod:
   cd /tmp
   git clone <your-repo>
   cd <repo-name>
   nyl render apps.yaml
   ```

### Sync Issues

If Applications fail to sync:

1. Check Application health:
   ```bash
   argocd app get <app-name>
   ```

2. View sync operation details:
   ```bash
   argocd app get <app-name> --show-operation
   ```

3. Force refresh and sync:
   ```bash
   argocd app sync <app-name> --force
   ```

## Next Steps

- [ApplicationGenerator Reference](/nyl/argocd/application-generator/) - Detailed field documentation
- [Best Practices](/nyl/argocd/best-practices/) - Production recommendations
- [Multi-Cluster Setup](/nyl/argocd/best-practices/#multi-cluster) - Managing multiple clusters
