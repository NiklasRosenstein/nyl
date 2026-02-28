# RemoteManifest

`RemoteManifest` fetches YAML/JSON documents from a remote HTTPS URL and feeds
them into Nyl's normal render pipeline.

## API Version

- `nyl.niklasrosenstein.github.com/v1`

## Schema

```yaml
apiVersion: nyl.niklasrosenstein.github.com/v1
kind: RemoteManifest
metadata:
  name: <name>
spec:
  url: https://example.com/path/manifests.yaml
  overrideNamespace: false
```

### Fields

- `spec.url` (required): HTTPS URL containing one or more YAML/JSON documents.
- `spec.overrideNamespace` (optional, default `false`): when `true`, fetched resources that already have `metadata.namespace` will have that value replaced with `RemoteManifest.metadata.namespace`.

## Behavior

- URL must use `https://`.
- Content is parsed as YAML multi-document stream.
- Parsed resources are processed recursively like local resources.
- Remote content is not rendered as a Jinja template.
- When `spec.overrideNamespace: true`, remote manifests with `metadata.namespace` are rewritten to `RemoteManifest.metadata.namespace`.
- Special case: for `ClusterRoleBinding` (`rbac.authorization.k8s.io/*`), `subjects[*].namespace` is also rewritten (ServiceAccount subjects are forced to the override namespace).
- Fetch or parse failures stop the command (`render`, `diff`, `apply`).

## Example

```yaml
apiVersion: nyl.niklasrosenstein.github.com/v1
kind: RemoteManifest
metadata:
  name: shared-crds
spec:
  url: https://example.com/platform/crds.yaml
```
