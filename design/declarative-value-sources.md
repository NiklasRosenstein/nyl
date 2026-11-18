# Declarative value sources

**Status:** proposed, revised after a case study (below). No schema or state
changes yet.

*Structural* templating is Jinja that makes a document invalid YAML before it
renders, or that decides a value's shape or type (`{{ x | tojson }}`,
`{% if %}`, `{% for %}`). Quoted string interpolation
(`'pr-{{ environment.name }}'`) is not the problem and stays. This note
replaces structural templating with a few typed source clauses. It deliberately
does **not** add a general mechanism that resolves sources anywhere.

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

**Usefulness:**
- The gains are documents that are valid YAML before rendering, lintable, and written with flow-style labels; type errors before execution; and a defined merge instead of a loop that could emit duplicate keys.
- Editing is not cheaper. The benefit is correctness and readability.

## Open decision

Typed **template parameters** still need Jinja. A boolean parameter that
reaches a binding (`clusterWide: {value: {{ params.crds | tojson }}}`) cannot
be written with these clauses. There are two options:
- add `{fromParameter: name}` inside `EnvironmentTemplate.spec.values`, resolved when an instance is created; parameters are declared and typed;
- accept the remaining `| tojson` in templates.

## Not proposed

- **Resolving sources anywhere.** It collides with real data, lets chart output become input, has no type at the site, and leaves the scope rules implicit.
- **Type-preserving `'{{ x }}'` interpolation.** It is implicit and changes the output of existing documents.
- **A promise that every field becomes resolvable.** That would be a second templating language arriving one field at a time. `| tojson` stays acceptable for rare fields.

## Compatibility

- The changes are additive: existing documents render byte-identically.
- There is no state or ownership-index change.
- The Jinja and declarative forms of each case-study project must render identical specs.
