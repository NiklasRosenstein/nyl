# ArgoCD with Nyl Helm Chart

This is a Helm chart to install ArgoCD with Nyl (Rust version) as a Config Management Plugin. The chart uses Nyl's `HelmChart` resource to deploy ArgoCD with the Nyl CMP (Config Management Plugin) as a sidecar container.

**⚠️ Migration Notice**: This chart uses the Rust version of Nyl. See the [Migration Guide](#migration-from-python-to-rust) below if you're upgrading from the Python version.

## Goals

* Bootstrap an ArgoCD instance with Nyl as a Config Management Plugin from zero to fully functional in a single command.
* Have ArgoCD immediately own its own installation after bootstrapping.
* If anything goes wrong, be able to easily re-run the command to get back to a fully functional state.
* Demonstrate using SOPS to inject secrets into manifests and Helm chart values (Note: SOPS not yet implemented in Rust version).

## Installation

### Prerequisites

- Kubernetes cluster (1.19+)
- Helm 3.x
- kubectl configured to access your cluster
- Nyl CLI installed (for CRD installation)

### Add the Helm Repository

```bash
helm repo add nyl https://niklasrosenstein.github.io/nyl
helm repo update
```

### Install the Chart

The chart requires the Nyl CRDs to be installed first:

```bash
# Install Nyl CRDs
nyl crds | kubectl apply -f -

# Install ArgoCD with Nyl using default values
helm install argocd nyl/argocd-with-nyl

# Or install from local chart directory
helm install argocd ./chart
```

### Customization

You can customize the installation using a values file:

```bash
# Create a custom values file
cat > custom-values.yaml <<EOF
namespace:
  name: argocd

nyl:
  image:
    tag: "1.0.0"
  envSecret:
    enabled: true
    name: argocd-nyl-env
    data:
      SOPS_AGE_KEY: "your-sops-key-here"

selfManage:
  enabled: true
  repoURL: https://github.com/your-org/your-repo.git
  path: chart

additionalValues:
  configs:
    secret:
      argocdServerAdminPassword: "\$2a\$10\$..."  # bcrypt hash
  server:
    ingress:
      enabled: true
      hosts:
        - argocd.example.com
EOF

# Install with custom values
helm install argocd nyl/argocd-with-nyl -f custom-values.yaml
```

### Configuration Options

See `values.yaml` for all available configuration options.

## Chart Structure

This is a standard Helm chart with the following structure:

```
chart/
├── Chart.yaml              -- Helm chart metadata
├── values.yaml             -- Default configuration values
├── templates/              -- Helm templates
│   ├── namespace.yaml      -- ArgoCD namespace
│   ├── nyl-secret.yaml     -- Secret for Nyl environment variables (optional)
│   ├── argocd-helmchart.yaml -- Nyl HelmChart resource for ArgoCD
│   └── argocd-application.yaml -- Self-management Application (optional)
├── .helmignore             -- Files to exclude from package
└── README.md               -- This file
```

### How It Works

This Helm chart uses Nyl's `HelmChart` custom resource (`nyl.niklasrosenstein.github.com/v1`) to deploy ArgoCD. The `HelmChart` resource tells Nyl to:

1. Fetch the ArgoCD Helm chart from the official repository
2. Inject the Nyl CMP container as a sidecar in the ArgoCD repo-server
3. Configure the necessary volumes and environment variables

When Nyl processes the `HelmChart` resource, it will render the ArgoCD Helm chart with the specified values and apply the resulting Kubernetes manifests.

## Migration from Python to Rust

If you're upgrading from the Python version of Nyl, here's what you need to know:

### Breaking Changes

1. **Command renamed**: `nyl template` → `nyl render`
   - Update your plugin.yaml, scripts, and documentation
   - The ArgoCD plugin.yaml in this repository has been updated

2. **Image version**: New major version (1.0.0)
   - Update your Helm values to use the new image tag
   - Example: `ghcr.io/helsing-ai/nyl/argocd-cmp:1.0.0`

3. **SOPS support**: Not yet implemented in Rust version
   - **Workaround**: Use Kubernetes secrets provider or Null provider temporarily
   - SOPS support is planned for a future release
   - The SOPS binary is included in the image for future use

### Migration Steps

1. **Update image reference** in your ArgoCD application:
   ```yaml
   image: ghcr.io/helsing-ai/nyl/argocd-cmp:1.0.0
   ```

2. **Update any local scripts** that use `nyl template`:
   ```bash
   # Old
   nyl template --apply

   # New
   nyl render --apply
   ```

3. **Handle SOPS secrets** (if applicable):
   - Option A: Wait for SOPS support in Rust version
   - Option B: Use Kubernetes secrets provider temporarily
   - Option C: Use Null provider for non-production environments

4. **Test your manifests** with the Rust version:
   ```bash
   nyl render manifest.yaml > /tmp/output.yaml
   kubectl diff -f /tmp/output.yaml
   ```

### Benefits of Rust Version

- **10x faster** rendering performance
- **75% less memory** usage
- **<100MB image** (down from 200MB+)
- **No Python runtime** required
- **Static binary** with zero dependencies

### What Still Works

- Existing `nyl-project.yaml` files (no changes needed)
- `HelmChart`, `Component`, `NylRelease`, `ApplicationGenerator` resources
- Template syntax (MiniJinja is Jinja2-compatible)
- Git repository handling
- Kubernetes API access

### Additional Resources

- [Complete migration guide](../MOVE_TO_RUST.md)
- [Rust implementation details](../nyl/IMPLEMENTATION.md)
- [Feature comparison table](../MOVE_TO_RUST.md#feature-comparison)
