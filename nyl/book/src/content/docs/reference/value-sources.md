---
title: 'Value sources'
---

A value source says where a value comes from instead of writing it inline: the
environment's values, a Release input, a template parameter, another unit's
receipt, a promotion record, or a file. Every source is written the same way,
an object whose only key is the source's `$` name, and is replaced by the value
it names:

```yaml
target: {$fromEnvironment: /target}
includeCrds: {$fromInput: crds}
vpc_id: {$fromUnit: {unit: network, output: vpcId}}
```

This page lists every source, where each is valid, when it resolves, and what
happens when it finds no value. The guides explain the workflows:
[Release inputs](/nyl/kubernetes-gitops/release-inputs/) for target bindings,
and [References between units](/nyl/orchestration/runs/#references-between-units)
for unit specs.

For a worked example with every source together, see the
[Helm chart previews scenario](https://github.com/NiklasRosenstein/nyl/blob/develop/design/reference-scenarios.md#scenario-6-helm-chart-previews):
a platform deploys its own Helm chart for a `develop` environment and a preview
per pull request, with the image pinned by digest through `$fromUnit` and
`$fromInput`, the chart's CRD and operator switched by a boolean template
parameter, sizing from a `$fromGit` lock, and state read with
`$fromPublication`. Its project is
`nyl/tests/scenarios/reference/helm-preview/project` in the Nyl repository,
and CI runs it with Helm on every change.

## Summary

| Source | Shorthand | Object form | Where it is valid | Resolves | When it finds no value | Type check |
| --- | --- | --- | --- | --- | --- | --- |
| [`$fromEnvironment`](#fromenvironment) | a JSON Pointer: `{$fromEnvironment: /target}` | `{pointer, default?, optional?}` | Unit documents. | Render time, when the unit renders. | `default`, else `optional: true` removes the field, else an error. | The field's own, as a literal. |
| [`$fromInput`](#frominput) | a name: `{$fromInput: image}` | `{name, pointer?}` | Every resource of a Release bundle: plain manifests, HelmChart fields and chart `values`, a Component's `spec`, a RemoteManifest, and `metadata.namespace`. | Render time, when the bundle's files load. | The input's declared default; a `pointer` that selects nothing is an error. | The input's declared `type`, then the field's own, as a literal. |
| [`$fromParameter`](#fromparameter) | a name: `{$fromParameter: pr}` | `{name, pointer?}` | An EnvironmentTemplate's `spec.values`. | When an instance's values are computed. | The parameter's declared default; a required parameter must be passed. | The parameter's declared `type` and `enum`. |
| [`$fromUnit`](#fromunit) | none | `{unit, output \| artifact \| observation, …}` | Unit documents and Release-input bindings. | Execution time. | The consumer waits for the producer. | The producer's declared output type, and the input's where it is bound. |
| [`$fromPromotion`](#frompromotion) | none | `{value, path?}` | Unit documents and Release-input bindings. | Execution time. | The consumer waits for a record. | The input's where it is bound. |
| [`$fromFile`, `$fromGit`, `$fromPublication`](#fromfile-fromgit-and-frompublication) | none | see below | Release-input bindings. | Render time. | The input's declared default; a `pointer` that selects nothing in a file that exists is an error. | The input's declared `type` and `enum`. |
| [`$literal`](#literal) | — | any value | Wherever sources are. | With the document. | — | As the literal. |

## How sources work

### One form

A source is an object whose only key is a known `$` source. It replaces the
whole value it stands for, at any depth of a document's objects and lists:

- A source next to other keys is an error, because it would replace them.
- A source with a shorthand takes its main field as a string:
  `{$fromEnvironment: /target}` is `{$fromEnvironment: {pointer: /target}}`, and
  `{$fromInput: image}` is `{$fromInput: {name: image}}`. A shorthand that looks like
  the other one is an error that suggests the fix: `{$fromInput: /image}` "takes a
  name; did you mean image?", and `{$fromEnvironment: target}` "takes a JSON
  Pointer; did you mean /target?".
- `pointer` is a JSON Pointer, such as `/web/tier`; empty or omitted selects
  the whole value.

### Missing values

When a source finds nothing, or `$fromEnvironment` finds `null`:

- `default` applies. It must not be `null`.
- With `optional: true` the key is removed, so the document is the same as one
  that omits the field.
- Otherwise it is an error that names the location and where the value was
  looked for: the environment, the instance, or the Release, with the keys
  that exist there and the nearest one.

`default` and `optional` are exclusive, and only `$fromEnvironment` takes them:
`$fromInput` and `$fromParameter` use the default their declaration gives, and
`$fromUnit` and `$fromPromotion` wait for their producer.

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
  schema: {$literal: {$fromUnit: not-a-reference}}
```

In a unit document, `$literal` unwraps when the unit renders, unless its
content holds `$` keys: then it unwraps when a run resolves the unit, so its
content never reads as a reference.

### Reserved keys and common typos

In the typed fields of a Nyl resource every `$` key is reserved, so an unknown
one, such as `$ref` in a HelmChart's `chart`, is an error. In free-form data
(chart values, a Component's `spec`, plain Kubernetes manifests, a unit's
`variables` and `values`, environment values, a binding's literal) other `$`
keys such as `$ref` and `$schema` are data, except these common typos of a source's name, which
are errors that name the source you probably meant:

| Typo | Example | Meant |
| --- | --- | --- |
| Another case or separator | `$frominput`, `$from_input`, `$FromInput` | `$fromInput` |
| The noun alone | `$input`, `$unit`, `$environment` | `$fromInput`, `$fromUnit`, `$fromEnvironment` |
| A plural | `$fromInputs`, `$fromUnits`, `$fromParameters` | `$fromInput`, `$fromUnit`, `$fromParameter` |
| An abbreviation | `$fromEnv`, `$fromParam`, `$fromParams` | `$fromEnvironment`, `$fromParameter` |
| What a unit reference reads | `$fromOutput`, `$fromArtifact` | `$fromUnit` |
| A `…From` suffix | `$inputFrom`, `$environmentFrom`, `$unitFrom` | `$fromInput`, `$fromEnvironment`, `$fromUnit` |
| `$from` alone | `$from` | lists every source |

Wrap the object in `$literal` to keep one of them as data.

In a unit document, the kind's schema says which fields are typed: where it
has no place for a `$` key that is no source, the key is an unknown source,
reported with the typo's suggestion or else the source it is closest to,
compared the same way. An object whose schema names its fields is typed, such
as an OciImage's `context`, even when the schema does not reject other keys;
where the schema leaves an object open, such as a Terraform unit's
`variables`, the key is data:

```console
$ nyl validate
…
2026-11-20T05:26:47.605293Z ERROR nyl: Configuration error: units are invalid:
  - dev: NYL-UNIT-INVALID: unit web-image (/nyl/units.yaml): spec.repository.$fromEnviroment is not a value source; `$` keys are reserved for value sources in the typed fields of a resource; did you mean $fromEnvironment? (see https://niklasrosenstein.github.io/nyl/reference/value-sources/) (correct the unit's spec so that it matches its kind's schema)
Hint: Check your nyl.toml syntax and structure. Run 'nyl validate --strict' for detailed validation.
```

## Contexts

Where a document sits decides which sources resolve in it. A source a context
does not accept is an error that says where it works and how to get the value
instead. A source a context *carries* is checked and left in place for the
document it lands in, which resolves it.

| Context | Resolves | Carries |
| --- | --- | --- |
| The typed fields of a Release bundle's Nyl resources: a HelmChart's, a RemoteManifest's, a Release's, and a Component's envelope (`$` keys reserved) | `$fromInput`, `$literal` | — |
| The free-form data of a Release bundle: a HelmChart's `values`, a Component's `spec`, and plain Kubernetes manifests (foreign `$` keys are data) | `$fromInput`, `$literal` | — |
| A Release's `spec.inputs` declarations | — | — |
| A unit document, when the unit renders | `$fromEnvironment` | `$fromUnit` and `$fromPromotion` to the run; in an inline target's `releaseInputs`, also `$fromFile`, `$fromGit`, and `$fromPublication` to the target's rendering |
| A unit's rendered spec, when a run resolves it | `$fromUnit`, `$fromPromotion`, `$literal` | — |
| An EnvironmentTemplate's `spec.values` | `$fromParameter` | `$fromUnit`, `$fromPromotion`, `$fromFile`, `$fromGit`, `$fromPublication`, `$literal` |
| A declared Environment's or EnvironmentGroup's `values` | — | as template values |
| A DeploymentTarget's `releaseInputs`, declared or inline | `$fromFile`, `$fromGit`, `$fromPublication`, `$literal`; `$fromUnit` and `$fromPromotion` through orchestration | — |
| What recursive expansion generates: a chart's output, a Component's, a remote manifest | — (any source is an error) | — |
| Any other field, such as a target's `values` or a template's `parameters` | — | — |

### Data and configuration

A value read from outside your configuration is data and is never read for
sources: a unit's output, artifact, or observed value, a file read with
`$fromFile`, `$fromGit`, `$fromPublication`, or HelmChart `valuesFrom`, a promotion record,
a template parameter, and whatever rendering generates. A value copied from
your configuration carries its sources to where it lands:
`{$fromEnvironment: /target}` can read an inline target whose bindings hold
`$fromUnit`, and the run resolves that `$fromUnit` like one written in the unit. The
environment's values cannot read the environment's values.

Two kinds of data are checked for sources anyway, because one there is always
a mistake:

- A `valuesFrom` file that holds a value source is an error: the file is data,
  so the source would reach the chart as an object. Move it to the inline
  `values`, where it resolves.

  ```console
  $ nyl render apps/web/web.yaml
  2026-11-20T08:07:08.948496Z ERROR nyl: Configuration error: HelmChart "web" in apps/web/web.yaml: spec.valuesFrom[0]: values file apps/web/values-base.yaml holds a $fromInput source at image.tag; value sources are not resolved inside valuesFrom files, which are data; move it to the inline values (see https://niklasrosenstein.github.io/nyl/reference/value-sources/)
  ```

- A value source in the output of recursive expansion (what a HelmChart's or
  a Component's chart renders, a RemoteManifest's documents, and a HelmChart
  either of them generates) is an error that names the resource that generated
  it: sources resolve only in the files of a Release bundle. Foreign `$` keys
  such as `$ref` in that output stay data.

### Carried sources and `nyl validate`

A source that an environment's or a template's values carry is checked where
the values land: the unit document that reads them with `$fromEnvironment`, in
the context at that place. A `$fromFile` that lands in an inline target's
`values` instead of its `releaseInputs`, for example, fails the unit's render:

```console
$ nyl validate
…
Configuration error: Rendering the units of Environment preview-sample-1 at 0000000000000000000000000000000000000000 failed: Configuration error: NYL-UNIT-INVALID: unit in units.yaml document 4: spec.target.inline.values.config holds a $fromFile source, which fills a Release input and is valid only in a DeploymentTarget's releaseInputs; bind it to a Release input in the DeploymentTarget's releaseInputs (see https://niklasrosenstein.github.io/nyl/reference/value-sources/); it came with the environment's values that $fromEnvironment read here
```

[`nyl validate`](/nyl/reference/cli/validate/) renders every unit for every
Environment and for sample instances of every EnvironmentTemplate, so these
checks run before any instance exists. A source the values may not hold at
all, such as `$fromInput` in a template's values, fails there too. A carried
source that no unit of an environment reads never resolves; `nyl validate`
warns about it with `NYL-VALUES-SOURCE-UNREAD`, per environment and per sample
instance:

```console
$ nyl validate
…
⚠ NYL-VALUES-SOURCE-UNREAD: spec.values.unused of instance preview-sample-1 holds a $fromFile source that no unit of the environment reads: a source in values resolves only where a unit reads it with $fromEnvironment (read the value in a unit with $fromEnvironment, or remove it from the values)
```

The check follows each source into the units' rendered specs, so a value a
unit reads with structural Jinja counts as read as well.

### Literal-only fields

`apiVersion`, `kind`, `metadata.name`, and `metadata.labels` are read by
discovery and selection before anything resolves, so a source in them is an
error, in a unit document and in every resource of a Release bundle.
`metadata.namespace` is not one of them: a Release, its Nyl resources, and its
manifests may read it with `{$fromInput: namespace}`.

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

### Editor schemas

The published JSON Schemas offer the sources a context accepts next to the
top-level `spec` fields of the resources that take them, generated from the
context table: `$fromEnvironment`, `$fromUnit`, `$fromPromotion`, and `$literal` for the
unit kinds, `$fromInput` and `$literal` for HelmChart and RemoteManifest, and the
binding sources for a DeploymentTarget's `releaseInputs`. Sources are valid
at any depth of those fields, in a plain manifest, in a Component's `spec`,
and in `metadata.namespace` too, but editors are not offered them there, and
may flag such a source that Nyl accepts. See
[Editor schemas](/nyl/reference/editor-schemas/).

### Jinja

Jinja in Nyl documents is for string interpolation, such as
`'pr-{{ environment.name }}'`. Templating that decides a value's shape or type,
such as `{{ x | tojson }}` or `{% if %}` around fields, makes the document
invalid YAML before it renders and hides type errors until execution. Use it
only where no source fits.

## Sources

### `$fromEnvironment`

`$fromEnvironment` reads a value from the environment's effective values: an Environment's values over its
groups' values, or an EnvironmentTemplate instance's values.

```yaml
apiVersion: units.gitops.nyl/v1
kind: KubernetesPublication
metadata:
  name: kubernetes
  labels: {dev: 'true', preview: 'true'}
spec:
  target: {$fromEnvironment: /target}
  teardownWait: {$fromEnvironment: {pointer: /teardownWait, default: {strategy: manual}}}
  approval: {$fromEnvironment: {pointer: /approval, optional: true}}
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
    vpc_id: {$fromUnit: {unit: network, output: vpcId, environment: {$fromEnvironment: {pointer: /networkFrom, optional: true}}}}
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
environment, so a unit with `$fromEnvironment` needs `-e`.
[`nyl validate`](/nyl/reference/cli/validate/) renders every unit for every
Environment that selects it and for sample instances of every
EnvironmentTemplate, so a pointer one environment lacks fails before any run.

A missing required value fails the unit's render with `NYL-UNIT-INVALID`:

```console
$ nyl validate
…
Configuration error: Rendering the units of Environment dev at 0000000000000000000000000000000000000000 failed: Configuration error: NYL-UNIT-INVALID: unit in units.yaml document 1: spec.timeout.$fromEnvironment reads /timout, which is not in the values of Environment dev: the values have keys "target", "timeout" (nearest to "timout": "timeout"); set it in the values, or give the source a default, or optional: true to omit the field
```

### `$fromInput`

`$fromInput` reads a value from a Release input, in any resource of the Release's bundle: the Release
entry file and every file its `spec.include` selects. One bundle then serves
targets that bind different values, without templating its structure:

```yaml
apiVersion: k8s.nyl/v1
kind: Release
metadata:
  name: web
  namespace: {$fromInput: namespace}
spec:
  inputs:
    namespace: {type: string, default: web}
    image: {type: string}
    replicas: {type: integer, default: 2}
    crds: {type: boolean, default: true}
    features: {type: object, default: {lookup: false}}
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: web
  namespace: {$fromInput: namespace}
spec:
  replicas: {$fromInput: replicas}
  template:
    spec:
      containers:
        - name: web
          image: {$fromInput: image}
---
apiVersion: k8s.nyl/v1
kind: HelmChart
metadata:
  name: worker
  namespace: {$fromInput: namespace}
spec:
  chart: {name: ./chart}
  includeCrds: {$fromInput: crds}
  lookup: {$fromInput: {name: features, pointer: /lookup}}
  values:
    image: {$fromInput: image}
    schema: {$ref: '#/definitions/x'}   # foreign `$` keys in chart values are data
```

| Where | Context |
| --- | --- |
| Plain Kubernetes manifests, a HelmChart's `values`, a Component's `spec` | Free-form data: foreign `$` keys such as `$ref` stay data. |
| The fields of a HelmChart, a RemoteManifest (such as `url`), the Release, and a Component's envelope | Typed: every `$` key is reserved. |
| `metadata.namespace` of any of them | Allowed. `apiVersion`, `kind`, `metadata.name`, and `metadata.labels` are literal. |
| A Release's `spec.inputs` | Literal declarations: no source. |

It resolves when Nyl loads the bundle's files, before anything reads them, so
`lookup` read from an input decides the cache and the lookup scope exactly as
a literal would, and a manifest's namespace is resolved before namespace
defaults apply. A HelmChart's inline `values` resolve before
[`valuesFrom`](#helmchart-valuesfrom) merges, and still merge last. The
value has the input's declared type: the target's binding, or the declared
default when the target binds none. An input a target binds with `$fromUnit`
or `$fromPromotion` has no value before a run, so
[`nyl validate`](/nyl/reference/cli/validate/) gives it a placeholder of its
declared type, and what a `pointer` selects in an `object` or `array` input
is unknown and not checked: an `object` input that holds an image artifact
can be read with `{$fromInput: {name: image, pointer: /digest}}`. A `pointer`
into a `string`, `integer`, `number`, or `boolean` input, a `pointer` that selects nothing, a
`null` value, an input the Release does not declare, and a resource outside a
Release that declares inputs are errors. String interpolation such as
`'web-{{ inputs.tier }}'` stays Jinja.

The Release's inputs are part of the Release's and the target's render cache
keys, and the resolved chart values are part of the Helm cache key, so a
changed binding renders again. What a chart or a remote manifest generates
takes no `$fromInput`: see [Data and configuration](#data-and-configuration).

```console
$ nyl render apps/web/web.yaml
2026-11-20T08:07:09.027321Z ERROR nyl: Configuration error: Deployment "web" in apps/web/web.yaml: spec.template.spec.containers[0].env[0].value holds a $fromUnit source, which is resolved by orchestration and is valid only in unit documents and in a DeploymentTarget's releaseInputs; declare a Release input and bind it on the target with $fromUnit (see https://niklasrosenstein.github.io/nyl/reference/value-sources/); spec.template.spec.containers[0].image.$inputs is not a value source; did you mean $fromInput? (write {$literal: …} to keep the key as data)
```

### `$fromParameter`

`$fromParameter` reads a value from a parameter an EnvironmentTemplate
instance binds, at any depth of the
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
    replicas: {$fromParameter: {name: sizes, pointer: /web/replicas}}
    target:
      inline:
        # …
        releaseInputs:
          rise/rise:
            clusterWide: {$fromParameter: crds}
            image: {$fromUnit: {unit: web-image, artifact: image}}   # carried to the unit
```

It resolves when an instance's values are computed: at
`nyl state init --template`, at `nyl reconcile --template`, and at every later
command that names the instance, from the parameters its `state.yaml`
records. Strings with template syntax render first; units render after, so a
unit's `$fromEnvironment` reads the resolved value. The parameter's declared
default applies when the instance is created without it. An undeclared
parameter fails the instance with `NYL-TEMPLATE-INVALID`, and
[`nyl validate`](/nyl/reference/cli/validate/) computes the values of sample
instances of every template, so it fails before any instance exists:

```console
$ nyl validate
…
2026-11-20T08:07:52.188460Z ERROR nyl: Configuration error: NYL-TEMPLATE-INVALID: EnvironmentTemplate preview does not render to a valid environment preview-sample-1: spec.values.replicas.$fromParameter names parameter "size", which EnvironmentTemplate preview does not declare (declared: crds, pr, sizes); declare it in spec.parameters, or name a declared one (fix EnvironmentTemplate preview in source)
Hint: Check your nyl.toml syntax and structure. Run 'nyl validate --strict' for detailed validation.
```

A template's values read parameters, not Release inputs. `$fromInput` that
names a declared parameter is an error that gives the `$fromParameter` to write
instead:

```console
$ nyl validate
…
2026-11-20T08:06:03.723230Z ERROR nyl: Configuration error: NYL-TEMPLATE-INVALID: EnvironmentTemplate preview does not render to a valid environment preview-sample-1: spec.values.target.inline.releaseInputs.platform/platform.clusterWide holds a $fromInput source, which reads a Release input and resolves only in the resources of a Release bundle; EnvironmentTemplate preview declares parameter "crds", which its values read with $fromParameter; write {$fromParameter: crds} (see https://niklasrosenstein.github.io/nyl/reference/value-sources/) (fix EnvironmentTemplate preview in source)
Hint: Check your nyl.toml syntax and structure. Run 'nyl validate --strict' for detailed validation.
```

### `$fromUnit`

`$fromUnit` reads a value from another unit's receipt: its output, published
artifact, or observed value. Only a run resolves it: in a unit's spec at any depth, and in a
Release-input binding of a target that a
[KubernetesPublication](/nyl/orchestration/resource-guides/kubernetes-publication/#release-inputs-from-other-units)
names.

```yaml
values:
  host: {$fromUnit: {unit: database, output: host}}
  image: {$fromUnit: {unit: web-image, artifact: image, pointer: /reference}}
  networkId: {$fromUnit: {environment: infra, unit: network, output: id}}
```

| Field | Meaning |
| --- | --- |
| `unit` | The producing unit. |
| `output`, `artifact`, `observation` | Exactly one: a declared output, a published artifact, or a declared [observed value](/nyl/orchestration/runs/#observed-values). |
| `pointer` | A JSON Pointer inside the value. |
| `environment` | The producer's environment, for a [cross-environment reference](/nyl/orchestration/runs/#multi-environment-runs). In a unit document it may be an `$fromEnvironment` source. |
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

### `$fromPromotion`

`$fromPromotion` reads a value from a promotion record, written through a [PromotionPath](/nyl/orchestration/promotion/):
`value`, the promoted value's name, and optionally `path`, the PromotionPath
it must come from. Like `$fromUnit`, only a run resolves it, in a unit's spec or a
Release-input binding. A promotion without a record blocks the consumer
(`NYL-PROMOTION-MISSING`).

```yaml
releaseInputs:
  web/web:
    image: {$fromPromotion: {value: webImage}}
```

### `$fromFile`, `$fromGit`, and `$fromPublication`

These read a Release input's value from a file: `$fromFile` from a file of
this repository, `$fromGit` from a file at a locked commit, and
`$fromPublication` from a state file in the target's publication branch. They
are valid in a DeploymentTarget's `releaseInputs`. See [Release inputs](/nyl/kubernetes-gitops/release-inputs/)
for the workflow.

| Source | Reads |
| --- | --- |
| `$fromFile` | `path`, a YAML or JSON file of this repository under the [local path rule](/nyl/reference/configuration/#local-paths), and `pointer` inside it. |
| `$fromGit` | A file of a Git repository at a locked `commit`, with `repositoryRef` or `repository`, `revision`, `path`, and `pointer`. |
| `$fromPublication` | A state file in the target's own publication branch: `path` relative to its path prefix, `pointer`, and optionally `carryFileFromWorktree`. |

A `$fromFile` file that does not exist, or a `$fromPublication` branch or file that
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
    database: {$fromFile: {path: environments/staging/database.yaml, pointer: /database}}
    sizing: {$fromGit: {repositoryRef: {name: sizing}, revision: main, commit: 3f1c…, path: web.json}}
    tag: {$fromPublication: {path: state/tag.json, pointer: /tag}}
    host: {$fromUnit: {unit: database, output: host}}
    raw: {$literal: {$fromFile: not-a-source}}
```

The effective value is the binding's, otherwise the Release's declared
`default`. Every value is checked against the input's declared `type` and
`enum`; `null` satisfies no type. Nyl checks every binding of a target before
rendering any Release and reports all problems together: unknown Releases or
inputs, missing required values, and type mismatches. A binding reads either
`$fromUnit` and `$fromPromotion` or the sources rendering reads, not both.

## HelmChart `valuesFrom`

A HelmChart can read its chart values from values files of this repository and
from `object` Release inputs, merged in order, before its inline `values`. Use
it for whole objects and for files; use [`$fromInput`](#frominput) in the inline
`values` for one field, at any depth.

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

The inline `values` have their `$fromInput` sources resolved before the merge.
The merged values are data: Nyl passes them to Helm as they are, and a values
file that holds a value source is an error that says to move it to the inline
`values` (see [Data and configuration](#data-and-configuration)). There are no
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
2026-11-20T08:07:09.102785Z ERROR nyl: Configuration error: HelmChart "web" in apps/web/web.yaml: spec.includeCrds holds a $fromUnit source, which is resolved by orchestration and is valid only in unit documents and in a DeploymentTarget's releaseInputs; declare a Release input and bind it on the target with $fromUnit (see https://niklasrosenstein.github.io/nyl/reference/value-sources/)
```

A common typo, and a source with other keys next to it:

```console
$ nyl validate
…
2026-11-20T08:07:44.058147Z ERROR nyl: Configuration error: NYL-UNIT-INVALID: unit in units.yaml document 1: spec.values.host holds $fromUnit next to other keys (pointer); a value source replaces the whole object, so it must be the object's only key (wrap the object in $literal to keep it as data); spec.values.region.$fromEnv is not a value source; did you mean $fromEnvironment? (write {$literal: …} to keep the key as data)
Hint: Check your nyl.toml syntax and structure. Run 'nyl validate --strict' for detailed validation.
```
