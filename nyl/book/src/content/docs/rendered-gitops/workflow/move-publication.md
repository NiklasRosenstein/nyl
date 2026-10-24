---
title: 'Move a Publication'
---

Changing a target's `publication` (its repository, `revision`, or `pathPrefix`)
starts a new publication. Nyl reconciles only inside the current prefix, so it
leaves the old prefix, its catalog, and its ownership index untouched, and it
does not carry `fromPublication` state across. `diff-tree --against source`
warns with `publication_moved` in the pull request that makes the change.

Move a target from `prod/` to `production/` on one publication branch in this
order:

1. Before merging, copy the target's state files to the new prefix in a commit
   on the publication branch, for example with
   `mkdir -p production/state && cp prod/state/images.json production/state/`.
   Copy rather than move, because the old publication still reads them until
   the merge. A prefix without an ownership index may hold declared state
   files, so the first publication reads them and adopts carried ones.
2. Merge the change and run `publish-tree`. It writes every manifest and a
   fresh `_nyl/index.json` under `production/`.
3. Point Argo CD at the new prefix: apply the new parent catalog Application
   once, as after a first publication. It keeps its name, so it replaces the
   old parent's source path, and the child Applications follow the new
   catalog.
4. Delete the old prefix, including `prod/_nyl/index.json`, in one commit on
   the publication branch. Do not move the old index: it records the old
   destination, and Nyl rejects an index recorded for another publication.

When the repository or branch changes, do the same across the two branches.
