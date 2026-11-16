---
title: 'Promotion'
---

Promotion moves what one environment proved into another: a source commit, an
image digest, or any value a unit or Release should take from the source
instead of rebuilding or retyping it. The link is a
[PromotionPath](/nyl/orchestration/resource-guides/promotion-path/);
[`nyl promote`](/nyl/reference/cli/promote/) writes the record, and the target
reads it when it reconciles.

:::note[Status]
M6 delivers promotion in steps. `nyl promote`, `fromPromotion` sources and
bindings, artifact verification, `nyl get states`, `promotion-candidates`,
and `promotions`, `status` lineage, `record: source`, and several paths into
one environment with the supersede check and hotfixes work today. Value-only
coverage follows.
:::

## The unit of promotion

One recorded state of the source: the transition commit pair of one
`reconcile` run, which ties together the source commit, the desired documents,
and the receipts with their artifacts. A promotion records that state (`from`),
its source commit, and every value read from it, so the promoted source and the
promoted values always come from the same run.

Without flags `nyl promote` takes the newest state whose evidence reaches the
path's level: at `published`, every unit the target also selects has a current
receipt; at `attested`, every required attestation passes for that state,
within `maxAttestationAge`. A failing attestation blocks a state on every path.
`--revision` and `--state-revision` choose a state, and rolling back is
promoting an older one:

```bash
nyl promote dev-to-prod --revision 3e7b9c… --evidence published --reason "Roll back release 2"
```

## Reading what was promoted

- **A promoted source.** An Environment with
  `source: {fromPromotion: {path: dev-to-prod}}` runs its units at the source
  commit of the newest record on its path (or `paths`). The record's path and
  sequence are written to the environment record as `sourceChosenBy` and
  `appliedPromotionSequence`. Before the first promotion `reconcile` and `plan`
  exit 2 with `NYL-PROMOTION-MISSING`; its fix names `nyl promote <path>`.
  With `record: source` the record lives in the Environment itself, as
  described [below](#keeping-the-record-in-source).
- **Bindings.** `{fromPromotion: {value: webImage}}` in a unit's spec, or in a
  DeploymentTarget's `releaseInputs`, resolves through the one binding
  resolver. With `path` it reads that path's record; without it, the newest
  record carrying the value. In an environment whose source is promoted, a
  binding without `path` reads the record that supplied the source commit, so
  the commit and the values proven with it move together: a value that record
  lacks is never taken from an older record of another path. A binding with
  no record yet blocks its unit, and the reason names `nyl promote`.

## Hotfixes

A promoted environment runs only what its source proved, so a fix that
cannot wait for dev's unreleased work takes a path of its own. prod takes its
source from either path, and its bindings name no path, so they follow
whichever record supplied the source:

```yaml
# prod
spec:
  source:
    fromPromotion: {paths: [dev-to-prod, hotfix-to-prod]}
  protectedRefs: [main, 'release/*']
---
apiVersion: gitops.nyl/v1
kind: Environment
metadata: {name: prod-hotfix}
spec:
  source: {revision: release/prod}     # cut from what prod runs
  unitSelector: {matchLabels: {hotfix: 'true'}}   # the image and a publication to a target of its own
  attestations: [qa]
---
apiVersion: gitops.nyl/v1
kind: PromotionPath
metadata: {name: hotfix-to-prod}
spec:
  from: {environment: prod-hotfix}
  to: {environment: prod}
  evidence: attested
  values:
    webImage: {select: {unit: web-image, artifact: image, pointer: /reference}}
```

1. **Start.** Cut `release/prod` from the source commit prod runs
   (`nyl status -e prod` shows it) and commit the fix there.
2. **Try.** CI reconciles `prod-hotfix` on pushes to `release/prod`: it builds
   the fix and deploys it to the hotfix target, where it is attested.
3. **Ship.** `nyl promote hotfix-to-prod`, then `nyl reconcile -e prod`: prod
   runs the fix commit with the hotfix image, and keeps its own approvals.
4. **Merge back.** Merge `release/prod` into `main`. Until `main` contains the
   fix, the next `nyl promote dev-to-prod` is refused with
   `NYL-PROMOTE-SUPERSEDES`, naming the missing commits.
5. **Clean up.** Keep `release/prod` until prod runs a commit reachable from
   `main`: prod's source commit must stay fetchable.

`nyl promote` checks that a promotion keeps what other paths promoted into
the target since its own path last promoted: the promoted source commit and
each value must contain the other records' commits, by their changes (as
`git cherry` compares them). A merge commit or cherry-picks of each fix
commit pass; a squash merge of several fix commits does not, because the
squash has the changes of none of them on its own. The check is symmetric: a
hotfix branch cut before a later dev promotion is refused too, and is cut
again from what prod runs. When a fix was made differently or is dropped on
purpose, `--supersede` records that:

```bash
nyl promote dev-to-prod --supersede '<range>=Squash-merged into main' --reason "Release 3"
```

The refusal prints the range to copy, and the record lists each superseded
commit with its reason; [`nyl promote`](/nyl/reference/cli/promote/#superseding)
describes the rule and its options. `nyl get promotions -e prod` shows which
record each binding reads, and `nyl status -e prod` a hotfix that waits for
the next reconcile.

## Inspecting promotion

Three read-only forms of [`nyl get`](/nyl/reference/cli/get-promotions/)
show what a promotion would see, and `status` shows what a promoted
environment runs:

- `nyl get states -e dev` lists dev's recorded states, newest first, with the
  source commit each ran, which units have a current receipt in it, and the
  attestations recorded for it. The desired commit it prints is what
  `nyl promote --state-revision` takes.
- `nyl get promotion-candidates dev-to-prod` judges each of dev's states
  against the path, with the same rules and the same reading of state as
  `nyl promote`: the level each reaches, what it lacks (a unit without a
  current receipt, a pending, aged, or failing attestation), the values it
  would carry, the state `nyl promote` would take now (or why it would
  refuse), the state the path's record names, and the state prod runs now.
  A value-only path is judged per value, as `nyl promote` takes each value
  from its own state. Artifacts are not verified here.
  `nyl promote dev-to-prod --dry-run` then shows the exact record.
- `nyl get promotions -e prod` lists prod's PromotionRecords, the one its
  last run applied, and every earlier promotion from prod's desired history.
- `nyl status -e prod` shows the lineage of what prod runs: the path and
  sequence of the applied record, the source commit, and the source state it
  came from, and a newer record waiting for the next reconcile.

```console
$ nyl get promotion-candidates dev-to-prod
promotion-candidates dev-to-prod: dev -> prod, evidence published
  nyl promote would take eed4c2359d485417a9882145d8ceb6d59378a406
eed4c2359d485417a9882145d8ceb6d59378a406 published (selected)
  run:     00000000-0000-7000-9000-000000000006
  source:  cf41525d1ed208e5fb4f7e804f78de250ef37102
  value webImage: registry.invalid:5000/web@sha256:bc69a88a97d841aac5366b9cdee643b6056aeadbac6ef1a03b9ee58bbeeb0805
bc1847b0292df2e0548131fe07c6bfd71777a2a6 published (promoted, running)
  run:     00000000-0000-7000-9000-000000000003
  source:  c966aa0c15d9c431c15728aec36d32e7adef056e
  value webImage: registry.invalid:5000/web@sha256:88e3d32b27feb6648d7e9ce25a18b1124b27849a11e8f9d5d0c1d70767567a75
```

## Keeping the record in source

By default the record lives in the target's state, at
`desired/promotions/<path>.yaml`. With `record: source` it lives in the
target Environment's own `source` block instead, so the promoted source
commit and the values it runs with are one reviewed change in Git:

```yaml
apiVersion: gitops.nyl/v1
kind: Environment
metadata:
  name: prod
spec:
  source:
    fromPromotion:
      path: dev-to-prod
      record: source
      promoted: # written by nyl promote, never by hand
        path: dev-to-prod
        sequence: 2
        from:
          environment: dev
          run: 0192f0c4-8d0a-7c3e-9a51-3c1f0e7d2b90
          desiredCommit: 77aa…
          observedCommit: 41f0…
        sourceCommit: 3e7b9c…
        values:
          webImage:
            value: registry.example.com/web@sha256:4f0c…
            select: {unit: web-image, artifact: image, pointer: /reference}
            unit: {name: web-image, uid: 2b90…}
            executionKey: sha256:…
        # evidence, requested, and the other fields of every PromotionRecord
```

- **Writing.** `nyl promote dev-to-prod` writes the block, with the same
  fields as a record in state, as one commit on the branch the entry
  worktree has checked out, and pushes it to that branch's upstream
  fast-forward only. It writes nothing to prod's state. Write
  `fromPromotion` in the Environment itself, in block style, one key per
  line; the rest of the file stays as written. A detached HEAD, a branch
  ahead of its upstream, uncommitted changes to the Environment's file, or a
  push the remote refuses for branch protection exit 1 with
  `NYL-PROMOTE-SOURCE-UNWRITABLE`, each with its own fix. A push refused
  because the branch moved since the checkout writes nothing and exits 2
  with `NYL-PROMOTE-SOURCE-REJECTED`: update the checkout and promote again.
  A promotion is never forced. The [command
  page](/nyl/reference/cli/promote/#record-source) lists every case.
- **Reading.** Every command that resolves prod (`plan`, `reconcile`,
  `build`, and the multi-environment runs) reads the block as the entry
  worktree declares it, like the rest of `source`, and checks a block no run
  applied yet against its PromotionPath (its source environment, values, and
  selectors) and the state of dev it names (its run, source commit, and
  every value read again from that state). A block that does not match, or
  names a state dev's history does not hold, is refused with
  `NYL-PROMOTION-RECORD-INVALID`. `status` and `nyl get` show the block and
  refuse nothing. `source-locks` never touches the block.
- **Sequence.** The block's `sequence` rises with every promotion into prod,
  and prod's state records the highest one a run applied. A run from an
  older checkout, such as a re-run of an old CI job, or after the promotion
  commit was reverted, finds a lower sequence and exits 1 with
  `NYL-PROMOTION-SEQUENCE-OLDER`, writing nothing. A block with the
  sequence prod applied must be the block prod applied: a sequence is never
  reused, such as by a promotion on another branch. Rolling back is a new
  promotion of the older state, with a higher sequence:
  `nyl promote dev-to-prod --revision <source commit> --reason "…"`.
- **Inspecting.** `status`, `nyl get promotions`, and
  `nyl get promotion-candidates` show the block as the path's record. Earlier
  blocks are in the Git history of the Environment's file; `nyl get
  promotions` lists only promotions recorded in state as its history.

## How a promotion reads the source

`nyl promote` reads the source environment's state history in Git: the desired
and observed logs, the documents as of each state, and the attestations
recorded for each state, found by walking the observed history from the state's
commit to the tip. It never takes the source's lease. The record's `sequence` is
one above the highest ever recorded into the target, taken from the target's
desired history, its current records, the sequence a run applied, and a
`record: source` block, so restoring older state with
[`nyl state restore`](/nyl/reference/cli/state/) never hands a sequence out
twice.

Only the newest 100 states of the source, within the newest 500 commits of
each state ref, are considered.
