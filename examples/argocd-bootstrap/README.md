# ArgoCD Bootstrap

Bootstraps ArgoCD using the OCI-published Nyl Helm chart. The deployed ArgoCD
instance includes the Nyl CMP sidecar and a self-managed `Application` that
keeps itself in sync with this directory.

## Environment Variables

| Variable           | Default                      | Description                              |
|--------------------|------------------------------|------------------------------------------|
| `NYL_CHART_OWNER`      | `niklasrosenstein`           | GitHub owner for the OCI chart registry  |
| `NYL_CHART_VERSION`    | `0.1.0`                      | Helm chart version tag                   |
| `NYL_IMAGE_TAG`        | `develop`                    | Container image tag for the Nyl sidecar  |
| `NYL_REPO_URL`         | *(required)*                 | Git repository URL for self-management   |
| `NYL_TARGET_REVISION`  | `HEAD`                       | Git revision for ArgoCD to track         |

## Usage

```bash
export NYL_CHART_VERSION="0.1.0-sha-abc1234"
export NYL_IMAGE_TAG="sha-abc1234"
export NYL_REPO_URL="https://github.com/NiklasRosenstein/nyl-rs.git"
export NYL_TARGET_REVISION="HEAD"

nyl apply examples/argocd-bootstrap/
```

## What Gets Deployed

1. **NylRelease** – declares the `argocd` release in the `argocd` namespace.
2. **HelmChart** – pulls the Nyl chart from the OCI registry and renders it.
3. **ApplicationGenerator** – creates an ArgoCD `Application` that syncs this
   directory, enabling self-management.
