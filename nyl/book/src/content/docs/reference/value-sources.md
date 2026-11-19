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
A HelmChart reads its values from files and Release inputs with
[`valuesFrom`](#helmchart-values-valuesfrom), and two of its fields from Release
inputs with [`fromInput`](#helmchart-fields-frominput). An EnvironmentTemplate's
values read the instance's parameters with
[`fromParameter`](#template-parameters-fromparameter).

## Summary

| Source | Where it is valid | Resolves | When it finds no value | Type check |
| --- | --- | --- | --- | --- |
| [`value`, `fromFile`, `fromGit`, `fromPublication`](#release-input-bindings) | A Release-input binding of a DeploymentTarget, declared or inline. | Render time. | The Release input's declared default applies; without one, an error. A `pointer` that selects nothing in a file that exists is an error. | The input's declared `type` and `enum`. |
| [`fromUnit`, `fromPromotion`](#unit-references) | A Release-input binding of a target that a KubernetesPublication names, and any depth of a unit spec's free-form maps. | Execution time. | The consumer is blocked until the value exists; a reference that resolves to no value is an error. | The producer's declared output type, and the Release input's type where it is bound. |
| [`fromValues`](#unit-fields-fromvalues) | The fields of a unit spec that declare it, and `environment` of each `fromUnit` in a unit spec. | Render time, when the environment's units render. | `default`, else `optional: true` removes the field, else an error. | By the unit's kind, as the literal. |
| [`valuesFrom`](#helmchart-values-valuesfrom) | A HelmChart in a file of a Release bundle. | Render time, when the bundle's files load. | An error: a missing file, a `pointer` that selects nothing, or an undeclared input. | `input` reads only `object` inputs; without `key`, a mapping. |
| [`fromInput`](#helmchart-fields-frominput) | `includeCrds` and `lookup` of a HelmChart in a Release bundle whose Release declares the input. | Render time, when the bundle's files load. | The input's declared default applies; a `pointer` that selects nothing is an error. | Boolean. |
| [`fromParameter`](#template-parameters-fromparameter) | Any depth of an EnvironmentTemplate's `spec.values`. | When an instance's values are computed: `state init --template`, `reconcile --template`, every command that names the instance, and `nyl validate`'s sample instances. | The parameter's declared default applies; a required parameter must be passed. An undeclared parameter, or a `pointer` that selects nothing, is an error. | The parameter's declared `type` and `enum`, when `--param` is parsed. |

Each section below says where its clause is an error instead of data, so a
clause written in the wrong place fails with where it works instead of reaching
a reader unresolved.

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
documents, including the inline target of a KubernetesPublication. A declared
DeploymentTarget is not a unit document: its `fromUnit` bindings take a literal
`environment`, and a `fromValues` there fails with where it works.

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

## HelmChart values: `valuesFrom`

A HelmChart can read its chart values from values files of this repository and
from `object` Release inputs, merged in order, before its inline `values`:

```yaml
apiVersion: k8s.nyl/v1
kind: HelmChart
metadata:
  name: web
  namespace: web
spec:
  chart: {name: ./chart}
  valuesFrom:
    - file: values-base.yaml            # relative to this file's directory
    - file: /shared/values-preview.yaml # from the Git worktree root
      pointer: /web
    - input: chartValues                # an object input of this Release
    - input: chartValues
      pointer: /monitoring
      key: monitoring
  values:
    fullnameOverride: web
```

Each entry sets exactly one source, and optionally `pointer` and `key`:

| Field | Meaning |
| --- | --- |
| `file` | A YAML file holding one mapping, read without templating. The path rules are those of [Cluster and DeploymentTarget `valuesFrom`](/nyl/kubernetes-gitops/targets-and-clusters/#file-backed-values): relative to the directory of the file that declares the HelmChart, `/` for the Git worktree root, never outside the worktree or through a symbolic link, and only a Git-visible file outside the project's `.nyl/cache`. In a remote ApplicationGroup source, `/` is the root of its checkout. |
| `input` | A Release input of type `object` that the Release of this bundle declares. Its value is the target's binding, or the declared default. |
| `pointer` | A JSON Pointer that selects a value inside the file or input; the whole value when omitted. |
| `key` | Mount the selected value under this key. Without `key`, the selected value must be a mapping, merged at the values root. |

**Where it is valid.** In a HelmChart in a file of a Release bundle: the
Release entry file or a file its `spec.include` selects, also without a Release
for `file` entries. An `input` entry needs a Release that declares the input. A
HelmChart that rendering generates, such as one a chart outputs, cannot use
`valuesFrom`.

**When it resolves.** At render time, when Nyl loads the bundle's files and
before it renders the chart. Entries merge in order and the inline `values`
merge last: maps merge key by key, and every other value, lists included,
replaces. That is Helm's own rule for several `-f` files.

**The values are data.** The merged values go to Helm as they are. Nyl never
reads source clauses in them, before or after the merge, so an object such as
`{fromInput: …}` inside `values` or a values file reaches the chart unchanged.

**Missing values.** There are no defaults: a missing file, a `pointer` that
selects nothing, and an input the Release does not declare are errors. An
optional input's declared default applies when the target binds none.

**Types.** An `input` entry reads only an input of type `object`. Without
`key`, the value it contributes must be a mapping.

**Caching.** Each file is an input of the Release's render, and of its target's
tree, like an included file, and the merged values are part of the Helm render
cache key, so changing a file or an input renders the chart again.

**Errors.** Each error names the HelmChart, its file, and the entry:

```console
$ nyl render apps/web/web.yaml
2026-11-19T01:32:01.925507Z ERROR nyl: Configuration error: HelmChart "web" in apps/web/web.yaml: spec.valuesFrom[1]: Release input "chartValues" has type string; valuesFrom reads only inputs of type object
Hint: Check your nyl.toml syntax and structure. Run 'nyl validate --strict' for detailed validation.
```

## HelmChart fields: `fromInput`

`spec.includeCrds` and `spec.lookup` of a HelmChart read a boolean Release
input with `{fromInput: {name, pointer?}}`, so one Release bundle serves
targets that need different settings:

```yaml
spec:
  inputs:
    crds: {type: boolean, default: true}
    features: {type: object, default: {lookup: false}}
---
apiVersion: k8s.nyl/v1
kind: HelmChart
metadata:
  name: web
spec:
  chart: {name: ./chart}
  includeCrds: {fromInput: {name: crds}}
  lookup: {fromInput: {name: features, pointer: /lookup}}
```

**Where it is valid.** Only on these two fields, in a HelmChart in a file of a
Release bundle whose Release declares the input. Every other place in a Nyl
resource of the bundle treats an object whose only field is `fromInput` as an
error; `values`, `valuesFrom` data, and a Component's `spec` are chart data and
are never checked.

**When it resolves.** At render time, when Nyl loads the bundle's files: before
anything reads the HelmChart, so `lookup` read from an input decides the cache
and the lookup scope exactly as a literal would.

**Missing values.** The clause has no `default`: the input's declared default
applies when the target binds none. A `pointer` that selects nothing, and a
`null` value, are errors.

**Types.** The value must be a boolean.

**Errors.** An input the Release does not declare, an input read outside a
Release that declares inputs, and a value that is not a boolean fail the render
with a message that names the HelmChart, its file, and the field.

## Template parameters: `fromParameter`

An EnvironmentTemplate's `values` read the instance's parameters with
`{fromParameter: {name, pointer?}}`, so a value keeps the parameter's type
where a quoted Jinja string would make it a string:

```yaml
apiVersion: gitops.nyl/v1
kind: EnvironmentTemplate
metadata:
  name: preview
spec:
  parameters:
    pr: {type: integer}
    crds: {type: boolean, default: false}
    sizes: {type: object, default: {web: {replicas: 1}}}
  values:
    hostname: 'pr-{{ params.pr }}.preview.example.com'   # a string: Jinja is fine
    replicas: {fromParameter: {name: sizes, pointer: /web/replicas}}
    target:
      inline:
        # …
        releaseInputs:
          rise/rise:
            clusterWide: {value: {fromParameter: {name: crds}}}
```

| Field | Meaning |
| --- | --- |
| `name` | A parameter the template declares in `spec.parameters`. |
| `pointer` | A JSON Pointer that selects a value inside the parameter; the whole value when omitted. |

**Where it is valid.** At any depth of an EnvironmentTemplate's `spec.values`,
as a [reference object](#reference-objects): exactly one field, and an object
with other fields next to it is an error. That includes an inline target and
the `value` of its bindings, as above. Everywhere else, a declared
Environment's or EnvironmentGroup's `values`, a DeploymentTarget, a unit
document, a Nyl resource of a Release bundle, and the template's other fields,
the clause is an error.

**When it resolves.** When an instance's values are computed, which is the one
step that turns a template and its parameters into the instance's Environment:
at `nyl state init --template`, at `nyl reconcile --template`, and at every
later command that names the instance with `-e`, from the parameters its
`state.yaml` records. Strings with template syntax render first; the clause
then resolves, and its value is used as it is, never templated again. Units
render after that, so a unit's [`fromValues`](#unit-fields-fromvalues) reads the
resolved value, and a binding written `{value: {fromParameter: …}}` reaches the
Release as the literal `{value: true}`.

**Missing values.** The clause has no `default`: the parameter's declared
default applies when the instance is created without it, and a parameter
without a default must be passed with `--param`. A `pointer` that selects
nothing, or `null`, is an error.

**Types.** The value is the bound parameter, checked against its declared
`type` and `enum` when `--param` is parsed, so `--param crds=true` is a boolean.
A value bound to a Release input is checked against the input's type again when
the target renders.

**Errors.** A clause that does not resolve fails the instance with
`NYL-TEMPLATE-INVALID`. A parameter the template does not declare names the
declared ones. [`nyl validate`](/nyl/reference/cli/validate/) computes the
values of sample instances of every template, so the error appears before any
instance exists:

```console
$ nyl validate
…
Configuration error: Rendering the units of Environment preview-sample-1 at 0000000000000000000000000000000000000000 failed: Configuration error: NYL-TEMPLATE-INVALID: EnvironmentTemplate preview does not render to a valid environment preview-sample-1: spec.values.replicas.fromParameter names parameter "replica", which EnvironmentTemplate preview does not declare (declared: crds, pr); declare it in spec.parameters, or name a declared one (fix EnvironmentTemplate preview in source)
```

A clause outside a template's values fails where the resource is read:

```console
$ nyl validate
2026-11-19T01:47:50.126319Z ERROR nyl: Configuration error: dev.yaml#document-1: spec.values.replicas holds a fromParameter clause where none resolves; fromParameter resolves only in an EnvironmentTemplate's spec.values, see https://niklasrosenstein.github.io/nyl/reference/value-sources/#template-parameters-fromparameter
```
