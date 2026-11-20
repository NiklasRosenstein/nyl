---
title: 'Value sources'
---

A value source says where a value comes from instead of writing it inline: a
file of this repository, a locked Git file, publication state, another unit's
receipt, or a promotion record. This page lists every source Nyl accepts, where
each is valid, when it resolves, and what happens when it finds no value. The
guides explain the workflows: [Release inputs](/nyl/kubernetes-gitops/release-inputs/)
for target bindings, and [References between units](/nyl/orchestration/runs/#references-between-units)
for unit specs.

Unit fields read the environment's values with [`fromValues`](#unit-fields-fromvalues).
Typed source clauses for HelmChart fields and EnvironmentTemplate values are
being added. This page documents each one as it ships; until then, those
places take literal values and Jinja.

## How resolution works

### Render time and execution time

Nyl resolves a source at one of two moments:

- **Render time.** The value comes from the declaration and the files Nyl reads
  while it renders: the Git worktree, a locked Git commit, or the target's
  publication branch. Every command that renders the same document reads the
  same value, whether `nyl render`, `render-tree`, `publish-tree`, or a
  publication unit in `nyl reconcile`.
- **Execution time.** The value comes from orchestration state: another unit's
  current receipt, or a promotion record. Only orchestration resolves it, when a
  run resolves the unit, so it can change between runs without a commit. A
  source whose value does not exist yet **blocks** its unit, which waits instead
  of failing. Ordinary rendering rejects execution-time sources.

### Reference objects

A source is written as an object with exactly one field, the source's name,
such as `{fromUnit: {unit: database, output: host}}`. It replaces the whole
value it stands for:

- An object whose only field is a source name is a reference, wherever the
  source is accepted.
- An object that has a source name next to other fields is an error, because
  the reference would replace them.
- Every other object is data, and so is a source name in a place that does not
  accept it.

### Jinja

Jinja in Nyl documents is for string interpolation, such as
`'pr-{{ environment.name }}'`. Templating that decides a value's shape or type,
such as `{{ x | tojson }}` or `{% if %}` around fields, makes the document
invalid YAML before it renders and hides type errors until execution. Use it
only where no source fits.

## Release-input bindings

A DeploymentTarget binds the inputs a Release declares in
`spec.releaseInputs.<group>/<release>.<input>`. Each binding sets exactly one
source. See [Release inputs](/nyl/kubernetes-gitops/release-inputs/) for the
workflow.

| Source | Resolves | Reads |
| --- | --- | --- |
| `value` | render time | An inline literal, never templated. |
| `fromFile` | render time | `path`, a YAML or JSON file of this repository under the [local path rule](/nyl/reference/configuration/#local-paths), and `pointer` inside it. |
| `fromGit` | render time | A file of a Git repository at a locked `commit`, with `repositoryRef` or `repository`, `revision`, `path`, and `pointer`. |
| `fromPublication` | render time | A state file in the target's own publication branch: `path` relative to its path prefix, `pointer`, and optionally `carryFileFromWorktree`. |
| `fromUnit` | execution time | A unit's output, artifact, or observed value; the fields are those of [unit references](#unit-references). Only a target that a [KubernetesPublication](/nyl/orchestration/resource-guides/kubernetes-publication/#release-inputs-from-other-units) names resolves it. |
| `fromPromotion` | execution time | A value recorded through a [PromotionPath](/nyl/orchestration/promotion/): `value` and optionally `path`. |

`pointer` is a JSON Pointer, such as `/web/tier`; empty or omitted selects the
whole document.

**Missing values.** The effective value is the binding's, otherwise the
Release's declared `default`. A `fromFile` file that does not exist, or a
`fromPublication` branch or file that does not exist yet, leaves the input
unbound, so the default applies. A `pointer` that selects nothing in a file
that exists is an error. A required input without a value is an error that
names the input and its description.

**Types.** Every value is checked against the input's declared `type` and
`enum`; `null` satisfies no type.

**Errors.** Nyl checks every binding of a target before rendering any Release
and reports all problems together: unknown Releases or inputs, missing required
values, and type mismatches.

## Unit references

Unit specs accept `fromUnit` and `fromPromotion` reference objects at any depth
of their free-form maps, such as a Command's `values` or a Terraform unit's
`variables`. Both resolve at execution time. See
[References between units](/nyl/orchestration/runs/#references-between-units)
for the workflow.

```yaml
values:
  host: {fromUnit: {unit: database, output: host}}
  image: {fromUnit: {unit: web-image, artifact: image, pointer: /reference}}
  networkId: {fromUnit: {environment: infra, unit: network, output: id}}
  webImage: {fromPromotion: {value: webImage, path: dev-to-prod}}
```

`fromUnit` takes:

| Field | Meaning |
| --- | --- |
| `unit` | The producing unit. |
| `output`, `artifact`, `observation` | Exactly one: a declared output, a published artifact, or a declared [observed value](/nyl/orchestration/runs/#observed-values). |
| `pointer` | A JSON Pointer inside the value. |
| `environment` | The producer's environment, for a [cross-environment reference](/nyl/orchestration/runs/#multi-environment-runs). |
| `kind` | The expected artifact kind. |
| `evidence`, `attestations` | What the producer must prove beyond a current receipt; see [Requiring attestations](/nyl/orchestration/runs/#requiring-attestations). |

`fromPromotion` takes `value`, the promoted value's name, and optionally
`path`, the PromotionPath it must come from. See
[Promotion](/nyl/orchestration/promotion/) for which record a binding reads.

**Missing values.** A unit document declares no defaults. A producer without a
current receipt, an attestation that has not passed, or a promotion without a
record blocks the consumer, which keeps its previous resolution and runs in a
later run or wave. A reference that resolves to no value is an error.

**Types.** A referenced output has the type its producer declares, and a value
bound to a Release input is checked against the input's declared type before
anything is published. A sensitive output cannot be referenced.

**Errors.** Each reference is a dependency on its producer. A cycle fails with
`NYL-DEPENDENCY-CYCLE`, a unit the environment does not select with
`NYL-DEPENDENCY-UNKNOWN`, and an undeclared attestation with
`NYL-ATTESTATION-UNDECLARED`. [`nyl validate`](/nyl/reference/cli/validate/)
checks references without state.

## Unit fields: `fromValues`

A few fields of a unit spec read their value from the environment's `values`
instead of writing it inline, so one unit document serves environments that
need different values without structural Jinja:

```yaml
apiVersion: units.gitops.nyl/v1
kind: KubernetesPublication
metadata:
  name: kubernetes
  labels: {dev: 'true', preview: 'true'}
spec:
  target: {fromValues: {pointer: /target}}
  teardownWait: {fromValues: {pointer: /teardownWait, default: {strategy: manual}}}
  approval: {fromValues: {pointer: /approval, optional: true}}
  timeout: 10m
```

**Where it is valid.** Only the fields that declare it, and only in unit
documents:

| Field | Units |
| --- | --- |
| `spec.approval`, `spec.timeout` | Every kind. |
| `spec.target`, `spec.teardownWait` | [KubernetesPublication](/nyl/orchestration/resource-guides/kubernetes-publication/). |
| `environment` of a `fromUnit` reference | Every `fromUnit` in the spec, at any depth, such as in `variables`, `values`, or an inline target's bindings. |

The clause replaces the whole field. Every other place treats an object whose
only field is `fromValues` as an error, never as data, and so does a clause in a
document that is not a unit. The list grows only when a project still needs
structural Jinja for a field.

```yaml
variables:
  # dev's own network, or the environment that networkFrom names
  vpc_id: {fromUnit: {unit: network, output: vpcId, environment: {fromValues: {pointer: /networkFrom, optional: true}}}}
```

**When it resolves.** At render time, when an environment's units render: in
`nyl plan`, `nyl reconcile`, `nyl validate`, `nyl build -e`, and every other
command that renders units. `pointer` is a JSON Pointer into the environment's
effective `values`: an Environment's values over its groups' values, or an
EnvironmentTemplate instance's values. Those values are already rendered, so the
value the clause reads is used as it is and never templated again. A
`fromValues` inside `fromUnit.environment` resolves first, so the reference
names its environment before the run resolves the reference itself at
execution time. A direct `nyl build` without `-e` has no environment, so a
unit with a clause needs `-e`.

**Missing values.** When nothing is at `pointer`, or the value there is `null`:

- `default` applies. It must not be `null`.
- With `optional: true` the field is removed, so the spec is the same as one
  that omits the field.
- Otherwise it is an error.

`default` and `optional` are exclusive.

**Types.** The resolved value is checked exactly as the same literal would be,
by the unit's kind. A field filled from values renders the same spec as the
literal, so its unit's execution key is the same too: replacing Jinja with
`fromValues` re-runs nothing.

**Errors.** A clause that cannot resolve fails the unit's render with
`NYL-UNIT-INVALID`. A missing required value names the environment, the keys
that exist where the pointer stops matching, and the nearest one:

```console
$ nyl validate
…
Configuration error: Rendering the units of Environment dev at 0000000000000000000000000000000000000000 failed: Configuration error: NYL-UNIT-INVALID: unit in units.yaml document 1: spec.timeout.fromValues reads /timout, which is not in the values of Environment dev: the values have keys "backendDir", "target", "timeout" (nearest to "timout": "timeout"); set it in the values, or give the clause a default, or optional: true to omit the field
```

[`nyl validate`](/nyl/reference/cli/validate/) renders every unit for every
Environment that selects it, and for sample instances of every
EnvironmentTemplate, so a pointer that one environment lacks fails before any
run or instance exists.
