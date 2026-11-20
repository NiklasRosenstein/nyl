---
title: 'Value sources'
---

A value source says where a value comes from instead of writing it inline: the
environment's values, a Release input, a template parameter, another unit's
receipt, a promotion record, or a file. Every source is written the same way,
an object whose only key is the source's `$` name, and is replaced by the value
it names:

```yaml
target: {$environment: /target}
includeCrds: {$input: crds}
vpc_id: {$unit: {unit: network, output: vpcId}}
```

This page lists every source, where each is valid, when it resolves, and what
happens when it finds no value. The guides explain the workflows:
[Release inputs](/nyl/kubernetes-gitops/release-inputs/) for target bindings,
and [References between units](/nyl/orchestration/runs/#references-between-units)
for unit specs.

## Summary

| Source | Shorthand | Object form | Where it is valid | Resolves | When it finds no value | Type check |
| --- | --- | --- | --- | --- | --- | --- |
| [`$environment`](#environment) | a JSON Pointer: `{$environment: /target}` | `{pointer, default?, optional?}` | Unit documents. | Render time, when the unit renders. | `default`, else `optional: true` removes the field, else an error. | The field's own, as a literal. |
| [`$input`](#input) | a name: `{$input: image}` | `{name, pointer?}` | The fields of a HelmChart in a Release bundle. | Render time, when the bundle's files load. | The input's declared default; a `pointer` that selects nothing is an error. | The field's own, as a literal. |
| [`$parameter`](#parameter) | a name: `{$parameter: pr}` | `{name, pointer?}` | An EnvironmentTemplate's `spec.values`. | When an instance's values are computed. | The parameter's declared default; a required parameter must be passed. | The parameter's declared `type` and `enum`. |
| [`$unit`](#unit) | none | `{unit, output \| artifact \| observation, …}` | Unit documents and Release-input bindings. | Execution time. | The consumer waits for the producer. | The producer's declared output type, and the input's where it is bound. |
| [`$promotion`](#promotion) | none | `{value, path?}` | Unit documents and Release-input bindings. | Execution time. | The consumer waits for a record. | The input's where it is bound. |
| [`$file`, `$git`, `$publication`](#file-git-and-publication) | none | see below | Release-input bindings. | Render time. | The input's declared default; a `pointer` that selects nothing in a file that exists is an error. | The input's declared `type` and `enum`. |
| [`$literal`](#literal) | — | any value | Wherever sources are. | With the document. | — | As the literal. |

## How sources work

### One form

A source is an object whose only key is a known `$` source. It replaces the
whole value it stands for, at any depth of a document's objects and lists:

- A source next to other keys is an error, because it would replace them.
- A source with a shorthand takes its main field as a string:
  `{$environment: /target}` is `{$environment: {pointer: /target}}`, and
  `{$input: image}` is `{$input: {name: image}}`. A shorthand that looks like
  the other one is an error that suggests the fix: `{$input: /image}` "takes a
  name; did you mean image?", and `{$environment: target}` "takes a JSON
  Pointer; did you mean /target?".
- `pointer` is a JSON Pointer, such as `/web/tier`; empty or omitted selects
  the whole value.

### Missing values

When a source finds nothing, or `$environment` finds `null`:

- `default` applies. It must not be `null`.
- With `optional: true` the key is removed, so the document is the same as one
  that omits the field.
- Otherwise it is an error that names the location and where the value was
  looked for: the environment, the instance, or the Release, with the keys
  that exist there and the nearest one.

`default` and `optional` are exclusive, and only `$environment` takes them:
`$input` and `$parameter` use the default their declaration gives, and
`$unit` and `$promotion` wait for their producer.

### Types

The document with its sources resolved is checked exactly as if the values
were written inline: a unit's kind checks its spec, a HelmChart's fields keep
their types, and a Release input or a template parameter is checked against
its declaration. A field filled from a source renders the same document as the
literal, so a unit's execution key does not change when Jinja gives way to a
source.

### `$literal`

`{$literal: <value>}` yields its content verbatim, and nothing inside it is
read as a source. Use it for data that looks like one:

```yaml
values:
  schema: {$literal: {$unit: not-a-reference}}
```

In a unit document, `$literal` unwraps when the unit renders, unless its
content holds `$` keys: then it unwraps when a run resolves the unit, so its
content never reads as a reference.

### Reserved keys and likely slips

In the fields of a Nyl resource every `$` key is reserved, so an unknown one,
such as `$ref` in a HelmChart's `chart`, is an error. In free-form data (chart
values, a unit's `variables` and `values`, environment values, a binding's
literal) other `$` keys such as `$ref` and `$schema` are data, except these
likely slips, which are errors that name the source you probably meant:
`$value`, `$values`, `$env`, `$inputs`, `$param`, `$params`, `$parameters`,
`$units`, `$output`, and `$artifact`. Wrap the object in `$literal` to keep
one of them as data.

## Contexts

Where a document sits decides which sources resolve in it. A source a context
does not accept is an error that says where it works and how to get the value
instead. A source a context *carries* is checked and left in place for the
document it lands in, which resolves it.

| Context | Resolves | Carries |
| --- | --- | --- |
| A HelmChart in a Release bundle, outside its `values` | `$input`, `$literal` | — |
| A unit document, when the unit renders | `$environment` | `$unit` and `$promotion` to the run; in an inline target's `releaseInputs`, also `$file`, `$git`, and `$publication` to the target's rendering |
| A unit's rendered spec, when a run resolves it | `$unit`, `$promotion`, `$literal` | — |
| An EnvironmentTemplate's `spec.values` | `$parameter` | `$unit`, `$promotion`, `$file`, `$git`, `$publication`, `$literal` |
| A declared Environment's or EnvironmentGroup's `values` | — | as template values |
| A DeploymentTarget's `releaseInputs`, declared or inline | `$file`, `$git`, `$publication`, `$literal`; `$unit` and `$promotion` through orchestration | — |
| Any other field, such as a target's `values` or a template's `parameters` | — | — |

The Release, Component, and RemoteManifest resources of a bundle, and a
HelmChart's chart `values`, take no source yet.

### Data and configuration

A value read from outside your configuration is data and is never read for
sources: a unit's output, artifact, or observed value, a file read with
`$file`, `$git`, `$publication`, or HelmChart `valuesFrom`, a promotion record,
and a template parameter. A value copied from your configuration carries its
sources to where it lands: `{$environment: /target}` can read an inline target
whose bindings hold `$unit`, and the run resolves that `$unit` like one written
in the unit. The environment's values cannot read the environment's values.

### Literal-only fields

`apiVersion`, `kind`, `metadata.name`, and `metadata.labels` are read by
discovery and selection before anything resolves, so a source in them is an
error. `metadata.namespace` is not one of them.

### Render time and execution time

- **Render time.** The value comes from the declaration and the files Nyl reads
  while it renders: the environment's values, a Release input, the Git
  worktree, a locked Git commit, or the target's publication branch. Every
  command that renders the same document reads the same value.
- **Execution time.** The value comes from orchestration state: another unit's
  current receipt, or a promotion record. Only a run resolves it, so it can
  change between runs without a commit. A source whose value does not exist
  yet **blocks** its unit, which waits instead of failing. Ordinary rendering
  rejects execution-time sources.

### Jinja

Jinja in Nyl documents is for string interpolation, such as
`'pr-{{ environment.name }}'`. Templating that decides a value's shape or type,
such as `{{ x | tojson }}` or `{% if %}` around fields, makes the document
invalid YAML before it renders and hides type errors until execution. Use it
only where no source fits.

## Sources

### `$environment`

Reads the environment's effective values: an Environment's values over its
groups' values, or an EnvironmentTemplate instance's values.

```yaml
apiVersion: units.gitops.nyl/v1
kind: KubernetesPublication
metadata:
  name: kubernetes
  labels: {dev: 'true', preview: 'true'}
spec:
  target: {$environment: /target}
  teardownWait: {$environment: {pointer: /teardownWait, default: {strategy: manual}}}
  approval: {$environment: {pointer: /approval, optional: true}}
  timeout: 10m
---
apiVersion: units.gitops.nyl/v1
kind: OpenTofu
metadata:
  name: database
  labels: {dev: 'true', preview: 'true'}
spec:
  variables:
    # dev's own network, or the environment that networkFrom names
    vpc_id: {$unit: {unit: network, output: vpcId, environment: {$environment: {pointer: /networkFrom, optional: true}}}}
```

| Field | Meaning |
| --- | --- |
| `pointer` | A JSON Pointer into the values. The shorthand. |
| `default` | The value when nothing, or `null`, is at `pointer`. Not `null`. |
| `optional` | `true` removes the field when nothing is at `pointer`. |

It resolves when an environment's units render: in `nyl plan`,
`nyl reconcile`, `nyl validate`, `nyl build -e`, and every other command that
renders units. The values are already rendered, so the value is used as it is
and never templated again. A direct `nyl build` without `-e` has no
environment, so a unit with `$environment` needs `-e`.
[`nyl validate`](/nyl/reference/cli/validate/) renders every unit for every
Environment that selects it and for sample instances of every
EnvironmentTemplate, so a pointer one environment lacks fails before any run.

A missing required value fails the unit's render with `NYL-UNIT-INVALID`:

```console
$ nyl validate
…
Configuration error: Rendering the units of Environment dev at 0000000000000000000000000000000000000000 failed: Configuration error: NYL-UNIT-INVALID: unit in units.yaml document 1: spec.timeout.$environment reads /timout, which is not in the values of Environment dev: the values have keys "target", "timeout" (nearest to "timout": "timeout"); set it in the values, or give the source a default, or optional: true to omit the field
```

### `$input`

Reads a Release input in the fields of a HelmChart of the Release's bundle,
such as `includeCrds` and `lookup`, so one bundle serves targets that need
different settings:

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
  includeCrds: {$input: crds}
  lookup: {$input: {name: features, pointer: /lookup}}
```

It resolves when Nyl loads the bundle's files, before anything reads the
HelmChart, so `lookup` read from an input decides the cache and the lookup
scope exactly as a literal would. The input's declared default applies when
the target binds none; a `pointer` that selects nothing, a `null` value, an
input the Release does not declare, and a HelmChart outside a Release that
declares inputs are errors. A HelmChart's chart `values` take Release inputs
through [`valuesFrom`](#helmchart-valuesfrom) or string interpolation
(`'{{ inputs.image }}'`).

```console
$ nyl render apps/web/web.yaml
2026-11-20T02:01:00.837509Z ERROR nyl: Configuration error: HelmChart "web" in apps/web/web.yaml: spec.includeCrds.$input takes a name; did you mean crds?
Hint: Check your nyl.toml syntax and structure. Run 'nyl validate --strict' for detailed validation.
```

### `$parameter`

Reads a parameter an EnvironmentTemplate instance binds, at any depth of the
template's `spec.values`, so a value keeps the parameter's type where a quoted
Jinja string would make it a string:

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
    replicas: {$parameter: {name: sizes, pointer: /web/replicas}}
    target:
      inline:
        # …
        releaseInputs:
          rise/rise:
            clusterWide: {$parameter: crds}
            image: {$unit: {unit: web-image, artifact: image}}   # carried to the unit
```

It resolves when an instance's values are computed: at
`nyl state init --template`, at `nyl reconcile --template`, and at every later
command that names the instance, from the parameters its `state.yaml`
records. Strings with template syntax render first; units render after, so a
unit's `$environment` reads the resolved value. The parameter's declared
default applies when the instance is created without it. An undeclared
parameter fails the instance with `NYL-TEMPLATE-INVALID`, and
[`nyl validate`](/nyl/reference/cli/validate/) computes the values of sample
instances of every template, so it fails before any instance exists:

```console
$ nyl validate
…
2026-11-20T02:01:10.922110Z ERROR nyl: Configuration error: Configuration error: NYL-TEMPLATE-INVALID: EnvironmentTemplate preview does not render to a valid environment preview-sample-1: spec.values.replicas.$parameter names parameter "size", which EnvironmentTemplate preview does not declare (declared: pr, sizes); declare it in spec.parameters, or name a declared one (fix EnvironmentTemplate preview in source)
```

### `$unit`

Reads a unit's output, published artifact, or observed value from its current
receipt. Only a run resolves it: in a unit's spec at any depth, and in a
Release-input binding of a target that a
[KubernetesPublication](/nyl/orchestration/resource-guides/kubernetes-publication/#release-inputs-from-other-units)
names.

```yaml
values:
  host: {$unit: {unit: database, output: host}}
  image: {$unit: {unit: web-image, artifact: image, pointer: /reference}}
  networkId: {$unit: {environment: infra, unit: network, output: id}}
```

| Field | Meaning |
| --- | --- |
| `unit` | The producing unit. |
| `output`, `artifact`, `observation` | Exactly one: a declared output, a published artifact, or a declared [observed value](/nyl/orchestration/runs/#observed-values). |
| `pointer` | A JSON Pointer inside the value. |
| `environment` | The producer's environment, for a [cross-environment reference](/nyl/orchestration/runs/#multi-environment-runs). In a unit document it may be an `$environment` source. |
| `kind` | The expected artifact kind. |
| `evidence`, `attestations` | What the producer must prove beyond a current receipt; see [Requiring attestations](/nyl/orchestration/runs/#requiring-attestations). |

Each reference is a dependency on its producer. A producer without a current
receipt, or an attestation that has not passed, blocks the consumer, which
keeps its previous resolution and runs in a later run or wave. A referenced
output has the type its producer declares, and a sensitive output cannot be
referenced. A cycle fails with `NYL-DEPENDENCY-CYCLE`, a unit the environment
does not select with `NYL-DEPENDENCY-UNKNOWN`, an undeclared attestation with
`NYL-ATTESTATION-UNDECLARED`, and a malformed reference with
`NYL-REFERENCE-INVALID`. A unit's stored spec keeps its references, and a
stored reference that does not parse is an error, never a dropped dependency.

### `$promotion`

Reads a value recorded through a [PromotionPath](/nyl/orchestration/promotion/):
`value`, the promoted value's name, and optionally `path`, the PromotionPath
it must come from. Like `$unit`, only a run resolves it, in a unit's spec or a
Release-input binding. A promotion without a record blocks the consumer
(`NYL-PROMOTION-MISSING`).

```yaml
releaseInputs:
  web/web:
    image: {$promotion: {value: webImage}}
```

### `$file`, `$git`, and `$publication`

Read a Release input's value from a file, in a DeploymentTarget's
`releaseInputs`. See [Release inputs](/nyl/kubernetes-gitops/release-inputs/)
for the workflow.

| Source | Reads |
| --- | --- |
| `$file` | `path`, a YAML or JSON file of this repository under the [local path rule](/nyl/reference/configuration/#local-paths), and `pointer` inside it. |
| `$git` | A file of a Git repository at a locked `commit`, with `repositoryRef` or `repository`, `revision`, `path`, and `pointer`. |
| `$publication` | A state file in the target's own publication branch: `path` relative to its path prefix, `pointer`, and optionally `carryFileFromWorktree`. |

A `$file` file that does not exist, or a `$publication` branch or file that
does not exist yet, leaves the input unbound, so its declared default applies.
A `pointer` that selects nothing in a file that exists is an error.

### Release-input bindings

A DeploymentTarget binds the inputs a Release declares in
`spec.releaseInputs.<group>/<release>.<input>`. A binding is the input's value:
a literal, or one source. Sources inside a literal resolve too.

```yaml
releaseInputs:
  platform/web:
    replicas: 3
    image: registry.example.com/web@sha256:4f0c…
    database: {$file: {path: environments/staging/database.yaml, pointer: /database}}
    sizing: {$git: {repositoryRef: {name: sizing}, revision: main, commit: 3f1c…, path: web.json}}
    tag: {$publication: {path: state/tag.json, pointer: /tag}}
    host: {$unit: {unit: database, output: host}}
    raw: {$literal: {$file: not-a-source}}
```

The effective value is the binding's, otherwise the Release's declared
`default`. Every value is checked against the input's declared `type` and
`enum`; `null` satisfies no type. Nyl checks every binding of a target before
rendering any Release and reports all problems together: unknown Releases or
inputs, missing required values, and type mismatches. A binding reads either
`$unit` and `$promotion` or the sources rendering reads, not both.

## HelmChart `valuesFrom`

A HelmChart can read its chart values from values files of this repository and
from `object` Release inputs, merged in order, before its inline `values`. Use
it for whole objects and for files; use [`$input`](#input) for one field.

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

It is valid in a HelmChart in a file of a Release bundle: the Release entry
file or a file its `spec.include` selects, also without a Release for `file`
entries. A HelmChart that rendering generates, such as one a chart outputs,
cannot use `valuesFrom`. Entries merge in order and the inline `values` merge
last: maps merge key by key, and every other value, lists included, replaces.
That is Helm's own rule for several `-f` files.

The merged values are data: Nyl passes them to Helm as they are. There are no
defaults: a missing file, a `pointer` that selects nothing, and an input the
Release does not declare are errors. Each file is an input of the Release's
render, and of its target's tree, like an included file, and the merged
values are part of the Helm render cache key, so changing a file or an input
renders the chart again.

## Errors

Every error names the location of the source. A source in a context that does
not accept it says where it works:

```console
$ nyl render apps/web/web.yaml
2026-11-20T02:01:10.850437Z ERROR nyl: Configuration error: HelmChart "web" in apps/web/web.yaml: spec.includeCrds holds a $unit source, which is resolved by orchestration and is valid only in unit documents and in a DeploymentTarget's releaseInputs; declare a Release input and bind it on the target with $unit (see https://niklasrosenstein.github.io/nyl/reference/value-sources/)
Hint: Check your nyl.toml syntax and structure. Run 'nyl validate --strict' for detailed validation.
```

A likely slip, and a source with other keys next to it:

```console
$ nyl validate
…
Configuration error: Rendering the units of Environment dev at 0000000000000000000000000000000000000000 failed: Configuration error: NYL-UNIT-INVALID: unit in units.yaml document 1: spec.values.host holds $unit next to other keys (pointer); a value source replaces the whole object, so it must be the object's only key (wrap the object in $literal to keep it as data); spec.values.region.$values is not a value source; did you mean $environment? (write {$literal: …} to keep the key as data)
```
