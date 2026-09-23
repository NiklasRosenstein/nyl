# Release inputs and bindings

**Status:** draft M1 contract, implemented by M2. See [ROADMAP.md](../ROADMAP.md).

This contract lets a Release declare typed inputs and a DeploymentTarget bind
them. Bindings in M2 resolve without orchestration: from inline values, from
project files, or from locked Git state. The orchestration-only binding kinds
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
| `value` | Yes | Inline literal. DeploymentTargets are static, so this is never templated |
| `fromFile` | Yes | Project-relative YAML or JSON file; `pointer` defaults to `""` |
| `fromGit` | Yes | A file at a locked commit; `pointer` defaults to `""` |
| `fromUnit` | Reserved | Recorded unit output in an orchestrated environment (M5) |
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
- Locks are refreshed by a path-addressed update, because one target can hold
  many `fromGit` bindings. The update's `--check` mode reports stale locks
  without writing files, for CI. Whether this extends
  `nyl update source-locks` or is a new `nyl update input-locks` is still open.

`fromUnit` and `fromPromotion`:

- Their shapes are defined with the orchestration contract.
- `render-tree`, `publish-tree`, `diff-tree`, and direct commands reject them
  with a message naming the binding and saying it needs orchestrated
  execution.
- Orchestrated execution passes their resolved values to rendering as an
  explicit, pinned input snapshot.

## Direct commands

`nyl render`, `diff`, and `apply` (proposal):

- With `--target`, bindings apply when the Release file belongs to exactly one
  of the target's selected ApplicationGroups. When it belongs to more than one,
  `--application-group` chooses.
- Without a target, only defaults apply.
- `--input <name>=<json>` and `--inputs <file>` override bindings, for local
  experimentation. The overrides are recorded as provenance.
- Tree commands (`render-tree`, `diff-tree`, `publish-tree`) accept no input
  overrides, so published output always reproduces from committed source.

## Remote ApplicationGroup sources

A remote source renders in a restricted session without secrets or the process
environment. Centrally bound inputs pass data from the platform project into
remote templates.

Proposal: a remote group rejects input bindings unless its source opts in, for
example with `rendererConfig.admitInputs: true`. `fromFile` and `fromGit` still
resolve centrally, and the remote session receives only the resolved values of
its own Releases.

## Provenance, caching, and validation

- Each resolved input is recorded in the dependency recorder as canonical JSON
  keyed by `<group>/<release>/<input>`. It is part of the render-cache key.
- The ownership index's `inputs` map gains:
  - `@input/<group>/<release>/<input>` → `sha256:<digest of canonical JSON>`
  - `@git/<credential-free-url>@<commit>/<path>` → `sha256:<blob digest>` for each
    `fromGit` source
- Keys starting with `@` are not project paths, following the existing `@remote`
  convention, so project-file hashing never interprets them.
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
only, observe, or apply), and DeploymentTarget itself stays unchanged. A target
referenced by no publication unit belongs to no environment and rejects
`fromUnit` and `fromPromotion` bindings; a target referenced by publication
units in two environments is an error.

Environments that share a cluster add risks beyond the current per-target
checks:

- Namespace collisions between environments.
- Cluster-scoped resources, such as CRDs, that only one environment may own.
- Argo CD names generated into one control-plane namespace. These are already
  required to be target-qualified.

Detecting cross-target conflicts on a shared Cluster is a later validation, and
it belongs to the layer that sees all targets on that Cluster.

## Remaining questions

| Question | Needed by |
| --- | --- |
| Lock update command: extend `source-locks` or a new `input-locks` | M2 |
| Remote group admission field name and default | M2 |
| Direct-command flag names and override precedence | M2 |
