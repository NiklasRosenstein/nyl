# Domain resource collections

**Status:** proposal for [#173](https://github.com/NiklasRosenstein/nyl/issues/173),
milestone M9. Nothing here exists yet. Progress is tracked on the
[Domain resources roadmap](https://niklasrosenstein.github.io/nyl/domain-resources/roadmap/)
in the documentation, generated from
[`nyl/book/src/lib/roadmaps/domain-resources.mjs`](../nyl/book/src/lib/roadmaps/domain-resources.mjs). It
extends the [orchestration core contract](orchestration-core.md) and the
[infrastructure units](infrastructure-units.md) and changes neither: a project
that declares no ResourceDefinition never sees any of it.

Platform teams define domain types, such as `AwsAccount`, `AwsAccessGrant`,
`GitHubRepository`, or `BaoPolicy`. Product teams declare instances of them in
paths they own. Units that the platform registers explicitly pick up those
declarations as typed input collections. Nyl collects, validates, admits, and
bundles the declarations. Terraform, OpenTofu, or a command implements them,
and the native tool keeps its own state.

```text
 definition ─► discovery ─► admission ─► collection ─► resolution ─► bundle ─► driver adaptation ─► execution
 (stage 0)     (stage 1)    (stage 2)    (stage 3)     (stage 4)     (stage 5)  (stage 6)            (core contract)
 ─────────── project-wide, environment-independent ───────────┘└──────────── per unit and environment ─────────────
```

Stages 0–2 depend only on the source commit S, so their results are the same
for every environment and every entry point: `nyl validate`, `nyl prepare`,
`plan`, `reconcile`, `exec --worktree`, and CI all call them. Stages 3–6 run
during unit resolution, in the layer the core contract already defines. No
stage has a command-specific variant.

## Principles

- **Data, not units.** A domain resource is never a unit. It has no lifecycle,
  receipt, or driver. A unit consumes it as an input. Changing a resource
  changes the input of the units that consume it. It never creates, deletes,
  or replaces a unit.
- **Literal declarations.** Domain resources are not templates. A reviewer, a
  CODEOWNERS rule, and a policy all see the same bytes that reach the bundle.
  Per-environment variation belongs in the consuming unit, whose spec is
  templated per environment.
- **Explicit membership.** A unit names the type, the labels, and the paths it
  collects. Nyl never links resources by matching names.
- **Trust comes from configuration.** `nyl.toml` decides where definitions and
  policies may come from. Source-path context comes from Git, never from a
  field the author can edit.
- **One bundle, many adapters.** The bundle is tool-neutral. Each driver adapts
  it, and the built-in kinds adapt it through the reference resolver they
  already use. A new abstraction needs a definition and an implementation, not
  a new driver.

## Stage 0: Definitions

```yaml
apiVersion: gitops.nyl/v1
kind: ResourceDefinition
metadata:
  name: awsaccounts.domain.acme.example
spec:
  group: domain.acme.example
  kind: AwsAccount
  version: v1
  ownership: project               # project | environment; see Ownership
  description: An AWS account in the organization, owned by one team.
  schema:                          # JSON Schema 2020-12 for `spec`, self-contained
    type: object
    required: [email, ou]
    additionalProperties: false
    properties:
      email: {type: string, format: email}
      ou: {type: string, enum: [workloads, sandbox, security]}
      budgetUsd: {type: integer, minimum: 0, default: 100}
      owner: {type: string, x-nyl-reference: {kind: GitHubTeam}}
```

| Field | Meaning |
| --- | --- |
| `group`, `kind`, `version` | Identify the type that domain resources use as `apiVersion: <group>/<version>` and `kind`. Groups ending in `nyl` are reserved |
| `ownership` | The scope in which at most one unit may own a resource (see [Ownership](#ownership)); default `project` |
| `schema` | The schema of the resource's `spec`, in JSON Schema 2020-12 without remote `$ref` (local `$defs` only) |
| `description` | Shown by `nyl get resources` and in generated editor schemas |

- **Trusted location.** Definitions are read only from the paths
  `nyl.toml` lists, so the platform's CODEOWNERS entry for those paths
  controls them:

  ```toml
  [resources]
  definitions = ["/platform/definitions/**"]
  policies = ["/platform/policies/**"]
  ```

  A ResourceDefinition outside those paths is an error
  (`NYL-RESOURCE-DEFINITION-UNTRUSTED`). Without a `[resources]` table, nothing
  in this document applies.
- **One version at a time.** A definition serves one `version`. Moving a type to
  `v2` is one change that updates the definition, every resource, and the
  consuming units together. Conversion between versions is an
  [open decision](#open-decisions).
- **Defaults are applied.** Schema `default` values are written into the
  resource, as Kubernetes does for structural schemas. Implementations then
  read effective values and do not repeat the defaults. Other annotations,
  such as `description` and `format`, do not change the bundle.
- **Schema extensions.** Two keywords carry Nyl-specific meaning:
  - `x-nyl-reference: {kind}`. The string must name an existing resource of
    that kind in the project catalog. This is referential integrity checked
    in stage 2, not a dependency edge.
  - `x-nyl-accepts-references: true`. The field may hold a `fromUnit` or
    `fromPromotion` reference object (see [stage 4](#stage-4-resolution)).
    Any other field that holds a reference object is a schema error.
- Definitions are pinned by S like every other file in the project. Sharing
  definitions across repositories through a locked `GitSource` is an
  [open decision](#open-decisions).

## Stage 1: Discovery

```yaml
# teams/payments/aws.yaml
apiVersion: domain.acme.example/v1
kind: AwsAccount
metadata:
  name: payments-prod
  labels: {team: payments, stage: production}
  annotations: {note: "PCI scope"}
spec:
  email: aws+payments-prod@acme.example
  ou: workloads
  owner: payments
```

- Discovery uses the project-wide Git visibility rule that rendered GitOps
  resources follow. A document is a domain resource when its `apiVersion` and
  `kind` match a definition. Files that discovery already treats as Release
  files or ApplicationGroup sources are skipped, so a Kubernetes manifest is
  never picked up as a domain resource. A document that names a defined group
  with an unknown kind or version is an error, which catches typos.
- **Envelope.** `apiVersion`, `kind`, `metadata.name`, `metadata.labels`, and
  `metadata.annotations` are the only metadata fields. Template syntax
  anywhere in the document is an error (`NYL-RESOURCE-TEMPLATED`).
- **Identity** is `<group>/<kind>/<name>` and is unique across the project.
  Directories are not namespaces. A duplicate is an error that names both
  locations (`NYL-RESOURCE-DUPLICATE`). Moving a file between directories
  keeps the identity and changes only provenance.
- **Provenance** is recorded per resource: the worktree-rooted path, the
  document index, and the digest of the document's bytes. It comes from the
  Git checkout, never from the document.
- **Reserved annotations.** Annotations under `*.gitops.nyl/` are rejected
  (`NYL-RESOURCE-RESERVED-ANNOTATION`), because stage 2 uses them to give
  policies trusted context.

The result is the project's **catalog**: every domain resource with its
identity, provenance, and parsed document, sorted by identity.

## Stage 2: Admission

Every catalog resource goes through three checks. Each check reads only S,
so its result applies to every environment and can be cached under the
[cache invariants](../AGENTS.md#rendered-gitops-cache-invariants).

1. **Schema.** The definition validates `spec`, then applies its defaults.
   Reference objects in `x-nyl-accepts-references` fields are skipped here.
   Stage 4 checks their resolved values against the field's subschema.
2. **Referential integrity.** Every `x-nyl-reference` field must name a
   resource in the catalog.
3. **Policy.** Kyverno validating policies run against the resource. They are
   loaded only from `[resources] policies`.
   - Nyl evaluates a copy of the resource with trusted context added as
     annotations: `source.gitops.nyl/path` (`/teams/payments/aws.yaml`),
     `source.gitops.nyl/directory`, and `source.gitops.nyl/document`. Authors
     cannot set these annotations (stage 1), so a policy that restricts a
     folder can rely on them:

     ```yaml
     apiVersion: policies.kyverno.io/v1
     kind: ValidatingPolicy
     metadata: {name: sandbox-only-outside-platform}
     spec:
       matchConstraints:
         resourceRules:
           - {apiGroups: [domain.acme.example], apiVersions: [v1], resources: [awsaccounts], operations: [CREATE]}
       validations:
         - expression: >-
             object.metadata.annotations['source.gitops.nyl/path'].startsWith('/platform/')
             || object.spec.ou == 'sandbox'
           message: Teams may only declare sandbox accounts.
     ```

   - Only validating policies are accepted. A mutating policy in the policy
     paths is an error (`NYL-RESOURCE-POLICY-MUTATES`), because mutation would
     make the bundle differ from what was reviewed.
   - Nyl derives a CRD from each definition, so `kyverno apply` can resolve
     kinds, plural resource names, and schemas offline. Which policy types
     work offline needs to be checked against the Kyverno CLI version that
     the render path already uses.
   - The Kyverno CLI is an external tool. Its version is part of the
     admission cache key, as for rendering.

**Failure scope.** Admission results are per resource. `nyl validate` and
`plan` report every failure. A unit whose collection includes a failing
resource fails resolution and has no effects. Units that do not consume it
still run, so one team's broken declaration only blocks the units that
consume it. Membership never depends on the content that failed, because
selection reads only the envelope and the path (stage 3). A pull request
pipeline that runs `nyl validate` catches failures before merge.

CODEOWNERS is review, and admission is enforcement. Neither replaces the
other: CODEOWNERS decides who must approve a change to `teams/payments/`,
and a policy decides what that folder can declare at all, whoever approved
it.

## Stage 3: Collection

A unit declares its collections with the new common field `collections`, which
is flattened into every kind like `env` and `dependsOn`:

```yaml
apiVersion: units.gitops.nyl/v1
kind: OpenTofu
metadata: {name: aws-accounts, labels: {tier: org}}
spec:
  source: {path: /platform/aws-accounts}
  backend: {bucket: acme-tofu-state, key: org/aws-accounts}
  collections:
    accounts:
      type: {apiVersion: domain.acme.example/v1, kind: AwsAccount}
      selector: {matchLabels: {stage: '{{ values.stage }}'}}   # optional
      paths: ['/teams/**', '/platform/accounts/**']           # optional
      access: own                                             # own | read
      allowEmpty: false
      removal: approve                                        # approve | auto
    teams:
      type: {apiVersion: domain.acme.example/v1, kind: GitHubTeam}
      access: read
  variables:
    accounts: {fromCollection: {collection: accounts, each: /spec}}
```

| Field | Default | Meaning |
| --- | --- | --- |
| `type` | required | One definition's `apiVersion` and `kind` |
| `selector` | `{}`: all | `matchLabels` over literal `metadata.labels`, as `unitSelector` |
| `paths` | all | Globs over the resource's trusted, worktree-rooted path, under the local path rule |
| `access` | `own` | `own` claims the members (see [Ownership](#ownership)); `read` only reads them |
| `allowEmpty` | `false` | An empty collection fails resolution (`NYL-RESOURCE-COLLECTION-EMPTY`) unless `true` |
| `removal` | `approve` | Whether dropping members needs `--approve` (see [Removal](#removal-and-retention)) |

- The unit's spec, `collections` included, is templated per environment as
  today, so a selector can follow `values` or `environment`. The domain
  resources themselves never change per environment.
- Membership is the set of catalog resources of `type` that match both
  `selector` and `paths`. It is computed only from the envelope and the
  trusted path, so it is known even when a member fails admission.
- **Ordering.** Members are keyed by `metadata.name` and sorted bytewise.
  File order, discovery order, and directory layout never affect the bundle.
- A resource may be a member of several collections and of several units'
  collections.
- `plan` and `nyl get unit <u> --collections` list the members and say for
  each one why it matched: its labels, and which of the `paths` globs matched
  its path.

### Ownership

Sharing a declaration is not sharing native ownership. `access: own` is a
unit's claim that its implementation creates and manages what the members
describe:

- With the definition's `ownership: project`, at most one unit across all
  declared environments and template instances may own a resource. With
  `ownership: environment`, at most one unit per environment may own it.
  Resolution rejects a second owner and names both units
  (`NYL-RESOURCE-OWNER-OVERLAP`), the same way it rejects overlapping
  [ownership scopes](orchestration-core.md#identity).
- `access: read` collections, such as a GitHub team list that an access-grant
  unit reads to map names to IDs, claim nothing.
- Like a Command's effects, a claim is a declaration Nyl cannot verify.
  Preventing two configurations from fighting over one account is the point
  of making it explicit.

## Stage 4: Resolution

Each member's `spec` may hold reference objects in fields its definition marks
`x-nyl-accepts-references`:

```yaml
apiVersion: domain.acme.example/v1
kind: AwsAccessGrant
metadata: {name: payments-admins, labels: {team: payments}}
spec:
  account: payments-prod                         # x-nyl-reference: {kind: AwsAccount}
  permissionSet: AdministratorAccess
  principalGroupId:
    fromUnit: {unit: identity, output: groupIds, pointer: /payments-admins}
```

- References in members resolve in the consuming unit's environment, through
  the one resolver in `nyl_core::bindings`
  ([`resolve_document`](../crates/nyl-core/src/bindings/mod.rs)), with the
  freshness, evidence, and blocking rules of
  [References](orchestration-core.md#references). Each reference adds a
  dependency edge from the consuming unit to the producer.
- These edges exist at S before any value does, so the pure dependency graph,
  the CI export, and `plan` see them and report `blockedBy` as for any other
  reference. A blocked pointer is located inside the bundle, such as
  `/collections/grants/items/payments-admins/spec/principalGroupId`, and the
  report cites the member's source path.
- A resolved value is checked against the field's subschema. A mismatch is a
  resolution error for the consuming unit.
- Template lookups cannot read runtime state, because members are never
  templates.

## Stage 5: Bundle

Resolution produces one **resource bundle** per unit with collections:

```yaml
apiVersion: gitops.nyl/v1
kind: ResourceBundle
collections:
  accounts:
    apiVersion: domain.acme.example/v1
    kind: AwsAccount
    items:
      payments-prod:
        labels: {team: payments, stage: production}
        spec: {email: aws+payments-prod@acme.example, ou: workloads, owner: payments, budgetUsd: 100}
      search-prod:
        labels: {team: search, stage: production}
        spec: {email: aws+search-prod@acme.example, ou: workloads, owner: search, budgetUsd: 400}
```

- Items hold `labels` and the admitted, defaulted, resolved `spec`.
  Annotations stay out, because they are notes for people and for policies.
- The bundle is versioned like every state file, with golden files and a
  migration for each format change.
- **Provenance** is a separate `ResourceBundleProvenance` document. It holds
  each item's source path, document index, and document digest, the
  definition's identity and digest, and, per reference pointer, the receipt
  citation that `DesiredUnit.provenance` uses today. Implementations never
  receive it (see the key rule below).
- **State.** The desired document's `resolvedSpec.collections` records each
  collection's digest and member names. The full bundle and its provenance
  are written beside it as `desired/units/<unit>/bundle.yaml` and
  `desired/units/<unit>/bundle-provenance.yaml`. Teardown, `exec` with the
  deployed spec, `prepare --deployed`, and audit can then read exactly what
  ran without rendering S again. The receipt records the member names of each
  collection, which [removal](#removal-and-retention) compares against.
- **Execution key.** The canonical digest of the bundle enters the key through
  `resolvedSpec`. Provenance does not. So:
  - editing a member, adding or removing one, or a schema default changing an
    effective value re-runs the unit;
  - moving a file to a place that the collection's `paths` and the admission
    policies treat the same, editing an annotation, rewording a definition's
    `description`, or changing an unrelated team's declarations does not;
  - a move into or out of a collection's `paths`, or across a folder that a
    policy treats differently, changes membership or admission, and with them
    the bundle or whether the unit may run;
  - an implementation reads only inputs the key covers. If the implementation
    received provenance, a file move could change its result without changing
    the key.
- Domain resources live in Git, so they never contain secrets. Credentials
  reach the implementation only through the unit's `env`.

## Stage 6: Driver adaptation

The bundle reaches an implementation in one of two ways. Neither needs a
kind-specific option.

**Through references.** `fromCollection` is a new reference form for the
reference resolver, accepted wherever a kind accepts references:

| Reference | Resolves to |
| --- | --- |
| `fromCollection: {collection}` | The collection's `items` map: name to `{labels, spec}` |
| `fromCollection: {collection, each: <pointer>}` | Name to the value at `pointer` inside each item, such as `/spec` or `/spec/email`; a member without it is a resolution error |
| `fromCollection: {collection, pointer: <pointer>}` | A value inside the collection document, such as `/items/payments-prod/spec` |

It is the only projection, and it is decidable. A map keyed by name is what
Terraform's `for_each` expects:

```hcl
variable "accounts" {
  type = map(object({ email = string, ou = string, owner = string, budgetUsd = number }))
}

resource "aws_organizations_account" "this" {
  for_each  = var.accounts
  name      = each.key
  email     = each.value.email
  parent_id = local.ou_ids[each.value.ou]
  lifecycle { prevent_destroy = true }
}
```

The OpenTofu and Terraform drivers write `variables` into their temporary
variables file as they already do. Nyl's generic preparation writes no
`terraform.tfvars.json`. The variables file is a driver detail.

**Through the whole bundle.** The `Command` kind adds `collections` to the
`NYL_INPUTS` file, next to `values`. A plugin driver would receive the bundle
in its serialized `DesiredUnit`.

### Driver contract changes

- `DesiredUnit` gains `bundle: Option<ResourceBundle>`, loaded from the sidecar
  file. Drivers read it but never write it.
- A new optional capability `prepare` returns the native inputs that the
  driver's `plan`, `reconcile`, `verify`, and `exec` pass to the tool:

  ```rust
  fn prepare(&self, ctx: &ExecutionContext, unit: &DesiredUnit) -> Result<Supported<NativeInputs>>;

  struct NativeInputs {
      files: BTreeMap<RelativePath, Vec<u8>>, // e.g. variables.tfvars.json, backend.tfbackend.json
      env: BTreeMap<String, EnvSource>,       // names only: passthrough, secret key, or fixed value
      steps: Vec<Vec<String>>,                // the argument vectors a native run would use
  }
  ```

  Drivers build every operation from `prepare`, so what `nyl prepare --native`
  shows is what runs. The driver conformance suite checks this with a
  recording `ToolRunner`: the files and arguments `reconcile` passes must
  equal `prepare`'s. `NativeInputs` never contains secret values.
- Ownership claims are checked by Nyl from `collections`, not by drivers.

## Preparation: `nyl prepare`

`prepare` is the inspection and development entry point. It runs stages 0–6
through the same code as `reconcile` and writes the results to a directory.
It needs no lease and writes no state:

```text
nyl prepare -e <env> --unit <u> [--deployed | --source <rev>] [--native]
            [--fixture-outputs <file>] [--output-dir <dir>] [-o yaml|json]
```

| Output | Content |
| --- | --- |
| `bundle.yaml` | The resource bundle |
| `bundle-provenance.yaml` | Where each item and resolved value came from |
| `resolved-spec.yaml` | The unit's resolved spec and execution key |
| `native/` | With `--native`: the driver's `NativeInputs` files and a `steps.txt` with the native commands, secrets shown by name |

- **Source.** By default, files come from the current worktree, uncommitted
  changes included, as for `nyl build`. `--source <rev>` reads a commit.
  `--deployed` reads the unit's desired document and bundle from state, which
  is what is deployed, for inspection and breakglass.
- **References** resolve read-only against current receipts, as `nyl get
  output` does. A blocked reference exits 2, lists the blocked pointers with
  their source paths, and writes no `native/` directory.
- **Fixtures.** `--fixture-outputs` supplies producer outputs for tests
  without credentials:

  ```yaml
  identity: {groupIds: {payments-admins: 0f1e-…}}
  ```

  Fixture values are marked `fixture` in provenance. Only `prepare` accepts
  them, and `prepare` writes no state, so they cannot reach reconciliation.
- Exit categories: 0 complete, 1 configuration, admission, or resolution
  error, 2 blocked.
- Once inputs are materialized, the implementation works with its own tool,
  for example `tofu plan -var-file=out/native/variables.tfvars.json` against
  an isolated backend. Resolving dependencies still requires Nyl.

## Planning and inspection

`plan` gains a collections section per unit, from the same typed report:

```text
aws-accounts (OpenTofu)  update  awaiting-approval: members removed
  collections:
    accounts  12 members  +1 search-staging  -1 legacy-sandbox  ~1 payments-prod (/spec/budgetUsd)
    teams      8 members  (read)
  dependencies:
    identity  via grants/payments-admins /spec/principalGroupId  (/teams/payments/aws.yaml#2)
```

- Removed members carry the warning `NYL-RESOURCE-MEMBERS-REMOVED`. It is
  separate from `NYL-PLAN-UNITS-LEAVING`: the unit stays, and its
  implementation will likely destroy something.
- `nyl get resources [--kind <k>] [-l <selector>]` lists the catalog. With `-e`,
  it adds the consuming units and the access of each.
  `nyl get resource AwsAccount/payments-prod` shows the source location, the
  admission results, and every collection that includes it.
- `nyl schema resources --output-dir <dir>` writes an editor JSON Schema per
  definition, envelope included, for the YAML language server.

## Removal and retention

Removing a member is an input change of the consuming unit, which plans and
applies it like any other change. A Nyl-level hold or `Retain` on the unit
protects the unit, not the native resources its next apply removes. The guide
and examples must say this plainly.

- With `removal: approve`, the default, a resolution that drops members the
  current receipt's collection had makes the unit `awaiting-approval` with
  reason `members-removed`, even under `approval: auto`. `--approve <unit>`
  releases it. A `bind: plan` approval already covers removals through its
  change digest.
- Nyl keeps no state about native resources. Retaining a native resource is
  the implementation's job, for example `prevent_destroy` on accounts. The
  supported way to stop managing an account without closing it is
  breakglass: hold, `nyl exec … -- state rm`, remove the declaration, resume.
- Revoking a grant is the normal case: delete the declaration, review the
  plan's destroy of that grant, and approve.

## Breakglass

The [operator exec](orchestration-core.md#operator-exec) flow applies
unchanged. `nyl exec` without `--worktree` uses the deployed desired document
and its stored bundle. With `--worktree`, stages 0–6 run against the worktree
snapshot, so a temporary edit to a declaration reaches the tool, and the
record lists the bundle digest and the members that differ from the deployed
bundle. Changes made with the native tool outside `nyl exec` are invisible to
Nyl, and its records say nothing about them.

## Unit instances per resource: `forEach`

A unit can be instantiated once per domain resource, for example one OpenTofu
configuration per team that declares a `TeamWorkspace`. Each team then has
its own state, lock, and blast radius, and the platform writes the template
only once:

```yaml
apiVersion: units.gitops.nyl/v1
kind: OpenTofu
metadata:
  name: team-aws                      # literal base name
  labels: {tier: org}
forEach:                              # literal, read at discovery like the envelope
  type: {apiVersion: domain.acme.example/v1, kind: TeamWorkspace}
  selector: {matchLabels: {aws: enabled}}   # optional
  paths: ['/teams/**']                      # optional
spec:                                 # rendered once per (environment, resource)
  source: {path: /platform/team-aws}
  backend: {key: 'teams/{{ resource.metadata.name }}'}
  collections:
    accounts:
      type: {apiVersion: domain.acme.example/v1, kind: AwsAccount}
      selector: {matchLabels: {team: '{{ resource.metadata.name }}'}}
  {% if resource.spec.sandbox %}
  approval: auto
  {% endif %}
```

- **One structural pass.** `resource` joins `values`, `environment`, and, in
  template instances, `params` in the context of the unit's ordinary
  structural pass. Full Jinja works, blocks included, and nothing is nested
  or escaped. `${ … }` [template values](release-inputs.md#template-values)
  keep their role for fields expanded later in a narrower context.
- **`resource`** holds the instance's domain resource as admitted: its
  `apiVersion`, `kind`, `metadata.name`, `metadata.labels`, and defaulted
  `spec`. Fields that hold references stay reference objects, because the
  structural pass runs before resolution. To use a resolved value, select
  the resource in a collection instead.
- **Static instances.** `forEach` is literal, like `apiVersion`, `kind`,
  `metadata.name`, and `labels`, and is never templated. Its `type`,
  `selector`, and `paths` follow the rules of
  [collection](#stage-3-collection), so the instance set is computed from
  the catalog of stages 0–2. That set depends only on S and is the same in
  every environment and from every entry point.
- **Names.** An instance is named `<unit>-<resource name>`, such as
  `team-aws-payments`. The name is not templated. A clash with any other
  unit or instance name, such as `team` + `aws-payments` against
  `team-aws` + `payments`, is a validation error naming both
  (`NYL-UNIT-INSTANCE-NAME`).
  References, `--unit`, approvals, and state address the instance by its
  full name.
- **Selection.** Environments select the base unit by its literal labels, and
  every instance comes with it. An instance follows the existing unit rules:
  identity, ownership scopes, deletion policy, and kind changes. A template
  whose backend key forgets `resource.metadata.name` is rejected because every
  instance claims the same scope.
- **Lifecycle.** Removing or renaming the resource, or deselecting it from
  `forEach`, is the instance leaving the ownership set. `plan` lists it under
  units leaving, with the resource as the reason, and its teardown waits for
  `--allow-teardown`, so a team that disappears cannot destroy anything
  without an explicit approval. Editing the resource re-renders only its
  own instance.
- **Authority stays with the platform.** The team authors only the resource.
  The template, its credentials, and its backend belong to the unit's
  author. The definition's admission policy decides which folders may declare
  a resource that instantiates units.
- **Escaping across the trust boundary.** Structural templating renders text
  and then parses YAML. Until now that text was platform-authored; `resource`
  is not. A string from `resource` printed with `{{ … }}` must therefore
  consist only of letters, digits, spaces, and `. _ - / @ + =`, which cannot
  change YAML structure whether the template places it in a plain or a quoted
  scalar, so `'teams/{{ resource.metadata.name }}'` renders as
  `'teams/payments'`. Any other string fails rendering with
  `NYL-UNIT-RESOURCE-UNSAFE-VALUE`, naming the field, so `"x\napproval: auto"`
  never becomes structure. A template inserts such a value as a whole YAML
  value with `{{ resource.spec.description | tojson }}`, which renders a
  quoted JSON string, or a list or object. Names already follow the DNS label
  rule, and schemas should constrain the other fields that templates use.
- `nyl get units` lists the instances under their base unit. `nyl get unit
  team-aws` shows the `forEach` match for each instance.

### Static discovery

`forEach` follows the scan, render, and verify pattern that
[discovery](../crates/nyl-render/src/gitops/discovery.rs) already uses for
templatable control resources, and that units are specified to reuse:

1. **Split.** The file is cut into YAML documents as text, at `---` and `...`
   lines in column 0, before anything renders.
2. **Scan.** The static envelope scanner reads `apiVersion`, `kind`,
   `metadata.name`, and `metadata.labels`. It first parses the raw document
   as YAML. When block-level template syntax prevents that, it falls back to
   column-0 keys and the first indent under `metadata:`, skipping `{%` and
   `{#` lines. It also lifts out the top-level `forEach:` key, either with its
   indented block or as a one-line flow mapping. The block must contain no
   template syntax and must parse as YAML on its own; otherwise discovery fails
   and names the document, as it does for a templated name.
3. **Expand.** The scanned entry (kind, base name, labels, `forEach`) is
   matched against the catalog of stages 0–2, which is literal too. The
   instance set is therefore known before any unit renders. Selection, the
   instance-name clash check, ownership claims, and the dependency graph all
   work on it.
4. **Render.** The raw document renders once per (environment, instance),
   with `resource` in the context of the structural pass.
5. **Verify.** As `render_effective_control` does today, the result must be
   exactly one document of the scanned kind with the scanned base name. It must
   also carry the same `forEach` as the scan. A document that renders to
   nothing is disabled for that instance, as for any unit.

The verification catches the case the scanner cannot see. A `forEach:` wrapped
in `{% if %}` is scanned as present, because the scanner skips the `{%` lines,
but could disappear when the document renders. The rendered document then no
longer matches its scan, and discovery reports the document instead of
instantiating it differently in some environments.

## Code homes

| Rule or type | Home |
| --- | --- |
| `ResourceDefinition`, the domain envelope, `collections`, `fromCollection`, `ResourceBundle`, its provenance, golden files | `nyl-core` (`resources::domain`, `bindings`) |
| Identity, duplicates, ordering, membership, ownership claims, removal detection | `nyl-core` pure rules, one module each, used by orchestration as `rules::collection` and `rules::resource_ownership` |
| Schema validation and defaulting (JSON Schema 2020-12, a new dependency to evaluate) | `nyl-core`, pure |
| Discovery of definitions and resources, the catalog, Kyverno admission | `nyl-render` (`gitops::discovery`, `validation`), returned through `SourceRenderer` as part of `RenderedSource` |
| Bundle assembly and the bundle digest in the execution key | `nyl-orchestration` resolution, `rules::execution_key` |
| `prepare` and `NativeInputs` per kind | `nyl-drivers` |
| `nyl prepare`, `get resources`, `schema resources` | `nyl-cli`, wiring only |

Contract walkthroughs become scenarios under
`nyl/tests/scenarios/walkthroughs/`. These include a member added, a member
removed and approved, a file moved without re-execution, a blocked
`fromUnit` inside a member, an admission failure that blocks only its
consumer, and two owners rejected.

## Rollout

| Slice | Delivers | Needs |
| --- | --- | --- |
| R1 | Definitions, discovery, schema and referential admission, `nyl validate`, `get resources`, `schema resources` | M2 (useful without orchestration) |
| R2 | `collections`, ownership, the bundle and its state files, `fromCollection`, Command adaptation, `nyl prepare` without `--native` | M3 |
| R3 | Kyverno admission with trusted context | R1 |
| R4 | `prepare` capability and `--native`, OpenTofu/Terraform adaptation, removal approval, exec with bundles | M4, M8's `exec` |
| R5 | `forEach` unit instances: the `resource` context, YAML-safe values, instance names, leaving and teardown | R2 |
| R6 | The worked guide, validated as a tier-2 reference scenario | R2–R5 |

## Guide sketch: implementing a domain resource

This is the outline of the docs guide the issue asks for. It would go under
`orchestration/resource-guides/` once the feature exists. The example is
AWS accounts and access grants, with GitHub and Bao declarations next to them.

```text
platform/
  definitions/aws.yaml github.yaml bao.yaml     # ResourceDefinitions (CODEOWNERS: @platform)
  policies/teams.yaml                           # Kyverno ValidatingPolicies (CODEOWNERS: @platform)
  aws-accounts/  aws-access/  github/  bao/     # implementations and their units
teams/
  payments/aws.yaml github.yaml bao.yaml        # declarations (CODEOWNERS: @payments)
  search/…
```

1. **Define the type.** Write a ResourceDefinition with a strict schema
   (`additionalProperties: false`, enums for anything an implementation maps,
   defaults for optional fields). Register its path in `[resources]
   definitions`, run `nyl schema resources`, and add a team's first
   `AwsAccount`. `nyl validate` shows schema errors with file and pointer.
2. **Register the implementation.** Add an OpenTofu unit with a
   `collections.accounts` entry, `access: own`, and the `paths` it accepts.
   `nyl get unit aws-accounts --collections` shows each member and why it
   matched.
3. **Consume the bundle.** Map it with
   `variables.accounts: {fromCollection: {collection: accounts, each: /spec}}`,
   declare the matching `map(object(…))` variable, and `for_each` over it.
   Add `prevent_destroy` to resources that must survive removal.
4. **Inspect.** `nyl prepare -e org --unit aws-accounts --native` writes the
   bundle, its provenance, and the variables file. Run
   `tofu plan -var-file=…` against a scratch backend. Move a declaration to
   another folder that the collection's `paths` still match and re-run: same
   bundle digest, same execution key. Change a budget: new key.
5. **Reference another unit.** Add `AwsAccessGrant` with a
   `principalGroupId` that accepts references, set it with `fromUnit` to the
   identity unit's output, and watch `prepare` exit 2 with the blocked
   pointer until `identity` has a current receipt.
6. **Test.** Fixture tests run `nyl prepare --fixture-outputs` and assert on
   the bundle, along with `nyl validate` cases for good and bad declarations.
   Integration tests run `tofu test` with the prepared variables against an
   isolated backend or account.
7. **Bound authority.** Add CODEOWNERS entries per team folder, then a
   ValidatingPolicy on `source.gitops.nyl/path` that allows teams only
   `ou: sandbox`. Show the same declaration rejected under `teams/` and
   admitted under `platform/`.
8. **Breakglass.** Hold the unit, `nyl exec -e org --unit aws-accounts -- state
   …` to repair, resume, and approve the next plan.
9. **Remove.** Delete a grant's declaration: the plan shows
   `-1 payments-admins` and the grant's destroy, and approval revokes it.
   Delete an account's declaration: `prevent_destroy` fails the plan, and the
   retirement flow from step 8 detaches the account instead.

## Open decisions

| Decision | Needed by |
| --- | --- |
| JSON Schema crate and the exact subset (formats asserted or annotation-only) | R1 |
| Definition sharing across repositories through a locked `GitSource`, refreshed by `nyl update source-locks` | After R1 |
| Version conversion between definition versions, or coordinated bumps only | After R2 |
| Bundle size limits for desired state, and whether large bundles stay in state or are referenced by digest | R2 |
| Offline Kyverno coverage of `ValidatingPolicy` for derived CRDs, and whether CODEOWNERS owners are offered as trusted policy context | R3 |
| Generating HCL variable types from definitions (`nyl schema hcl`) | After R4 |
| Whether per-member removal approval should be per member (`--approve aws-accounts:legacy-sandbox`) | R4 |
| Whether the character set for `resource` strings printed into templates should be wider, for example to allow `:` inside quoted scalars | R5 |
| Whether instance names use a separator that cannot occur in unit names, which would make the name clash check unnecessary | R5 |
