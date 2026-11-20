---
title: 'Release Inputs'
---

A Release may declare typed inputs that each DeploymentTarget binds. The
resources of the Release read them with
[`{$fromInput: name}`](/nyl/reference/value-sources/#frominput), which keeps the
input's type, and templates read them as `inputs.<name>` for string
interpolation. Inputs suit values that differ
per environment and need one traceable source, such as an immutable image
reference or connection facts produced by another tool. A Release without
`spec.inputs` renders exactly as before.

## Declaring inputs

```yaml
apiVersion: k8s.nyl/v1
kind: Release
metadata:
  name: web
  namespace: web
spec:
  inputs:
    image:
      type: string
      description: Immutable image reference
    replicas:
      type: integer
      default: 2
    tier:
      type: string
      enum: [small, large]
      default: small
    database:
      type: object
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: web
  namespace: web
spec:
  replicas: {$fromInput: replicas}
  template:
    spec:
      containers:
        - name: web
          image: {$fromInput: image}
          args: ['--tier={{ inputs.tier }}']   # string interpolation: Jinja is fine
```

- `type` is one of `string`, `integer`, `number`, `boolean`, `object`, and
  `array`. `integer` accepts integers only, `number` any number, and `null`
  satisfies no type. `object` and `array` check only the top-level type; leave
  deeper structure to downstream validators such as a chart's
  `values.schema.json`.
- `default` makes an input optional and must satisfy `type` and `enum`.
- `enum` is a non-empty list of allowed values for scalar types.
- Input names match `^[a-z][a-zA-Z0-9]*$`.
- A Release that declares inputs needs a literal `metadata.name` and a literal
  `spec.inputs` block, so Nyl knows the declarations before rendering.
  Templating cannot add, remove, or change them.

`$fromInput` and `inputs` work in the Release entry file and in every file
attached through `spec.include`: in plain Kubernetes manifests, a HelmChart's
fields and chart `values`, a Component's `spec`, a RemoteManifest, and the
`metadata.namespace` of any of them and of the Release itself. `apiVersion`,
`kind`, `metadata.name`, and `metadata.labels` stay literal. What a chart or
a remote manifest generates takes no `$fromInput`: inputs reach it only through
what the bundle passes in, as with target `values`.
`target.spec.releaseInputs` is not part of the `target` template context.

### Passing inputs to a chart

Write `{$fromInput: image}` to place an input in a chart value, at any depth of
`values`, and `{$fromInput: crds}` for a HelmChart field such as `includeCrds`.
For an `object` input that holds a whole block of chart values, merge it with
`valuesFrom`. Never write `{{ inputs.x | tojson }}`: a source keeps the type
without templating the document's structure.

```yaml
apiVersion: k8s.nyl/v1
kind: Release
metadata:
  name: web
  namespace: web
spec:
  inputs:
    chartValues:
      type: object
      default: {}
    crds:
      type: boolean
      default: true
    tag:
      type: string
---
apiVersion: k8s.nyl/v1
kind: HelmChart
metadata:
  name: web
  namespace: web
spec:
  chart: {name: ./chart}
  includeCrds: {$fromInput: crds}
  valuesFrom:
    - file: values-base.yaml
    - input: chartValues
  values:
    fullnameOverride: web
    image: {repository: registry.example.com/web, tag: {$fromInput: tag}}
```

The target's `chartValues` binding merges over `values-base.yaml`, and
`values` merges last, with its `$fromInput` sources already resolved. See [HelmChart `valuesFrom`](/nyl/reference/value-sources/#helmchart-valuesfrom)
and [`$fromInput`](/nyl/reference/value-sources/#frominput)
for the rules.

## Binding inputs on a target

Bindings live on the DeploymentTarget, keyed by the Release's identity on the
target, `<applicationGroup>/<release>`:

```yaml
apiVersion: k8s.gitops.nyl/v1
kind: DeploymentTarget
metadata:
  name: staging
spec:
  reconciler: {argocd: {}}
  clusterRef: {name: shared}
  publication: {repositoryRef: {name: deploy}, revision: deploy, pathPrefix: staging}
  releaseInputs:
    platform/web:
      image: registry.example.com/web@sha256:4f0c…
      database:
        $fromFile:
          path: environments/staging/database.yaml
          pointer: /database
```

A binding is the input's value: a literal, such as `image` above, or a value
source. Targets are static, so a literal is never templated; wrap a literal
that looks like a source in `{$literal: …}`.

| Source | Resolves from |
| --- | --- |
| `$fromFile` | A YAML or JSON file of this repository holding one document. `path` follows the [local path rule](/nyl/reference/configuration/#local-paths); `pointer` is a JSON Pointer, the whole document by default. The file must be visible to Git, like discovered resources: not ignored, and outside the output and vendor subtrees. |
| `$fromGit` | A YAML or JSON file of a Git repository at a locked commit, selected with `repositoryRef` or an inline `repository`. `path` is repository-relative. |
| `$fromPublication` | A state file in this target's own publication branch, relative to its path prefix, read at the commit publication builds on. With `carryFileFromWorktree`, a working-tree file of this run is rendered from and written into the publication. |
| `$fromUnit`, `$fromPromotion` | Resolved by orchestration. Ordinary rendering rejects them. An environment's [`KubernetesPublication`](/nyl/orchestration/resource-guides/kubernetes-publication/) resolves `$fromUnit` and renders the target with the values pinned. |

[Value sources](/nyl/reference/value-sources/#release-input-bindings) lists
each source with when it resolves and what it does when it finds no value.

An inline target written in an EnvironmentTemplate's `values` can bind an input
to a template parameter with `{$fromParameter: crds}`: the
instance's values resolve the source first, so the binding holds the
parameter's typed value, such as a boolean. See
[`$fromParameter`](/nyl/reference/value-sources/#fromparameter).

The effective value is the target binding if there is one, otherwise the
Release default. A binding replaces the value whole; unlike `values`, inputs
are never deep-merged, so every value has exactly one source.

Nyl resolves and checks every input of a target before rendering any Release
and reports all problems together:

- a key that names no Release the target renders, or a Release without inputs;
- a binding for an input the Release does not declare;
- a required input without a binding, named with its description;
- a value that does not match the declared type or `enum`.

Keys of a selected ApplicationGroup whose `spec.enabled` is false, or whose
document renders to no document for the target, are ignored, so `enabled` can
still toggle the group. Remote ApplicationGroups receive
resolved values like local groups; the binding key on the platform's own target
is the admission, and the remote session never sees binding definitions or
file paths.

## Locked Git state

`$fromGit` follows the ApplicationGroup source-lock pattern. `revision` is the
human-readable branch or tag; `commit` is the full 40-character lowercase
commit ID rendering reads. Rendering never resolves `revision`. A commit missing from the local Git cache
is fetched by ID; offline rendering fails with a message naming the lock.
Locked files follow the [vendor policy](/nyl/reference/configuration/#remote-artifact-vendoring):
`nyl vendor` captures each one, and mode `required` renders them only from the
snapshot.

```yaml
releaseInputs:
  platform/web:
    tier:
      $fromGit:
        repositoryRef: {name: platform-state}
        revision: main
        commit: 3f1c9a…        # moved by nyl update source-locks
        path: staging/sizing.json
        pointer: /web/tier
```

`nyl update source-locks` refreshes these locks together with ApplicationGroup
source locks, so CI has one `--check` gate for every Git lock. `--target
production` selects one target's locks; filters choose which locks move,
never where. Every lock moves to the head of its revision.

A target should not `$fromGit`-lock a file in its own publication: each of its
publications moves the branch head, so the lock is stale again right after.
Read the target's own state with `$fromPublication` instead.

## Publication state

`$fromPublication` serves write-back workflows: a tool such as an image build
commits a state file to the deploy branch, and Nyl renders manifests from it
into the same branch.

```yaml
releaseInputs:
  platform/web:
    image:
      $fromPublication:
        path: state/images.json    # relative to the target's pathPrefix
        pointer: /web
```

- `publish-tree` reads the file at the branch head it builds on and pushes
  with a compare-and-swap, so the published commit holds the state and the
  manifests rendered from it. A writer that pushes meanwhile makes the
  publication fail instead of interleaving. Rendering the same state again
  creates no commit.
- `render-tree` and `diff-tree` fetch the branch and report the commit they
  read; `--offline` reads the cached head.
- While the branch or the file does not exist, the input is unbound and its
  Release default applies. A file whose `pointer` does not resolve is an error.
- The path must lie outside every directory a generated Argo CD Application
  syncs: workload Release directories and `_nyl`.

With `carryFileFromWorktree`, the state file comes from this run's working tree instead:

```yaml
      $fromPublication:
        path: state/images.json
        pointer: /web
        carryFileFromWorktree: build/images.json   # untracked output of this CI run
```

When the carry file exists, Nyl renders from it and writes its bytes to `path`
in the publication commit; otherwise it writes the branch copy back, so the
last carried value persists. The target owns `path`, adopting a file another
tool committed there before, and rejects a change or deletion of it by another
writer. Removing only `carryFileFromWorktree` keeps the file as committed state
that the target no longer owns, because the binding still reads it; removing
the binding deletes the file from the publication branch, while disabling its
ApplicationGroup keeps the file until the group is enabled again. The carry file must not be tracked by Git; it is
excluded from the dirty-worktree check, and the clean-`HEAD` verification of
`publish-tree` renders with the same bytes. Bindings naming one path must all
carry the same file, or none.

Publication state is never vendored. `nyl vendor --check` reads it like the
next render would, and falls back to the cached branch head offline. With
vendor mode `required`, keep state from choosing remote artifacts such as
chart versions: a new artifact needs `nyl vendor` and a source commit first.

## Direct commands

`nyl render`, `nyl diff`, and `nyl apply` resolve a Release's inputs the way
`render-tree` does, so `nyl render --target production api.yaml` matches the
published manifests:

```text
effective input = --input / --inputs override
                  otherwise the target binding
                  otherwise the Release default
```

With a target, the binding comes from the selected ApplicationGroup whose local
source contains the Release file. When several or none do, for example a
Release of a remote group rendered from a local checkout, the command fails and
names the target's groups: pass `--application-group <name>` to apply that
group's bindings, or `--defaults-only` to render with defaults and overrides
only. A target never falls back to defaults silently. Without a target, only
defaults and overrides apply.

```bash
nyl render --target production applications/workloads/api.yaml
nyl render --input image='"registry.example.com/api@sha256:4f0c"' --inputs local.yaml api.yaml
```

`--input <name>=<json>` takes a JSON value, so strings are quoted; `--inputs`
reads a YAML or JSON object, and individual `--input` flags win over it.
Overrides must name declared inputs and are type-checked like bindings. Tree
commands accept no overrides: published output reproduces from committed
source and the publication state. A Release rendered with overrides or
`--defaults-only` is therefore not the published one, and the direct command
settles it alone instead of taking it from the compiled target; see
[The Release's Application](/nyl/kubernetes-rendering/pipeline/#the-releases-application).

## Provenance

Resolved inputs are part of the render-cache key. The ownership index records
each `$fromFile` file under its path, each effective input as
`@input/<group>/<release>/<input>` with the SHA-256 digest of its canonical
JSON value (RFC 8785, with exact digits for integers beyond ±(2^53 - 1)), each `$fromGit` file as `@git/<url>@<commit>/<path>` with the
digest of its bytes, and each publication state file as `@publication/<path>`
(read from the base commit) or `@carried/<path>` (from a carry file). Keys under `@input/` and `@git/` are reserved for these
entries; a project file whose key equals one fails the render instead of being
overwritten. Inputs are not a secret channel: their digests and the rendered
manifests are published, so keep secrets in the secrets provider.
