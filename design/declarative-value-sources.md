# Declarative value sources

**Status:** proposed, revised after a case study (below). No schema or state
changes yet.

*Structural* templating is Jinja that makes a document invalid YAML before it
renders, or that decides a value's shape or type (`{{ x | tojson }}`,
`{% if %}`, `{% for %}`). Quoted string interpolation
(`'pr-{{ environment.name }}'`) is not the problem and stays. This note
replaces structural templating with a few typed source clauses. Jinja stays
for string interpolation and is otherwise a last resort: an example that still
needs structural Jinja is a gap to close with a clause, not a pattern to
document. The note deliberately does **not** add a general mechanism that
resolves sources anywhere.

## Proposal

### 1. `HelmChart.spec.valuesFrom`

```yaml
kind: HelmChart
spec:
  chart: {name: ../helm/rise}
  valuesFrom:
    - file: ../helm/rise/values-preview.yaml   # same rules as Cluster/DeploymentTarget valuesFrom
    - input: chartValues                       # an object-typed Release input
      pointer: /optional
      key: optionalMountKey
  values: {fullnameOverride: '{{ inputs.name }}'}
```

- Entries `deep_merge_value` in order, then inline `values`. Maps merge and everything else replaces, which is also Helm's `-f` rule.
- `input` entries are allowed only in a Release bundle that declares the input.
- The merged values are data. Nyl never walks them for source clauses, before or after the merge, and that includes values that embed Nyl resources.
- Every cache that covers a HelmChart's output must key on the merged values, file bytes included.

### 2. `fromValues` on declared unit fields

In unit documents, a field that its owner declares *resolvable* accepts a
literal or one clause:

```yaml
target: {fromValues: {pointer: /target}}
teardownWait: {fromValues: {pointer: /teardownWait, default: {strategy: manual}}}
approval: {fromValues: {pointer: /approval, optional: true}}
```

- **Where the pointer reads.** `pointer` is rooted at the environment's effective `values`, from an Environment or a template instance. The value it reads is the already rendered value and is never templated again.
- **When a value is missing.** If nothing is at the pointer, or the value is null:
  - `default` applies if set, and it must be non-null;
  - with `optional: true` the key is removed, so the rendered spec is identical to omitting the field;
  - otherwise it is an error.
- **Who declares which fields are resolvable.** Core declares resolvable common fields (`approval`, `timeout`); each driver declares its own (`KubernetesPublication.target`, `teardownWait`). Core does the substitution generically, and the declaring side validates the type exactly as it does for a literal. Errors are `NYL-UNIT-INVALID`. The list is closed and grows only by decision.
- **Inside `fromUnit`.** `fromUnit.environment` is the one sanctioned nested case. Render-time sources resolve before execution-time ones.
- **One place, fail-closed.** Resolution happens in one place, environment unit rendering, so direct and environment paths agree. No clause may survive rendering: a surviving one is an error, never data.
- **Errors.** A missing pointer names the environment, the keys that exist, and the nearest match. `nyl validate` checks every pointer against every environment and every template sample instance that selects the unit.

### 3. `fromInput` on two HelmChart fields

`includeCrds` and `lookup` accept `{fromInput: {name, pointer?}}`. There is no
`default` on the clause: the Release input's declared default applies.

### 4. `fromParameter` in `EnvironmentTemplate.spec.values`

```yaml
kind: EnvironmentTemplate
spec:
  parameters:
    pr: {type: integer}
    crds: {type: boolean, default: false}
  values:
    networkFrom: dev
    target:
      inline:
        releaseInputs:
          rise/rise:
            clusterWide: {value: {fromParameter: {name: crds}}}
```

- **Where clauses are resolved.** A reference object `{fromParameter: {name, pointer?}}` is resolved at any depth of the template's `spec.values`, and nowhere else. This is the same reference-object rule as `fromUnit` in unit `values`: exactly one field, no siblings, anything else is data. That includes inline targets and their bindings, which is the case that needed `| tojson`.
- **What a clause resolves to.** The value is the instance's bound parameter, which is already type-checked against the declaration when `--param` is parsed, with the declared default applied. The clause has no `default` of its own.
- **When it is resolved.** Parameters are stored in the instance's `state.yaml`, so resolution happens whenever the instance's effective values are computed: at `state init`, at `reconcile --template`, and at every later command. That is before units render, so `fromValues` reads resolved values.
- **What is an error.** An unknown parameter name is an error, and so is a clause anywhere outside the template's `values`, for example in a declared Environment.
- **What `validate` checks.** `nyl validate` resolves the clauses for every sample instance, so a template whose values reference a missing parameter fails before any instance exists.

## Implementation shape

### Resolve on `Value`, then deserialize

Typed fields resolve on the `serde_json::Value` of a document before anything
deserializes it into its typed struct:

```text
authored YAML → Jinja (strings only) → Value
  → resolve the declared clauses (nyl-core, pure)
  → reject any clause that survived
  → validate and deserialize, exactly as today
```

- **Drivers never see a clause.** They already take the spec as `&Value` (`Driver::validate_spec`, `teardown_wait`, …), and their typed structs keep resolved types.
- **Execution keys are unchanged.** They hash the resolved spec, so a field filled by a clause hashes exactly like the same literal. Moving a document from Jinja to clauses re-runs nothing.
- **Rejected alternative: `Resolvable<T>` as a runtime type** (`pub target: Resolvable<TargetRef>`). Every reader would have to unwrap it, "already resolved" would be invisible in the type, and drivers would handle a concept that belongs to core.

### The schema is the declaration

`Resolvable<T, S>` is a schema-only type, applied with `#[schemars(with = …)]`:

```rust
/// nyl-core `bindings`: a clause kind and the clause object it takes.
pub trait SourceKind { const FIELD: &'static str; type Clause: JsonSchema; }
pub enum FromValues {}   // FIELD = "fromValues", Clause = ValuesSource
pub enum FromInput {}    // FIELD = "fromInput",  Clause = InputSource

/// `anyOf: [T, {<FIELD>: Clause}]` plus `"x-nyl-resolvable": <FIELD>`.
pub struct Resolvable<T, S: SourceKind>(PhantomData<(T, S)>);

// a driver's spec struct: runtime type unchanged
#[schemars(with = "Resolvable<TargetRef, FromValues>")]
pub target: TargetRef,

// HelmChartSpec
#[schemars(with = "Resolvable<Option<bool>, FromInput>")]
pub include_crds: Option<bool>,
```

- **The source is a type parameter.** The clause a field accepts depends on the kind of document it sits in, not on the field: unit specs take `fromValues`, and Nyl resources in a Release bundle take `fromInput`. Each struct lives in one kind of document, so its fields name their source, and the editor schema only offers a clause that can resolve there.
- **Core reads the marker.** For each unit kind, core walks `driver.spec_schema()` once and collects the JSON pointers marked `x-nyl-resolvable`. Core's own schema marks the common unit fields; HelmChart's schema marks its two fields. The resolver substitutes only at those pointers. That is how "each driver declares its own fields" keeps the driver boundary: the declaration is the driver's schema, and core never names a kind or a field.
- **The caller supplies one kind of clause.** Unit rendering supplies `FromValues` and Release rendering supplies `FromInput`. A marker that names another kind is a configuration error, so a struct reused somewhere else fails loudly instead of resolving from the wrong place.
- **Several sources on one field** (`Resolvable<T, (FromValues, FromInput)>`) is the natural extension. It is added only when a real case needs it.

### Clause types

These live in nyl-core `bindings`, next to `InputBinding`:

```rust
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ValuesSource {
    pub pointer: String,
    #[serde(default, deserialize_with = "present_non_null")]
    pub default: Option<Value>,   // rejects `default: null` instead of reading it as absent
    #[serde(default)]
    pub optional: bool,           // exclusive with `default`
}
pub struct InputSource     { pub name: String, pub pointer: Option<String> }
pub struct ParameterSource { pub name: String, pub pointer: Option<String> }
```

### Per mechanism

| Mechanism | Code change |
|---|---|
| `HelmChart.valuesFrom` | A runtime field, `values_from: Vec<HelmValuesSource>`. Each entry holds exactly one of `file` (`ValuesFromFile`'s path rules) or `input`, plus `pointer` and `key`. The renderer merges the entries before it calls Helm. |
| `fromValues` on unit fields | Only the schema attributes. Resolution happens once, in environment unit rendering (`render_environment_units`), which also serves the direct path. |
| `fromInput` on `includeCrds` and `lookup` | Only the schema attributes. Resolution runs against the Release's inputs before the HelmChart is deserialized in the render pipeline. |
| `fromParameter` | No struct change, because `values` is already a free-form `Value`. The any-depth walker (`resolve_document`) takes the set of clause kinds it accepts as a parameter instead of the fixed `REFERENCE_KINDS`: template values accept `fromParameter`, and unit `values` and `variables` keep `fromUnit` and `fromPromotion`. |

- **`fromUnit.environment` is the exception.** It sits inside a reference object within a free-form map, not at a fixed schema path. The any-depth walker resolves `fromValues` inside each `fromUnit` it finds. Its schema still says `Resolvable<Option<String>, FromValues>` for editors, but the marked pointers do not drive resolution there.
- **The surviving-clause check is explicit.** After substitution, an object whose only key is a known clause name is an error. A typed field with `deny_unknown_fields` would catch most survivors anyway, but a field typed as free-form `Value` would not.
- **Every value a clause reads enters the dependency recorder** at the layer that consumes it, as the cache invariants require. Environment values and Release inputs already do.
- **One resolver, one shell call.** The resolver is pure (`resolve_declared::<S>(field, spec, pointers, provider, issues)`), and each source kind is one provider method, `SourceProvider<S>::provide`, as in `bindings::Providers`. The clause type carries its own validation and missing-value rule (`SourceClause`: `default`, `optional`), so the resolver applies them without naming a kind; a provider answer of `null` counts as missing for every kind. Bindings, unit references and declared fields share one resolver, as the architecture rules require.

## Case study

**Subjects:**
- the Rise draft (a Release plus a publication unit);
- the reference scenarios `preview` and `promotion`;
- independently, the end-to-end example and `hotfix`.

**Method:** three reviewers.
- A spec-only author rewrote projects it had not seen rewritten.
- A task-based user performed five change tasks against both versions.
- An adversarial reviewer checked the spec against the code.

| Measure | Before | After |
|---|---|---|
| Structural-templating sites, own rewrite (4 projects) | 10 | 0 |
| Same, spec-only author (2 projects it had not seen rewritten) | 3 | 0, matching the own rewrite |
| Unit documents that are valid YAML before rendering | 0 of 4 | 4 of 4 |
| Edit size for the 5 change tasks | same | same |
| Typo in a required pointer is caught | late or silent | at render, before any effect |

**Findings that changed the proposal:**
- **Any-depth resolution in HelmChart `values` dropped.** Data from `valuesFrom` would become code, and charts that embed Nyl resources would be resolved eagerly. The tier-3 rewrite showed it is unnecessary.
- **`default: null` replaced by `optional: true`.** `null` satisfies no input type, serde cannot tell `default: null` apart from an absent `default`, and emitting `field: null` changes a unit's execution key.
- **Resolvable fields declared by their owner.** `teardownWait` and `target` are driver-owned (`COMMON_UNIT_FIELDS` lists only common ones), so a shared list would break the driver boundary.
- **Fail-closed invariant added.** Several readers parse references with `if let Ok` (`rules/multi_environment.rs:260`), so an unresolved clause inside `fromUnit` would silently drop a cross-environment edge.
- **A defaulted pointer still fails silently on a typo.** `validate` therefore checks pointers against all selecting environments, and error messages suggest the nearest key.
- **Template parameters had no clause.** A typed parameter reaching a binding still needed `| tojson`, which led to §4.

**Usefulness:**
- The gains are documents that are valid YAML before rendering, lintable, and written with flow-style labels; type errors before execution; and a defined merge instead of a loop that could emit duplicate keys.
- Editing is not cheaper. The benefit is correctness and readability.

## Not proposed

- **Resolving sources anywhere.** It collides with real data, lets chart output become input, has no type at the site, and leaves the scope rules implicit.
- **Type-preserving `'{{ x }}'` interpolation.** It is implicit and changes the output of existing documents.
- **A promise that every field becomes resolvable.** That would be a second templating language arriving one field at a time. New clause sites are added by decision, when a real project still needs structural Jinja.

## Compatibility

- The changes are additive: existing documents render byte-identically.
- There is no state or ownership-index change.
- The Jinja and declarative forms of each case-study project must render identical specs.
