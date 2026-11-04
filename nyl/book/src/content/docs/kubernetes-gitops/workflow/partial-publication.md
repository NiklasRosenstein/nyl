---
title: 'Partial Publication'
---

```bash
nyl publish-tree --target production --application-group payments
nyl publish-tree --target production --release payments/api
```

A partial publication replaces selected ApplicationGroups or Releases of one
DeploymentTarget and keeps everything else the destination already publishes.
Nyl renders only the selection and reads the remaining manifests from a
verified, pinned destination commit. Whole-tree ownership, Namespace, CRD,
project, and configured manifest validation run on the combined candidate.
Unselected Release templates and inputs are not evaluated; static project
discovery and control-resource validity still apply. The
[`publish-tree` reference](/nyl/reference/cli/publish-tree/#partial-publication)
lists the options.

## Selecting

- Selecting a group replaces its published membership, so it can remove
  Releases. A removed or disabled group can be selected by its published name.
- Selecting one Release requires a literal `metadata.name` and requires the
  Release to exist; removing it requires selecting its group. Select the group
  for Releases with templated names.
- In a group touched only by Release selectors, a new Release that is neither
  selected nor published stays unpublished, and the summary lists it.

## What stays the same

Retained workloads and their Applications keep their bytes and their original
provenance. When an update would change a retained owner or carried state that a
retained Release consumes, Nyl fails and names the Release to add to the
selection. Generated shared artifacts, such as derived AppProjects, may change
to admit the combined candidate's namespaces and resource kinds, and the
summary lists them. Selected and retained state files must stay outside every
candidate Release directory and must not overlap rendered files.

## When a full publication is required

- The destination has never been published, or its ownership index predates
  version 3. Run one full publication first.
- An indexed file is missing or was modified outside Nyl.
- Target-wide configuration changed: publication coordinates, the effective
  Cluster and Argo CD destination, target values, renderer search paths,
  aliases, label stripping, vendoring, or validation policy. Imperative settings
  such as `[release] history_limit` do not count.

An ApplicationGroup configuration change, including its effective AppProject,
requires selecting the whole group.

## Provenance

A partially published tree can contain several source revisions. Its index has
no target-wide `sourceCommit`; instead, it records the source commit and input
digests per Release, without copying input values or defaults. Retained
Releases keep their input, Git, publication, and carried-state evidence. The
commit's source trailer describes this operation, not every retained Release,
so a partial publication does not show that the whole target matches one source
revision.

A destination update during rendering fails publication; rerun to render against
the new baseline. `--dry-run` prepares the complete candidate without pushing.
