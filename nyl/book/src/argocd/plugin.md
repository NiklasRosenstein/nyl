# Plugin Installation

The Nyl ArgoCD plugin enables ArgoCD to render Nyl manifests directly from Git repositories. This guide covers plugin installation and configuration.

## Prerequisites

- ArgoCD 2.4+ installed in your cluster
- Access to modify ArgoCD's configuration
- Nyl binary available in the ArgoCD repo-server

## Installation Methods

### Method 1: Custom ArgoCD Image (Recommended)

Build a custom ArgoCD repo-server image with Nyl included:

```dockerfile
FROM quay.io/argoproj/argocd:v2.9.3

# Switch to root to install Nyl
USER root

# Download and install Nyl
RUN curl -L https://github.com/NiklasRosenstein/nyl/releases/download/vX.Y.Z/nyl-linux-amd64 \
    -o /usr/local/bin/nyl && \
    chmod +x /usr/local/bin/nyl

# Switch back to argocd user
USER argocd
```

Deploy this custom image by updating the `argocd-repo-server` deployment:

```yaml
spec:
  template:
    spec:
      containers:
      - name: argocd-repo-server
        image: your-registry/argocd-with-nyl:v2.9.3
```

### Method 2: Init Container

Use an init container to download Nyl into a shared volume:

```yaml
spec:
  template:
    spec:
      initContainers:
      - name: install-nyl
        image: alpine:3.18
        command:
        - sh
        - -c
        - |
          apk add --no-cache curl
          curl -L https://github.com/NiklasRosenstein/nyl/releases/download/vX.Y.Z/nyl-linux-amd64 \
            -o /plugins/nyl
          chmod +x /plugins/nyl
        volumeMounts:
        - name: plugins
          mountPath: /plugins
      containers:
      - name: argocd-repo-server
        volumeMounts:
        - name: plugins
          mountPath: /usr/local/bin/nyl
          subPath: nyl
      volumes:
      - name: plugins
        emptyDir: {}
```

## Plugin Configuration

Configure the Nyl plugin in the ArgoCD ConfigMap:

```yaml
apiVersion: v1
kind: ConfigMap
metadata:
  name: argocd-cm
  namespace: argocd
data:
  configManagementPlugins: |
    - name: nyl
      generate:
        command: ["/bin/sh", "-c"]
        args:
        - |
          TEMPLATE_INPUT="${ARGOCD_ENV_NYL_CMP_TEMPLATE_INPUT:-${NYL_CMP_TEMPLATE_INPUT:-}}"
          test -n "$TEMPLATE_INPUT" || { echo "NYL_CMP_TEMPLATE_INPUT is required" >&2; exit 1; }
          nyl render "$TEMPLATE_INPUT"
```

## Verification

Test the plugin installation:

1. Create a test Application using the Nyl plugin:

```yaml
apiVersion: argoproj.io/v1alpha1
kind: Application
metadata:
  name: test-nyl
  namespace: argocd
spec:
  project: default
  source:
    repoURL: https://github.com/your-org/your-repo
    targetRevision: HEAD
    path: path/to/nyl/manifests
    plugin:
      name: nyl
  destination:
    server: https://kubernetes.default.svc
    namespace: default
```

2. Check the Application status:

```bash
kubectl get application test-nyl -n argocd
argocd app get test-nyl
```

3. Verify manifest rendering:

```bash
argocd app manifests test-nyl
```

## Passing Environment Variables

You can pass environment variables to the Nyl plugin for configuration:

```yaml
source:
  plugin:
    name: nyl
    env:
    - name: NYL_CMP_TEMPLATE_INPUT
      value: apps.yaml
    - name: NYL_RELEASE_NAME
      value: my-app
    - name: NYL_RELEASE_NAMESPACE
      value: production
```

## Troubleshooting

### Plugin Not Found

If ArgoCD reports "plugin not found", check:

1. The plugin name matches exactly: `nyl`
2. The argocd-cm ConfigMap is in the `argocd` namespace
3. The repo-server pods have been restarted after configuration changes

```bash
kubectl rollout restart deployment argocd-repo-server -n argocd
```

### Command Not Found

If the plugin fails with "nyl: command not found":

1. Verify Nyl binary is in the PATH: `/usr/local/bin/nyl`
2. Check file permissions (should be executable)
3. Test manually in the repo-server pod:

```bash
kubectl exec -it deployment/argocd-repo-server -n argocd -- nyl --version
```

### Profile Not Found

If Nyl reports "Profile 'default' not found":

1. Ensure your repository contains `nyl-profiles.yaml`
2. Check the profile name matches what you're referencing
3. Verify the file is in the repository root or search path

## Next Steps

- [Bootstrap ArgoCD with Nyl](./bootstrapping.md)
- [Use ApplicationGenerator](./application-generator.md)
- [Best Practices](./best-practices.md)
