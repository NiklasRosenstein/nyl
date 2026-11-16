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
and `promotions`, and `status` lineage work today. `record: source`,
`hotfix` paths with `--supersede`, and value-only coverage follow.
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
  `record: source` is not supported yet.
- **Bindings.** `{fromPromotion: {value: webImage}}` in a unit's spec, or in a
  DeploymentTarget's `releaseInputs`, resolves through the one binding
  resolver. With `path` it reads that path's record; without it, the newest
  record carrying the value (in a promoted environment, only records on its
  source paths, so the commit and the values proven with it move together). A
  binding with no record yet blocks its unit, and the reason names
  `nyl promote`.

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
  refuse), and the state the path's record already names.
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
bc1847b0292df2e0548131fe07c6bfd71777a2a6 published (promoted)
  run:     00000000-0000-7000-9000-000000000003
  source:  c966aa0c15d9c431c15728aec36d32e7adef056e
  value webImage: registry.invalid:5000/web@sha256:88e3d32b27feb6648d7e9ce25a18b1124b27849a11e8f9d5d0c1d70767567a75
```

## How a promotion reads the source

`nyl promote` reads the source environment's state history in Git: the desired
and observed logs, the documents as of each state, and the attestations
recorded for each state, found by walking the observed history from the state's
commit to the tip. It never takes the source's lease. The record's `sequence` is
one above the highest ever recorded into the target, taken from the target's
desired history, so restoring older state with
[`nyl state restore`](/nyl/reference/cli/state/) never hands a sequence out
twice.

Only the newest 100 states of the source, within the newest 500 commits of
each state ref, are considered.
