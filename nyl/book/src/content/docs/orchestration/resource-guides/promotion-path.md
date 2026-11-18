---
title: 'Using PromotionPath'
---

See the [PromotionPath resource reference](/nyl/reference/resources/gitops.nyl/v1/promotion-path/) for the API, example, and field definitions.

:::note[Status]
M6 delivers promotion in steps. The resource, its validation, `nyl get promotion-paths`,
the record type, external attestations, and [`nyl promote`](/nyl/reference/cli/promote/)
exist today, and an Environment can take its source and its bindings from a promotion
(see the [promotion guide](/nyl/orchestration/promotion/)). The inspection commands
and `record: source` follow.
:::

## What a path says

A PromotionPath is the explicit link between a source Environment and a target
Environment. It says how strong the evidence must be and which fields of the
source become named values that `fromPromotion` bindings read. Source and target
may differ in unit names, input names, and document shape; the path states the
mapping.

```yaml
apiVersion: gitops.nyl/v1
kind: PromotionPath
metadata:
  name: dev-to-prod
spec:
  from: {environment: dev}
  to: {environment: prod}
  evidence: attested
  maxAttestationAge: 1h
  values:
    webImage:
      select: {unit: web-image, artifact: image, pointer: /reference}
    databaseSource:
      select: {unit: database, input: /source}
```

- `from` and `to` name declared Environments, never an instance of an
  EnvironmentTemplate. The path's name is a DNS label and names the record file.
- `evidence` is `published` (the source produced the value: a current receipt) or
  `attested` (published, and every attestation the covered units and the source
  environment declare passes). `attestations` replaces the required names per
  scope, or adds requirements at `published`; `maxAttestationAge` makes older
  attestations not count.
- `verifyArtifacts` (default `true`) checks that promoted artifacts still exist
  before a record is written. `coverage` is keyed by a unit that runs as
  several deployments, such as a KubernetesPublication: per unit, it narrows
  which deployments count toward its running-scoped attestations (`groups`,
  `exclude`) and adds deployments that must meet them without contributing
  values (`require`). `nyl validate` checks each name against what the unit
  runs; see [Coverage](/nyl/orchestration/promotion/#coverage).
- A path whose target does not take its source from it promotes values only:
  a value selected as a publication's Release input comes from the state its
  Release's deployment runs; see
  [Value-only paths](/nyl/orchestration/promotion/#value-only-paths).

## Selecting values

Each value reads one source field, with the JSON Pointer semantics of `fromUnit`
references:

| Selector | Reads |
| --- | --- |
| `{unit, input: <pointer>}` | The source unit's resolved spec: what it ran, such as `/source` |
| `{unit, output, pointer?}` | A declared, non-sensitive output of the unit's receipt |
| `{unit, artifact, kind?, pointer}` | A field of an artifact the unit's receipt lists, relative to the artifact's `spec` |

The target reads a value with a `fromPromotion` binding, naming the value and,
optionally, the path:

```yaml
releaseInputs:
  platform/web:
    image: {fromPromotion: {path: dev-to-prod, value: webImage}}
```

## Validation

`nyl validate` checks every path against the units the environments render, and
`nyl plan -e <environment>` reports the same findings for the paths touching that
environment. Every finding has a stable code and a fix:

| Code | Reported when |
| --- | --- |
| `NYL-PROMOTE-ENVIRONMENT-UNKNOWN` | `from` or `to` is not a declared Environment (an instance of a template is not one) |
| `NYL-PROMOTE-SELECTOR-UNIT` | A selector, or an `attestations` scope, names a unit the source environment does not select or the path does not cover |
| `NYL-PROMOTE-SELECTOR-INVALID` | A pointer resolves neither in the unit's rendered spec nor in its kind's schema, or an output is undeclared or `sensitive` |
| `NYL-PROMOTE-VALUE-TYPE` | A value's type does not fit a unit field or Release input that reads it |
| `NYL-PROMOTE-VALUE-TYPE-UNCHECKED` | The declared type of a field a binding fills cannot be read, such as a Release input declared in a remote ApplicationGroup source that is neither fetched nor cached; an error in `nyl validate`, a warning in `plan`, `reconcile`, `promote`, and `get promotion-candidates` |
| `NYL-PROMOTE-BINDING-UNKNOWN` | A `fromPromotion` binding names a path or value no path declares |
| `NYL-ATTESTATION-UNDECLARED` | The path requires an attestation its unit or environment scope does not declare |
| `NYL-PROMOTE-COVERAGE-UNKNOWN` | `coverage` names a unit the source does not declare, select, or enable, a unit whose kind runs no deployments, or a group or deployment the unit does not run; the message lists the names it has |
| `NYL-PROMOTE-COVERAGE-INEFFECTIVE` | A path that promotes only values sets a unit's `groups`, which only a source path uses, or the unit records nothing per deployment (a KubernetesPublication without `observe`) |
| `NYL-PROMOTE-COVERAGE-UNCHECKED` | A coverage unit's deployments cannot be listed, such as when its target does not render; an error in `nyl validate`, a warning in `plan`, `reconcile`, `promote`, and `get promotion-candidates` |
| `NYL-PROMOTE-UNSUPPORTED` | `changeGate: pullRequest`, or a DeploymentTarget in `from` or `to`: both arrive with M11 |

Types are compared when both sides are known: what a selector reads (a rendered
literal, a declared output's type, the kind's schema at the pointer, or the
schema the producing kind declares for an artifact) against the place a
`fromPromotion` binding fills: the field of a unit's spec as its kind's schema
types it, or, for a binding in a DeploymentTarget's `releaseInputs`, the type
the Release declares for that input (`spec.inputs.<name>.type`). An unknown
side fits anything.

The Release's declaration is read from the ApplicationGroups the target
selects, without rendering the target. A group with a remote source is
fetched at its pinned commit, or read from the Git cache when the fetch fails;
`nyl validate --offline` and `nyl reconcile --local` read the cache only and
never use the network. When the source is neither fetched nor cached, the
input stays unchecked (`NYL-PROMOTE-VALUE-TYPE-UNCHECKED`), and resolution
still checks the promoted value against the declaration when the publication
unit runs.

## Where records live

`nyl promote` will write one `PromotionRecord` per path into the target
environment's desired state, `desired/promotions/<path>.yaml`, or, with
`record: source`, into the Environment's `source.fromPromotion.promoted` block.
The record has one shape in both places. See
[promotion records](/nyl/orchestration/environments-and-state/#promotion-records)
for how the file behaves with older Nyl versions, `state move`, and
`state restore`.

## See also

- [Environment](/nyl/reference/resources/gitops.nyl/v1/environment/)
- [`nyl get`](/nyl/reference/cli/project-resources/)
- [`nyl validate`](/nyl/reference/cli/validate/)
