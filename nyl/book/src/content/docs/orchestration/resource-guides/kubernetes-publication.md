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

`target` names a static DeploymentTarget. The unit places that target into its
environment: a target belongs to at most one environment, and a target no
publication unit names rejects `fromUnit` bindings. `mode` is `publish`, the
default. Inline targets and `mode: observe` are not available yet; the unit
rejects `mode: observe` with `NYL-KUBERNETES-MODE-UNSUPPORTED`, because nothing
would check what the publication promises. The common unit fields (`dependsOn`,
`timeout`, `env`, and so on) apply; `timeout` defaults to `10m`.

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
`branch`, `pathPrefix`, `commit`, and `indexDigest`. The push is a
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

## Teardown

When the unit leaves its environment's ownership set, it tears down in two
phases: it publishes the tree without its workload Applications, waits under
`teardownWait` for Argo CD to remove the workloads, and then removes exactly the
files its `_nyl/index.json` records. Files other tools committed in the prefix
stay. See [Publication teardown waits](/nyl/orchestration/deletion/#publication-teardown-waits)
for the strategies and the confirmation commands.

Resolution records the target's effective teardown settings in the stored
document: whether the catalog prunes removed Applications, the Applications'
`applicationDeletionPolicy`, each owned Namespace's `deletePolicy`, and whether a
parent owns the catalog. `status`, `plan`, and every teardown check them
without rendering, and an unmet blocking requirement holds the teardown until
the target is fixed or the invocation passes `--allow-incomplete`.
`teardownSettings` is derived from the rendered tree and is rejected when
written by hand.
