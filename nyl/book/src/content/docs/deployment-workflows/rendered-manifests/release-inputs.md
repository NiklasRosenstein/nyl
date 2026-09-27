---
title: 'Release inputs'
---

A Release may declare typed inputs that each DeploymentTarget binds. Templates
read them as `inputs.<name>`, next to `values`. Inputs suit values that differ
per environment and need one traceable source, such as an immutable image
reference or connection facts produced by another tool. A Release without
`spec.inputs` renders exactly as before.

## Declaring inputs

```yaml
apiVersion: k8s.gitops.nyl/v1
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
  replicas: {{ inputs.replicas }}
  # …
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

`inputs` is visible in the Release entry file and in every file attached
through `spec.include`. Components, HelmCharts, and RemoteManifests receive
inputs only through explicit templating in the Release bundle, as with target
`values`. `target.spec.releaseInputs` is not part of the `target` template
context.

## Binding inputs on a target

Bindings live on the DeploymentTarget, keyed by the Release's identity on the
target, `<applicationGroup>/<release>`:

```yaml
apiVersion: k8s.gitops.nyl/v1
kind: DeploymentTarget
metadata:
  name: staging
spec:
  clusterRef: {name: shared}
  publication: {repositoryRef: {name: deploy}, revision: deploy, pathPrefix: staging}
  releaseInputs:
    platform/web:
      image:
        value: registry.example.com/web@sha256:4f0c…
      database:
        fromFile:
          path: environments/staging/database.yaml
          pointer: /database
```

Each binding sets exactly one source:

| Source | Resolves from |
| --- | --- |
| `value` | An inline literal. Targets are static, so it is never templated. |
| `fromFile` | A YAML or JSON file of this repository holding one document. `path` follows the [local path rule](/nyl/configuration/#local-paths); `pointer` is a JSON Pointer, the whole document by default. The file must be visible to Git, like discovered resources: not ignored, and outside the output and vendor subtrees. |
| `fromGit` | A YAML or JSON file of a Git repository at a locked commit, selected with `repositoryRef` or an inline `repository`. `path` is repository-relative. |
| `fromUnit`, `fromPromotion` | Reserved for orchestration. Ordinary rendering rejects them. |

The effective value is the target binding if there is one, otherwise the
Release default. A binding replaces the value whole; unlike `values`, inputs
are never deep-merged, so every value has exactly one source.

Nyl resolves and checks every input of a target before rendering any Release
and reports all problems together:

- a key that names no Release the target renders, or a Release without inputs;
- a binding for an input the Release does not declare;
- a required input without a binding, named with its description;
- a value that does not match the declared type or `enum`.

Keys of a selected ApplicationGroup whose `spec.enabled` is false are ignored,
so `enabled` can still toggle the group. Remote ApplicationGroups receive
resolved values like local groups; the binding key on the platform's own target
is the admission, and the remote session never sees binding definitions or
file paths.

## Locked Git state

`fromGit` follows the ApplicationGroup source-lock pattern. `revision` is the
human-readable branch or tag; `commit` is the full 40-character lowercase
commit ID rendering reads. Rendering never resolves `revision`. A commit missing from the local Git cache
is fetched by ID; offline rendering fails with a message naming the lock.

```yaml
releaseInputs:
  platform/web:
    tier:
      fromGit:
        repositoryRef: {name: platform-state}
        revision: main
        commit: 3f1c9a…        # moved by nyl update source-locks
        path: staging/sizing.json
        pointer: /web/tier
```

`nyl update source-locks` refreshes these locks together with ApplicationGroup
source locks, so CI has one `--check` gate for every Git lock. `--target
production` selects one target's locks. Locks of one repository and revision
move to the same commit. When their locked files lie inside another target's
publication prefix on that branch, the locks move to that target's newest
publication commit, so a production target can promote the state a dev target
published by moving its lock in a reviewed change. Locks of one revision that
read several targets' publications are rejected, because they cannot move to
one commit.

## Provenance

Resolved inputs are part of the render-cache key. The ownership index records
each `fromFile` file under its path, each effective input as
`@input/<group>/<release>/<input>` with the SHA-256 digest of its canonical
JSON value, and each `fromGit` file as `@git/<url>@<commit>/<path>` with the
digest of its bytes. Keys under `@input/` and `@git/` are reserved for these
entries; a project file whose key equals one fails the render instead of being
overwritten. Inputs are not a secret channel: their digests and the rendered
manifests are published, so keep secrets in the secrets provider.
