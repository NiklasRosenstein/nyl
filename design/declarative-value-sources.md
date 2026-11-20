# Declarative value sources

**Status:** implemented for unit documents, EnvironmentTemplate and
environment values, DeploymentTarget Release-input bindings, and every
resource of a Release bundle.

*Structural* templating is Jinja that makes a document invalid YAML before it
renders, or that decides a value's shape or type (`{{ x | tojson }}`,
`{% if %}`, `{% for %}`). Quoted string interpolation
(`'pr-{{ environment.name }}'`) is not the problem and stays. This note
describes one uniform model of typed value sources in its place.
Jinja stays for string interpolation and is otherwise a last resort: an
example that still needs structural Jinja is a gap to close with a source, not
a pattern to document.

## Principles

- one syntax, `$` keys, shared by every source;
- one walker that recognises a source at any depth;
- one table that says, per context, which sources resolve there;
- specific errors for the likely mistakes.

Every source that reads a value is named for its direction (`$fromUnit`,
`$fromInput`, …), so a reader with limited context sees that it pulls a value
in; `$literal` reads from nowhere, so it has no prefix.

## The model

An object whose only key is a known `$` source is a reference, at any depth of
a document the user wrote, and is replaced by the value it names:

```yaml
target: {$fromEnvironment: /target}
teardownWait: {$fromEnvironment: {pointer: /teardownWait, default: {strategy: manual}}}
approval: {$fromEnvironment: {pointer: /approval, optional: true}}
variables:
  vpc_id: {$fromUnit: {unit: network, output: vpcId, environment: {$fromEnvironment: {pointer: /networkFrom, optional: true}}}}
```

| Source | Shorthand | Object form | Reads |
| --- | --- | --- | --- |
| `$fromEnvironment` | a JSON Pointer, `{$fromEnvironment: /target}` | `{pointer, default?, optional?}` | The environment's effective values: an Environment's over its groups', or a template instance's. |
| `$fromInput` | a name, `{$fromInput: image}` | `{name, pointer?}` | A Release's declared, typed input. |
| `$fromParameter` | a name, `{$fromParameter: pr}` | `{name, pointer?}` | An EnvironmentTemplate instance's typed parameter. |
| `$fromUnit` | none | `{unit, output \| artifact \| observation, pointer?, environment?, kind?, evidence?, attestations?}` | A unit's output, artifact, or observed value from its current receipt. |
| `$fromPromotion` | none | `{value, path?}` | A value recorded through a PromotionPath. |
| `$fromFile`, `$fromGit`, `$fromPublication` | none | `{path, pointer?}`; `{repository \| repositoryRef, revision, commit, path, pointer?}`; `{path, pointer?, carryFileFromWorktree?}` | A file of this repository, a file at a locked commit, a state file in the target's publication branch. |
| `$literal` | — | any value | Its content, verbatim and never walked. |

Every source shares parsing, the missing-value rules, and the type checks;
only the context decides which sources are allowed. The rules:

- **Shorthand misuse.** `{$fromInput: /image}` fails with "`$fromInput` takes a name;
  did you mean image?", and `{$fromEnvironment: target}` with "`$fromEnvironment`
  takes a JSON Pointer; did you mean /target?".
- **Missing values.** When a source finds nothing (or `null` for
  `$fromEnvironment`): `default` applies, and it must not be `null`; with
  `optional: true` the key is removed, so the rendered spec is identical to
  omitting the field; otherwise it is an error that names the location, the
  context (the environment, the instance, or the Release), the keys that
  exist, and the nearest one. `default` and `optional` are exclusive. `$fromInput`
  and `$fromParameter` have no `default` of their own: the declaration's applies.
- **Types.** The resolved document is deserialized and validated exactly as a
  literal would be, so a field's schema checks the value; a Release input or a
  template parameter is also checked against its declaration.
- **Siblings.** A known source next to other keys is an error, because the
  source would replace them.
- **`$literal`.** `{$literal: <anything>}` is the escape hatch for data that
  looks like a source.
- **The `$` namespace.** Inside Nyl's typed resource fields every `$` key is
  reserved, so an unknown one is an error. Inside free-form data (chart values,
  a Component's spec, plain manifests, a unit's `variables` and `values`)
  foreign keys such as `$ref` or `$schema` stay data, but a common typo of a
  source's name is an error with a suggestion: another case or separator
  (`$frominput`, `$from_input`, `$FromInput`), the noun alone (`$input`,
  `$unit`), a plural or abbreviation (`$fromInputs`, `$fromParams`, `$env`,
  `$fromOutput`, `$fromArtifact`), a `…From` suffix (`$inputFrom`,
  `$environmentFrom`), and `$from` alone, which lists every source; case,
  `_`, and `-` are ignored. In a unit document the kind's `spec_schema` says
  which fields are typed: where the schema has no place for a `$` key that is
  no source, the key is an unknown source, reported with the typo's
  suggestion or else the nearest source under the same normalisation. An
  object whose schema names its members (`properties` or `required`) is typed
  even when the schema does not close it; only an object the schema leaves
  free-form (no members named) or opens explicitly (`additionalProperties`)
  holds such a key as data. The check reads only the schema, so it is the
  same for every driver, and it runs before the driver's own validation, whose
  error for the same spec (such as a required field the `$` key stands in
  for) would not name the typo.

### Contexts

| Context | Resolves | Carries |
| --- | --- | --- |
| Release bundle, typed fields: a HelmChart's, a RemoteManifest's, a Release's, a Component's envelope (`$` keys reserved) | `$fromInput`, `$literal` | — |
| Release bundle, free-form data: a HelmChart's `spec.values`, a Component's `spec`, plain Kubernetes manifests (foreign `$` keys are data) | `$fromInput`, `$literal` | — |
| A Release's `spec.inputs` declarations | — | — |
| Unit document, when the unit renders | `$fromEnvironment` | `$fromUnit`, `$fromPromotion` to the run; in an inline target's `releaseInputs` also `$fromFile`, `$fromGit`, `$fromPublication` to the target's rendering |
| Unit document, when a run resolves it | `$fromUnit`, `$fromPromotion`, `$literal` | — |
| EnvironmentTemplate `spec.values` | `$fromParameter` | `$fromUnit`, `$fromPromotion`, `$fromFile`, `$fromGit`, `$fromPublication`, `$literal`, checked where the values land |
| Declared Environment and EnvironmentGroup `values` | — | as template values |
| DeploymentTarget `releaseInputs`, declared or inline | `$fromFile`, `$fromGit`, `$fromPublication`, `$fromUnit`, `$fromPromotion` (the last two only through orchestration), `$literal` | — |
| Output of recursive expansion: a chart's or a Component's output, a remote manifest, a HelmChart either generates | — (any source is an error naming the expanding resource) | — |
| Anything else, such as a target's other fields or a template's parameter declarations | — | — |

A source a context rejects is an error that teaches the fix, such as `$fromUnit`
in a Release: "declare a Release input and bind it on the target with
`$fromUnit`". Where the context's provider knows a source valid there that
reads what was meant, the fix names it instead: `$fromInput: crds` in an
EnvironmentTemplate's values that declares parameter `crds` says to write
`{$fromParameter: crds}`. A bare literal binding is the input's value.

A configuration mistake with value sources in what a unit renders, such as a
Release bundle of a KubernetesPublication's target, fails the unit with
`NYL-UNIT-INVALID` and the fix the message names, in a run as in `nyl
validate`; messages name files as provenance records them, relative to the
project, never a run's temporary checkout.

`nyl validate` compiles a target an environment places with a placeholder of
the declared type for each Release input bound with `$fromUnit` or
`$fromPromotion`. What a `$fromInput` `pointer` selects in such an input of
type `object` or `array` is unknown (a pointer into a scalar input selects
nothing and stays an error), like a value a run is blocked on: validation does not check the
pointer or the selected value's type (the value is a placeholder string), and
a `valuesFrom` entry with a `pointer` into it contributes nothing; the input's
declared type and every structural check still apply.

- **Data vs. configuration.** A value read from outside the user's
  configuration (a unit's output, artifact, or observed value; a file read with
  `$fromFile`, `$fromGit`, `$fromPublication`, or a `valuesFrom` file; a promotion record;
  a template parameter) is data and is never walked. A value copied from the
  user's configuration (an environment's values) carries its references:
  `{$fromEnvironment: /target}` can read an inline target whose bindings hold
  `$fromUnit`, and the `$fromUnit` resolves when the run resolves the unit. The
  environment's values cannot read the environment's values.
- **Literal-only fields.** `apiVersion`, `kind`, `metadata.name`, and
  `metadata.labels` are read by discovery and selection before anything
  resolves, so a source there is an error. `metadata.namespace` may hold one.
- **`$literal` in unit documents.** A unit renders before a run resolves it,
  and the rendered spec is stored. `$literal` unwraps when the unit renders,
  unless its content holds `$` keys; then it unwraps when the run resolves the
  unit, so its content never reads as a reference.
- **Data that must hold no source.** A known source in a `valuesFrom` file is
  an error with the hint to move it to the inline values, and a known source
  in the output of recursive expansion is an error naming the resource that
  generated it: both are data, where a source would silently reach the chart
  or the cluster as an object. Foreign `$` keys there stay data.
- **Carried sources.** Template and environment values carry sources to the
  unit documents that read them with `$fromEnvironment`, whose context at that
  place checks them, so a source that lands where it is not valid fails the
  unit's render with a note that the values carried it. `nyl validate`
  renders every environment and sample instances of every template, so these
  checks run before an instance exists; a carried source that no selected
  unit's rendered spec holds landed nowhere, and validate warns about it
  (`NYL-VALUES-SOURCE-UNREAD`).
- **HelmChart `valuesFrom`** stays for whole-object merges and files:
  `[{file | input, pointer?, key?}]`, merged in order, inline `values` (with
  their `$fromInput` sources resolved) last. Its merged values are data.
- **Component value imports** stay as they are: target values reach nested
  invocations without sources.

## Implementation shape

```text
authored YAML → Jinja (strings only) → Value
  → resolve the context's sources at any depth (nyl-core, pure)
  → validate and deserialize, exactly as a literal
```

- **One walker.** `bindings::resolve_document(field, pointer, document,
  scope, providers, issues)` walks a `serde_json::Value`, classifies each
  object (data, a source, an error), and asks the context
  (`bindings::Context::treats`) what to do with each source: resolve, carry,
  or reject. `bindings::find_clauses` walks the same way without reading
  anything, for callers that need the references of a document before any has
  a value: the dependency graph, teardown order, consumer protection, observed
  value changes, and PromotionPath readers. A `Scope` gives parts of a
  document their own context, such as an inline target's `releaseInputs` in a
  publication unit.
- **One provider trait.** Each source is one `Providers` method; a caller
  implements the sources it reads. Release-input bindings (`bindings::resolve`)
  read a binding that is one source through its provider, so a source with no
  value yet leaves the slot unbound and the declared default applies; a literal
  binding is walked for sources inside it.
- **Drivers never see a source they cannot use.** Render-time sources resolve
  in environment unit rendering, which every entry point uses; execution-time
  sources resolve in the run's reference resolver. Drivers take the spec as
  `&Value` and their typed structs keep resolved types.
- **Execution keys are unchanged.** They hash the resolved spec, so a field
  filled by a source hashes exactly like the same literal.
- **No schema-driven machinery.** The walker needs no per-field markers:
  which sources a field accepts follows from its context alone.
- **Editor schemas** are generated from the context table
  (`bindings::source_variants`, `bindings::offer_sources`): a Release-input
  binding offers its sources, and the top-level `spec` fields of the unit
  kinds, HelmChart, and RemoteManifest offer theirs as `anyOf` alternatives.
  Deeper fields, free-form data, and `metadata.namespace` show only their
  literal type, so an editor may flag a source there that Nyl accepts; that
  keeps the schemas small and every offered source correct.
- **Release bundles** resolve each resource right after its file is parsed
  (`render::sources`), before anything reads it, so `lookup` detection, cache
  decisions, and namespace defaults see resolved values. The Release's inputs
  are part of the Release and target cache keys, and the resolved documents
  are what the Helm and remote manifest caches key on.
- **Stored specs.** A unit's desired document stores its rendered spec with
  its `$fromUnit` and `$fromPromotion` references. Every reader of stored specs parses
  them with the walker, and a reference that does not parse is an error, never
  a dropped dependency edge.
