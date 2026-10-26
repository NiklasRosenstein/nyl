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
  Releases, the same way they admit domain resources.
- **Collections.** A Release can consume domain resource collections, and
  the resources they reference, as template data.
- **One Release per resource.** A Release can be instantiated once per domain
  resource with `forEach`.

The rendering layer and orchestration share one implementation of
collections, in the `nyl-collections` crate.

## Compatibility

- A Release without `metadata.labels`, `spec.manifests`, `spec.collections`,
  or `forEach`, and an ApplicationGroup source without `selector`, render
  exactly as before: the same bytes, cache keys, and ownership index.
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
  manifests: [/templates/web-app.yaml, deploy/extra/*.yaml]
  inputs:
    replicas: {type: integer, default: 3}
```

- **`metadata.labels`** are new. They are literal, because selection happens
  before rendering. A Release whose labels contain template syntax fails
  discovery (`NYL-RELEASE-TEMPLATED-LABELS`), naming the file and document.
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
  their recorded paths.
- `spec.include` resolves relative to the Release's directory, not under the
  local path rule. It keeps working, warns when read, and gets a row in
  `reference/deprecations.md`. The warning names the equivalent
  `spec.manifests` entries. A Release that sets both fails validation.

### Shared files

A Release may also live in a file next to other Releases, or next to control
resources such as a `gitops.yaml`:

- A file with exactly one Release and no control resource is a **Release
  file**. Its other documents belong to the Release, as today.
- In any other file that holds a Release, a **shared file**, every Release
  must declare `spec.manifests` (`NYL-RELEASE-NO-MANIFESTS`). A document that
  is neither a Release nor a control resource is an error
  (`NYL-RELEASE-SHARED-FILE-MANIFEST`), because it belongs to no Release.
- Each Release document of a shared file renders on its own, cut out as text
  at document markers, as control resources are. A Jinja block or `{% set %}`
  therefore cannot span documents in a shared file. In a Release file the
  whole file still renders as one template.
- Path-based membership applies to the shared file's path, so every Release
  in it matches the same `include` globs. Selectors tell them apart.

## The catalog

Discovery already scans every Git-visible file of the project for control
resources. The same pass records each literal Release envelope in the
**Release catalog**:

- **Catalog key.** `<path>#<document>`: the recorded path of the file and the
  document's position, as provenance records it. Files may move, and names
  may repeat across files, so the catalog does not key by name.
- **Deployed identity.** An ApplicationGroup that selects a Release deploys it
  as `<group>/<metadata.name>`. Two members of one group with the same name
  are an error naming both catalog keys, as today.
- **Remote sources.** A remote ApplicationGroup source has its own catalog of
  its checkout, built in the restricted session.
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
  sources:
    - path: /teams/payments          # paths: CODEOWNERS of /teams/payments
    - path: /
      include: ['**/*.yaml']
      selector:                      # labels, anywhere in the project
        matchLabels: {team: payments}
```

- A source gains `selector`, a `matchLabels` selector over literal Release
  labels, the same shape as `applicationGroupSelector` and `unitSelector`.
  The default `{}` selects every Release.
- A Release is a member when its file matches the source's `path`,
  `include`, `exclude`, and `recursive`, **and** its labels match `selector`.
  This is the membership rule of [collections](domain-resources.md#stage-3-collection),
  and it gives three modes with one rule:

  | Mode | Configuration | Who decides membership |
  | --- | --- | --- |
  | Path | `path` and `include`, no `selector` | Owners of the paths, through CODEOWNERS |
  | Label | a wide `path` and a `selector` | Anyone who can write a Release with those labels |
  | Both | a narrow `path` and a `selector` | The path owners, narrowed by labels |

- **Trust.** Membership grants more than deployment. A group without
  `projectRef` generates an AppProject that admits the namespaces of its
  members. With label membership, anyone who can commit a matching Release
  can therefore widen that project, for example to `kube-system`. Path
  membership with CODEOWNERS prevents that by review, and
  [Release admission](#release-admission) prevents it by policy. The
  ApplicationGroup guide states this next to the `selector` field.

## Release admission

Kyverno validating policies from `[resources] policies` in `nyl.toml` also
admit Releases. The table works without `definitions`.

- Policies match `k8s.nyl/v1` `Release`. Nyl supplies its CRD, derived from
  the Release schema, as it derives one for each domain resource definition.
- They run per target on the **rendered** Release, because its namespace and
  `additionalNamespaces` may be templated. Nyl evaluates a copy with trusted
  annotations, which Release metadata cannot carry:
  `source.gitops.nyl/path`, `source.gitops.nyl/directory`,
  `source.gitops.nyl/document`, `target.gitops.nyl/name`, and
  `applicationgroup.gitops.nyl/name`.

  ```yaml
  validations:
    - expression: >-
        object.metadata.annotations['source.gitops.nyl/path'].startsWith('/platform/')
        || !(object.metadata.namespace in ['kube-system', 'argocd'])
      message: Only the platform deploys into system namespaces.
  ```

- A rejected Release fails the target's render, like any configuration
  error, and produces no partial tree (`NYL-RELEASE-ADMISSION`). The error
  names the policy, the Release's catalog key, and the target.
- The Kyverno CLI version is part of the render-cache key, as it already is
  for rendering.

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
  name: {{ name | tojson }}
  namespace: {{ team.labels.namespace | tojson }}
subjects:
  - kind: Group
    name: {{ (org.spec.login ~ ':' ~ name) | tojson }}
roleRef: {apiGroup: rbac.authorization.k8s.io, kind: ClusterRole, name: edit}
---
{% endfor %}
```

- **Declaration.** `spec.collections` has the shape of a unit's
  [`collections`](domain-resources.md#stage-3-collection): `type`,
  `selector`, `paths`, `access`, and `allowEmpty`. It is literal and needs a
  literal `metadata.name`, like `spec.inputs`, so membership is known before
  rendering. Selector values that vary per target are
  [template values](release-inputs.md#template-values), `${ … }`, expanded
  with the structural context: `values`, `cluster`, and `target`.
- **Access.** Releases only read collections. `access` defaults to `read`,
  and `access: own` fails with `NYL-RELEASE-COLLECTION-OWN`. A rendered
  workload resource already has at most one Application owner per destination
  cluster, which catches two Releases that render the same object. `removal`
  and transition-rule approvals apply only to owning consumers (see
  [Ownership](domain-resources.md#ownership)), so they do not apply here.
- **Template variables.** `collections.<name>` holds the collection as the
  bundle does: `apiVersion`, `kind`, and `items` keyed by name, each with
  `labels` and the admitted, defaulted `spec`. `referenced` holds the
  [referenced resources](domain-resources.md#referenced-resources) keyed by
  `<apiVersion>/<kind>` and name. Both are visible wherever `inputs` is: the
  Release document, its own file, and its `manifests`. They reach Components,
  HelmCharts, and RemoteManifests only through explicit templating.
- **YAML-safe printing.** Domain resources are written by teams; the Release
  template is written by the platform. A string from `collections` or
  `referenced` printed with `{{ … }}` follows the rule of
  [`forEach`](domain-resources.md#unit-instances-per-resource-foreach): only
  letters, digits, spaces, and `. _ - / @ + =`, otherwise the render fails
  with `NYL-RELEASE-RESOURCE-UNSAFE-VALUE`. Values are inserted whole with
  `| tojson`. The rule is enforced on the values themselves, not by
  inspecting the template, so it also holds after filters and string
  concatenation.
- **Admission failures.** A member or referenced resource that fails
  admission fails the render of every target whose Release selects it, with
  the resource's error. A target's tree is all or nothing, unlike a unit
  graph, where only the consumers stop.
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
  Source paths are provenance, not key. Moving a declaration without
  changing membership or admission keeps the cache and the published tree.
  Catalog discovery and admission run only when `nyl.toml` has a
  `[resources]` table, and their result depends only on the source commit.
- **Direct commands.** `nyl render`, `diff`, and `apply` build the same
  bundle. Without `--target`, a `${ … }` selector has no `values` to expand
  and the command fails, naming the collection, as an unbound input does.

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
  literal and read at discovery; the instance set is computed from the
  admitted catalog; `resource` joins the context of the Release file and its
  manifests; and the same YAML-safe printing rule applies. Static discovery
  scans, expands, renders, and verifies as described there.
- **Names.** An instance is named `<release>-<resource name>`, such as
  `tenant-acme`, and deployed as `<group>/tenant-acme`. A clash with another
  member of the same group fails with `NYL-RELEASE-INSTANCE-NAME`, naming
  both.
- **Membership.** Groups select the base Release by its path and literal
  labels, and every instance comes with it. Instances carry the base labels.
- **Inputs.** A binding under `<group>/<release>` applies to every instance.
  A binding under `<group>/<instance>` replaces it for that instance, input by
  input. The effective value of an input is the instance binding, otherwise
  the base binding, otherwise the default.
- **Lifecycle.** Removing or deselecting the resource removes its instance,
  and with it the workload Application, from the target's catalog. The
  catalog Application already requires confirmation before it prunes an
  Application, so nothing is deleted without an explicit sync.
- A Release file with `forEach` renders once per instance, together with its
  other documents and its `manifests`.

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
| `nyl-core` | The types: `ResourceDefinition`, the domain envelope, `collections`, `ResourceBundle`, its provenance, golden files; the reference resolver |
| `nyl-collections` | Schema validation and defaulting (the JSON Schema dependency), identity and duplicates, membership and ordering, ownership claims, the reference closure, bundle assembly and its digest, removal detection, transition rules |
| `nyl-render` | Discovery of definitions, domain resources, and the Release catalog; Kyverno admission; Release `collections` and `forEach` |
| `nyl-orchestration` | Resolving references inside a bundle and checking resolved values against their field schema, through `nyl-collections` |

`nyl-collections` is pure like `nyl-core`: no Git, processes, network, clock,
or environment. The dependency rule becomes: `nyl-orchestration` depends only
on `nyl-core` and `nyl-collections`. That keeps the compiler's guarantee
against effects in orchestration, while the JSON Schema dependency stays out
of `nyl-core`.

## Rollout

| Slice | Delivers | Needs |
| --- | --- | --- |
| R8 | Release labels, `spec.manifests`, the `spec.include` deprecation, shared files, the catalog, `nyl get releases`, and source `selector` | M2 |
| R9 | Release admission with trusted annotations | R3, R8 |
| R10 | `nyl-collections`, Release `collections`, referenced resources, YAML-safe printing, bundle provenance in the ownership index | R1, R8 |
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
| How direct commands address one `forEach` instance | R11 |
