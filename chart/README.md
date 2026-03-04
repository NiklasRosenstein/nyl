# ArgoCD with Nyl Helm Chart

This is a Helm chart to install ArgoCD with Nyl as a Config Management Plugin. The chart uses Nyl's `HelmChart` resource to deploy ArgoCD with the Nyl CMP (Config Management Plugin) as a sidecar container.

## Goals

* Bootstrap an ArgoCD instance with Nyl as a Config Management Plugin from zero to fully functional in a single command.
* If anything goes wrong, be able to easily re-run the command to get back to a fully functional state.

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

argocd:
  values:
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
│   └── argocd-helmchart.yaml -- Nyl HelmChart resource for ArgoCD
├── .helmignore             -- Files to exclude from package
└── README.md               -- This file
```

### How It Works

This Helm chart uses Nyl's `HelmChart` custom resource (`nyl.niklasrosenstein.github.com/v1`) to deploy ArgoCD. The `HelmChart` resource tells Nyl to:

1. Fetch the ArgoCD Helm chart from the official repository
2. Inject the Nyl CMP container as a sidecar in the ArgoCD repo-server
3. Configure the necessary volumes and environment variables

When Nyl processes the `HelmChart` resource, it will render the ArgoCD Helm chart with the specified values and apply the resulting Kubernetes manifests.
