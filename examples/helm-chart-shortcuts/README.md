# Helm Chart Shortcuts Examples

This directory contains examples of using the HelmChart shortcut syntax for deploying Helm charts with minimal configuration.

For full documentation on the shortcut feature, see the [HelmChart Shortcuts](../../nyl/book/src/reference/resources/helmchart.md#component-shortcut-syntax) documentation.

## Examples

- `http-chart.yaml` - HTTP Helm repository (Bitnami nginx with version)
- `http-chart-no-version.yaml` - HTTP Helm repository (Bitnami redis with version)
- `oci-chart.yaml` - OCI registry chart (Bitnami nginx via Docker Hub)
- `git-chart.yaml` - Git repository chart (Prometheus from GitHub)
- `local-chart.yaml` - Local component path (existing behavior)

## Usage

Render any example:
```bash
nyl render examples/helm-chart-shortcuts/http-chart.yaml --offline --kube-version 1.28.0 --kube-api-versions networking.k8s.io/v1
```

Note: The `--offline` flag in `nyl render` only skips profile/cluster discovery; it does not prevent network operations such as `helm pull` or `git clone`, so remote charts still require network access.
