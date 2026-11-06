---
title: 'Using KubernetesPublication'
---

A `KubernetesPublication` unit renders one [DeploymentTarget](/nyl/kubernetes-gitops/resource-guides/deployment-target/)
and publishes its tree to the target's Git branch, as
[`publish-tree`](/nyl/reference/cli/publish-tree/) does, with the values other
units produced. Orchestration runs it after the units its target's
`releaseInputs` read, such as the image build whose digest a Release deploys
and the infrastructure whose outputs it connects to.

```yaml
apiVersion: units.gitops.nyl/v1
kind: KubernetesPublication
metadata:
  name: kubernetes
  labels: {stage: dev}
spec:
  target: '{{ values.target }}'
  mode: publish
  teardownWait: {strategy: manual}
```

`target` names a static DeploymentTarget, or declares one inline (see
[Inline targets](#inline-targets)). The unit places that target into its
environment: a target belongs to at most one environment, and a target no
publication unit names rejects `fromUnit` bindings. `mode` is `publish`, the
default. `mode: observe` is not available yet; the unit rejects it with
`NYL-KUBERNETES-MODE-UNSUPPORTED`, because nothing would check what the
publication promises. The common unit fields (`dependsOn`, `timeout`, `env`,
and so on) apply; `timeout` defaults to `10m`.

## Inline targets

A unit can declare its DeploymentTarget in `spec.target.inline`, as a
DeploymentTarget `spec` rendered with the unit for each environment, so one
definition publishes a target of its own in every environment that selects it:

```yaml
apiVersion: units.gitops.nyl/v1
kind: KubernetesPublication
metadata:
  name: kubernetes
  labels: {stage: preview}
spec:
  target:
    inline:
      clusterRef: {name: preview-cluster}
      applicationGroupSelector: {matchLabels: {app: web}}
      publication:
        repositoryRef: {name: deploy}
        revision: previews
        pathPrefix: '{{ environment.name }}'
      values: {hostname: '{{ values.hostname }}'}
      releaseInputs:
        web/web:
          image: {fromUnit: {unit: web-image, artifact: image, pointer: /reference}}
```

The unit's templates see the environment's `values` and `environment`, as the
other unit fields do. The inline target is named `<environment>-<unit>`, here
`dev-kubernetes`, so adding or removing publication units never renames another
unit's target; `target.name` overrides the name. Target names are unique across
declared and inline targets. Relative paths in `valuesFrom` start at the file
that declares the unit.

A unit publishes to one destination, its target's name and publication
repository, branch, and prefix. Changing any of them, such as renaming the
target or the prefix, does not move the published tree: the next publication
fails before writing anything with `NYL-KUBERNETES-TARGET-CHANGED`, naming what
changed, until you restore the previous destination or replace the unit. To
replace it, run `nyl teardown -e <env> --unit <unit>`, which removes the tree
the unit published (its stored document still names the old target), and then
reconcile, which publishes the new target as a new incarnation. `nyl plan`
does not show the change, because a publication renders only once the units its
target reads have run.

From then on the target is an ordinary one. It is parsed as strictly as a
declared target, rendered and published through the same compile and publish
path, and checked with every other target by
[`nyl validate`](/nyl/reference/cli/validate/), including that the Argo CD
Application and AppProject names it generates differ from every other target's
on the same cluster and Argo CD namespace. Use `applicationNameTemplate` and
`projectTemplate` with `target.metadata.name` where several targets select one
ApplicationGroup. Its `fromUnit` bindings work as in a declared target. The
renderer fills `target.name` and `target.sourceFile` (the file that declares the
unit); writing `sourceFile` by hand is an error.

Every publication render adds the inline targets of every declared
environment's enabled units to the project's targets, so the checks across
targets run when a unit resolves in `nyl reconcile`, as well as in
`nyl validate`: a unit selected by `dev` and `staging` on one cluster whose
Application names do not depend on the target, or prefixes that nest, fail the
render of either environment's unit.

## Release inputs from other units

The target binds the values with
[Release input bindings](/nyl/kubernetes-gitops/release-inputs/). The same
reference forms that units use read an artifact or an output:

```yaml
apiVersion: k8s.gitops.nyl/v1
kind: DeploymentTarget
metadata:
  name: dev
spec:
  clusterRef: {name: dev}
  applicationGroupSelector: {matchLabels: {app: web}}
  publication:
    repositoryRef: {name: platform}
    revision: deploy
    pathPrefix: dev
  releaseInputs:
    web/web:
      image: {fromUnit: {unit: web-image, artifact: image, pointer: /reference}}
      databaseHost: {fromUnit: {unit: database, output: host}}
```

Each `fromUnit` binding is a dependency, so the publication waits until
`web-image` and `database` have current receipts. Orchestration resolves the
references with the resolver every other unit uses and records the values in
the unit's resolved spec under `releases`, keyed by `<group>/<release>`. The
publication renders the target with exactly those values, and a value the
Release's declared input type does not accept fails the unit before anything is
published. Bindings of other kinds (`value`, `fromFile`, `fromGit`,
`fromPublication`) resolve while rendering.

The published ownership index records every resolved input as
`@input/<group>/<release>/<input>` with the digest of its value, so the tree
shows which values produced it. The same Releases render with static `value` or
`fromFile` bindings in a target that no environment owns, for example in
`render-tree` and `publish-tree`; only `fromUnit` bindings need orchestration.

## Execution key and result

The execution key covers everything the render read: files and directory
listings, resolved Release inputs, charts, Kubernetes capabilities, resolved
sources, and tool fingerprints, plus the resolved spec. A render that cannot
observe an input completely executes on every reconcile. Publishing an
unchanged tree creates no commit.

The unit records a `PublishedTree` artifact named `tree` with `repository`,
`branch`, `pathPrefix`, `target`, `commit`, and `indexDigest`. The push is a
compare-and-swap on the branch head. When another publication moved the head
first, nothing was published, and the unit builds its commit again on the new
head, up to three times and within its `timeout`. A target with
`fromPublication` bindings is rendered again on the new head: when the state
files it read are unchanged, it publishes there. When one changed, the unit
ends without publishing as a retryable failure, `NYL-KUBERNETES-STATE-CHANGED`,
and the next run resolves it again with the new state.

`detect-drift` publishes nothing: it compares the branch with the tree the unit
renders. It reports `drifted` when they differ, or when an owned file was
edited outside Nyl, and `unknown`, with the reason, when it cannot compare,
for example when the remote is unreachable.

## Ownership

The published `_nyl/index.json` records the unit that owns the prefix: the
environment, the unit, and the incarnation's uid (the `owner` member of the
version 3 index; indexes without one, as `publish-tree` writes them, stay
readable). A publication or teardown refuses a prefix that names another
incarnation while that incarnation still exists in its environment's state,
active, held, deleting, or retained. It fails with
`NYL-KUBERNETES-OWNER-FENCED`, naming the owner, and publishes and removes
nothing; the failure is retryable, because the next run reads state again. A
target moves between environments once the old environment's incarnation is
gone: its teardown completed, or `nyl state forget` released it. The first
publication of the new owner records itself in the index. When the state of
the owner's environment cannot be read, or the environment is no longer
declared, the fence holds. The first orchestrated publication over a prefix
without an owner adopts it and upgrades its index, and `publish-tree` refuses a
prefix an orchestrated unit owns.

Each refusal has a code and a way out:

| Code | Meaning and fix |
| --- | --- |
| `NYL-KUBERNETES-OWNER-FENCED` | Another unit incarnation that still exists owns the prefix. Tear that unit down or release it with `nyl state forget`, then run again. |
| `NYL-KUBERNETES-OWNER-UNKNOWN` | The owner's environment was renamed or removed, or its state cannot be read, so Nyl cannot tell whether the owner exists. Declare the environment again and tear its unit down, or release the prefix deliberately (below). |
| `NYL-TREE-OWNED-BY-UNIT` | `publish-tree` or `render-tree` met a prefix an orchestrated unit owns, for example after `nyl state forget` released the unit. Publish through the environment that owns it, or release the prefix deliberately (below). |
| `NYL-KUBERNETES-TARGET-CHANGED` | The unit's destination changed since it last published. Restore it, or replace the unit as described under [Inline targets](#inline-targets). |

To release a prefix deliberately, commit to the publication branch a change to
`<prefix>/_nyl/index.json` that removes its `owner` member and sets `version`
to `3`. The prefix is then unowned, and the next publication adopts it.

## Teardown

When the unit leaves its environment's ownership set, it tears down in two
phases: it publishes the tree without its workload Applications, waits under
`teardownWait` for Argo CD to remove the workloads, and then removes exactly the
files its `_nyl/index.json` records. Files other tools committed in the prefix
stay. See [Publication teardown waits](/nyl/orchestration/deletion/#publication-teardown-waits)
for the strategies and the confirmation commands.

`teardownWait` is `{strategy: manual}` (the default: an operator confirms with
`nyl teardown --confirm-removed`) or `{strategy: delay, duration: 10m}` (phase 1
completes after `duration`, recorded as not observed). The `observe` strategy
needs to read Argo CD and is rejected with
`NYL-KUBERNETES-TEARDOWN-WAIT-UNSUPPORTED` until the unit can.

### Teardown readiness

Resolution records the target's effective teardown settings in the stored
document: whether the catalog prunes removed Applications, the Applications'
`applicationDeletionPolicy`, each owned Namespace's `deletePolicy`, and whether a
parent owns the catalog. The unit itself judges them, from the stored document
without rendering, so `status`, `plan`, `get units -o wide`, and every teardown
agree:

| Requirement | Unmet, and the fix |
| --- | --- |
| The catalog prunes removed workload Applications | Blocking, `NYL-TEARDOWN-CATALOG-MANUAL-SYNC`: enable the catalog's `syncPolicy.automated` with `prune: true`. |
| Deleting an Application deletes its workloads | Blocking, `NYL-TEARDOWN-APPLICATIONS-ORPHANED`: set `applicationDeletionPolicy` to `Foreground` or `Background`. |
| Owned Namespaces do not hold deletion | Blocking, `NYL-TEARDOWN-NAMESPACE-CONFIRM`: set the Namespaces' `deletePolicy` to `Automatic` or `Retain`. |
| Owned Namespaces are removed | Remnant, `NYL-TEARDOWN-NAMESPACES-REMAIN`: set `deletePolicy: Automatic`. |
| The catalog Application and AppProjects are removed | Remnant, `NYL-TEARDOWN-CATALOG-REMAINS`: let a parent own the catalog (`catalogApplication.enabled: false`). |

A tree without a catalog Application leaves the catalog to a parent whose
settings Nyl cannot see; those rows read `external: not checked` and never
block. `nyl plan` and `nyl reconcile` warn about every unmet requirement and
remnant of each publication that would be torn down, so the problem is known
long before a teardown runs. A blocking requirement holds a teardown
(`NYL-TEARDOWN-INCOMPLETE`) until the target is fixed or the invocation passes
`--allow-incomplete`. `teardownSettings` is derived from the rendered tree and
is rejected when written by hand.
