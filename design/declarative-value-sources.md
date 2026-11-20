# Declarative value sources

**Status:** implemented for unit documents, EnvironmentTemplate values,
DeploymentTarget Release-input bindings, and HelmChart fields in Release
bundles. `$input` in HelmChart `spec.values`, a Component's `spec`, a
RemoteManifest, and plain Kubernetes manifests of a Release bundle is the
next step (see [Open](#open)).

*Structural* templating is Jinja that makes a document invalid YAML before it
renders, or that decides a value's shape or type (`{{ x | tojson }}`,
`{% if %}`, `{% for %}`). Quoted string interpolation
(`'pr-{{ environment.name }}'`) is not the problem and stays. This note
replaces structural templating with one uniform model of typed value sources.
Jinja stays for string interpolation and is otherwise a last resort: an
example that still needs structural Jinja is a gap to close with a source, not
a pattern to document.

## Why one model

An earlier draft of this note proposed a separate clause per use, each with
its own syntax, its own place (some on a closed list of fields that each
driver marked in its schema), and its own rule for a missing value; target
bindings wrapped every literal in an object of its own. A UX review of the
draft found that users had to learn field by field where each clause was
allowed, that a clause in the wrong place was silently data, that the literal
wrapper was noise, and that a pointer and a name were easy to swap. The
decision was to replace all of it, as a clean break without a deprecation
period, by one model:

- one syntax, `$` keys, shared by every source;
- one walker that recognises a source at any depth;
- one table that says, per context, which sources resolve there;
- specific errors for the mistakes the review saw.

## The model

An object whose only key is a known `$` source is a reference, at any depth of
a document the user wrote, and is replaced by the value it names:

```yaml
target: {$environment: /target}
teardownWait: {$environment: {pointer: /teardownWait, default: {strategy: manual}}}
approval: {$environment: {pointer: /approval, optional: true}}
variables:
  vpc_id: {$unit: {unit: network, output: vpcId, environment: {$environment: {pointer: /networkFrom, optional: true}}}}
```

| Source | Shorthand | Object form | Reads |
| --- | --- | --- | --- |
| `$environment` | a JSON Pointer, `{$environment: /target}` | `{pointer, default?, optional?}` | The environment's effective values: an Environment's over its groups', or a template instance's. |
| `$input` | a name, `{$input: image}` | `{name, pointer?}` | A Release's declared, typed input. |
| `$parameter` | a name, `{$parameter: pr}` | `{name, pointer?}` | An EnvironmentTemplate instance's typed parameter. |
| `$unit` | none | `{unit, output \| artifact \| observation, pointer?, environment?, kind?, evidence?, attestations?}` | A unit's output, artifact, or observed value from its current receipt. |
| `$promotion` | none | `{value, path?}` | A value recorded through a PromotionPath. |
| `$file`, `$git`, `$publication` | none | as the former binding kinds | A file of this repository, a file at a locked commit, a state file in the target's publication branch. |
| `$literal` | — | any value | Its content, verbatim and never walked. |

Every source shares parsing, the missing-value rules, and the type checks;
only the context decides which sources are allowed. The rules:

- **Shorthand misuse.** `{$input: /image}` fails with "`$input` takes a name;
  did you mean image?", and `{$environment: target}` with "`$environment`
  takes a JSON Pointer; did you mean /target?". These are the two slips the
  review saw most.
- **Missing values.** When a source finds nothing (or `null` for
  `$environment`): `default` applies, and it must not be `null`; with
  `optional: true` the key is removed, so the rendered spec is identical to
  omitting the field; otherwise it is an error that names the location, the
  context (the environment, the instance, or the Release), the keys that
  exist, and the nearest one. `default` and `optional` are exclusive. `$input`
  and `$parameter` have no `default` of their own: the declaration's applies.
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
  foreign keys such as `$ref` or `$schema` stay data, but a fixed list of
  likely slips (`$value`, `$values`, `$env`, `$inputs`, `$param`, `$params`,
  `$parameters`, `$units`, `$output`, `$artifact`) is an error with a
  suggestion.

### Contexts

| Context | Resolves | Carries |
| --- | --- | --- |
| Release bundle: the fields of a HelmChart | `$input`, `$literal` | — |
| Unit document, when the unit renders | `$environment` | `$unit`, `$promotion` to the run; in an inline target's `releaseInputs` also `$file`, `$git`, `$publication` to the target's rendering |
| Unit document, when a run resolves it | `$unit`, `$promotion`, `$literal` | — |
| EnvironmentTemplate `spec.values` | `$parameter` | `$unit`, `$promotion`, `$file`, `$git`, `$publication`, `$literal`, checked where the values land |
| Declared Environment and EnvironmentGroup `values` | — | as template values |
| DeploymentTarget `releaseInputs`, declared or inline | `$file`, `$git`, `$publication`, `$unit`, `$promotion` (the last two only through orchestration), `$literal` | — |
| Anything else, such as a target's other fields or a template's parameter declarations | — | — |

A source a context rejects is an error that teaches the fix, such as `$unit`
in a Release: "declare a Release input and bind it on the target with
`$unit`". A bare literal binding is the input's value; there is no wrapper.

- **Data vs. configuration.** A value read from outside the user's
  configuration (a unit's output, artifact, or observed value; a file read with
  `$file`, `$git`, `$publication`, or a `valuesFrom` file; a promotion record;
  a template parameter) is data and is never walked. A value copied from the
  user's configuration (an environment's values) carries its references:
  `{$environment: /target}` can read an inline target whose bindings hold
  `$unit`, and the `$unit` resolves when the run resolves the unit. The
  environment's values cannot read the environment's values.
- **Literal-only fields.** `apiVersion`, `kind`, `metadata.name`, and
  `metadata.labels` are read by discovery and selection before anything
  resolves, so a source there is an error. `metadata.namespace` may hold one.
- **`$literal` in unit documents.** A unit renders before a run resolves it,
  and the rendered spec is stored. `$literal` unwraps when the unit renders,
  unless its content holds `$` keys; then it unwraps when the run resolves the
  unit, so its content never reads as a reference.
- **HelmChart `valuesFrom`** stays for whole-object merges and files:
  `[{file | input, pointer?, key?}]`, merged in order, inline `values` last.
  Its merged values are data.
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
- **No schema-driven machinery.** The walker needs no per-field markers, so
  the earlier `Resolvable<T, S>` schema type, the `x-nyl-resolvable` pointer
  lists, and the catalogue's per-kind field lists are gone. Editor schemas
  offer the sources of a Release-input binding; unit fields show only their
  literal type, so an editor may flag a source there that Nyl accepts.
- **Stored specs.** A unit's desired document stores its rendered spec with
  its `$unit` and `$promotion` references. Every reader of stored specs parses
  them with the walker, and a reference that does not parse is an error, never
  a dropped dependency edge. This changed the stored form in place: state
  written by Nyl 0.7 is not readable as is and is re-created with
  `nyl state init --fresh`.

## Open

- `$input` in HelmChart `spec.values`, a Component's `spec`, a RemoteManifest,
  and plain Kubernetes manifests of a Release bundle, and `metadata.namespace`
  sources there.
- An error for any `$` source in the output of recursive expansion (chart
  output, remote manifests), and a hint for one in a `valuesFrom` file.
- `nyl validate` checking the sources a template's values carry against the
  context they land in, per sample instance.
