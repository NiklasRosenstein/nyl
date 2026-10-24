---
title: 'Value Imports'
---

A local component can declare which Cluster and DeploymentTarget values it
understands and where they belong in its chart values. Nyl then supplies those
values as defaults to every invocation rendered for a target, so platform
defaults such as scheduling, storage, or backup settings are declared once per
Cluster or target instead of at every call site.

Value imports are opt-in. A chart without a contract receives only its
invocation's `spec`, and Nyl never forwards `values` on its own.

## Declare a contract

Place `nyl-component.yaml` next to the component's `Chart.yaml`:

```text
components/example/v1/PostgresDb/
  Chart.yaml
  nyl-component.yaml
  values.yaml
  values.schema.json
  templates/
```

```yaml
# components/example/v1/PostgresDb/nyl-component.yaml
apiVersion: components.nyl/v1alpha1
valueImports:
  - name: scheduling
    from: /postgres/scheduling
    to: /scheduling
  - name: storageClass
    from: /postgres/storageClass
    to: /storage/storageClass
  - name: backup
    from: /postgres/backup
    to: /backup
    required: true
```

Each import has these fields:

| Field | Meaning |
| --- | --- |
| `name` | Unique name, used in diagnostics and invocation opt-outs. |
| `from` | JSON Pointer into the effective target `values`. |
| `to` | JSON Pointer into the chart values. |
| `required` | `true` fails the render when `from` is absent. Defaults to `false`. |

Pointers follow RFC 6901: `~1` encodes `/` and `~0` encodes `~` within a key,
as in `/nodeSelector/scheduling.example.com~1pool`. Neither pointer may be
empty, and no destination may equal or contain another import's destination.
Nyl rejects a contract with unknown fields, an unknown `apiVersion`, or invalid
pointers when it renders an invocation of the component.

The contract travels with the chart, so a component copied into another
project keeps its imports.

## Provide the values

The platform declares the values once, in the Cluster or the DeploymentTarget.
Nyl merges the two as described in
[value overlays](/nyl/rendered-gitops/targets-and-clusters/#value-overlays):

```yaml
apiVersion: k8s.nyl/v1
kind: Cluster
metadata:
  name: prod-eu-1
spec:
  values:
    postgres:
      scheduling:
        nodeSelector:
          scheduling.example.com/pool: compute
        tolerations:
          - key: scheduling.example.com/pool
            operator: Equal
            value: compute
            effect: NoSchedule
      storageClass: local-path-retain
      backup:
        destinationPath: s3://example-backups/postgresql
        retentionPolicy: 30d
```

An invocation then carries only its own settings:

```yaml
apiVersion: components.k8s.nyl/v1
kind: example/v1/PostgresDb
metadata:
  name: app-db
  namespace: app
spec:
  storage:
    size: 20Gi
  backup:
    retentionPolicy: 7d
```

Helm receives:

```yaml
scheduling:
  nodeSelector:
    scheduling.example.com/pool: compute
  tolerations:
    - key: scheduling.example.com/pool
      operator: Equal
      value: compute
      effect: NoSchedule
storage:
  storageClass: local-path-retain
  size: 20Gi
backup:
  destinationPath: s3://example-backups/postgresql
  retentionPolicy: 7d
```

Changing `retentionPolicy` in the Cluster values changes every PostgresDb
invocation rendered for its targets, except `app-db`, which sets its own.

## Precedence and merging

From lowest to highest precedence:

```text
chart values.yaml < value imports < invocation spec
```

Imports and `spec` merge with the same rule as Cluster and target values:
objects merge recursively, while arrays, scalars, and `null` replace. As a
result:

- An explicit `false`, `0`, `""`, or `[]` in `spec` replaces the imported value.
- An explicit `null` in `spec` replaces the imported value, and Helm then
  removes the key, including its chart default.
- An empty object in `spec` keeps the imported keys; it does not clear them.
- A present `null`, `false`, or empty value in the target values is imported as
  written. Only an absent key counts as missing.

Helm validates `values.schema.json` against the merged values, so a strict
schema accepts properties that only an import supplies and rejects an invalid
imported value.

A `spec` that sets a scalar or array where an import descends, such as
`storage: 20Gi` with an import to `/storage/storageClass`, fails the render
with a message naming the import.

## Missing values and targetless renders

An optional import whose `from` is absent contributes nothing. A required
import whose `from` is absent fails the render with the component, import,
pointers, and target in the message. Setting the destination in `spec` does not
satisfy a required import: `required` declares that the component depends on
the platform providing the value.

`nyl render` without `--target` has no target values. Optional imports
contribute nothing there, and required imports fail; render with `--target` or
exclude the import.

## Opt out in an invocation

`valueImports` on the invocation, outside `spec`, controls which imports apply:

```yaml
apiVersion: components.k8s.nyl/v1
kind: example/v1/PostgresDb
metadata:
  name: scratch-db
  namespace: app
valueImports:
  exclude: [backup]    # skip named imports, including required ones
spec:
  storage:
    size: 1Gi
```

`valueImports.enabled: false` skips every import. Each name in `exclude` must
be declared by the component, so a misspelled name fails instead of being
ignored.

## Defaults, not policy

Value imports provide defaults. An invocation can still override any imported
value in `spec` or opt out of an import, so imports do not enforce platform
policy; use [validation](/nyl/kubernetes/validation/manifest-validation/) or admission
control for rules that must hold. Imports also do not provision anything the
values point to, such as storage buckets or credentials, and like all target
values they are not a channel for secrets.

## Scope

- Imports read only the effective `values` of the session's target. They cannot
  read `secrets`, `env`, files, live cluster state, or
  [Release inputs](/nyl/rendered-gitops/release-inputs/), and a remote source
  session sees no more than it already admits.
- Contracts apply to local components, including components reached through
  `project.aliases`. Nyl does not read a contract from a remote chart shortcut
  or a `HelmChart` resource, so third-party charts never import values. A remote
  shortcut invocation accepts `valueImports.enabled` but no `exclude` names.
- A component whose chart renders another component invocation is expanded
  again, and that invocation applies its own component's contract against the
  same target values. Parent chart values are not imported. Ordinary Helm
  subcharts follow Helm's value rules.
- `render`, `diff`, `apply`, `render-tree`, `diff-tree`, and `publish-tree`
  resolve imports identically. The contract file is part of the component
  directory and the target values are part of the render context, so changing
  either invalidates cached renders that depend on it.

Run with `RUST_LOG=nyl_render::components=debug` to log each import Nyl
applies or skips, and whether the invocation overrides it.
