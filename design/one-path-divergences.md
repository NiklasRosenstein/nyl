# One path: current divergences

Nyl's core principle is that everything it does is declared in Git and that
every invocation takes the same path over those declarations: a local or
bootstrapping run, a CI render, and a publication. A render on a laptop must be
the render CI publishes for Argo CD.

This note records where the direct commands (`nyl render`, `nyl diff`,
`nyl apply`) currently diverge from the rendered GitOps path (`render-tree`,
`diff-tree`, `publish-tree`, and what Argo CD then applies), the fix proposed
for each, and the decision each fix needs. Delete an entry when its fix lands.

## 1. Namespaces of manifests without one

**Divergence.** The tree path leaves `metadata.namespace` unset and the
generated Application's destination fills it: the ApplicationGroup's
`destinationNamespace`, else the Release namespace
(`crates/nyl-render/src/gitops/tree.rs`, the `destination_namespace` of each
workload). The direct path fills it itself, from the Release namespace or
`--namespace`, else the kube context's default namespace
(`nyl/src/cli/namespace_resolution.rs`, `resolve_manifest_namespaces` and
`adjust_duplicate_keys_for_namespace_resolution`). It never consults
`destinationNamespace`.

**Effect.** For a Release in a group with `destinationNamespace`, `nyl apply`
deploys into a different namespace than Argo CD does. Without a Release,
`nyl render` output depends on the local kubeconfig.

**Proposed fix.** Resolve the namespace from the declaration only: the group's
`destinationNamespace` when `--target` selects the Release's group, then the
Release namespace or `--namespace`, else an error naming both. Never use the
kube context.

**Decision.** Rendering without a Release and without `--namespace` fails where
it used the kube context namespace before.

## 2. Secret inputs

**Divergence.** Direct commands load the project secrets provider and admitted
`NYL_*` process environment by default (`RenderSession::for_cli` in
`crates/nyl-render/src/render/session.rs`). Tree commands exclude them unless
`--allow-secret-inputs` is given (`compile_target_tree_inner` in
`crates/nyl-render/src/gitops/tree.rs`).

**Effect.** A Release whose templates read secrets or the environment renders
differently locally than in CI, or renders locally and fails in CI.

**Proposed fix.** Give the direct commands the same default and the same
`--allow-secret-inputs` flag.

**Decision.** Breaking for anyone who renders or applies with secrets today;
they add the flag. Needs a changelog entry.

## 3. Additional namespaces

**Divergence.** `render-tree` synthesizes missing `Namespace` objects for the
destination and `spec.additionalNamespaces`, with the group's managed namespace
policy (`ensure_managed_namespace` in `crates/nyl-render/src/gitops/layout.rs`).
`nyl apply` creates only the release namespace, owns none, and requires other
namespaces to be written as manifests (`nyl apply` reference, notes).

**Effect.** The same Release yields different objects locally and through
Argo CD.

**Proposed fix.** Run the same namespace synthesis in the direct path.

**Decision.** Whether `nyl apply` then owns those Namespaces. Owning them puts
them under release pruning, which deletes everything in a namespace a later
revision drops; the tree path guards this with Argo CD's deletion confirmation
policy. Not owning them keeps the synthesis but leaves the objects unpruned.

## 4. Cluster access during `nyl render`

**Divergence.** `nyl render` without `--offline` connects to the cluster for
namespace resolution and duplicate-key handling
(`should_initialize_cluster_clients` in `nyl/src/cli/commands/render.rs`).
Tree rendering never does; scopes come from the Cluster's recorded
`clusterScopedKinds`.

**Effect.** `nyl render` needs credentials CI rendering does not, and its output
can depend on what the live cluster serves.

**Proposed fix.** With entry 1 fixed, take scopes from
`Cluster.spec.kubernetes` as the tree path does, so `nyl render` never needs the
cluster. `diff` and `apply` keep their cluster access for their own work.

**Decision.** None beyond entry 1; Clusters without recorded scopes need
`nyl capture cluster` first, as for tree commands.

## Already aligned

- With `--target`, the Cluster resource is authoritative for Helm capabilities;
  live discovery does not change a render.
- `nyl render --target` applies the Release input bindings `render-tree`
  applies.
- Tree commands reject `--input` overrides, so published output reproduces from
  committed source.
- `nyl apply` applies resources in Argo CD's order.
