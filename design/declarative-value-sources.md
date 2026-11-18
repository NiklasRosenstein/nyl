# Declarative value sources

**Status:** proposed. No schema or state changes yet.

Three places force *structural* templating, where the document is invalid YAML
until it renders and the template decides its shape or types. Quoted string
interpolation (`'rise-{{ environment.name }}'`) is not the problem and stays.
This note replaces each of the three with a typed, declarative form.

The trigger is a Nyl setup for rise-deploy/rise. One Release renders the
repository's chart for `develop` and for every pull-request preview. Its
HelmChart looks like this today:

```yaml
spec:
  includeCrds: {{ inputs.clusterWide | tojson }}          # (2) typed scalar
  values:
    metacontroller:
      install: {{ inputs.clusterWide | tojson }}          # (2)
{%- for key, value in inputs.chartValues | items %}       # (1) object merge
    {{ key }}: {{ value | tojson }}
{%- endfor %}
```

The publication unit it belongs to does this:

```yaml
spec:
  target: {{ values.target | tojson }}                    # (3) target per environment
```

## 1. `HelmChart.spec.valuesFrom`

```yaml
apiVersion: k8s.nyl/v1
kind: HelmChart
spec:
  chart: {name: ../helm/rise}
  valuesFrom:
    - file: ../helm/rise/values-preview.yaml
    - input: chartValues
    - input: database
      pointer: /helm
      key: postgresql
  values:
    fullnameOverride: '{{ inputs.name }}'
```

- An entry sets exactly one of `file` or `input`.
  - `file` follows `ValuesFromFile` on Cluster and DeploymentTarget: the same path rule, Git visibility, and a single YAML mapping document.
  - `input` names a Release input whose effective value, after the optional `pointer`, must be an object.
- `key` mounts the mapping under one literal key, as in `ValuesFromFile`.
- **Merge:** entries apply in order, then inline `values` on top. Each step uses `deep_merge_value`: maps merge and everything else replaces. This is also Helm's rule for repeated `-f` files.
- **Where it applies:**
  - `input` entries are allowed only in a Release bundle that declares the input. Outside a Release, `input` is a configuration error naming the HelmChart.
  - `file` entries work everywhere.
- **Cache key:** every cache that covers a HelmChart's output must key on the merged values, file bytes included. The implementation verifies this rather than assuming it.

## 2. `fromInput` for typed fields

`includeCrds` and `lookup` accept `{fromInput: {name, pointer?}}` as well as a literal. The resolved value must be a boolean. An input whose declared type cannot yield one is rejected before rendering, and a value that is missing or null after the pointer is an error, never a silent default.

```yaml
spec:
  includeCrds: {fromInput: {name: clusterWide}}
```

This deliberately has no general form. A chart value such as
`metacontroller.install` belongs in a `valuesFrom` input, i.e. in the target's
binding, not in a reference node inside `values`.

## 3. `target: {fromValues: …}` on publication units

`KubernetesPublication.spec.target` accepts `{fromValues: {pointer}}` as well
as a name or `{inline: …}`.

- The pointer reads the environment's effective `values`, from either an Environment or a template instance.
- The value there must be a string (a target name) or an object `{inline: …}`.
- Inline targets keep their current rules: the name is `<environment>-<unit>`, templates inside the inline spec render with the instance, and `validate` checks every template through sample instances.

```yaml
kind: KubernetesPublication
metadata:
  name: rise
  labels: {stage: develop, preview: 'true'}   # flow style is fine again
spec:
  target: {fromValues: {pointer: /target}}
```

The unit document becomes valid YAML before rendering. The block-style-labels
workaround (`scan_unit_envelope`'s static scanner) then becomes unnecessary for
such units, but it stays for templated ones.

## Not proposed

- **Type-preserving interpolation** (a scalar that is exactly `'{{ inputs.x }}'` keeps the input's type). It works everywhere, but it is implicit, and it changes the output of existing documents that quote a numeric input and rely on getting a string.
- **Reference nodes inside arbitrary resources** (`{$input: …}` in any manifest). That would be a second templating language with the potential to collide with real data.

## Compatibility

- All three forms are additive schema changes, and every existing document renders unchanged.
- There is no state or ownership-index change.
- JSON schemas are regenerated.
- Errors reuse the existing configuration codes. No unit-specific codes are added; `target` resolution failures stay `NYL-UNIT-INVALID`.

## The Rise Release afterwards

```yaml
kind: HelmChart
spec:
  chart: {name: ../helm/rise}
  includeCrds: {fromInput: {name: clusterWide}}
  valuesFrom: [{input: chartValues}]          # per target, incl. metacontroller.install
  values:
    fullnameOverride: '{{ inputs.name }}'
    image: {repository: ghcr.io/rise-deploy/rise, digest: '{{ inputs.imageDigest }}'}
```

The `digest` value assumes a small chart change in Rise. Today the chart only
joins `repository:tag`.

## Order

1. `valuesFrom` (highest value)
2. `target: {fromValues}`
3. `fromInput` on `includeCrds` and `lookup`

## Open questions

- Should `valuesFrom.input` also accept `fromUnit`-style sources directly, bypassing Release inputs? The proposal is no: inputs stay the single typed boundary between a target and a Release.
