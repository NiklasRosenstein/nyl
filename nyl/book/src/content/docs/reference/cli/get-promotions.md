---
title: 'get states and promotions'
---

Three forms of `nyl get` inspect promotion: the recorded states of an
environment, how a
[PromotionPath](/nyl/orchestration/resource-guides/promotion-path/) judges its
source's states, and the promotion records an environment holds. They only
read state, never take a lease, and need no commit identity. What the evidence
levels mean and how a state is chosen is explained once in the
[promotion guide](/nyl/orchestration/promotion/).

```bash
nyl get states -e <env> [-o json|yaml|name] [--local] [path]
nyl get promotion-candidates <promotion-path> [-o json|yaml|name] [path]
nyl get promotions -e <env> [-o json|yaml|name] [--local] [path]
```

## get states

The environment's recorded states, newest first: each `reconcile` or
`recover` run's transition commit pair, with its desired commit (what
`nyl promote --state-revision` takes), its run, its observed commit, the
source commit it ran, which units have a current receipt in it, and the
attestations recorded for it. An attestation recorded for a state after a
later run replaced it still counts, as it does for `nyl promote`.

```console
$ nyl get states -e dev
f9f6ca7294ba9878a5b9041449f96fbdafc61b81
  run:          00000000-0000-7000-9000-000000000005
  observed:     4e4083d3b551972a3ac11e9ddded4b26f0a66d63
  source:       a58c4e7d30645b5ef02b69e9398ad41bfddf2471
  receipts:     svc, web-image
  attestations: none
a8311eba6cb32cce44edf9af5347360c801c3bb6
  run:          00000000-0000-7000-9000-000000000003
  observed:     2beefe4e2892e0d39d09469fe542f9042b85f88b
  source:       59bbc8e48f22deddcf69acb4f836af3480d4ac13
  receipts:     svc, web-image
  attestations:
    environment/qa pass at 2026-09-24T10:00:00Z: QA signed off release 1
```

`-o json` prints a `StatesReport`: `states[]` with `run`, `desiredCommit`,
`observedCommit`, `sourceCommit`, `sourceChosenBy`, `units[]` (`name`,
`kind`, `receipt`, `artifacts`), and `attestations[]` (`scope`, which is a unit
or `environment`, `name`, `result`, `at`, `reason`). `-o name` prints the
desired commits.

## get promotion-candidates

The path's source states, newest first, judged exactly as
[`nyl promote`](/nyl/reference/cli/promote/) judges them, with the units the
target selects as its entry worktree renders them. Each state shows the level
it reaches up to the path's own (`attested`, `published`, or `none`), what it
lacks below the path's level, and the values it would carry. `selected` marks
the state `nyl promote` without flags would take now, `promoted` the state
the path's record in the target names, and `running` the state the target
runs now (its last run applied that record and no newer one waits). When
`nyl promote` would refuse, the report says why, with its code, and still
exits 0.

A path that promotes only values takes each value from its own state, as
`nyl promote` does: each value is judged for its consumer (the Application
of the Release whose input it reads, or else the unit it reads), and a state
is `eligible` when it can supply some value. A publication's `accepted` and
`healthy` count only for its current receipt, so an older state whose
publication attestations passed shows them as `not-running`
([What runs now](/nyl/orchestration/promotion/#what-runs-now)). The text form adds each
value's level, and `-o json` lists it under `byValue`.

The candidates are judged on evidence only: artifacts are not verified, so a
candidate whose image is gone from the registry still shows as `selected`,
and `nyl promote` then refuses it with `NYL-PROMOTE-ARTIFACT-MISSING`.

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

`-o json` prints a `PromotionCandidatesReport`: `path`, `source`,
`environment` (the target), `evidence` (the path's level), `refusal` (the
finding `nyl promote` would refuse with, when it would), and `candidates[]`
with `run`, `desiredCommit`, `observedCommit`, `sourceCommit`, `level`,
`eligible`, `selected`, `promoted`, `running`, `values`, `byValue` (`level`
and `eligible` per value, for a value-only path), and `missing[]`: each with
`unit` (absent for an environment-wide attestation), `attestation`, `reason`
(`no-receipt`, `pending`, `aged`, `failing`, `not-running`, or `coverage`
for a `coverage` name no attestation lists), and
`message`.

A path `nyl promote` refuses before it reads state, such as an unknown path
(`NYL-PROMOTE-BINDING-UNKNOWN`), a form M6 does not support
(`NYL-PROMOTE-UNSUPPORTED`, also for a target with `record: source`), or a
path validation rejects, is refused here the same way, exit 1.

## get promotions

The PromotionRecords in the environment's desired state, highest sequence
first, the record whose source commit the environment's last run applied, the
record the next run takes its source from, each `fromPromotion` binding of
the environment's units with the record that supplies it, and every
promotion its desired history records, newest first, including records a
later promotion replaced. Environments that share a desired ref
(`spec.state.path`) list only their own promotions.

```console
$ nyl get promotions -e prod
dev-to-prod #2
  from:     dev run 00000000-0000-7000-9000-000000000006 (desired eed4c2359d485417a9882145d8ceb6d59378a406)
  source:   cf41525d1ed208e5fb4f7e804f78de250ef37102
  value webImage: registry.invalid:5000/web@sha256:bc69a88a97d841aac5366b9cdee643b6056aeadbac6ef1a03b9ee58bbeeb0805
  evidence: published
  requested by Scenario <scenario@example.invalid> at 2026-09-24T10:00:00Z: Release 2
history, newest first:
  #2 dev-to-prod f87ef955c1237c7576e8c9585135a45613d83138: Release 2
  #1 dev-to-prod 3bb91c9e97ff73a14be1f5c90f5db4f918944477: Release 1
```

A record the last run applied is marked `(applied)`. With several source
paths (`source.fromPromotion.paths`), `source from <path> #<sequence>` names
the newest record on them, which the next run takes its source commit from.
Under `bindings:` each binding is listed with its unit, its place in the
unit's spec (a DeploymentTarget's `releaseInputs` are in its publication
unit's spec), the value, and the supplying record: a binding without `path`
in an environment whose source is promoted follows the record that supplied
the source, the same choice a run makes, and `no record yet` means its unit
waits (`NYL-PROMOTION-MISSING`). The bindings are those of the units'
desired documents, as the last run rendered them.

`-o json` prints a `PromotionsReport`: `promotions[]`, each the exact record
state holds; `applied` (`path`, `sequence`); `source` (`path`, `sequence`);
`bindings[]` with `unit`, `pointer`, `value`, the binding's own `path`, and
`supplier` (`path`, `sequence`); and `history[]` with the `promote` commit,
`path`, `sequence`, `sourceCommit`, `from` (`environment`, `run`,
`desiredCommit`), `requestedBy`, and `reason`, read
from each commit's summary and trailers. `-o name` prints the paths.

## Options

| Option | Meaning |
| --- | --- |
| `-e <env>` | The environment whose state `states` and `promotions` read. |
| `--local` | Read this clone's local state branches (`states`, `promotions`). |
| `-o table\|json\|yaml\|name` | The output format; `wide` prints the text form. |

## History

Each form reads its state refs' history after one sync, then the documents of
each state from what that sync fetched, so the number of round trips to the
state remote does not grow with the history. `states` and
`promotion-candidates` list the newest 100 states, the ones `nyl promote`
considers.

## Exit codes

| Exit | When |
| --- | --- |
| 0 | The state was read, whatever it shows. A refusal of `nyl promote` (such as `NYL-PROMOTE-NO-EVIDENCE`) is shown in the candidates' `refusal` and still exits 0. |
| 1 | The state cannot be read (for example `NYL-STATE-MISSING`), or the path is refused as `nyl promote` refuses it. |
