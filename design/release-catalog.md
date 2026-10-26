# Release catalog and collections in rendering

**Status:** proposal, slices R8 to R11 of milestone M9. Nothing here exists
yet. Progress is tracked on the
[Domain resources roadmap](https://niklasrosenstein.github.io/nyl/domain-resources/roadmap/).
It builds on [Release inputs](release-inputs.md) and
[domain resource collections](domain-resources.md), and changes neither for
projects that do not use it.

A Release is a Kubernetes deployment boundary: one workload Argo CD
Application per target. Today it lives in the file that holds its manifests,
and an ApplicationGroup finds it by path. This proposal makes the Release an
inventoried resource of its own:

- **Manifests by reference.** A Release can point at its manifests with
  `spec.manifests`. Keeping them in the Release's own file stays the default.
- **Selection by labels.** An ApplicationGroup source can select Releases by
  label as well as by path.
- **Policy admission.** Policies decide which folders may declare which
  Releases, and what those Releases may render.
- **Collections.** A Release can consume domain resource collections, and
  the resources they reference, as template data.
- **One Release per resource.** A Release can be instantiated once per domain
  resource with `forEach`.

The rendering layer and orchestration share one implementation of
collections, in the `nyl-collections` crate.

Two inventories appear below. The **Release catalog** lists Release
envelopes and exists in every project. The **domain catalog** of
[domain resources](domain-resources.md#stage-1-discovery) exists only when
`nyl.toml` has a `[resources]` table. Neither is the Argo CD catalog
Application.

## Compatibility

- A Release without `metadata.labels`, `spec.manifests`, `spec.collections`,
  or `forEach`, and an ApplicationGroup source without `selector`, render
  exactly as before: the same bytes, cache keys, and ownership index.
- A Release in a file that also holds control resources is ignored today.
  It stays ignored, with a warning, unless it declares `spec.manifests` (see
  [Shared files](#shared-files)).
- `spec.include` keeps working and is deprecated in favour of
  `spec.manifests` (see [Manifests](#manifests)).
- The deployed identity of a Release on a target stays
  `<applicationGroup>/<release>`. Bindings, generated names, and the
  ownership index keep their keys.

## The Release envelope

```yaml
apiVersion: k8s.nyl/v1
kind: Release
metadata:
  name: web
  namespace: web
  labels: {team: payments, tier: frontend}
spec:
  manifests: [/templates/web-app.yaml, apps/web/extra/*.yaml]
  inputs:
    replicas: {type: integer, default: 3}
```

- **Static fields.** `metadata.labels`, `spec.manifests`,
  `spec.collections`, and `forEach` are new and literal, like `spec.inputs`,
  because membership, discovery, and the bundle need them before rendering.
  Any of them requires a literal `metadata.name`. Template syntax in one of
  them fails discovery (`NYL-RELEASE-TEMPLATED-ENVELOPE`), naming the file,
  the document, and the field.
- **Scan, render, verify.** Every document whose top-level `kind` is
  `Release` is scanned with the static envelope scanner that control
  resources use, including its fallback for documents that block-level
  template syntax keeps from parsing. After rendering, the rendered Release
  must carry the scanned `metadata.name`, `metadata.labels`,
  `spec.manifests`, `spec.collections`, `forEach`, and `spec.inputs`;
  otherwise the render fails and names the document. A Release that the scanner
  cannot read at all keeps today's behaviour: it is a member only by path,
  and it may not use any of the static fields.
- **Values.** The template context keeps three distinct sources:

  | Variable | Set by | Varies per | Shape |
  | --- | --- | --- | --- |
  | `values` | The platform, Cluster then DeploymentTarget | target | untyped, deep-merged |
  | `inputs` | Declared by the Release, bound by the target | target | typed, replaced whole, with provenance |
  | `collections` | Domain resources selected by the Release | target, through `${ … }` selectors | admitted data with provenance |

  A Release never writes into `values`, so a team file cannot override
  cluster facts. Per-Release constants are input defaults: two Releases that
  share a manifest template declare different defaults, and a target can
  still bind them.

### Manifests

`spec.manifests` lists the files that belong to the Release, in addition to
the other documents of the Release's own file.

- Each entry is a glob under the
  [local path rule](../nyl/book/src/content/docs/reference/configuration.md#local-paths):
  relative to the directory containing `nyl.toml`, with leading `..`
  allowed, or rooted at the Git worktree with a leading `/`. A match never
  leaves the worktree or traverses a symbolic link.
- Matches follow the Git visibility rule of discovery: tracked, or untracked
  but not ignored. Only `.yaml`, `.yml`, and `.json` files may match. Every
  entry must match at least one file other than the Release's own file.
- A matched file must contain no Release and no control resource. The rule
  `spec.include` already applies to included files carries over.
- Several Releases may list the same file. That is how one template serves
  several Releases, each with its own input defaults and bindings.
- Matched files render with the Release's context, as included files do
  today, and enter the dependency recorder and the ownership index under
  their recorded paths. A file listed by a selected Release is never a
  Release candidate of its own.
- **Deprecating `spec.include`.** `include` resolves relative to the
  Release's directory, may be templated, and walks the filesystem without the
  Git visibility rule. It keeps working, warns when read, and gets a row in
  `reference/deprecations.md`. The warning names the equivalent
  `spec.manifests` entries. The row says that ignored files no longer match,
  so an entry that matched only ignored files fails until the file is added
  to Git. A Release that sets both fails validation.

### Shared files

A Release may also live in a file next to other Releases, or next to control
resources such as a `gitops.yaml`:

- A file with exactly one Release and no control resource is a **Release
  file**. Its other documents belong to the Release, as today, and the whole
  file renders as one template.
- In any other file that holds a Release, a **shared file**, only Releases
  that declare `spec.manifests` take part. A Release without it is skipped
  with the warning `NYL-RELEASE-SHARED-WITHOUT-MANIFESTS`, which preserves
  today's behaviour for Releases in control files. A document that is
  neither a Release nor a control resource is an error
  (`NYL-RELEASE-SHARED-FILE-MANIFEST`), because it belongs to no Release.
- Each Release document of a shared file is cut out as text at document
  markers and rendered on its own, with the full Release context: `values`,
  `secrets` and `env` where the invocation admits them, `cluster`, `target`,
  `inputs`, and `collections`. A Jinja block or `{% set %}` therefore cannot
  span documents in a shared file.
- Path membership applies to the shared file's path, so every Release in it
  matches the same `include` globs. Selectors tell them apart.

## The Release catalog

The Release catalog lists every scanned Release of the Git worktree, read
from the same Git-visible file set that local ApplicationGroup sources select
from, not only beneath the `nyl.toml` directory:

- **Catalog key.** `<path>#<document>`. The path is recorded under the local
  path rule. The document is the position in the raw file, cut at document
  markers before anything renders, as for control resources. A template that
  expands to several documents does not shift it.
- **Deployed identity.** An ApplicationGroup that selects a Release deploys it
  as `<group>/<metadata.name>`. Two members of one group with the same name
  are an error naming both catalog keys, as today.
- **Remote sources.** A remote ApplicationGroup source builds the catalog of
  its own checkout in the restricted session, keyed by the source's
  repository and path.
- `nyl get releases` lists the catalog: key, name, namespace, labels, and,
  with `--target`, the groups that select each Release on that target.
  `nyl validate` reports a Release that no group selects on any target as a
  notice, not an error. Such a Release is often a template that is still
  being written.

## ApplicationGroup membership

```yaml
apiVersion: k8s.gitops.nyl/v1
kind: ApplicationGroup
metadata: {name: payments}
spec:
  source:
    path: /teams
    include: ['**/*.yaml']
    selector:
      matchLabels: {team: payments}
```

- The group's single `source` gains `selector`, a `matchLabels` selector over
  literal Release labels, the same shape as `applicationGroupSelector` and
  `unitSelector`. The default `{}` selects every Release.
- A Release is a member when its file matches the source's `path`,
  `include`, `exclude`, and `recursive`, **and** its labels match `selector`.
  This is the membership rule of [collections](domain-resources.md#stage-3-collection).
  One rule gives three modes:

  | Mode | Source | Who decides membership |
  | --- | --- | --- |
  | Path | a narrow `path`, no `selector` | Owners of the path, through CODEOWNERS |
  | Label | a wide `path`, such as `/`, with a `selector` | Anyone who can write a Release with those labels under the path |
  | Both | a narrow `path` with a `selector` | The path owners, narrowed by labels |

- **Trust.** Membership grants more than deployment. A group without
  `projectRef` generates an AppProject that admits its members' namespaces
  and every resource kind they render, cluster-scoped kinds included. With
  label membership, anyone who can commit a matching Release could widen
  that project, for example with a `ClusterRoleBinding` or a Release in
  `kube-system`. A source with a `selector` therefore requires one of these
  guards, checked by validation (`NYL-RELEASE-SELECTOR-UNGUARDED`):
  - an AppProject that derives nothing from its members: `projectRef`, or a
    `projectTemplate` that declares `destinations`,
    `clusterResourceWhitelist`, and `namespaceResourceWhitelist`;
  - or [Release admission](#release-admission), enabled by
    `[resources] policies`. Until R9 ships, only the first guard exists.

  Path membership needs neither guard, because CODEOWNERS reviews every
  member.

## Release admission

Kyverno validating policies from `[resources] policies` in `nyl.toml` admit
Releases and what they render. The table works without `definitions`.

- **What is evaluated.** Per target, after rendering, Nyl evaluates the
  Release and each of its rendered manifests. Policies therefore see what the
  Release would deploy, not only its envelope, which is how they bound
  cluster-scoped kinds as well as namespaces. Nyl supplies a CRD for
  `k8s.nyl/v1` `Release`, derived from its schema, as it derives one for each
  domain resource definition.
- **Effective namespace.** The evaluated Release carries its effective
  destination namespace in `metadata.namespace`, after
  `ApplicationGroup.spec.destinationNamespace`. Its declared namespace is in
  an annotation.
- **Trusted annotations.** Nyl adds them to the evaluated copies; Release
  metadata cannot carry annotations, and rendered manifests keep theirs
  except under `*.gitops.nyl/`, which are replaced:

  | Annotation | Value |
  | --- | --- |
  | `source.gitops.nyl/repository` | `local`, or the credential-free URL of a remote source |
  | `source.gitops.nyl/path`, `source.gitops.nyl/directory`, `source.gitops.nyl/document` | The Release's catalog key, split; for a remote source, relative to its checkout |
  | `release.gitops.nyl/name`, `release.gitops.nyl/declared-namespace` | The Release |
  | `target.gitops.nyl/name`, `applicationgroup.gitops.nyl/name` | Where it is rendered |

  A remote source controls its own paths, so a policy that trusts a path
  must also check the repository:

  ```yaml
  validations:
    - expression: >-
        (object.metadata.annotations['source.gitops.nyl/repository'] == 'local'
         && object.metadata.annotations['source.gitops.nyl/path'].startsWith('/platform/'))
        || !(object.kind in ['ClusterRole', 'ClusterRoleBinding'])
      message: Only the platform renders cluster-wide RBAC.
  ```

- A rejected Release or manifest fails the target's render, like any
  configuration error, and produces no partial tree (`NYL-RELEASE-ADMISSION`).
  The error names the policy, the Release's catalog key, the manifest, and
  the target.
- Policy files and the Kyverno CLI version are part of the render-cache key.

## Collections in Releases

```yaml
apiVersion: k8s.nyl/v1
kind: Release
metadata: {name: team-rbac, namespace: platform-rbac}
spec:
  manifests: [/platform/templates/team-rbac.yaml]
  collections:
    teams:
      type: {apiVersion: domain.acme.example/v1, kind: GitHubTeam}
      selector: {matchLabels: {stage: '${ values.stage }'}}
      paths: ['/teams/**']
```

```yaml
# /platform/templates/team-rbac.yaml
{% for name, team in collections.teams.items | items %}
{% set org = referenced['domain.acme.example/v1/GitHubOrganization'][team.spec.org] %}
apiVersion: rbac.authorization.k8s.io/v1
kind: RoleBinding
metadata:
  name: {{ name }}
  namespace: {{ team.labels.namespace }}
subjects:
  - kind: Group
    name: {{ (org.spec.login ~ ':' ~ name) | tojson }}
roleRef: {apiGroup: rbac.authorization.k8s.io, kind: ClusterRole, name: edit}
---
{% endfor %}
```

- **Declaration.** `spec.collections` has the shape of a unit's
  [`collections`](domain-resources.md#stage-3-collection): `type`,
  `selector`, `paths`, `access`, and `allowEmpty`. It is literal (see
  [the envelope](#the-release-envelope)). Selector values that vary per
  target are [template values](release-inputs.md#template-values), `${ … }`,
  expanded with the structural context: `values`, `cluster`, and `target`.
  Membership is therefore known before the Release renders.
- **Access.** Releases only read collections. `access` defaults to `read`,
  and `access: own` fails with `NYL-RELEASE-COLLECTION-OWN`. A rendered
  workload resource already has at most one Application owner per destination
  cluster, which catches two Releases that render the same object. `removal`
  and transition-rule approvals apply only to owning consumers (see
  [Ownership](domain-resources.md#ownership)).
- **Template variables.** `collections.<name>` holds the collection as the
  bundle does: `apiVersion`, `kind`, and `items` keyed by name, each with
  `labels` and the admitted, defaulted `spec`. `referenced` holds the
  [referenced resources](domain-resources.md#referenced-resources) keyed by
  `<apiVersion>/<kind>` and name. Both are visible wherever `inputs` is: the
  Release document, its own file, and its `manifests`. They reach Components,
  HelmCharts, and RemoteManifests only through explicit templating.
- **YAML-safe output.** Domain resources are written by teams, and the
  template by the platform. In a Release that declares `collections` or
  `forEach`, every `{{ … }}` that emits a string must emit only letters,
  digits, spaces, and `. _ - / @ + =`, or a value that `tojson` produced.
  Otherwise the render fails with `NYL-RELEASE-UNSAFE-OUTPUT`, naming the
  file and line. The check runs in the template engine's output formatter,
  so it holds whatever filters or concatenation built the string, and it
  applies to platform-authored strings in that Release as well. Such
  strings are written literally or through `tojson`.
- **Admission.** Members and referenced resources are admitted by schema,
  references, and policy before any Release reads them, so R10 follows R3. A
  failing resource fails the render of every target whose Release selects
  it, with the resource's error. A target's tree is all or nothing, unlike a
  unit graph, where only the consumers stop.
- **References in members.** A member field holding `fromUnit` or
  `fromPromotion` cannot resolve in `render-tree`, `diff-tree`,
  `publish-tree`, or the direct commands. They reject it with the binding
  rule's message, naming the member and the pointer. Orchestrated execution
  passes the resolved bundle in the pinned input snapshot (M5).
- **Remote sources.** A Release from a remote ApplicationGroup source that
  declares `collections` or `forEach` fails discovery
  (`NYL-RELEASE-COLLECTION-REMOTE`). Admitting collections across the remote
  boundary, as bindings are admitted today, is a later decision.
- **Provenance and caching.** Each collection's canonical bundle digest enters
  the dependency recorder under `<group>/<release>/<collection>`, and the
  ownership index's `inputs` map gains `@collection/<group>/<release>/<collection>`.
  Source paths are provenance, not key: moving a declaration without
  changing membership or admission keeps the cache and the published tree.
  The domain catalog and its admission depend on the Git-visible worktree
  contents, the policy files, and the Kyverno CLI version, and are computed
  once per invocation for every target.

## One Release per resource: `forEach`

```yaml
apiVersion: k8s.nyl/v1
kind: Release
metadata:
  name: tenant
  namespace: 'tenant-{{ resource.metadata.name }}'
  labels: {tier: tenants}
forEach:
  type: {apiVersion: domain.acme.example/v1, kind: Tenant}
  selector: {matchLabels: {hosting: shared}}
spec:
  manifests: [/platform/templates/tenant/*.yaml]
  inputs:
    quotaCpu: {type: string, default: '4'}
```

- `forEach` follows the unit rules of
  [`forEach`](domain-resources.md#unit-instances-per-resource-foreach): it is
  literal and read at discovery, the instance set is computed from the
  admitted domain catalog, and `resource` joins the context of the Release
  file and its manifests. The YAML-safe output rule above applies.
- **Names.** An instance is named `<release>-<resource name>`, such as
  `tenant-acme`, and deployed as `<group>/tenant-acme`. A clash with another
  member of the same group fails with `NYL-RELEASE-INSTANCE-NAME`, naming
  both.
- **Membership.** Groups select the base Release by its path and literal
  labels, and every instance comes with it. Instances carry the base labels.
- **Inputs.** A binding under `<group>/<release>` applies to every instance.
  A binding under `<group>/<instance>` replaces it for that instance, input by
  input. The effective value of an input is the instance binding, otherwise
  the base binding, otherwise the default. A base key is valid while the
  group selects the base Release, even with no instances, so removing the
  last tenant never breaks the target's bindings. An instance key that names
  no current instance is an error, as for any key that names no Release.
- **Keys.** Provenance uses the instance's deployed identity:
  `@input/<group>/<instance>/<input>` and
  `@collection/<group>/<instance>/<collection>`.
- **Lifecycle.** Removing or deselecting the resource removes its instance,
  and with it the workload Application, from the target's tree. The catalog
  Application already requires confirmation before it prunes an
  Application, so nothing is deleted without an explicit sync.
- A Release file with `forEach` renders once per instance, together with its
  other documents and its `manifests`.

## Direct commands

`nyl render`, `diff`, and `apply` take the Release from the compiled target
(`gitops::direct`) and must agree with `render-tree`. With the static fields
known before rendering, the order is the same for every entry point:

1. Scan the Release catalog, and the domain catalog when configured.
2. Resolve the Release's groups on the target by path **and** labels.
3. Compute membership, bundles, and `forEach` instances.
4. Render.

- A Release file argument works as today. In a shared file, or for a base
  Release with `forEach`, the command fails with the list of Releases or
  instances and asks for `--release <name>`, which also selects one
  instance (`--release tenant-acme`). `--application-group` still decides
  between groups.
- A file listed only in other Releases' `spec.manifests` is not a Release.
  The command fails and names the Releases that list it.
- Without `--target`, a `${ … }` selector has no `values` to expand and the
  command fails, naming the collection, as an unbound input does.

## Referenced resources

[Referenced resources](domain-resources.md#referenced-resources) are part of
the bundle for units and Releases alike. In short: the bundle carries every
resource reachable from its members through `x-nyl-reference` fields,
transitively and read-only, and their content is part of the bundle digest.
A Release may consume `GitHubTeam` declarations while each team references a
`GitHubOrganization`, and the template reads the organization without
collecting it.

## The `nyl-collections` crate

Collections are shared by rendering and orchestration, so their rules live in
one crate that both use:

```text
nyl-render ────────┬► nyl-collections ─► nyl-core
nyl-orchestration ─┘
```

| Home | Owns |
| --- | --- |
| `nyl-core` | The types: `ResourceDefinition`, the domain envelope, `collections`, `ResourceBundle`, its provenance, golden files; the reference resolver; the `RuleEvaluator` port for CEL rules |
| `nyl-collections` | Schema validation and defaulting (the JSON Schema dependency), identity and duplicates, membership and ordering, ownership claims, the reference closure, bundle assembly and its digest, removal detection, transition-rule decisions over results the `RuleEvaluator` returns |
| `nyl-render` | Discovery of the domain and Release catalogs, Kyverno admission, Release `collections` and `forEach` |
| `nyl-orchestration` | Resolving references inside a bundle and checking resolved values against their field schema, through `nyl-collections` |

`nyl-collections` is pure like `nyl-core`: no Git, processes, network, clock,
or environment. CEL evaluation sits behind the `RuleEvaluator` port, so the
open choice between a Rust CEL crate and the Kyverno CLI does not affect
purity. The dependency rule becomes: `nyl-orchestration` depends only on
`nyl-core` and `nyl-collections`. That keeps the compiler's guarantee
against effects in orchestration, while the JSON Schema dependency stays out
of `nyl-core`.

## Rollout

| Slice | Delivers | Needs |
| --- | --- | --- |
| R8 | Release labels and the static envelope with verification, `spec.manifests`, the `spec.include` deprecation, shared files, the Release catalog, `nyl get releases`, source `selector` with the AppProject guard, and `--release` for direct commands | M2 |
| R9 | Release admission over Releases and their rendered manifests, with trusted annotations, as the second guard for `selector` | R3, R8 |
| R10 | `nyl-collections`, Release `collections`, referenced resources, YAML-safe output, bundle provenance in the ownership index | R1, R3, R8 |
| R11 | `forEach` on Releases, instance names, instance bindings | R10 |

R8 is useful without domain resources. R10 builds the parts of R2 that need
no orchestration: membership, the bundle, and referenced resources move into
`nyl-collections` there, and R2 adds ownership claims, state files, and the
execution key on top.

## Open decisions

| Decision | Needed by |
| --- | --- |
| A template helper for referenced resources, such as `ref(type, name)`, instead of indexing `referenced` by type | R10 |
| `access: own` for Releases, with the ownership index as the baseline for removal approval and transition rules | After R10 |
| Admitting collections in remote sources, keyed by `<group>/<release>` like bindings | After R10 |
