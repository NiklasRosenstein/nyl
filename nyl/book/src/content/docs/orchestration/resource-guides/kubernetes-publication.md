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
  teardownWait: {strategy: manual}
```

`target` names a static DeploymentTarget, or declares one inline (see
[Inline targets](#inline-targets)). The unit places that target into its
environment: a target belongs to at most one environment, and a target no
publication unit names rejects `fromUnit` bindings. A unit publishes only,
unless it declares `observe: {}`: it then publishes the same way and also
reads the reconciler that runs its target, Argo CD (see
[Observing Argo CD](#observing-argo-cd)), and the live cluster (see
[Observed values](#observed-values)). The common unit fields
(`dependsOn`, `timeout`, `env`, and so on) apply; `timeout` defaults to `10m`.

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
| `NYL-TREE-OWNED-BY-UNIT` | `publish-tree`, `render-tree`, or `nyl build -- --push` met a prefix an orchestrated unit owns, for example after `nyl state forget` released the unit. Publish through the environment that owns it, or release the prefix deliberately (below). |
| `NYL-KUBERNETES-TARGET-CHANGED` | The unit's destination changed since it last published. Restore it, or replace the unit as described under [Inline targets](#inline-targets). |

To release a prefix deliberately, commit to the publication branch a change to
`<prefix>/_nyl/index.json` that removes its `owner` member and sets `version`
to `3`. The prefix is then unowned, and the next publication adopts it.

## Observing Argo CD

Publishing is a receipt: the tree is on the branch. Whether Argo CD took it
and the workloads are healthy is a separate fact that only Argo CD knows. A
unit that declares `observe: {}` declares two attestations, `accepted` and
`healthy`, and `nyl attest` reads Argo CD to record them:

```yaml
apiVersion: units.gitops.nyl/v1
kind: KubernetesPublication
metadata:
  name: kubernetes
spec:
  target: '{{ values.target }}'
  observe: {}
  env: {passthrough: [KUBECONFIG]}      # the credential for the access the ArgoCDInstance declares
```

`observe` is an object so that options can be added later; only `{}` is
accepted, and a field inside it is a validation error. It opts the unit in and
says nothing about which reconciler runs: that follows the reconciler of the
unit's target, its ArgoCDInstance, so the declaration stays the same if the
target's reconciler changes. There is no `mode` field; a unit without
`observe` only publishes, declares no attestations, and waits for an operator
when it is torn down.

```bash
nyl attest -e dev
nyl attest -e dev --unit kubernetes -o json
```

How Argo CD is read, the Kubernetes API of the control-plane Cluster or the
Argo CD API server, is declared once on the
[ArgoCDInstance](/nyl/kubernetes-gitops/resource-guides/argocd-instance/#reading-argo-cd)
in Git, never chosen per command, and the credential comes only from the
unit's admitted `env` (`KUBECONFIG` for the Kubernetes API; the declared token
variable, `ARGOCD_AUTH_TOKEN` by default, for the API server). Resolution
records what the readings need in the stored document (`reconciler`: its
`kind` (`argocd`), the instance, how it is read, the publication destination,
the Applications the target expects Argo CD to run, and the anchors that show
a reading comes from the Argo CD that runs the target), so `nyl attest`
renders nothing; an authored `reconciler` is rejected. Only what the render
derives keys the unit: how Argo CD is read (`access`, `context`, and the
publication URL) is recorded but never re-executes it.

Matching reads the publication branch, from the destination the unit
publishes to, with the project's `[[git.credentials]]` as a publication does,
so the unit admits the same Git credential variables for `nyl attest` as for
publishing.

`nyl attest` judges every Application the target generates, the catalog
Application included, against the publication of the unit's current receipt:

- **Running revision.** An Application runs the revision of its newest
  `status.history` entry, which Argo CD appends for full syncs only. Whether
  it is Synced or OutOfSync is ignored, because an Application that runs an
  older publication reports OutOfSync once a newer one is published. A selective
  sync, a full sync that failed or is still running, or a dry run, as new as
  that entry leaves the Application running a mix of revisions (`mixed`),
  which proves no revision (the Application keeps only its latest operation, so
  a dry run may hide a selective sync before it).
- **Matching.** An Application *runs the publication* when the directory it
  sources, and the recorded inputs of its Release, are the same at its running
  revision as at the receipt's publication commit. The commit Argo CD reports
  may be another target's commit on a shared branch or a write-back: only the
  Application's own directory counts. A commit that edited that directory
  without publishing (`unmatched`), or one not on the publication branch
  (`not-on-branch`), matches nothing.
- **Results.** `accepted` passes when every Application runs the receipt's
  publication, and `healthy` when, besides, every one reports `Healthy`.
  Otherwise the attestation fails with the Applications that fell short.

The attestations' details name the reconciler that was read (`reconciler`)
and list each Application (`deployments`) with its `id` (its name, qualified by
its namespace outside the instance's), role (`catalog` or `workload`),
directory, `state`, `health` (`Healthy`, `Progressing`, `Degraded`, or
`Unknown`, which covers anything else Argo CD reports and is never healthy;
Argo CD's own word, such as `Suspended`, is kept in `reported` and shown in
the reason, as in `web is Unknown (Suspended)`, and never affects a result),
the revision it runs (`running`), and the publication commit that introduced
what runs (`publication`), and, for one that fell short, why. The states are `accepted`,
`other-publication`, `unmatched`, `not-on-branch`, `mixed`, `never-synced`,
`missing`, `deleting`, `other-source` (its source, or the source of its last
sync, differs from the generated one, such as an edited `directory.recurse`,
`exclude`, or tool block), and `unreadable`. Argo CD's list leaves out
Applications the token may not read and still answers success, so with the API
server Nyl asks Argo CD (`can-i`) about every Application the list lacks: a token that may not
get it makes the unit `unknown`, and only an Application the token could see
counts as `missing`. With the Kubernetes method, a reading without the target's catalog Application
(or, without a catalog, its AppProjects) makes the unit `unknown` instead of
`missing`: a context for another Argo CD never records a failure. Apply the
catalog before the first attest, and point `spec.live.context` at the control
plane that runs the target. A target that generates no
Application attests both vacuously.

An attestation that could not establish its result is never recorded as a
failure. When Argo CD does not answer, the credential is rejected (an expired
token is reported as invalid or expired) or lacks permission, one Application
namespace of several cannot be read, or the publication branch cannot be
fetched, the unit is `unknown` (`NYL-ATTEST-UNKNOWN`, exit 2), the reason names
the fix, and the attestations recorded earlier stay. A recorded `fail` always
means Argo CD was read and an Application fell short; a failing attestation
exits 3 (`NYL-ATTESTATION-FAILED`). A new receipt starts with both attestations
pending until `nyl attest` runs again. Attest from where the credentials are,
typically a CI job after Argo CD synced. A repeated attest of an unchanged
system writes nothing for 15 minutes; after that it rewrites the attestation with
its new time, so a system a CI job re-verifies stays within `maxAttestationAge`
while one that cannot be reached goes stale.

Other units can wait for the attestations. A smoke test that should run only
against healthy workloads depends on `healthy`; it stays
[blocked](/nyl/orchestration/runs/#requiring-attestations) on the publication
until a `nyl attest` has recorded `healthy` for the publication's current
receipt, and runs in the next `reconcile` without executing the publication
again:

```yaml
apiVersion: units.gitops.nyl/v1
kind: Command
metadata:
  name: smoke
spec:
  command: [./smoke.sh]
  dependsOn: [{unit: kubernetes, attestations: [healthy]}]
```

`evidence: attested` instead of `attestations` requires both `accepted` and
`healthy`. A `dependsOn` consumer does not run again when only the publication's receipt changes;
[`runs`](/nyl/orchestration/runs/#requiring-attestations) shows how to make a smoke test
run against each new version. A unit without `observe` declares no attestations, so requiring one
of it is `NYL-ATTESTATION-UNDECLARED`.

`nyl attest --target` for a target no environment owns arrives with promotion
(M6), as does `nyl attest --name … --result …`.

## Observed values

A unit that declares `observe: {}` can read a field of a resource its target renders
from the live cluster, such as the address a cloud load balancer assigns to a
published Service, and let other units use it. The declaration, the way the
value is referenced, and how reconcile and `nyl observe` read it are the same
for every kind (see [Observed values](/nyl/orchestration/runs/#observed-values)); this
section is what belongs to the publication:

```yaml
spec:
  observe: {}
  observations:
    ingressIp:
      type: string
      from:
        resource: {apiVersion: v1, kind: Service, namespace: ingress, name: ingress-nginx}
        pointer: /status/loadBalancer/ingress/0/ip
  observationWait: 10m
  env: {passthrough: [KUBECONFIG]}
```

`from.resource` names the resource as the Kubernetes API does (`namespace` only
for a namespaced kind), and `pointer` is a [JSON Pointer](https://www.rfc-editor.org/rfc/rfc6901)
to one field of it; an empty pointer, which would select the whole resource, is
rejected. Observed values are committed in plain text to the state repository,
so observe only non-secret fields: Secrets are rejected, but a ConfigMap or a
custom resource can hold sensitive data too. A unit without `observe` that declares `observations` is rejected
(`NYL-OBSERVATIONS-UNSUPPORTED`): reading is a use of the reconciler and cluster
credentials that publishing alone does not need.

- **Only what the target renders.** Resolution renders the target and checks
  that one of its Releases renders the resource. A resource outside the
  rendered tree is rejected with `NYL-KUBERNETES-OBSERVATION-UNOWNED`, so the
  cluster the unit reads is the target's Cluster and nothing outside the
  declaration. A manifest without `metadata.namespace` belongs to the namespace
  its Application deploys to. Secrets are never read. Resolution records the
  deployment that owns each resource (an Application's id, as the
  reconciler facts name it) and the workload Cluster in the stored
  document (`observationSources` and `workload`; an authored one is rejected),
  so `observe` renders nothing.
- **Only once its Application runs the publication.** Argo CD is read as
  `nyl attest` reads it, and only the Application that owns the resource must
  run the receipt's publication, matched as for
  [`accepted`](#observing-argo-cd). An observation whose Application did not
  change in a publication resolves at once, and a slow, unrelated Application
  does not hold it back. Until then it is `pending`
  (`application-not-running-publication`, naming the Application and why it
  does not match); afterwards a pointer that does not resolve yet is `pending`
  (`field-not-set`), because controllers fill in status after Argo CD applies
  the manifest. A resource that does not exist after its Application runs the
  publication, and a value of the wrong `type`, are `failed`.
- **Which credential.** The workload Cluster may differ from the Argo CD
  control-plane Cluster, so the resource is read through the workload Cluster's
  own `spec.live.context`, in the kubeconfig the unit admits as `KUBECONFIG`
  (one kubeconfig can hold both contexts, and `KUBECONFIG` may list several
  files). Argo CD is read the way its ArgoCDInstance declares, with the
  credential that access needs. Credentials come only from the unit's admitted
  `env`: a missing one is a failed read that names the variable and the `env`
  entry that admits it, and Nyl never falls back to the process environment, the
  current kube context, `~/.kube/config`, or in-cluster configuration. A Cluster
  without `spec.live.context` cannot be read. The declared context says how to
  read, not what the unit produces, so changing it does not re-execute the unit.
  Run [`nyl observe`](/nyl/reference/cli/observe/) from a job that holds these
  credentials when the job that reconciles does not.

## Direct builds

`nyl build` renders the unit's tree without orchestration: no lease, receipt, or
state write, and nothing recorded. It renders the target as a run does, with
the same renderer and the unit's Release inputs, and with no process
environment or project secrets. Files come from the worktree, uncommitted
changes included, or from `--source <revision>`. With `-e`, the target's
`fromUnit` bindings resolve against that environment's current receipts, as
`nyl get output` reads them; without it, they fail the build and name `-e`. The
driver's own options follow `--`:

```bash
nyl build kubernetes -e dev
nyl build kubernetes -e dev -- --diff
nyl build kubernetes -- --push
```

A build prints one JSON document and exits 0:

| Arguments | Prints |
| --- | --- |
| none | A `RenderedTree`: where the tree publishes (`repository`, `branch`, `pathPrefix`), the `target`, the digest of every file (`files`), and the `indexDigest` a publication of this tree would record. |
| `--diff` | A `TreeDiff` comparing the render with the tree published at the destination: `changes` (`none`, `create` before anything is published at the prefix, or `update`), the `base` commit, the `owner` the published index names, and the `files` that differ, each `added`, `modified`, or `removed`. Only files are compared, not `_nyl/index.json`: `changes: none` can still be followed by a `--push` commit that only updates the index, for example with a new `sourceCommit`. |
| `--push` | The `PublishedTree` a run records: the commit, branch, and `indexDigest`. |

`--diff` reads the destination and writes nothing, so it also compares a tree
that an environment owns. Differences do not change the exit status, as in
[`diff-tree`](/nyl/reference/cli/diff-tree/); read `changes`. Files in the
prefix that the published `_nyl/index.json` does not record, such as state files
other tools committed, are not part of the tree.

`--push` publishes the render as one commit authored by the identity of the
repository's Git configuration, and only for a target no environment owns. It
refuses, before anything is published, with the codes below. The first, `NYL-KUBERNETES-BUILD-TARGET-OWNED`, then `NYL-KUBERNETES-BUILD-DIRTY-SOURCE` and `NYL-KUBERNETES-BUILD-IDENTITY`, are checked before the build's own render; `NYL-TREE-OWNED-BY-UNIT` is found when the publication prepares, after it. A refusal is a failed build, exit 3:

| Code | Meaning and fix |
| --- | --- |
| `NYL-KUBERNETES-BUILD-TARGET-OWNED` | An Environment selects a publication unit that places the target, even one being torn down. Publish with `nyl reconcile -e <environment>`, or deselect the unit from that environment first. |
| `NYL-TREE-OWNED-BY-UNIT` | The published index names a unit incarnation. A direct publication records no owner, so it cannot tell whether that unit still exists; release the prefix deliberately (see [Ownership](#ownership)). |
| `NYL-KUBERNETES-BUILD-DIRTY-SOURCE` | The worktree has uncommitted changes, which the index could not attribute to a commit. Commit them, or build a revision with `--source <revision>`. |
| `NYL-KUBERNETES-BUILD-IDENTITY` | No identity to author the commit. Set `user.name` and `user.email` in the repository's Git configuration. |

The index of a direct publication has version 3 and no owner, as `publish-tree`
writes it, so an environment that later selects the unit adopts the prefix.
`--diff` and `--push` are alternatives; any other argument fails with
`NYL-KUBERNETES-BUILD-ARGUMENT`.

A unit that declares its target [inline](#inline-targets) builds only with
`-e`: the target is named `<environment>-<unit>` and rendered with the
environment's values, so without it the build fails with
`NYL-KUBERNETES-BUILD-INLINE-TARGET`. An environment that selects the unit owns
its inline target, so `--push` is refused there too; `--diff` and the plain
render work.

## Teardown

When the unit leaves its environment's ownership set, it tears down in two
phases: it publishes the tree without its workload Applications, waits under
`teardownWait` for Argo CD to remove the workloads, and then removes exactly the
files its `_nyl/index.json` records. Files other tools committed in the prefix
stay. See [Publication teardown waits](/nyl/orchestration/deletion/#publication-teardown-waits)
for the strategies and the confirmation commands.

`teardownWait` is `{strategy: manual}` (the default for a unit without
`observe`: an operator confirms with `nyl teardown --confirm-removed`),
`{strategy: delay, duration: 10m}` (phase 1 completes after `duration`,
recorded as not observed), or `{strategy: observe}` (the default for a unit
that declares `observe: {}`, and an error for one that does not: phase 1 completes
once Argo CD no longer has any of the target's workload Applications, read as
[Observing Argo CD](#observing-argo-cd) describes, within the unit's `timeout`;
the wait concludes removal only from what the credential could see: the catalog
Application must be in the same reading (for a target without one, its
AppProjects must be visible; with the Kubernetes method and neither, the wait
fails closed and the API method is the way out), and with the API server each
missing Application needs Argo CD's `can-i` to say the token may get it).
A unit that waits by observing records the same reconciler facts at resolution
and needs the same credential admitted; without one, or when Argo CD cannot be
read, the teardown is uncertain with `observation-failed` and the reason.

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
