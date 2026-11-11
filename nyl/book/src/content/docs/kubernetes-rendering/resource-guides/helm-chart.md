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

Helm receives only `HelmChart.spec.values`. Component resources follow the same
rule and pass only their `spec` payload to Helm.

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

Some charts generate secrets once and reuse them later: they call Helm's `lookup`
for an existing Secret and run `genCA` or `genSignedCert` only when it finds
none. Cilium's chart does this for its TLS CA. Nyl renders charts offline, where
`lookup` finds nothing, so such a chart generates a new CA, and different bytes,
whenever the render cache is cold.

Declare `spec.lookup` on the HelmChart to let `lookup` read the objects of the
chart's Release instead:

```yaml
apiVersion: k8s.nyl/v1
kind: Release
metadata:
  name: cilium
  namespace: kube-system
---
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
  values:
    hubble:
      tls:
        auto:
          method: helm
  lookup: {}
```

`lookup: {}` takes no fields. It is off by default, and a chart without it renders
exactly as before. Where the objects come from is not part of the declaration: the
command that renders chooses, and every command that renders the same target takes
the same path.

### What `lookup` reads

With a DeploymentTarget, `lookup` reads the target's last publication: the objects
that `publish-tree` last published for this Release, at the commit of the
publication branch that `fromPublication` Release inputs read. `render`,
`diff`, `apply`, `render-tree`, `diff-tree`, `publish-tree`, and the publication
unit all read it, so they agree.

- `lookup` sees only the objects of the Release being rendered, the files that the
  [ownership index](/nyl/kubernetes-gitops/rendered-layout/#ownership-index) lists
  beneath `<group output>/<release>/`. Every other object is absent, including the
  objects of other Releases and anything the cluster holds that no publication does.
  A namespaced object that sets no namespace is in the namespace its Application
  deploys to.
- The first render has nothing to read: before the publication branch exists, or
  before the Release is published, every lookup is absent and the chart generates.
  After `publish-tree`, later renders find what it published and keep it. Generate
  once, publish, and the CA stays.
- A render without a target, and `--defaults-only` (which renders outside every
  ApplicationGroup), read nothing: `lookup` is always absent there. The chart still
  renders through the lookup API, which needs `--kube-version` and
  `--kube-api-versions` as for any targetless Helm render.
- The publication is read lazily, the first time a chart that declares `lookup`
  needs it, so a target whose charts declare none never reads it. A normal render
  refreshes the publication branch first, as `fromPublication` does; `--offline`
  reads the cached branch head.

An unreadable publication is never read as an absent one, because that would
silently regenerate a CA. A publication that cannot be fetched, a branch that was
never cached when `--offline` is given, an unparsable ownership index, a file
missing from the tree, and a file whose bytes do not match the index all fail the
render with `NYL-HELM-LOOKUP-SNAPSHOT-UNAVAILABLE`. [`--lookup=live`](#reading-the-live-cluster)
reads the cluster instead.

### Rotating a generated object

`--refresh-lookup <Kind/namespace/name>` treats one object as absent for one render,
so the chart generates it again; use `Kind/name` for a cluster-scoped object. Every
command that renders a target takes it, and it can be repeated:

```bash
nyl publish-tree --target production --refresh-lookup Secret/kube-system/cilium-ca
```

The next render without the flag reads the rotated object from the new publication
and keeps it. `render`, `diff`, and `apply` with the flag show or apply the rotated
object without publishing it, which a later `render` without the flag does not
reproduce.

### Reading the live cluster

`render`, `diff`, and `apply` can read the cluster instead of the publication with
`--lookup=live`. Helm still talks only to the local API and never gets the cluster or
its credentials: the API reads the cluster through Nyl's own client, with GET and LIST
only, and only for objects that belong to the Release. Those are the resources of the
Release's latest recorded revision (`nyl apply` records one) and, with a target, the
objects its last publication holds; any other object, whatever the cluster holds, is
absent.

- Without GitOps configuration, `--context` already chose the cluster, so the live
  cluster is the default source there (`--lookup=snapshot` finds nothing instead, since
  there is no publication). With a target, the publication stays the default.
- `render --target <name> --lookup=live` connects to the Cluster for this alone, as
  `diff` and `apply` do; `--context` then selects how to reach it.
- A render that reads the cluster is never cached: what the cluster holds changes
  without Nyl observing it, so every render looks it up again.
- A cluster that cannot be read is an error (`NYL-HELM-LOOKUP-LIVE-UNAVAILABLE`), never
  an absent object. `--lookup=live` without a target or `--context` has no cluster to
  read (`NYL-HELM-LOOKUP-LIVE-NEEDS-CLUSTER`).
- `render-tree`, `diff-tree`, `publish-tree`, `nyl build -- --push`, and the publication
  unit refuse `--lookup=live` (`NYL-HELM-LOOKUP-LIVE-REFUSED`), as they accept no
  `--context`: what a cluster holds today must not decide what Git publishes.

`diff --lookup=live` can legitimately differ from the publication: the cluster holds
the object as it is now, which may be a CA that was rotated there, or one that was
never published, while the publication holds what `publish-tree` last wrote. The two
diverge exactly when the cluster is not what Git declares; compare with a plain `diff`
to see the difference.

### Caching and diagnostics

The lookups a render makes are cache inputs. Nyl records each lookup with a digest
of its result, or that the object is absent, and reuses a cached render only while
every lookup still returns the same result, so a cache hit never assumes what an
object holds. A result that holds Secret data is recorded only as a fingerprint keyed
by the local cache, never as a guessable digest. Nothing Nyl logs contains the
contents of an object.

### How it works

A chart that declares `lookup` renders with `helm template --dry-run=server`
against a read-only Kubernetes API that Nyl serves on 127.0.0.1 for the duration of
that chart's render. Helm never gets a cluster or credentials: it reads a temporary
kubeconfig that trusts only the certificate generated for that render and carries a
random token, and `HELM_KUBE*` variables are cleared from its environment. The API
answers GET and LIST of the Release's objects, refuses everything else, and answers
`500` rather than `404` for anything it cannot answer, so Helm fails instead of
generating what a missing answer would hide.

Helm ignores `--kube-version` and `--api-versions` in this mode, so the API reports
the Cluster's declared Kubernetes version and lists:

- Helm's default API versions, read from the Helm binary Nyl runs;
- the Cluster's declared `spec.kubernetes.apiVersions`;
- the API versions of the CustomResourceDefinitions in the chart's `crds/`
  directory (and its unpacked subcharts'), and
- the API versions of the objects the publication holds.

Helm maps every object it renders through this list, so Nyl also lists the kinds an
offline render of the chart produces. A kind of an API version that none of these
declares fails with `NYL-HELM-LOOKUP-API-VERSION-UNDECLARED`: declare the API
version in the Cluster's `spec.kubernetes.apiVersions` (or `--kube-api-versions`
without a target), or ship its CustomResourceDefinition in the chart's `crds/`.

Charts that declare `lookup` also see Helm's own `contains CRDs` warning on stderr;
a warning on stderr is not an error.

### Errors

| Code | Meaning and fix |
| --- | --- |
| `NYL-HELM-LOOKUP-SNAPSHOT-UNAVAILABLE` | The publication that `lookup` reads cannot be read, or is corrupt. Nothing is treated as absent. Check network access to the publication repository; with `--offline`, render once without it so the branch is cached; for a corrupt publication, restore the file or republish. |
| `NYL-HELM-LOOKUP-API-VERSION-UNDECLARED` | A chart that declares `lookup` renders a kind whose API version no declaration covers. Declare the API version in `Cluster.spec.kubernetes.apiVersions`, or ship its CustomResourceDefinition in the chart's `crds/`. |
| `NYL-HELM-LOOKUP-LIVE-UNAVAILABLE` | `--lookup=live` could not read the cluster, or the release revisions that say which objects belong to the Release. Nothing is treated as absent. Check the connection and the credentials, or render without `--lookup=live` to read the last publication. |
| `NYL-HELM-LOOKUP-LIVE-NEEDS-CLUSTER` | `--lookup=live` was given where no cluster is selected. Pass `--target`, or `--context` in a project without GitOps configuration. |
| `NYL-HELM-LOOKUP-LIVE-REFUSED` | A tree or publication command (`render-tree`, `diff-tree`, `publish-tree`, `nyl build -- --push`) was given `--lookup=live`. Publications read lookups from the last publication only; use `render`, `diff`, or `apply` for live lookups. |
| `NYL-HELM-LOOKUP-FAILED` | The lookup API could not answer a request Helm made, for example one it does not implement. The message names the request. |

## Sync waves and phases

Generated resources inherit this resource's Argo CD sync wave and phase; see [Argo CD sync waves and phases](/nyl/kubernetes-rendering/pipeline/#argo-cd-sync-waves-and-phases).

## See Also

- [Rendering Pipeline](/nyl/kubernetes-rendering/pipeline/#helm-lookup) - How `lookup` takes part in rendering and caching
- [Git Integration](/nyl/kubernetes-rendering/git-integration/) - Git repository management
- [Configuration](/nyl/reference/configuration/) - Search paths and settings
- [Release](/nyl/reference/resources/k8s.nyl/v1/release/) - Release metadata
