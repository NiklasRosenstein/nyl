---
title: 'Using RemoteManifest'
---

See the [RemoteManifest resource reference](/nyl/reference/resources/k8s.nyl/v1/remote-manifest/) for the API, example, and field definitions.

## Behavior

- All URLs must use `https://`.
- Fetching uses Nyl's native HTTPS client (no shell-out), with HTTPS-only redirect policy.
- Request timeouts are enforced per URL (connect: 5s, total: 30s).
- Response size is limited to 30 MiB per URL; larger payloads fail fast.
- Content is parsed as YAML multi-document stream.
- When `spec.urls` is used, documents from all URLs are concatenated in order.
- Parsed resources are processed recursively like local resources.
- Remote content is not rendered as a Jinja template.
- When `spec.overrideNamespace: true`, remote manifests with `metadata.namespace` are rewritten to `RemoteManifest.metadata.namespace` (applied after all URLs are fetched).
- Special case: for `RoleBinding` and `ClusterRoleBinding` (`rbac.authorization.k8s.io/*`), `subjects[*].namespace` is also rewritten (ServiceAccount subjects are forced to the override namespace).
- Potential future rewrite targets (currently not handled): webhook service namespaces (`MutatingWebhookConfiguration`, `ValidatingWebhookConfiguration`, CRD conversion webhook), and `APIService.spec.service.namespace`.
- Fetch or parse failures stop the command (`render`, `diff`, `apply`).

## Examples

### Single URL

```yaml
apiVersion: k8s.nyl/v1
kind: RemoteManifest
metadata:
  name: shared-crds
spec:
  url: https://example.com/platform/crds.yaml
```

### Multiple URLs with a shared version parameter

```yaml
apiVersion: k8s.nyl/v1
kind: RemoteManifest
metadata:
  name: gateway-api-crds
  namespace: nginx-gateway
spec:
  params:
    # renovate: datasource=github-releases depName=kubernetes-sigs/gateway-api versioning=semver
    version: 1.5.1
  urls:
    - https://raw.githubusercontent.com/kubernetes-sigs/gateway-api/v{version}/config/crd/standard/gateway.networking.k8s.io_gatewayclasses.yaml
    - https://raw.githubusercontent.com/kubernetes-sigs/gateway-api/v{version}/config/crd/standard/gateway.networking.k8s.io_gateways.yaml
    - https://raw.githubusercontent.com/kubernetes-sigs/gateway-api/v{version}/config/crd/standard/gateway.networking.k8s.io_httproutes.yaml
```
