# Release inputs and bindings

**Status:** M1 contract for M2, all M2 questions settled. See [ROADMAP.md](../ROADMAP.md).

This contract lets a Release declare typed inputs and a DeploymentTarget bind
them. Bindings in M2 resolve without orchestration: from inline values, from
project files, from locked Git state, or from state files in the target's own
publication branch, either committed by another tool or carried from the
working tree by `publish-tree`. The orchestration-only binding kinds
are reserved here so that M5 and M6 can add them without changing the schema
shape.

## Compatibility

- Nothing changes for a Release without `spec.inputs` or a DeploymentTarget
  without `spec.releaseInputs`: the template context, rendered bytes, cache
  keys, and ownership index are exactly as before.
- The `inputs` template variable exists only while rendering a Release that
  declares inputs.
- The ownership index keeps format version 2. Input provenance is added as new
  keys in its existing `inputs` map.

## Declarations

```yaml
apiVersion: k8s.gitops.nyl/v1
kind: Release
metadata:
  name: web
  namespace: web
spec:
  inputs:
    image:
      type: string
      description: Immutable image reference
    replicas:
      type: integer
      default: 2
    tier:
      type: string
      enum: [small, large]
      default: small
    database:
      type: object
      description: Connection facts for the application database
```

| Field | Meaning |
| --- | --- |
| `type` | Required. One of `string`, `integer`, `number`, `boolean`, `object`, `array` |
| `description` | Optional documentation, surfaced in errors and inspection |
| `default` | Optional. Makes the input optional; must satisfy `type` and `enum` |
| `enum` | Optional, scalar types only. A non-empty list of allowed values of that type |

Type rules:

- `integer` accepts JSON integers only. `number` accepts any JSON number.
- `null` satisfies no type. A missing input is expressed by a `default`, never
  by `null`.
- `object` and `array` check only the top-level JSON type. Deeper structure is
  checked by downstream validators, such as a chart's `values.schema.json`.
  The fields `properties` and `items` are reserved for a later, additive
  extension.
- Every type is a subset of JSON Schema. A later `schema` field can extend a
  declaration without changing the meaning of existing ones.

Static-declaration rules:

- A Release that declares inputs must have a literal `metadata.name` and a
  literal, parseable `spec.inputs` block. The source file must parse before
  rendering, which is already the condition for ApplicationGroup Release
  discovery.
- Input names match `^[a-z][a-zA-Z0-9]*$` so templates can use
  `inputs.<name>` directly.
- After rendering, the rendered Release's `spec.inputs` must equal the static
  declaration. Templating cannot add, remove, or change declarations.

## Template visibility

- `inputs` is a top-level template variable, alongside `values`, `secrets`,
  `env`, `cluster`, and `target`.
- It is visible in the Release entry file and in every file attached through
  `spec.include`, because they form one deployment unit.
- It reaches Components, HelmCharts, and RemoteManifests only through explicit
  templating in the Release bundle, the same rule that applies to target values
  today.
- `target.spec.releaseInputs` is removed from the `target` template context.
  Templates see only the resolved, declared inputs of their own Release.

## Template values

A templated control resource is rendered as a whole before its fields are
used. Some fields hold a template of their own that is expanded later, in a
narrower context the structural pass does not have, such as
`ApplicationGroup.spec.applicationNameTemplate`, which is expanded once per
Release with `release` in scope. Such **template values** use `${ … }`:

```yaml
spec:
  applicationNameTemplate: '${ target.metadata.name }-${ release.metadata.name }'
```

- `${` is not template syntax in the structural pass, so the value reaches the
  field untouched. The field then expands `${ expression }` with its own
  context: the structural context plus what the field adds, such as
  `release`. Expressions and filters work as in `{{ … }}`; blocks do not.
- Each field that takes a template value says so in its schema description,
  together with the extra variables it provides. Units follow the same rule
  when they add such fields.
- `$${` writes a literal `${`.
- For compatibility, a template value that still contains `{{ … }}` after the
  structural pass, because it was protected with `{% raw %}`, is expanded as
  before. A value that mixes both forms is an error. The validation hint for
  colliding Application names suggests the `${ … }` form.

## Bindings

Bindings live on the DeploymentTarget. The target is static at discovery, and
it is where an environment's Kubernetes slice differs from another's.

```yaml
apiVersion: k8s.gitops.nyl/v1
kind: DeploymentTarget
metadata:
  name: staging
spec:
  clusterRef: {name: shared}
  publication: {…}
  releaseInputs:
    platform/web:                    # <ApplicationGroup>/<Release>
      image:
        value: registry.example.com/web@sha256:4f0c…
      database:
        fromFile:
          path: environments/staging/database.yaml
      tier:
        fromGit:
          repositoryRef: {name: platform-state}
          revision: main
          commit: 3f1c9a…            # 40-hex lock
          path: staging/sizing.json
          pointer: /web/tier
```

Keys are `<applicationGroup>/<release>`, the Release's rendered identity on the
target, because one Release name can appear in several groups.

| Condition | Result |
| --- | --- |
| Key names a Release that the target renders | Bindings apply |
| Key names a Release of a selected but disabled group | Ignored, so `enabled` can still toggle a group |
| Key names no Release, or a Release that declares no inputs | Error |
| Binding names an input the Release does not declare | Error |
| Required input without a binding | Error naming the target, key, input, and description |

A value is resolved as follows:

```text
effective input = target binding, if present
                  otherwise Release default
```

A binding replaces the value whole. Unlike `values`, inputs are not deep-merged,
so every effective value comes from exactly one traceable source.

### Binding kinds

Each binding sets exactly one of these fields:

| Field | M2 | Resolves from |
| --- | --- | --- |
| `value` | Yes | Inline literal. DeploymentTargets are static, so this is never templated; the inline targets of orchestration's `KubernetesPublication` units are the one exception, rendered per environment |
| `fromFile` | Yes | Project-relative YAML or JSON file; `pointer` defaults to `""` |
| `fromGit` | Yes | A file at a locked commit; `pointer` defaults to `""` |
| `fromPublication` | Yes | A file in the target's publication branch at the publication base commit; `pointer` defaults to `""` |
| `fromUnit` | Reserved | A recorded unit output or artifact field in an orchestrated environment (M5) |
| `fromPromotion` | Reserved | A value recorded through a PromotionPath (M6) |

`fromFile`:

- The path is normalized and project-relative. Absolute paths, parent traversal,
  and symlinks that leave the project are rejected.
- The file is a rendering input like any other: it enters the dependency
  recorder and the ownership index under its path.
- A file containing several YAML documents is rejected.

`fromGit`:

- Selects a repository with `repositoryRef` (a `gitops.nyl/v1` GitRepository)
  or inline `repository`, the same pair ApplicationGroup sources accept.
- `revision` is the human-readable ref. `commit` is the 40-hex lock that
  rendering uses. Rendering never resolves `revision`.
- The blob is read at `commit:path`. A commit missing from the local cache is
  fetched by commit. Offline rendering fails with an actionable message when the
  commit is not cached.
- `nyl update source-locks` refreshes `fromGit` locks together with
  ApplicationGroup source locks, so CI has one `--check` gate for every Git
  lock. A new `--target` filter selects one DeploymentTarget, alongside the
  existing `--group` filter.
- One target can hold many `fromGit` locks, and several bindings often lock the
  same repository at the same commit. The updater therefore groups locks by
  repository and `revision`: it resolves each group once and moves every lock
  in it to the same new commit. Bindings that share a current commit but name
  different revisions are addressed individually by their position in the
  document, never by matching the commit text alone.
- `--require healthy` follows the per-value promotion rule in the roadmap's
  health evidence section, so locks of one group may move to different
  commits. Each lock is one value:
  - The file it reads must lie inside one source target's prefix on that
    branch. A lock that reads a file outside every target prefix is an error
    that suggests dropping `--require healthy` for it.
  - Its covered Applications are the source target's Releases whose bindings
    read the same file and pointer.
  - The lock moves to a publication commit of the source target at which the
    value equals the value each covered Application runs; among equivalent
    commits it names the oldest, per the recorded-commit rule. If covered
    Applications run different values of the same file and pointer, the lock
    does not move and the updater reports the conflict.
  - Without `--require healthy`, a group moves to one new commit as described
    above. When the locked file lies inside a target's prefix on that branch,
    that commit is the target's newest publication commit, never a later
    commit made by another tool, such as a state file write-back the target
    has not yet published. Otherwise it is the branch head.
- With `--require healthy`, the updater writes the observation that justified
  the move next to each lock, so the pull request commits the evidence together
  with the lock:

  ```yaml
  fromGit:
    revision: deploy/dev
    commit: 9c1e…
    path: dev/state/images.json
    observed:                     # written by source-locks, never authored
      at: 2026-09-24T10:15:00Z
      applications:
        - name: web
          revision: 4b7d…         # last successful Argo CD sync
          health: Healthy
  ```

  - `observed` is a tool-written `fromGit` field in the binding schema; authors
    never write it. It contributes neither to the `@input` digest nor to the
    render-cache key, so it never changes rendered output. Like any edit, it
    changes the DeploymentTarget file and therefore its source provenance.
  - A lock with an `observed` block is health-gated: it deliberately lags the
    branch head. Plain `--check` does not compare it with the head; it verifies
    only that the observed publication commit equals `commit`, and needs no
    cluster access.
  - `--check --require healthy` takes a fresh observation, which needs Argo CD
    credentials, and reports a lock as stale when a newer publication is
    running and healthy, or when a lock without `observed` would move.

`fromPublication`:

This binding covers write-back workflows: a tool outside Nyl, such as an image
build job, commits a state file to the deploy branch, and Nyl renders manifests
from it into the same branch.

```yaml
releaseInputs:
  platform/web:
    image:
      fromPublication:
        path: state/web.json
        pointer: /image
```

- The path is relative to the target's publication path prefix, the same root
  the ownership index uses for its file entries. It is normalized; absolute
  paths, parent traversal, paths outside the prefix, and symlinks are rejected.
- **Base commit.** `publish-tree` already checks out the publication branch
  head B, commits the rendered tree on top of B, and pushes with a
  compare-and-swap that expects B. The state file is read at B. The published
  commit therefore contains the state file and the manifests rendered from it,
  and a concurrent writer makes the push fail instead of interleaving.
- **Provenance.** The ownership index records the state file's blob digest at
  B. Source commit S plus the state at B reproduce the render.
- **Idempotence.** Rendering the same state again produces an unchanged tree,
  and `publish-tree` creates no commit for an unchanged tree. A CI job
  triggered by pushes to the deploy branch therefore stops after Nyl's own
  publication.
- **Placement.** Without `carry`, the path must not be a file owned by this
  target; with `carry`, this target owns it. Either way it lies inside the
  target's prefix but outside every directory synced by a generated Argo CD
  Application: workload Release directories, `_nyl`, and the catalog.
  Otherwise Argo CD would try to apply the state file as a manifest. Nyl
  validates both. Reconciliation already preserves files it does not own.
- **Bootstrap.** When the publication branch or the file does not exist, the
  input is treated as unbound: its Release default applies, or rendering fails
  as for any required input. A file that exists but does not resolve `pointer`
  is an error.
- **Local commands.** `render-tree` and `diff-tree` fetch the publication
  branch and read the file at its current head. They report which commit they
  used, because their output is reproducible only together with it. `--offline`
  uses the cached head and says so.
- **Scope.** Only the target's own prefix on its own publication branch can be
  read. Another target's prefix or branch, or another repository, uses
  `fromGit`.
- **Review.** State changes arrive without a source-repository review. Use
  `fromGit` locks or promotion where review is required.
- **Concurrent writers.** Other writers must also push with a compare-and-swap.
  Nyl preserves unowned files but cannot merge a concurrent state change into
  its own commit.

**Carried state.** A state file does not have to be committed by another tool.
With `carry`, a file produced in the working tree during this run, and left
uncommitted, is written by `publish-tree` into the same commit as the manifests
derived from it:

```yaml
releaseInputs:
  platform/web:
    image:
      fromPublication:
        path: state/web.json       # location in the publication branch
        pointer: /image
        carry: build/web.json      # optional working-tree file from this run
```

- **File present.** Nyl reads `carry`, renders from it, and writes its bytes to
  `path` in the compare-and-swap publication commit. Every published commit
  thereby contains the input its manifests were rendered from.
- **File absent.** Nyl reads `path` at the base commit B and writes the same
  bytes back. The last carried value persists across source-only publications,
  and unchanged bytes produce no commit. When neither exists, the bootstrap
  rule applies.
- **Ownership.** With `carry`, `path` is a file owned by this target and listed
  in the ownership index, so a commit to it by another writer is rejected as a
  modification outside Nyl. A path is either carried by Nyl or committed by
  another tool, never both. The placement rule otherwise applies unchanged:
  the path lies outside every Argo CD-synced directory.
- **Working-tree rules.** `carry` is a normalized project-relative path that must
  not be tracked by Git; a tracked file is source and uses `fromFile`. It
  should normally be ignored through `.gitignore`. Declared `carry` paths are
  excluded from the source dirty check, and the clean-`HEAD` verification
  render receives the same carried bytes, so a carried file never forces
  `--allow-dirty`.
- **Local commands.** `render-tree`, `diff-tree`, and direct commands use the
  carried file when present, otherwise the base copy. In a pull-request build,
  `diff-tree` therefore shows the state change with the manifest changes it
  causes.
- **Trust.** Whoever controls the working tree of the publishing run controls
  the carried value. That is the same trust already given to the job that
  produced it, such as an image build.

**Promoting published state.** A carried or committed state file in one
target's publication branch can feed another target:

- Without orchestration, the other target binds it with `fromGit`, locked to a
  source publication commit. `nyl update source-locks --target <name>` promotes
  by moving the lock, and the pull request that commits it is the review. The
  targets may share a publication branch under different prefixes or publish to
  different branches; the lock makes promotion explicit either way. A shared
  branch cannot be followed with `fromPublication`, because the state path lies
  in the source target's prefix.
- `nyl update source-locks --target <name> --require healthy` moves a lock group
  only to a source publication that is running and healthy, as described in
  the roadmap's health evidence section, and writes the observation next to
  the lock (see `fromGit`).
- With orchestration, a PromotionPath with `from: {target: <name>}` selects the
  source target's published inputs; see the roadmap's promotion section.

Example: dev carries image IDs from its CI build, and production promotes them
by lock.

```yaml
# DeploymentTarget dev, publication prefix `dev`
releaseInputs:
  platform/web:
    image:
      fromPublication:
        path: state/images.json    # relative to the prefix: dev/state/images.json
        pointer: /web
        carry: build/images.json
---
# DeploymentTarget production
releaseInputs:
  platform/web:
    image:
      fromGit:
        repository: {repoURL: https://git.example.com/deploy.git}
        revision: deploy/dev
        commit: 9c1e…              # moved by `nyl update source-locks --target production`
        path: dev/state/images.json  # fromGit paths are repository-relative
        pointer: /web
```

`fromUnit` and `fromPromotion`:

- Their shapes are defined with the orchestration contract.
- `render-tree`, `publish-tree`, `diff-tree`, and direct commands reject them
  with a message naming the binding and saying it needs orchestrated
  execution.
- Orchestrated execution passes their resolved values to rendering as an
  explicit, pinned input snapshot.

## Direct commands

`nyl render`, `diff`, and `apply` resolve inputs so that `render --target dev`
matches what `render-tree` produces for the same Release:

```text
effective input = --input / --inputs override, if present
                  otherwise target binding, if a target is selected
                  otherwise Release default
```

- With `--target`, Nyl finds the target's selected ApplicationGroup whose source
  contains the Release file and applies that `<group>/<release>` binding. When
  two selected groups contain the file, `--application-group` chooses. When
  none does, which is always the case for a Release of a remote group rendered
  from a local checkout, the command fails and names the target's groups:
  `--application-group` names the group whose bindings apply, or
  `--defaults-only` deliberately renders with defaults and overrides only. A
  target never silently falls back to defaults.
- Without a target, only defaults and overrides apply.
- `--input <name>=<json>` sets one input; `--inputs <file>` reads a YAML or JSON
  object of inputs. Individual `--input` flags win over `--inputs`. Overrides
  are validated like bindings and recorded as provenance.
- `fromGit` resolves at its lock. `fromPublication` fetches the publication
  branch, reads its head (or the carried file when present), and reports the
  commit it used.
- Tree commands (`render-tree`, `diff-tree`, `publish-tree`) accept no
  overrides. Published output always reproduces from committed source, the
  recorded publication base commit, and carried files, which are part of the
  published commit.

## Remote ApplicationGroup sources

A remote source renders in a restricted session without secrets or the process
environment. It already receives the target's `values`, and it receives inputs
the same way as a local group: no opt-in field exists.

- The binding key is the admission. Values reach a remote Release only because
  the platform names that `<group>/<release>` and input on its own target.
- `fromFile`, `fromGit`, and `fromPublication` resolve centrally. The remote
  session receives only the resolved values for its own Releases, never binding
  definitions, file paths, or repository credentials.
- Remote code can declare inputs but cannot choose where their values come
  from, and it still receives no secrets.

## Provenance, caching, and validation

- Each resolved input is recorded in the dependency recorder as canonical JSON
  keyed by `<group>/<release>/<input>`. It is part of the render-cache key.
- The ownership index's `inputs` map gains new keys, with bare hex SHA-256
  digests like its existing entries:
  - `@input/<group>/<release>/<input>` → digest of the canonical JSON value
  - `@git/<credential-free-url>@<commit>/<path>` → blob digest for each
    `fromGit` source
  - `@publication/<path>` → blob digest for each `fromPublication` value read
    from the base commit, which is the published commit's parent
  - `@carried/<path>` → blob digest for each `fromPublication` value taken
    from a `carry` file in this run
  - Both publication forms use the prefix-relative path, like the index's
    `files` entries.
- Today only remote source files use an `@`-prefixed key (`@remote/<path>`).
  M2 reserves every key starting with `@` for entries that are not project
  paths, so project-file hashing never interprets them. They are new entries of
  the existing `inputs` map, which readers already accept as arbitrary keys, so
  the index keeps format version 2 and projects without inputs publish
  byte-identical indexes.
- Inputs are not a secret channel. Their digests and the rendered manifests are
  published. Secrets continue to flow through the secrets provider, and a
  `sensitive` input flag is out of scope for M2.
- Input validation runs before rendering the Release. Its failures are
  configuration errors, reported together per target, and do not produce
  partial trees.

## Environments and DeploymentTargets

A DeploymentTarget is already not a cluster. Several targets may reference one
Cluster with different values, ApplicationGroup selections, and publication
prefixes. Dev, staging, and production on one shared cluster are three targets
on one Cluster, and each target binds its own inputs. M2 needs no environment
concept.

For orchestration, an Environment is a larger and different boundary: it holds
image builds, Terraform configurations, and zero or more Kubernetes targets. A
production environment spanning two clusters has two targets.

| Relationship | Cardinality |
| --- | --- |
| Cluster → DeploymentTarget | One Cluster, many targets |
| Environment → DeploymentTarget | One environment, zero or more targets |
| DeploymentTarget → Environment | At most one, so its orchestrated bindings resolve unambiguously |

A target joins an environment when a Kubernetes publication unit in that
environment references it. The unit also carries the execution mode (publish
or observe), and DeploymentTarget itself stays unchanged. A target
referenced by no publication unit belongs to no environment and rejects
`fromUnit` and `fromPromotion` bindings; a target referenced by publication
units in two environments is an error.

Environments that share a cluster add risks beyond the current per-target
checks:

- Namespace collisions between environments.
- Cluster-scoped resources, such as CRDs, that only one environment may own.
- Argo CD names generated into one control-plane namespace. Today's check
  compares only targets sharing an explicit ArgoCDInstance, so targets on
  implicit per-target instances of one Cluster can generate the same
  Application and AppProject names in the same namespace. M2 extends the check
  to every pair of targets whose instances resolve to the same cluster and
  namespace.

Detecting cross-target conflicts on a shared Cluster is a later validation, and
it belongs to the layer that sees all targets on that Cluster.

## Remaining questions

None for M2. Orchestration binding shapes (`fromUnit`, `fromPromotion`) are
settled with the M1 unit and promotion contract.
