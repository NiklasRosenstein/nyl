---
title: 'Using HelmChart'
---

See the [HelmChart resource reference](/nyl/reference/resources/k8s.nyl/v1/helm-chart/) for the API, example, and field definitions.

## Chart Reference Methods

### Local Path

Reference a chart by filesystem path (absolute or relative) using the `name` field:

```yaml
apiVersion: k8s.nyl/v1
kind: HelmChart
metadata:
  name: nginx
  namespace: default
spec:
  chart:
    name: ./charts/nginx
```

### Chart Name

Reference a chart by name (without path separators), searched in configured search paths:

```yaml
apiVersion: k8s.nyl/v1
kind: HelmChart
metadata:
  name: nginx
  namespace: default
spec:
  chart:
    name: nginx
```

Configure search paths in `nyl.toml`:
```toml
[project]
helm_chart_search_paths = ["./charts", "/opt/helm-charts"]
```

### Git Repository

Reference a chart from a Git repository using the `git+` protocol prefix:

```yaml
apiVersion: k8s.nyl/v1
kind: HelmChart
metadata:
  name: nginx
  namespace: default
spec:
  chart:
    repository: git+https://github.com/bitnami/charts.git
    version: main
    name: bitnami/nginx
```

**Git Parameters:**
- **`repository`** (required): Git repository URL with `git+` prefix (HTTPS or SSH)
- **`version`** (optional): Branch, tag, or commit SHA (default: `HEAD`)
- **`name`** (optional): Subdirectory within the repository containing the chart

**Helm Dependencies:**

Charts from Git repositories with dependencies are automatically handled. If your chart has a `Chart.yaml` with dependencies or a `Chart.lock` file, Nyl will automatically run `helm dependency build` to fetch and build the dependencies before rendering the chart.

**Examples:**

```yaml
# Latest from main branch
chart:
  repository: git+https://github.com/example/charts.git
  version: main
  name: charts/myapp

# Specific version tag
chart:
  repository: git+https://github.com/example/charts.git
  version: v2.1.0
  name: charts/myapp

# Specific commit
chart:
  repository: git+https://github.com/example/charts.git
  version: abc123def456
  name: charts/myapp

# Root of repository (no subpath)
chart:
  repository: git+https://github.com/example/simple-chart.git
  version: main

# SSH URL
chart:
  repository: git+git@github.com:example/charts.git
  version: main
  name: charts/myapp
```

See [Git Integration](/nyl/kubernetes-rendering/git-integration/) for more details on Git support.

## Release Configuration

The Helm release is configured via the `metadata` fields:

```yaml
metadata:
  name: myapp           # Helm release name
  namespace: production # Target namespace
```

**Defaults:**
- `namespace`: Uses `default` if not specified

### Creating Namespaces

If you need to create the namespace before deploying the chart, add a Namespace resource:

```yaml
apiVersion: v1
kind: Namespace
metadata:
  name: production
---
apiVersion: k8s.nyl/v1
kind: HelmChart
metadata:
  name: myapp
  namespace: production
spec:
  chart:
    name: ./charts/myapp
  values:
    replicas: 3
```

When using ArgoCD, you can alternatively enable automatic namespace creation:

```yaml
apiVersion: argoproj.io/v1alpha1
kind: Application
spec:
  syncPolicy:
    syncOptions:
      - CreateNamespace=true
```

## Values

Chart values can be provided in multiple ways:

### Inline Values

```yaml
spec:
  values:
    image:
      repository: nginx
      tag: "1.25"
    replicas: 3
    service:
      type: LoadBalancer
```

### Values files and Release inputs

`spec.valuesFrom` reads chart values from YAML files of this repository and
from `object` Release inputs. Entries merge in order, and inline `values` merge
last; maps merge key by key and everything else replaces, as with Helm's `-f`:

```yaml
spec:
  chart: {name: ./charts/web}
  valuesFrom:
    - file: values-base.yaml       # relative to this file's directory
    - file: /shared/preview.yaml   # from the Git worktree root
      pointer: /web
    - input: chartValues           # an object input the Release declares
  values:
    fullnameOverride: web
```

`pointer` selects a value inside the file or input, and `key` mounts it under
one values key. Files follow the path and Git-visibility rules of Cluster
`valuesFrom`; an `input` entry is valid only in the bundle of a Release that
declares the input. Nyl passes the merged values to Helm as data and never
templates or resolves anything in them. See
[Value sources](/nyl/reference/value-sources/#helmchart-valuesfrom) for
every rule.

### Cluster and target values

Cluster and DeploymentTarget values are Nyl template inputs. They are not passed to
Helm implicitly. This keeps strict chart schemas effective and prevents an
unrelated target value from overriding or extending a chart's values.

Pass a target-aware value explicitly from the Nyl template context:

```yaml
spec:
  values:
    environment: "{{ values.environment }}"
```

Helm receives only `HelmChart.spec.values`, merged over `spec.valuesFrom`.
Component resources follow the same rule and pass only their `spec` payload to
Helm.

### Templating in Values

Values support Jinja2 templating:

```yaml
spec:
  values:
    image:
      tag: "{{ env.NYL_IMAGE_TAG }}"
    environment: "{{ values.environment }}"
```

## Complete Example

```yaml
apiVersion: k8s.nyl/v1
kind: Release
metadata:
  name: myapp
  namespace: production
---
apiVersion: k8s.nyl/v1
kind: HelmChart
metadata:
  name: myapp
  namespace: production
spec:
  chart:
    repository: git+https://github.com/company/charts.git
    version: v2.1.0
    name: applications/myapp
  values:
    replicas: 3
    image:
      repository: company/myapp
      tag: "{{ env.VERSION }}"
    ingress:
      enabled: true
      host: myapp.example.com
    resources:
      requests:
        cpu: 500m
        memory: 512Mi
      limits:
        cpu: 1000m
        memory: 1Gi
```

## Multi-Environment Deployments

Use the same chart with different values per environment:

```yaml
# base manifest
apiVersion: k8s.nyl/v1
kind: HelmChart
metadata:
  name: myapp
spec:
  chart:
    repository: git+https://github.com/company/charts.git
    version: stable
    name: myapp
  values:
    # Base values here
```

```yaml
apiVersion: k8s.gitops.nyl/v1
kind: DeploymentTarget
metadata:
  name: production
spec:
  reconciler: {argocd: {}}
  clusterRef:
    name: primary
  values:
    replicas: 5
    environment: production
    image:
      tag: v2.1.0
    resources:
      requests:
        cpu: 1000m
  publication:
    repository:
      repoURL: https://git.example.com/platform/deploy.git
    revision: deploy/production
```

Render for specific environment:
```bash
nyl render --target production app.yaml
```

## Reusing generated objects with `lookup`

Some charts generate a secret on first install and read it back with Helm's `lookup`
afterwards: they call `genCA` or `randAlphaNum` only when `lookup` finds nothing.
Cilium's chart does this for its Hubble CA. Nyl renders charts without a cluster, so
`lookup` finds nothing and such a chart generates new bytes on every cold render.

Set `spec.lookup: true` to let `lookup` read the objects the Release last published:

```yaml
apiVersion: k8s.nyl/v1
kind: HelmChart
metadata:
  name: cilium
  namespace: kube-system
spec:
  chart:
    repository: https://helm.cilium.io
    name: cilium
    version: 1.17.6
  lookup: true
```

In a Release bundle, `lookup`, `includeCrds`, and the HelmChart's other fields
can also read a Release input, so targets decide them without templating:
`lookup: {$input: lookup}`. See
[`$input`](/nyl/reference/value-sources/#input).

`lookup` is opt-in. A chart without it renders exactly as before. `spec.lookup: false` is the
explicit opt-out: it renders the same way, and tells Nyl you know the chart calls `lookup`
(see [Undeclared `lookup`](#undeclared-lookup)). The object form `spec.lookup: {}` from 0.7.0 is
no longer accepted: it fails with `NYL-HELM-LOOKUP-OBJECT-REMOVED`, so write `spec.lookup: true`.

[Components](/nyl/kubernetes-rendering/components/overview/) pass `spec` as chart values, so
they opt in elsewhere: the `k8s.nyl/helm-lookup: "true"` annotation on the Component, or
`lookup: true` in the local component's `nyl-component.yaml` (the annotation wins). See
[Lookup in components](/nyl/kubernetes-rendering/components/authoring-local-components/#helm-lookup).

### Undeclared `lookup`

When a template of the chart, a subchart under `charts/` (unpacked or `.tgz`), or a library
chart calls `lookup` and lookup is neither enabled nor disabled, Nyl warns with
`NYL-HELM-LOOKUP-UNDECLARED`: the call finds nothing, so the chart generates on every render.
Set `lookup: true` to read the Release's objects, or `lookup: false` to silence the warning.
The warning comes from a scan of the template sources for `lookup` used as a function inside
`{{ ... }}` (comments, strings, and fields such as `.Values.lookup` are skipped), so it is a
heuristic. It works for local charts and for charts pulled from a repository or registry.

A call inside `{{ define "name" }}` counts only when that template is reached: included with
`include "name"`, `template "name"` or `block "name"` from template content outside any define,
or from another template that is reached. So a library chart such as Bitnami's `common`, which
defines lookup helpers a chart may never include, does not make every chart warn. The warning
names where the call was found, for example `in library chart common, template
common.secrets.passwords.manage`, so choosing `false` is an informed choice. Names built at
render time (`include (print ...)`) and subcharts disabled by a `condition` are not modelled.

The warning shows when the Release renders, not when Nyl reuses its cached render, and once per
chart and resource per run.

### What `lookup` sees

- Only the objects of the Release being rendered, as the target's last publication
  holds them. That is the commit `$publication` inputs read. Other Releases and
  the cluster are invisible.
- Before the Release is first published, or without a target, nothing is found and the
  chart generates. After `publish-tree`, later renders read back what was published.
- Only what the chart looks up stays stable. A value the chart generates on every
  render, such as a leaf certificate it signs each time, still changes.
- A publication that can't be read fails the render with
  `NYL-HELM-LOOKUP-SNAPSHOT-UNAVAILABLE`; it is never treated as empty. The exceptions
  are `nyl validate` and `nyl vendor`, which treat a publication that was never fetched
  and can't be fetched as empty. `--offline` reads the cached publication.

### Rotating a generated object

`--refresh-lookup Kind/namespace/name` (`Kind/name` for a cluster-scoped object) hides
one object for one render, so the chart generates it again. It is repeatable and works
with every command that renders a target:

```bash
nyl publish-tree --target production --refresh-lookup Secret/kube-system/cilium-ca
```

Publish the result to keep the new object: `render`, `diff` and `apply` with the flag
only show or apply it. A reference that matches no lookup warns with
`NYL-HELM-LOOKUP-REFRESH-UNMATCHED`.

### Reading the live cluster

`render`, `diff` and `apply` read from the cluster instead with `--lookup=live`. It is
the default with `--context` and no GitOps configuration.

- `lookup` still sees only the Release's own objects: those in its latest revision
  recorded by `nyl apply`, plus, with a target, those in its publication. Nyl reads each
  one with a GET. Helm never talks to the cluster.
- If the cluster holds an object outside that set, the render fails with
  `NYL-HELM-LOOKUP-LIVE-UNRECORDED` instead of regenerating it. Releases installed by
  Helm or an older Nyl hit this. Run `nyl apply --lookup=snapshot` once to record them,
  or pass `--refresh-lookup` to regenerate the object on purpose.
- Live renders are never cached.
- Tree and publication commands refuse `--lookup=live`: what a cluster holds must not
  decide what Git publishes.

### How it works

Nyl runs `helm template --dry-run=server` against a read-only Kubernetes API that it
serves on 127.0.0.1 for the duration of that chart's render. Helm gets no real cluster
and no credentials. The API answers "not found" only when the object is really absent;
anything it can't answer fails the render instead of letting the chart generate.

Each lookup result is a render cache input (Secret data only as a keyed fingerprint),
so a cached render is reused only while every lookup returns the same result.

Requirements:

- Helm 3.17 or later.
- `.Capabilities.APIVersions` comes from Helm's defaults, the Cluster's
  `spec.kubernetes.apiVersions`, the chart's `crds/`, and Nyl's table of built-in kinds.
  It never depends on what the publication holds, so it is the same on every render. It
  also lists built-in kinds as `group/version/Kind` entries.
- A custom resource kind with no CRD in the chart's `crds/` must be declared as
  `group/version/Kind`, for example `monitoring.coreos.com/v1/ServiceMonitor`.

### Errors

| Code | Meaning and fix |
| --- | --- |
| `NYL-HELM-LOOKUP-SNAPSHOT-UNAVAILABLE` | The publication can't be read or is corrupt. Check access to the publication repository; with `--offline`, render once online first; for a corrupt publication, republish. |
| `NYL-HELM-LOOKUP-API-VERSION-UNDECLARED` | The chart renders or looks up an API version nothing declares. Declare it in `Cluster.spec.kubernetes.apiVersions`, or ship its CRD in the chart's `crds/`. |
| `NYL-HELM-LOOKUP-KIND-UNDECLARED` | The chart renders or looks up a custom kind nothing declares. Declare `group/version/Kind`, or ship its CRD in `crds/`. |
| `NYL-HELM-LOOKUP-VERSION-MISMATCH` | The object is held at a different API version than the chart asks for. Look it up at that version, or `--refresh-lookup` it to regenerate. |
| `NYL-HELM-LOOKUP-HELM-UNSUPPORTED` | Helm is older than 3.17. Install the Helm version the project pins. |
| `NYL-HELM-LOOKUP-REFRESH-UNMATCHED` | Warning: a `--refresh-lookup` reference matched no lookup. Check its spelling. |
| `NYL-HELM-LOOKUP-LIVE-UNAVAILABLE` | `--lookup=live` couldn't read the cluster or the Release's revisions. Check the connection, or drop `--lookup=live`. |
| `NYL-HELM-LOOKUP-LIVE-UNRECORDED` | The cluster holds an object no revision or publication of the Release lists. Run `nyl apply --lookup=snapshot` once, or `--refresh-lookup` the object. |
| `NYL-HELM-LOOKUP-LIVE-NEEDS-CLUSTER` | `--lookup=live` without a cluster. Pass `--target`, or `--context` without GitOps configuration. |
| `NYL-HELM-LOOKUP-LIVE-REFUSED` | A tree command got `--lookup=live`. Use `render`, `diff` or `apply` for live lookups. `nyl build` of a publication unit refuses it as `NYL-BUILD-ARGUMENT` (`lookup-live`). |
| `NYL-HELM-LOOKUP-ANNOTATION-INVALID` | A Component's `k8s.nyl/helm-lookup` annotation is not `"true"` or `"false"`. Quote the value. |
| `NYL-HELM-LOOKUP-ANNOTATION-MISPLACED` | A HelmChart carries `k8s.nyl/helm-lookup`. Set `spec.lookup: true` or `false` and remove the annotation. |
| `NYL-HELM-LOOKUP-UNDECLARED` | Warning: a chart calls `lookup` but lookup is not enabled or disabled. Opt in or out as above. |
| `NYL-HELM-LOOKUP-OBJECT-REMOVED` | `spec.lookup` is an object such as `{}`. It is a boolean now: write `spec.lookup: true`. |
| `NYL-HELM-LOOKUP-FAILED` | The lookup API couldn't answer a request Helm made. The message names the request. |

## Sync waves and phases

Generated resources inherit this resource's Argo CD sync wave and phase; see [Argo CD sync waves and phases](/nyl/kubernetes-rendering/pipeline/#argo-cd-sync-waves-and-phases).

## See Also

- [Rendering Pipeline](/nyl/kubernetes-rendering/pipeline/#helm-lookup) - How `lookup` takes part in rendering and caching
- [Git Integration](/nyl/kubernetes-rendering/git-integration/) - Git repository management
- [Configuration](/nyl/reference/configuration/) - Search paths and settings
- [Release](/nyl/reference/resources/k8s.nyl/v1/release/) - Release metadata
