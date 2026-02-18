# Remote Shortcuts & Aliases

Local components should be the default for repo-owned APIs. Use remote shortcuts and aliases when you need indirection or external chart sources.

## Shortcut Grammar

Component kind shortcuts use:

```text
<base>[#<name>][@<version>]
```

- `base`: repository URL or local path
- `name`: chart name or Git subpath
- `version`: chart version or Git ref

Remote bases are recognized by prefix:

- `http://` or `https://`
- `oci://`
- `git+`

## Remote Shortcut Examples

HTTP Helm repo:

```yaml
apiVersion: components.nyl.niklasrosenstein.github.com/v1
kind: https://charts.bitnami.com/bitnami#nginx@18.2.4
metadata:
  name: nginx-http
spec:
  replicaCount: 2
```

OCI:

```yaml
apiVersion: components.nyl.niklasrosenstein.github.com/v1
kind: oci://registry-1.docker.io/bitnamicharts/nginx@18.2.4
metadata:
  name: nginx-oci
```

Git:

```yaml
apiVersion: components.nyl.niklasrosenstein.github.com/v1
kind: git+https://github.com/prometheus-community/helm-charts#charts/prometheus@prometheus-25.28.0
metadata:
  name: prometheus
  namespace: monitoring
```

## Git URL Parsing Edge Case

Parsing is right-to-left for separators to avoid misinterpreting `@` in SSH-style URLs.

Example:

```text
git+git@github.com:org/repo#charts/app@main
```

Interpreted as:

- `base = git+git@github.com:org/repo`
- `name = charts/app`
- `version = main`

## Aliases for Stable APIs

Define stable domain resource types in `nyl.toml`:

```toml
[project.aliases]
"myapi.io/v1/MyKind" = "oci://registry-1.docker.io/bitnamicharts/nginx@18.2.4"
```

Then write manifests with standard `apiVersion/kind`:

```yaml
apiVersion: myapi.io/v1
kind: MyKind
metadata:
  name: my-kind-release
spec:
  replicaCount: 2
```

## When to Use What

Nyl supports three common patterns:

| Pattern | How you write it | Best for | Main benefit |
|---|---|---|---|
| Full `HelmChart` resource | `kind: HelmChart` + `spec.chart.*` | Explicit platform manifests | Maximum clarity and explicit chart fields |
| Component shortcut | `apiVersion: components...` + `kind: <shortcut>` | Fast authoring near chart source | Minimal boilerplate |
| `project.aliases` named kinds | `apiVersion/kind` mapped in `nyl.toml` | Domain APIs across teams | Stable semantic kinds decoupled from chart location |

- Local component path: internal platform APIs and reviewable chart source.
- Remote shortcut: quick direct dependency on an external chart source.
- Alias: stable contract for app teams while platform controls chart target centrally.
