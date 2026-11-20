# Domain resource collections

**Status:** proposal for [#173](https://github.com/NiklasRosenstein/nyl/issues/173),
milestone M9. Nothing here exists yet. Progress is tracked on the
[Domain modeling roadmap](https://niklasrosenstein.github.io/nyl/domain-modeling/roadmap/)
in the documentation, generated from
[`nyl/book/src/lib/roadmaps/domain-modeling.mjs`](../nyl/book/src/lib/roadmaps/domain-modeling.mjs). It
extends the [orchestration core contract](orchestration-core.md) and the
[infrastructure units](infrastructure-units.md) and changes neither: a project
that declares no ResourceDefinition never sees any of it.

Platform teams define domain types, such as `AwsAccount`, `AwsAccessGrant`,
`GitHubRepository`, or `BaoPolicy`. Product teams declare instances of them in
paths they own. Units that the platform registers explicitly pick up those
declarations as typed input collections. Nyl collects, validates, admits, and
bundles the declarations. Terraform, OpenTofu, or a command implements them,
and the native tool keeps its own state. Kubernetes Releases can read
collections too, as template data; the
[Release catalog](release-catalog.md) describes that consumer, and this
document the rules both share. Tools outside Nyl read the same bundles through
the [standalone export](#standalone-export-nyl-bundle), which needs neither
units nor Releases.

```text
 definition ─► discovery ─► admission ─► collection ─► resolution ─► bundle ─► driver adaptation ─► execution
 (stage 0)     (stage 1)    (stage 2)    (stage 3)     (stage 4)     (stage 5)  (stage 6)            (core contract)
 ───── project-wide, environment-independent ─────┘└─── per consumer and environment ───┘└──── per unit ────────
 └───────────────────────────── standalone: no unit, no driver ─────────────────────────┘
```

Stages 0–2 depend only on the source commit S, so their results are the same
for every environment and every entry point: `nyl validate`, `nyl get
resources`, `nyl bundle`, `nyl prepare`, `plan`, `reconcile`, `exec
--worktree`, and CI all call them. Stages 3–5 run for a consumer: a unit
during unit resolution, in the layer the core contract already defines, a
Release when it renders, or a selection given to the
[standalone export](#standalone-export-nyl-bundle). Only stage 6 needs a unit
and its driver. No stage has a command-specific variant.

## Principles

- **Data, not units.** A domain resource is never a unit. It has no lifecycle,
  receipt, or driver. A unit consumes it as an input. Changing a resource
  changes the input of the units that consume it. It never creates, deletes,
  or replaces a unit.
- **Literal declarations.** Domain resources are not templates. A reviewer, a
  CODEOWNERS rule, and a policy all see the same bytes that reach the bundle,
  apart from the `$fromUnit` and `$fromPromotion`
  [value sources](#value-sources-in-declarations) they may hold, which name
  exactly where a value comes from. Per-environment variation belongs in the
  consuming unit, whose spec is templated per environment.
- **One source model.** A domain resource is a context in the
  [value-source context table](declarative-value-sources.md#contexts), read by
  the same walker as every other document. No schema keyword decides where a
  source may stand.
- **A standalone seam.** Stages 0–5 need no unit, driver, Environment, Release,
  or Kubernetes. A project that declares only `[resources]`, definitions,
  policies, and declarations can validate them and export the admitted
  catalog and bundles as versioned documents for other tools, through the
  same code that feeds units.
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
    in stage 2, not a value source and not a dependency edge, so the field is
    literal-only.
  - `x-nyl-validations`: CEL rules on any schema node, including
    [transition rules](#transition-rules) that compare a resource with its
    previous version.
- **The schema describes resolved values.** A definition never says which
  fields may hold a value source; the
  [domain-resource context](#value-sources-in-declarations) does, for every
  definition alike. The schema validates what a field holds once its source
  resolves.
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

- Discovery uses the project-wide Git visibility rule that Kubernetes GitOps
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

### Value sources in declarations

A domain resource is one context of the
[value-source context table](declarative-value-sources.md#contexts), planned
with this design. Discovery walks each document with the one walker
(`bindings::resolve_document`) in that context, as it walks any other
document:

| Phase | Resolves | Carries |
| --- | --- | --- |
| Discovery and admission (stages 1–2), once per catalog | `$literal` | `$fromUnit` and `$fromPromotion`, at any depth of `spec`, to the consumer's resolution |
| Resolution (stage 4), per consumer and environment | `$fromUnit`, `$fromPromotion`, `$literal` | — |

```yaml
apiVersion: domain.acme.example/v1
kind: AwsAccessGrant
metadata: {name: payments-admins, labels: {team: payments}}
spec:
  account: payments-prod                         # x-nyl-reference: literal-only
  permissionSet: AdministratorAccess
  principalGroupId:
    $fromUnit: {unit: identity, output: groupIds, pointer: /payments-admins}
  sessionTags:
    - {key: team, value: payments}
    - {key: ticket, value: {$fromPromotion: {value: accessTicket}}}
```

- **Any depth of `spec`.** A source may stand wherever `spec` holds a value,
  inside objects and lists alike. The definition does not mark these fields:
  the context decides, so every definition accepts the same sources.
- **Other sources teach the fix.** Every other source is an error that names
  the location and says how to get the value:
  - `$fromEnvironment`: a declaration is the same in every environment; read
    the per-environment value in the consuming unit with `$fromEnvironment`;
  - `$fromInput` and `$fromParameter`: a declaration has no Release inputs or
    template parameters;
  - `$fromFile`, `$fromGit`, and `$fromPublication`: the declaration is
    already a file of the repository; write the value, or split it into its
    own declaration and reference it with an `x-nyl-reference` field;
  - `$fromCollection`: it reads a unit's collection and is valid in the unit's
    spec, not in a declaration.
- **Literal-only fields.** The envelope (`apiVersion`, `kind`,
  `metadata.name`, `metadata.labels`, and `metadata.annotations`) is read by
  discovery, selection, and policies before anything resolves, and every
  `x-nyl-reference` field is read by stage 2 and by the
  [reference closure](#referenced-resources) at S. A source in any of them is
  an error.
- **`$literal`** unwraps at discovery, so the schema checks its content,
  unless its content holds `$` keys; then it unwraps in stage 4, so its
  content never reads as a source, as in a unit document.
- **The `$` namespace** follows the definition's schema as it follows a unit
  kind's `spec_schema`: where the schema has no place for a `$` key that is no
  source, the key is an unknown source, reported with the typo suggestion;
  where it leaves an object open, the key is data.
- **Data, not configuration.** A value a source reads, such as a unit's
  output, is data and is never walked for further sources.
- **Editor schemas.** `nyl schema resources` offers `$fromUnit`,
  `$fromPromotion`, and `$literal` as `anyOf` alternatives next to the
  top-level `spec` fields that are not literal-only, generated from the same
  context table; deeper fields show only their literal type.
- **Stored with the declaration.** The catalog keeps each source as written.
  `nyl get resources` shows it that way, and only stage 4 replaces it.

## Stage 2: Admission

Every catalog resource goes through three checks. Each check reads only S,
so its result applies to every environment and can be cached under the
[cache invariants](../AGENTS.md#kubernetes-gitops-cache-invariants).

1. **Schema.** The definition validates `spec`, then applies its defaults. A
   position that holds a carried source is not yet known: it counts as
   present for `required` and `additionalProperties`, the checks of its own
   subschema are skipped, and no default is applied inside it. Everything
   else is validated and defaulted. An `x-nyl-validations` rule whose `self`
   holds a source runs in stage 4 instead. Stage 4 validates each resolved
   value against its field's subschema.
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

   - Policies see the declaration as written, carried sources included,
     because admission runs once per catalog, before any consumer resolves
     them.
   - Only validating policies are accepted. A mutating policy in the policy
     paths is an error (`NYL-RESOURCE-POLICY-MUTATES`), because mutation would
     make the bundle differ from what was reviewed.
   - Nyl derives a CRD from each definition, so `kyverno apply` can resolve
     kinds, plural resource names, and schemas offline. Which policy types
     work offline needs to be checked against the Kyverno CLI version that
     the render path already uses.
   - The Kyverno CLI is an external tool. Its version is part of the
     admission cache key, as for rendering.

### Transition rules

Some changes are fine to make once but not to change later, such as an AWS
account's email, which the provider can only change by replacing the
account. A definition states them as CEL rules on schema nodes, in the form
Kubernetes uses for `x-kubernetes-validations`:

```yaml
schema:
  properties:
    email:
      type: string
      x-nyl-validations:
        - rule: self == oldSelf
          message: Changing an account email replaces the account.
          effect: approve            # deny (default) | approve
          reason: email-change
    ou:
      type: string
      x-nyl-validations:
        - rule: "!(oldSelf == 'workloads' && self == 'sandbox')"
          message: Workload accounts cannot move back to the sandbox OU.
```

- `self` is the node's value. A rule that uses `oldSelf` is a transition
  rule: it applies only when the resource has a previous version, and only
  when the node existed in it. Rules without `oldSelf` apply to every
  version.
- A resource is matched with its previous version by identity. A new
  identity is a create, so a rename is a delete and a create, and deleting a
  resource is handled by [removal](#removal-and-retention), not by
  transition rules.
- **Effects.** `deny` fails admission like a schema error, so the consuming
  units are blocked. `approve` admits the change and makes it a **guarded
  change**: every consuming unit waits for approval of it (see
  [Guarded changes](#guarded-changes)).
- **Baselines.** What counts as the previous version depends on where the
  rule runs:
  - **When a unit runs**, the previous version is the resource as the
    bundle of the unit's current receipt holds it: what its native tool last
    received. This is the authoritative check, and it runs for units that
    own the resource (see [Ownership](#ownership)). It catches a change spread
    over several commits, and it applies per consuming unit, because two
    units may have last run different versions.
  - **Against a revision**, `--base <rev>` on `validate`, `plan`,
    `prepare`, `exec`, `bundle`, and `get resources` compares against the catalog at the merge base of
    `<rev>` and the source commit. In a pull request that is where the branch
    forked. This is the only baseline outside orchestration, and it judges
    the change since that revision rather than what was applied, which the
    docs must say (see [Choosing a base](#choosing-a-base)).
  - **No baseline**, `--no-base`, skips rules that use `oldSelf` and does not
    detect guarded changes. It is never implied.
- **Policies see the same comparison.** Kyverno receives the previous version
  as `oldObject` and the operation as `CREATE`, `UPDATE`, or `DELETE`, under
  the same baselines. Rules about the data belong in the definition, owned by
  its author. Rules about who may change what belong in policies, owned by
  the platform, for example only `platform/` may change an `ou`.
- Rules and their effects are part of the definition, so editing them is a
  reviewed change to a trusted path.

### Choosing a base

With `-e`, the consuming unit's receipt is always the baseline. `--base` adds
a comparison with a revision, as a pull request plan shows it, and
`--no-base` is rejected because the receipt comparison cannot be turned off.

Without `-e`, nothing records what was applied, so the invocation chooses.
So does `nyl bundle`, with or without `-e`, because an export has no
consuming unit and no receipt:

- A command that would compare with a previous version requires exactly one
  of `--base <rev>` and `--no-base`: `validate` when any definition declares a
  rule that uses `oldSelf`, `prepare` and `exec` when the unit has
  collections, and `bundle` when a selected or referenced type's definition
  declares such a rule. Neither fails with `NYL-RESOURCE-BASE-REQUIRED`, exit 1, whose
  fix names both options. Nyl never picks a base from the upstream branch,
  the default branch, or `nyl.toml`, because the right one depends on the
  job, and the refs a clone happens to have would otherwise decide the result.
- `--no-base` reports `baseline: none` and lists the rules it skipped, so a
  log shows what was not checked. It suits local development and a unit's
  first apply.
- A `<rev>` that does not resolve fails, never falling back to no baseline;
  a shallow CI clone has to fetch it.

| Job | Base |
| --- | --- |
| Pull request validate or plan | The target branch, such as `--base origin/main`; its merge base with the branch is used |
| Plan after merge | The last applied commit, read from a tracked ref with `--base <ref>` |
| Apply after merge | The same ref with `--advance <ref>`, which also moves it once the apply succeeds |
| Local development | `--no-base`, or the target branch to preview a pull request |

Comparing a job on `main` with `main`, such as `--base origin/main` there,
compares a commit with itself and detects nothing. Without a tracked ref, a
guarded change is therefore caught only in its pull request. Tracking is
optional; see [Tracking the last applied commit](#tracking-the-last-applied-commit).

### Tracking the last applied commit

Under orchestration the receipt records what ran, so nothing else is needed.
Outside orchestration, `nyl exec` can keep one Git ref per unit that points at
the commit it last applied, so that the jobs on `main` compare with what was
applied without custom scripting:

```sh
nyl exec --unit aws-accounts --base refs/nyl-applied/aws-accounts -- plan -out=tfplan
nyl exec --unit aws-accounts --advance refs/nyl-applied/aws-accounts -- apply tfplan
```

- **Any ref.** The argument is a full ref name on the source repository's
  `origin`, chosen by the pipeline: a custom ref such as
  `refs/nyl-applied/<unit>`, which is hidden from the branch list and
  triggers no CI, but cannot be protected and is rejected by some hosts
  ([Git host compatibility](../nyl/book/src/content/docs/orchestration/environments-and-state.md#git-host-compatibility));
  a tag, which is visible and protectable, but may trigger release
  pipelines; or a branch, which is protectable and visible. Nyl writes it
  through `nyl-git`, with the same transport and `[[git.credentials]]` as
  state pushes.
- **`--advance <ref>`** implies `--base <ref>` and excludes `--base` and
  `--no-base`. It belongs on the invocation that applies: Nyl cannot tell
  from the tool's arguments whether they change anything, so it advances
  whenever the tool exits 0.
- **Before the tool starts**, Nyl refuses with exit 125 and a stable code when
  the advance could not be recorded correctly:
  - `NYL-ADVANCE-DIRTY`: the unit is read from a worktree with uncommitted
    changes to files it reads, so no commit describes what would be applied.
  - `NYL-ADVANCE-NOT-DESCENDANT`: the ref points at a commit that is not an
    ancestor of the source commit, as for a retried job of an older commit
    after a newer one applied. Moving the ref would record an older state as
    the last applied one.
  - `NYL-ADVANCE-NO-PERMISSION`: a dry-run push with the ref's current value
    as its lease was rejected, so the credentials cannot write the ref.
    Protection rules that only reject the real update cannot be detected in
    advance; the recovery below covers them.
- **After the tool exits 0**, Nyl moves the ref from its value at the start to
  the source commit, as an atomic compare-and-swap. A tool that exits
  non-zero leaves the ref untouched, and `exec` exits with the tool's status.
- **When the update fails**, `exec` exits 125 with `NYL-ADVANCE-FAILED`, says
  that the apply itself succeeded, and prints the recovery: the push to run
  once the cause is fixed, with the expected old value as its lease, and,
  when the ref moved meanwhile, both commits and an instruction not to force
  it. Exit 125 after the tool ran is specific to `--advance`; it is the only
  case in which `exec` does not exit with the tool's own status once the
  tool has started. A missed advance fails closed: the next run compares with the older
  commit and asks again about guarded changes since then; it never skips
  one.
- **Bootstrapping.** A ref that does not exist yet fails with
  `NYL-ADVANCE-NO-REF`, naming two explicit ways to start: `--bootstrap`
  runs once without a baseline, as `--no-base` does, and creates the ref,
  with a lease that requires it to be absent; or the ref is created by hand
  at the commit that was last applied before adopting Nyl, which gives the
  first run a real baseline.
- The ref records only a commit. It is not a receipt, orchestration does not
  read it, and selecting the unit from an Environment later replaces it with
  the receipt baseline.

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
    accounts: {$fromCollection: {collection: accounts, each: /spec}}
```

| Field | Default | Meaning |
| --- | --- | --- |
| `type` | required | One definition's `apiVersion` and `kind` |
| `selector` | `{}`: all | `matchLabels` over literal `metadata.labels`, as `unitSelector` |
| `paths` | all | Globs over the resource's trusted, worktree-rooted path, under the local path rule |
| `access` | `own` | `own` claims the members (see [Ownership](#ownership)); `read` only reads them |
| `allowEmpty` | `false` | An empty collection fails resolution (`NYL-RESOURCE-COLLECTION-EMPTY`) unless `true` |
| `removal` | `approve` | Whether dropping members needs `--approve` (see [Removal](#removal-and-retention)); only with `access: own` |

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
- **Guards follow ownership.** The owner creates and deletes what a member
  describes, so the owner's runs carry the guards: dropped members under
  `removal: approve`, and transition rules evaluated against the receipt
  baseline, including guarded changes. A `read` collection gets neither. Its
  unit re-runs with the new data but never waits for an approval that the
  owner already needs. Setting `removal` on a `read` collection is an error
  (`NYL-RESOURCE-REMOVAL-READ`). Rules checked against `--base` judge the
  resource itself, not a consumer, so they apply whoever consumes it.
- Like a Command's effects, a claim is a declaration Nyl cannot verify.
  Preventing two configurations from fighting over one account is the point
  of making it explicit.

## Stage 4: Resolution

Stage 4 resolves the sources that the members and
[referenced resources](#referenced-resources) carry, with the same walker in
the resolution phase of the
[domain-resource context](#value-sources-in-declarations). Each source
resolves in the consumer's environment: the consuming unit's when a run
resolves the unit, and the one `-e` names for `nyl bundle`.

- Sources resolve through the one resolver in `nyl_core::bindings`
  ([`resolve_document`](../crates/nyl-core/src/bindings/mod.rs)) and a
  `Providers` implementation chosen by the entry point, with the freshness,
  evidence, and blocking rules of [References](orchestration-core.md#references):
  the run's for `reconcile`, `plan`, and `prepare`, the read-only snapshot
  providers of `nyl get output` for `nyl bundle -e`, and none for `nyl
  bundle` without `-e`, which leaves every source in place (see
  [Standalone export](#standalone-export-nyl-bundle)). Everything else in
  stages 3–5 is the same code for every entry point.
- For a unit, each source adds a dependency edge from the consuming unit to
  the producer. These edges exist at S before any value does, so the pure
  dependency graph, the CI export, and `plan` see them and report `blockedBy`
  as for any other reference. A blocked pointer is located inside the bundle,
  such as `/collections/grants/items/payments-admins/spec/principalGroupId`,
  and the report cites the member's source path.
- **Validation of resolved values.** Each resolved value is validated against
  its field's subschema, and that subschema's defaults are applied inside it.
  The resolved spec is then checked once more against the whole schema, so
  keywords that relate a field to its siblings, such as `if`/`then` or
  `dependentSchemas`, see the resolved value, and the `x-nyl-validations`
  rules that admission deferred run now. A mismatch is a resolution error for
  the consumer, located at the source's pointer and citing the member's source
  path.
- Resolved values are data: a unit's output that happens to hold a `$` key is
  never read as a source.
- Template lookups cannot read runtime state, because members are never
  templates.

## Stage 5: Bundle

Resolution produces one **resource bundle** per consumer: a unit with
collections, a Release, or an export:

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
referenced:
  domain.acme.example/v1/GitHubTeam:
    payments: {labels: {team: payments}, spec: {org: acme, maintainers: [alice]}}
    search: {labels: {team: search}, spec: {org: acme, maintainers: [bob]}}
  domain.acme.example/v1/GitHubOrganization:
    acme: {labels: {}, spec: {login: acme-corp}}
```

- Items hold `labels` and the admitted, defaulted, resolved `spec`.
  Annotations stay out, because they are notes for people and for policies.
- The bundle is versioned like every state file, with golden files and a
  migration for each format change. It is also a public contract for other
  tools, the same document whichever consumer it was built for (see
  [The bundle as a contract](#the-bundle-as-a-contract)).
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
  ran without rendering S again.
- **Baseline.** Each receipt keeps the bundle it executed as
  `observed/units/<unit>/bundle.yaml`, next to the observed unit, so
  [removal](#removal-and-retention) and [transition rules](#transition-rules)
  compare against what the native tool last received without walking Git
  history.
- **Execution key.** The canonical digest of the bundle enters the key through
  `resolvedSpec`. Provenance does not. So:
  - editing a member or a resource it references, adding or removing a
    member, or a schema default changing an effective value re-runs the unit;
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

### Referenced resources

A member often names another resource through an `x-nyl-reference` field,
such as a `GitHubTeam` whose `org` names a `GitHubOrganization`. The consumer
needs that resource's data, but does not manage it and should not have to
collect it. The bundle's `referenced` map therefore carries it:

- **Closure.** `referenced` holds every resource reachable from the members
  through `x-nyl-reference` fields, transitively, including through other
  referenced resources. It is keyed by `<apiVersion>/<kind>` and name, so a
  cycle adds each resource once. A resource that is also a member appears in
  both places.
- **Static.** The closure is computed from the admitted catalog at S, like
  membership, so it is known before resolution. Admission already guarantees
  that every reference names an existing resource. A referenced resource that
  fails admission fails resolution of the consumer, as a member does.
- **Read-only.** Referenced resources are never owned, whatever the
  collection's `access`, and the collection's `selector` and `paths` do not
  apply to them. They were admitted under their own path, and the consumer
  only reads them. Removal and transition-rule guards follow their owners'
  collections.
- **Sources inside.** Sources that referenced resources carry resolve like
  those of members, in the consumer's environment, are validated against
  their fields' subschemas in the same way, and add the same dependency
  edges. The `x-nyl-reference` fields that build the closure are
  literal-only, so a source can never change which resources it holds.
- **Digest and provenance.** `referenced` is part of the bundle and its
  digest. `ResourceBundleProvenance` records each referenced resource's
  source path, document index, and digest, and the reference pointers that
  reached it.

## Standalone export: `nyl bundle`

Domain modeling works on its own. Stages 0–5 read only the project's
`[resources]`, definitions, policies, and declarations, so a project with
nothing else (no Release, Environment, unit, driver, or cluster) can validate
its declarations and export them for tools Nyl does not drive: a service
catalog, a provisioning script, another IaC tool, or a CI job that checks
quotas. Two commands export, and both run stages 0–5 through the code that
feeds units, so an export and a unit's bundle cannot disagree:

| Command | Exports | Stages |
| --- | --- | --- |
| `nyl get resources -o yaml\|json` | The catalog: every matched declaration as admitted, with defaults applied and sources as written | 0–2 |
| `nyl bundle` | A `ResourceBundle` for a selection, with its referenced resources and provenance | 0–5 |

Neither writes state, takes a lease, loads a unit driver, renders a Release,
or contacts a cluster. Without `-e` they read nothing but the source, so
their output depends only on S and the arguments, and every machine and CI
job produces the same bytes.

### `nyl get resources`

```text
nyl get resources [<kind>/<name>] [--kind <type>] [-l <selector>] [--path <glob>]...
                  [-e <env>] [--base <rev>] [-o table|wide|yaml|json|name] [<project>]
```

- `--kind` takes the kind when one definition serves it (`AwsAccount`), or
  `<group>/<version>/<kind>`. `-l` is an equality selector over literal
  labels (`team=payments,stage=production`), as `matchLabels` in a
  collection. `--path` takes the globs of a collection's `paths`.
- The table shows the name, kind, labels, source location, and admission
  result. `-o yaml|json` prints a versioned `ResourceCatalog` document from
  the same report:

  ```yaml
  apiVersion: gitops.nyl/v1
  kind: ResourceCatalog
  items:
    - apiVersion: domain.acme.example/v1
      kind: AwsAccount
      metadata: {name: payments-prod, labels: {team: payments, stage: production}, annotations: {note: PCI scope}}
      spec: {email: aws+payments-prod@acme.example, ou: workloads, owner: payments, budgetUsd: 100}
      source: {path: /teams/payments/aws.yaml, document: 0, digest: sha256:…}
      admission: {admitted: true}
  ```

  `spec` is defaulted for admitted resources and as written for the others,
  whose `admission` lists the findings with their codes. Carried sources stay
  as written: the catalog is the same in every environment.
- Transition rules that use `oldSelf` run only with `--base <rev>`; without
  it the report says `baseline: none` and lists them as not evaluated.
  Listing is inspection, not a gate; `nyl validate` and `nyl bundle` are the
  gates.
- With `-e`, each resource also lists the units of that environment whose
  collections include it, with their `access`. Its values stay as written;
  `nyl bundle -e` resolves them.
- Exit 0 when every listed resource is admitted, 1 when one is not or the
  project's definitions fail.

### `nyl bundle`

```text
nyl bundle (--kind <type> [-l <selector>] [--path <glob>]... [--as <name>] [--allow-empty]
            | --collection <unit>/<collection>...)
           [-e <env>] [--worktree | --source <rev>] [--base <rev> | --no-base]
           [--allow-change <resource>:<reason>] [-o yaml|json] [--output-dir <dir>] [<project>]
```

- **Selection, like a collection.** `--kind`, `-l`, and `--path` select one
  collection's members by the rules of [stage 3](#stage-3-collection): type,
  literal labels, and trusted paths, from the envelope and path alone, keyed
  by name and sorted. `--as` names the collection in the bundle, by default
  the definition's plural (`awsaccounts`). An empty selection fails with
  `NYL-RESOURCE-COLLECTION-EMPTY` unless `--allow-empty`, as `allowEmpty`
  does.
- **A named collection.** `--collection <unit>/<collection>`, repeatable,
  takes a unit's declared collections instead. The unit document renders as
  `nyl prepare` renders it, in the `-e` environment or, without `-e`, as `nyl
  build` does, and only its envelope and the common `collections` field are
  read: the kind's spec is not validated and its driver is not loaded.
  Naming every collection of a unit gives the bundle that unit would receive,
  byte for byte.
- **Referenced resources** join the bundle by the
  [closure](#referenced-resources), with provenance, exactly as for a unit.
- **An export claims nothing.** Ownership, `removal`, and the receipt
  baseline belong to a unit's runs; an export behaves like an `access: read`
  collection.
- **Transition rules** follow [Choosing a base](#choosing-a-base) as for
  `prepare` outside orchestration: `--base <rev>` or `--no-base` is required
  when a definition among the selected and referenced types declares a rule
  that uses `oldSelf`, also with `-e`, because the receipt baseline belongs
  to a consuming unit. A guarded change exits 2 unless `--allow-change` names
  it.
- **Failure scope.** Only the selected members and their referenced resources
  must pass admission; another team's failing declaration does not stop the
  export.
- **Source files** come from the worktree or `--source`, under the rules of
  [Which files a unit is read from](#which-files-a-unit-is-read-from).

**Sources.** Without `-e`, nothing resolves: carried sources stay in place,
their fields are not checked against their subschemas yet, and the bundle
lists them under `unresolved`, so a tool that reads only the bundle cannot
mistake a source for a value:

```yaml
apiVersion: gitops.nyl/v1
kind: ResourceBundle
collections:
  grants:
    apiVersion: domain.acme.example/v1
    kind: AwsAccessGrant
    items:
      payments-admins:
        labels: {team: payments}
        spec:
          account: payments-prod
          permissionSet: AdministratorAccess
          principalGroupId: {$fromUnit: {unit: identity, output: groupIds, pointer: /payments-admins}}
referenced:
  domain.acme.example/v1/AwsAccount:
    payments-prod: {labels: {team: payments, stage: production}, spec: {email: aws+payments-prod@acme.example, ou: workloads, owner: payments, budgetUsd: 100}}
unresolved:
  - {pointer: /collections/grants/items/payments-admins/spec/principalGroupId, source: $fromUnit}
```

This is not an error: the command exits 0, and stderr lists each source with
the member's source path under the warning `NYL-RESOURCE-SOURCES-UNRESOLVED`.
With `-e <env>`, sources resolve in that environment, read-only against its
current receipts and promotion records, exactly as `nyl get output` resolves
a `$fromUnit` and with the same freshness and evidence rules; it needs read
access to the state repository and nothing else. A resolved bundle has no
`unresolved` key, and its resolved values pass stage 4's validation.

**Output.** The bundle goes to stdout as YAML, or JSON with `-o json`, and
diagnostics go to stderr. `--output-dir <dir>` writes `bundle.yaml` and
`bundle-provenance.yaml`, the files and bytes `nyl prepare` writes. The
provenance document records each item's source path, document index, and
digest, each definition's identity and digest, the receipt citation of each
resolved value (with `-e`, the state commit read), and the bundle's digest.

| Exit | Meaning |
| --- | --- |
| 0 | The bundle was written. Without `-e`, sources it holds are listed under `unresolved`. |
| 1 | A configuration, selection, admission, or resolution error: an unknown type, a `--collection` that names none, an empty selection, a selected or referenced resource that fails admission, a resolved value its subschema rejects, `NYL-RESOURCE-BASE-REQUIRED`, or an `-e` environment whose state cannot be read. |
| 2 | With `-e`, a source is blocked (a producer without a current receipt or its evidence, or no promotion record), or a guarded change is not named by `--allow-change`. stderr lists the blocked pointers with their source paths; nothing is written to stdout or `--output-dir`. |

### The bundle as a contract

`ResourceBundle`, `ResourceBundleProvenance`, and `ResourceCatalog` are
stable, versioned documents for tools other than Nyl:

- `nyl schema` publishes a JSON Schema for each, next to the resource
  schemas, and golden files pin each version.
- Within `gitops.nyl/v1`, a change only adds optional fields; readers ignore
  fields they do not know. Anything else is a new version, which the
  changelog announces.
- The bundle is canonical: items keyed by name and sorted bytewise, the
  `referenced` map keyed by `<apiVersion>/<kind>` and name, and a digest over
  its canonical JSON. The same members and resolved values give the same
  bytes from `nyl bundle`, `nyl prepare`, and a run, so a tool can compare
  digests instead of contents.
- A bundle holds no annotations, no provenance, and no secrets: declarations
  live in Git, and a `$fromUnit` cannot read a sensitive output.
- A bundle that a unit receives or Nyl stores never has `unresolved`; only an
  export without `-e` does.

### Why `nyl bundle`

The bundle command is a top-level verb, like `render`, `render-tree`,
`build`, and `prepare`, rather than a `nyl get` form:

- `nyl get` reports what the source declares or the state records, with one
  set of options (a name filter, `-o table|wide|yaml|json|name`) and exit 0 or
  1. A standalone bundle exists nowhere until stages 3–5 assemble it from a
  selection given on the command line; there is no name to get.
- It takes options that no `get` form has (`--as`, `--base`, `--no-base`,
  `--allow-change`, `--output-dir`, `--worktree`, `--source`) and exits 2 for
  a blocked source or a guarded change, as `prepare` does.
- `nyl get resources` stays a `get` form, because the catalog is what the
  source declares.

### Relation to `nyl prepare`

| | `nyl bundle` | `nyl prepare` |
| --- | --- | --- |
| Consumer | None; a selection or named collections | One unit |
| Stages | 0–5 | 0–6 |
| Needs | `[resources]` and declarations | The unit, and its driver for `--native` |
| Sources without `-e` | Left in place and listed under `unresolved`, exit 0 | An error naming `-e`, because the unit's native inputs need values |
| Writes | stdout, or `bundle.yaml` and `bundle-provenance.yaml` | Those, `resolved-spec.yaml`, and with `--native` the native inputs |
| Fixtures | No | `--fixture-outputs` |

For the same members and environment, `bundle.yaml` is the same bytes from
both. `prepare` is how a unit's author inspects what the unit receives;
`bundle` is how anything else reads the declarations.

## Stage 6: Driver adaptation

The bundle reaches an implementation in one of two ways. Neither needs a
kind-specific option.

**Through references.** `$fromCollection` is a new
[value source](declarative-value-sources.md) of the unit contexts: a unit's
spec carries it when the unit renders, and a run resolves it, like
`$fromUnit`, from the unit's own bundle. Every other context rejects it with
the fix, a declaration among them:

| Reference | Resolves to |
| --- | --- |
| `$fromCollection: {collection}` | The collection's `items` map: name to `{labels, spec}` |
| `$fromCollection: {collection, each: <pointer>}` | Name to the value at `pointer` inside each item, such as `/spec` or `/spec/email`; a member without it is a resolution error |
| `$fromCollection: {collection, pointer: <pointer>}` | A value inside the collection document, such as `/items/payments-prod/spec` |

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
  driver's `plan`, `reconcile`, `detect_drift`, and `exec` pass to the tool:

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
nyl prepare [-e <env>] --unit <u> [--deployed | --worktree | --source <rev>] [--base <rev> | --no-base]
            [--native] [--check <dir>] [--allow-change <resource>:<reason>]
            [--fixture-outputs <file>] [--output-dir <dir>] [-o yaml|json]
```

| Output | Content |
| --- | --- |
| `bundle.yaml` | The resource bundle |
| `bundle-provenance.yaml` | Where each item and resolved value came from |
| `resolved-spec.yaml` | The unit's resolved spec and execution key |
| `native/` | With `--native`: the driver's `NativeInputs` files and a `steps.txt` with the native commands, secrets shown by name |

- **Source.** See [Which files a unit is read from](#which-files-a-unit-is-read-from).
  `-e` names the environment whose values and receipts the unit resolves
  against. Without `-e`, see [Outside orchestration](#outside-orchestration).
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
  error, or `--check` found other inputs, 2 blocked by a reference or a
  guarded change.
- Once inputs are materialized, the implementation works with its own tool,
  for example `tofu plan -var-file=out/native/variables.tfvars.json` against
  an isolated backend. Resolving dependencies still requires Nyl.

### Which files a unit is read from

`prepare` and `exec` read a unit, its definitions, policies, collection
members, and source files from one of three sources:

| Source | Flag | Available |
| --- | --- | --- |
| The deployed desired document and bundle | `--deployed` | With `-e` |
| The worktree, uncommitted and untracked files included, as `nyl render` sees it | `--worktree` | Always |
| A commit | `--source <rev>` | Always |

- **Defaults.** `exec -e` uses the deployed spec, as the
  [operator exec](orchestration-core.md#operator-exec) contract defines.
  Every other form uses the worktree, because there is either nothing
  deployed or nothing executed.
- **Relevant changes.** Only uncommitted changes to files the invocation
  reads count; stages 0–6 record those files as they read them, so edits
  elsewhere in the repository never matter.
- **The worktree is used and has relevant changes.** `prepare` warns with
  `NYL-SOURCE-DIRTY`, lists the paths, and marks its output as dirty in
  provenance. `exec` without `-e` refuses with exit 125 unless `--worktree`
  was given explicitly, to run the uncommitted changes, or `--source HEAD`
  to run the commit, because otherwise an apply could use inputs that no
  commit records. This follows `publish-tree`, which refuses dirty
  provenance unless the user allows it. A clean CI checkout never meets it.
- **The worktree is not used but has relevant changes.** With `--deployed`,
  `exec -e`'s default, or `--source`, Nyl warns with
  `NYL-SOURCE-WORKTREE-IGNORED`, names the changed paths, and says to pass
  `--worktree` to use them. Passing `--deployed` or `--source` explicitly
  silences it: the caller knows the current changes will not run.
- `--base <rev>` compares with the merge base of `<rev>` and the commit the
  source is based on, which for the worktree is `HEAD`, so uncommitted edits
  are changes relative to that base.

### Outside orchestration

Nyl can compile inputs for an existing Terraform or OpenTofu pipeline without
Environments, state refs, or receipts. The unit document is the same; no
Environment selects it:

```sh
nyl prepare --unit aws-accounts --native --base origin/main --output-dir out/   # in a pull request
tofu -chdir=platform/aws-accounts init -backend-config=../../out/native/backend.tfbackend.json
tofu -chdir=platform/aws-accounts plan -var-file=../../out/native/variables.tfvars.json
```

- Without `-e`, `prepare` renders the unit as `nyl build` does, with no
  environment values and no state, from the worktree unless `--source`
  names a commit. A spec that uses
  `values`, `environment`, or a `$fromUnit` or `$fromPromotion` source,
  directly or inside a member, fails and names `-e`, because the native
  inputs need values. [`nyl bundle`](#standalone-export-nyl-bundle), which
  feeds no driver, lists such sources instead.
- Stages 0–6 are the same code as under orchestration, so the native inputs
  are byte-identical to what an orchestrated run of the same unit would pass.
  Adopting orchestration later means selecting the unit from an Environment.
- What remains: schema checks, transition rules against `--base` (see
  [Choosing a base](#choosing-a-base)), policy
  admission, ownership claims checked across all declared units, guarded
  changes through `--allow-change`, deterministic bundles, and provenance.
- What the pipeline takes over: deciding when to run, since there is no
  execution key to skip unchanged inputs; plan review and approval, since
  there are no receipts; and native state locking, as before.
- `nyl prepare --check <dir>` exits 1 when `<dir>` holds inputs other than
  what `prepare` would write now, so a later pipeline stage can confirm that
  an artifact passed from an earlier stage is still current.

Instead of wiring the prepared files into the tool by hand, a pipeline can
let Nyl run the tool. `nyl exec` without `-e` runs the unit's
[`exec` capability](orchestration-core.md#operator-exec) directly:

```sh
nyl exec --unit aws-accounts --base origin/main -- plan -out=tfplan                       # pull request
nyl exec --unit aws-accounts --advance refs/nyl-applied/aws-accounts -- apply tfplan      # after merge
```

- The driver prepares the tool exactly as it does under orchestration, from
  the same `NativeInputs`: for OpenTofu, `init` with the backend file and
  `-lockfile=readonly`, its own `TF_DATA_DIR`, and the variables file. The
  arguments after `--` are passed to the tool unchanged.
- There is no environment, so there is nothing to hold, no lease, no
  ownership-scope check against state, and nothing recorded except the
  optional [tracked ref](#tracking-the-last-applied-commit). Files come from
  the worktree or `--source`, under the rules of
  [Which files a unit is read from](#which-files-a-unit-is-read-from), and the
  tool runs in its own copy, so it never writes into the checkout.
- Admission, transition rules against `--base` or none with `--no-base`, and
  guarded changes apply as for `prepare`: a guarded change refuses with exit
  125 unless `--allow-change` names it, before the tool starts.
- Credentials are the unit's admitted `env`, as always; the process
  environment reaches the tool only through `env.passthrough`.
- With `-e`, `nyl exec` keeps its orchestrated meaning: a held unit, the
  lease, and a recorded repair.

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
- [`nyl get resources`](#nyl-get-resources) lists the catalog. With `-e`,
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
  current receipt's collection had is a guarded change with reason
  `members-removed`.
- Nyl keeps no state about native resources. Retaining a native resource is
  the implementation's job, for example `prevent_destroy` on accounts. The
  supported way to stop managing an account without closing it is
  breakglass: hold, `nyl exec … -- state rm`, remove the declaration, resume.
- Revoking a grant is the normal case: delete the declaration, review the
  plan's destroy of that grant, and approve.

### Guarded changes

A guarded change is a change that Nyl admits but does not apply without an
approval: members removed from a collection, or a member change that a
transition rule with `effect: approve` matches.

- The consuming unit becomes `awaiting-approval` even under `approval: auto`,
  with one reason per guarded change, such as `email-change` on
  `AwsAccount/payments-prod`. `plan` lists them next to the native change
  summary.
- `--approve <unit>` approves every guarded change in that unit's plan. The
  receipt records the approved reasons. A `bind: plan` approval already
  covers them, because the change digest includes the native consequences.
- **Outside orchestration** there is no approval record. `nyl prepare`,
  `nyl bundle`, and `nyl validate` exit 2 for a guarded change against `--base`, and `nyl exec`
  refuses with exit 125 before the tool starts, writing no
  native inputs, unless the invocation names it with
  `--allow-change <resource>:<reason>`. A pipeline ties that flag to its own
  approval, such as a protected CI environment.

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
  `spec`. Fields that hold a value source keep it as written, because the
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
  is not. In a unit with `forEach`, every `{{ … }}` that emits a string must
  therefore emit only letters, digits, spaces, and `. _ - / @ + =`, which
  cannot change YAML structure in a plain or a quoted scalar, or a value that
  `tojson` produced. Otherwise rendering fails with
  `NYL-UNIT-RESOURCE-UNSAFE-VALUE`, naming the field. The check runs in the
  template engine's output formatter, so it holds whatever filters or
  concatenation built the string: `'teams/{{ resource.metadata.name }}'`
  renders as `'teams/payments'`, and `"x\napproval: auto"` never becomes
  structure. A template inserts any other value whole with
  `{{ resource.spec.description | tojson }}`, which renders a quoted JSON
  string, or a list or object. Names already follow the DNS label rule, and
  schemas should constrain the other fields that templates use.
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
| `ResourceDefinition`, the domain envelope, `collections`, `$fromCollection`, `ResourceBundle`, its provenance, `ResourceCatalog`, golden files and published schemas | `nyl-core` (`resources::domain`, `bindings`) |
| The domain-resource context: which sources each phase resolves, carries, or rejects with its fix, the literal-only fields, and the editor-schema alternatives | `nyl-core` `bindings::Context`, the one context table every walker reads |
| Identity, duplicates, ordering, membership, ownership claims, the reference closure, removal detection, transition-rule decisions | `nyl-collections`, pure rules, one module each; CEL evaluation behind a `RuleEvaluator` port in `nyl-core` |
| Schema validation and defaulting (JSON Schema 2020-12, a new dependency to evaluate), with carried sources as not-yet-known positions, and the validation of resolved values | `nyl-collections`, pure |
| Bundle assembly from a selection, `unresolved`, and the canonical digest | `nyl-collections`, one function shared by units, [Releases](release-catalog.md), and `nyl bundle` |
| Discovery of definitions and resources, the catalog, Kyverno admission | `nyl-render` (`gitops::discovery`, `validation`), returned through `SourceRenderer` as part of `RenderedSource` |
| Resolving sources inside a bundle: `bindings::resolve_document` with the providers each entry point passes (the run's, the read-only snapshot providers of `nyl get output`, or none) | `nyl-core` `bindings`; the run's providers in `nyl-orchestration` resolution, the read-only ones in `nyl-orchestration` `shell::inspect`, which `nyl get output` already uses |
| The bundle digest in the execution key | `nyl-orchestration` `rules::execution_key` |
| `prepare` and `NativeInputs` per kind | `nyl-units`, in the kind's module |
| `nyl prepare`, `bundle`, `get resources`, `schema resources` | `nyl-cli`, wiring only: each passes the same catalog, selection, and providers to the shared stages |

Contract walkthroughs become scenarios under
`nyl/tests/scenarios/walkthroughs/`. These include a member added, a member
removed and approved, a file moved without re-execution, a blocked
`$fromUnit` inside a member, an admission failure that blocks only its
consumer, and two owners rejected.

## Rollout

| Slice | Delivers | Needs |
| --- | --- | --- |
| R1 | Definitions, discovery, schema and referential admission, the domain-resource context in the value-source table (sources carried, rejected with their fixes, and skipped by schema checks), `nyl validate`, `get resources`, `schema resources` | M2 (useful without orchestration) |
| R12 | The standalone export: `nyl-collections` membership, the reference closure, and bundle assembly; the `ResourceBundle`, `ResourceBundleProvenance`, and `ResourceCatalog` formats with golden files and published schemas; `nyl get resources -o yaml\|json`; `nyl bundle` with and without `-e` | R1, M6 |
| R2 | `collections`, ownership, the bundle in state files and the execution key, sources in members resolved by runs, `$fromCollection`, Command adaptation, `nyl prepare` without `--native` | M3, R12 |
| R3 | Kyverno admission with trusted context | R1 |
| R4 | `prepare` capability and `--native`, OpenTofu/Terraform adaptation, `prepare` and `exec` outside orchestration, the source rules for dirty worktrees, `prepare --check`, removal approval, exec with bundles | M4 |
| R5 | Transition rules: `x-nyl-validations` with `oldSelf`, `--base` and `--no-base` (on `nyl bundle` too), the receipt baseline, Kyverno `oldObject`, the `approve` effect for guarded changes, and tracked refs with `--advance` | R3, R4 |
| R6 | `forEach` unit instances: the `resource` context, YAML-safe values, instance names, leaving and teardown | R2 |
| R7 | The worked guide, validated as a tier-2 reference scenario | R2–R6 |
| R8–R11 | The [Release catalog](release-catalog.md#rollout): Release labels and `spec.manifests`, label selection in ApplicationGroups, Release admission, collections and `forEach` in Releases | M2, R1, R3, R12 |

R12 follows R1 and comes before R2: it is the first slice that assembles
bundles, so every later consumer (units in R2, Releases in R10) reuses its
code rather than building its own. Slice identifiers are stable; R12 is
numbered after the existing slices and ordered by what it needs.

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
   `variables.accounts: {$fromCollection: {collection: accounts, each: /spec}}`,
   declare the matching `map(object(…))` variable, and `for_each` over it.
   Add `prevent_destroy` to resources that must survive removal.
4. **Inspect.** `nyl prepare -e org --unit aws-accounts --native` writes the
   bundle, its provenance, and the variables file. Run
   `tofu plan -var-file=…` against a scratch backend. Move a declaration to
   another folder that the collection's `paths` still match and re-run: same
   bundle digest, same execution key. Change a budget: new key.
   `nyl bundle --collection aws-accounts/accounts` prints the same
   collection without preparing the unit, and `nyl bundle --kind AwsAccount
   -l team=payments` any other selection, for tools outside Nyl.
5. **Reference another unit.** Add `AwsAccessGrant` with a
   `principalGroupId`, set it with `$fromUnit` to the
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
8. **Guard changes.** Add a transition rule that makes an account email
   change need approval. Change an email in a pull request:
   `nyl validate --base origin/main` reports `email-change`, and the unit
   waits for `--approve` before the next apply.
9. **Breakglass.** Hold the unit, `nyl exec -e org --unit aws-accounts -- state
   …` to repair, resume, and approve the next plan.
10. **Remove.** Delete a grant's declaration: the plan shows
   `-1 payments-admins` and the grant's destroy, and approval revokes it.
   Delete an account's declaration: `prevent_destroy` fails the plan, and the
   retirement flow from step 9 detaches the account instead.

## Open decisions

| Decision | Needed by |
| --- | --- |
| JSON Schema crate and the exact subset (formats asserted or annotation-only), and how not-yet-known positions interact with `if`, `oneOf`, `uniqueItems`, and `contains` at admission | R1 |
| Whether `nyl bundle` can emit an earlier format version for tools pinned to one, or only the newest | R12 |
| Whether `nyl get resources` needs a more specific name, because `nyl get` also reports Nyl's own project resources | R12 |
| Whether one `nyl bundle` invocation takes several ad-hoc selections, each with its own `--as` | After R12 |
| Whether `--collection` also names a Release's collection, as `<group>/<release>/<collection>` | R10 |
| Definition sharing across repositories through a locked `GitSource`, refreshed by `nyl update source-locks` | After R1 |
| Version conversion between definition versions, or coordinated bumps only | After R2 |
| Bundle size limits for desired state, and whether large bundles stay in state or are referenced by digest | R2 |
| Offline Kyverno coverage of `ValidatingPolicy` for derived CRDs, and whether CODEOWNERS owners are offered as trusted policy context | R3 |
| Generating HCL variable types from definitions (`nyl schema hcl`) | After R4 |
| Whether per-member removal approval should be per member (`--approve aws-accounts:legacy-sandbox`) | R4 |
| Whether the character set for `resource` strings printed into templates should be wider, for example to allow `:` inside quoted scalars | R6 |
| Whether instance names use a separator that cannot occur in unit names, which would make the name clash check unnecessary | R6 |
| CEL implementation for `x-nyl-validations`: a Rust CEL crate, or rules evaluated through the Kyverno CLI | R5 |
| Whether `--allow-change` outside orchestration should be tied to a CI provider's own approvals, such as a protected environment | R5 |
| Whether drivers should classify tool arguments, such as OpenTofu `apply` and `destroy`, so `--advance` moves the ref only after an invocation that can change something | R5 |
| A default name for tracked refs, such as `refs/nyl-applied/<unit>`, and whether `nyl.toml` may set the prefix | R5 |
