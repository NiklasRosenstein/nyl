---
title: 'RemoteManifest'
---

`RemoteManifest` fetches YAML/JSON documents from one or more remote HTTPS URLs and feeds
them into Nyl's normal render pipeline.

## API Version

- `nyl.niklasrosenstein.github.com/v1`

## Schema

Single URL (legacy form):

```yaml
apiVersion: nyl.niklasrosenstein.github.com/v1
kind: RemoteManifest
metadata:
  name: <name>
spec:
  url: https://example.com/path/manifests.yaml
  overrideNamespace: false
```

Multiple URLs with parameter substitution:

```yaml
apiVersion: nyl.niklasrosenstein.github.com/v1
kind: RemoteManifest
metadata:
  name: <name>
spec:
  params:
    version: 1.2.3
  urls:
    - https://example.com/v{version}/crd-a.yaml
    - https://example.com/v{version}/crd-b.yaml
  overrideNamespace: false
```

### Fields

- `spec.url` (mutually exclusive with `spec.urls`): Single HTTPS URL containing one or more YAML/JSON documents.
- `spec.urls` (mutually exclusive with `spec.url`): List of HTTPS URL templates. Each entry may contain `{key}` placeholders resolved from `spec.params`.
- `spec.params` (optional, requires `spec.urls`): Map of parameter names to string values used for `{key}` substitution in `spec.urls` URLs.
- `spec.overrideNamespace` (optional, default `false`): when `true`, fetched resources that already have `metadata.namespace` will have that value replaced with `RemoteManifest.metadata.namespace`.

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
apiVersion: nyl.niklasrosenstein.github.com/v1
kind: RemoteManifest
metadata:
  name: shared-crds
spec:
  url: https://example.com/platform/crds.yaml
```

### Multiple URLs with a shared version parameter

```yaml
apiVersion: nyl.niklasrosenstein.github.com/v1
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
