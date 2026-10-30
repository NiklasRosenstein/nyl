# Partial tree publication

Partial publication replaces selected ApplicationGroups or group-qualified
Releases inside one DeploymentTarget. The destination's published desired tree
supplies the unselected workloads; selection never implies removing them.

## Replacement and validation

`publish-tree --application-group NAME` and `--release GROUP/RELEASE` are
repeatable and combine by union. With neither option the complete target is
compiled. Unknown selectors fail. A group selection replaces its membership,
including removed or disabled Releases. An individual Release must still exist;
removal requires selecting its group. Individual selection requires a literal
Release `metadata.name`; select the group for templated names. A removed or disabled group can be selected
by its published identity.

The shared compiler pins the destination branch, verifies the ownership index
and every indexed file, resolves only selected Release inputs, renders selected
Releases, and combines them with retained manifests. Namespace ownership,
resource ownership, Application names, CRD scopes, generated AppProjects, and
configured final-artifact validation operate on this complete candidate.
Unselected templates and input values are not evaluated. Static inventory and
control-resource validity remain project-wide requirements.

Retained workload files and Applications keep their bytes. A required ownership
transfer that would change a retained workload fails with the Release to add to
the selection. Generated shared artifacts may change from the candidate's full
usage, and the typed compilation report lists those changes. Carried state that
would change for a retained consumer is rejected. The same pinned baseline is
used for clean-HEAD verification and the eventual compare-and-swap publication;
a changed destination requires rebuilding the candidate, never rebasing bytes.

## Published metadata and migration

Version 3 ownership indexes add version 1 compilation metadata: effective
Release and group policies, workload paths, original rendering provenance and
input evidence, group/control fingerprints, and declared state bindings.
Input defaults and binding values are never copied into metadata; only state
references and input digests are recorded. Manifest bytes remain in the indexed
files, not in the metadata. Retained
entries keep their original source revision and input evidence. A mixed tree
has no target-wide `sourceCommit`; commit trailers identify the operation's
source revision, baseline, and selection.

Version 2 indexes remain readable and reconcilable. One full publication seeds
the metadata before partial publication is possible. Missing files, hash
mismatches, incompatible ownership, and unsupported metadata fail closed. A full
publication upgrades even when its manifest bytes have not changed.

A partial publication requires compatible target-wide control configuration:
publication coordinates, effective Cluster contracts, Argo CD destination,
target values and selection, and project settings. Project path settings are
fingerprinted relative to the configuration directory so the same policy works
in subsequent checkouts. Per-Release bindings are
resolved only for selected Releases. Unselected group configuration, including
its effective AppProject declaration, must match the baseline. Group-wide
changes require selecting the entire group; target-wide changes require a full
publish.

## Shared API and orchestration

`TreeRenderOptions.selection` and `TargetRender.selection` carry the same typed
replacement authority. `TargetRender.publication_base` can pin the destination
alongside an orchestrator's resolved input snapshot. The compiler returns a
`PublicationReport` naming the baseline, rendered/retained/removed Releases, and
changed shared artifacts. Publication uses the existing target-wide transaction
and compare-and-swap; separate selections do not create separate ownership
boundaries or authorize concurrent overwrites.

The target cache is bypassed for partial results; ordinary per-Release rendering
continues to use its dependency-verified cache. Recorded compilation includes
the destination index as an explicit dependency. A scoped publication is evidence
for its selected Releases, not evidence that the whole target matches one source
revision. Integrating scoped operations into orchestration receipts and promotion
coverage requires their caller to preserve this distinction. The existing
KubernetesPublication driver continues to select the full target.
