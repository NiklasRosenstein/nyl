---
title: 'promote'
---

`nyl promote` records what one environment proved as a `PromotionRecord` in
another environment's desired state, along a
[PromotionPath](/nyl/orchestration/resource-guides/promotion-path/). It reads
the source's recorded states and writes nothing to the source; it never
observes anything itself, so it runs from a workstation without cluster
credentials. How promotion works, and what the evidence levels mean, is
explained once in the [promotion guide](/nyl/orchestration/promotion/).

```bash
nyl promote dev-to-prod [options] [path]
```

```console
$ nyl promote dev-to-prod --reason "Release 1"
promote dev-to-prod: dev -> prod: promoted
  sequence: 1
  from: dev run 0192f0c4-8d0a-7c3e-9a51-3c1f0e7d2b90 (desired 43336cb1f683145875596fb562b3e83c207b2020)
  source commit: 138ebcac3476b60dc07f6a2079dd4a9178449453
  value webImage: registry.example.com/web@sha256:6467a827e54c18b6354f509ca95c8a8722786a511484a1d09543bc97e6831d05
  evidence: attested
    environment/qa
  artifacts verified: true
  desired: a6d7f81b83c462d9c82c9d009aea352d70f01425
```

The command runs under the **target** environment's lease, like every
command that writes state, and writes one desired commit with the operation
`promote`: the record at `desired/promotions/<path>.yaml` and a summary naming
the path, the sequence, and the source state. `--wait-lease` waits for a
running reconcile; a lease that stays held is exit 2.

## Options

| Option | Meaning |
| --- | --- |
| `--dry-run` | Take no lease and write nothing; report the chosen state and the record it would write, under a top-level `record` in `-o json`, with `outcome: done` and `dryRun: true` (`unchanged` when nothing would be written). Exits as the promotion would. |
| `--value <name>` | Promote only this value (repeatable) of a path that promotes values; the others stay at their recorded lineage, each naming the state it came from. A path that promotes the target's source moves its source commit and every value together, so it refuses `--value` with `NYL-PROMOTE-SELECTOR-INVALID`, as it does a name the path does not declare. |
| `--reason <text>` | Why, recorded as `requested.reason` and the commit's `Nyl-Reason`. Name the cause of a rollback or an override. |
| `--revision <commit>` | Take the newest qualifying state of the source that ran this source commit: a full 40-character commit ID, which is matched against the states as given and not fetched (the commit is fetched when the target reconciles at it), or a branch or tag name this clone resolves. A short SHA is rejected. |
| `--state-revision <commit>` | Take exactly this recorded state, by its desired state commit (`git log` of the source's desired ref). |
| `--evidence published` | Accept a state below the path's `attested` level, such as to roll back. The record carries an override and the command warns with `NYL-PROMOTE-EVIDENCE-OVERRIDE`. It never clears a failing attestation. |
| `--ignore-attestation <scope>/<name>=<reason>` | Set a failing attestation aside (repeatable); `<scope>` is a unit name or `environment`. The record keeps the reason and who gave it. |
| `--no-verify-artifacts` | Do not check that promoted artifacts still exist, for runners without registry access; the record says `artifactsVerified: false`. A path's `verifyArtifacts: false` does the same. |
| `--wait-lease <duration>` | Wait up to this long for another run's lease on the target. |
| `--author "Name <email>"` | Author of the state commit; defaults to Git's `user.name` and `user.email`. |
| `-o json` | The operation's typed `PromoteReport`. |

## Which units the target selects

The whole-source rule covers every unit the target environment also selects.
`nyl promote` takes that set from the target as the entry worktree renders it
now, not as it renders at each candidate state's source commit. A unit added to
or removed from the target's selection in source that is not yet checked out is
judged when the checkout has it.

## Unchanged

When the chosen state and values are what the path's record already names, and
that record is the newest on the target, `nyl promote` writes nothing and exits
0 with `outcome: unchanged`, so a promote job can be repeated. After another
path recorded in between, or when the source has a newer state, a promotion is
a promotion like any other and takes the next sequence.

## Artifacts

A value that reads an artifact is checked before it is recorded: the driver of
the unit that produced it says whether the artifact still exists (for an
`OciImage`, `docker buildx imagetools inspect` of the recorded digest). A
missing artifact exits 1 with `NYL-PROMOTE-ARTIFACT-MISSING`, and nothing is
written. A kind that cannot verify artifacts is a warning,
`NYL-PROMOTE-ARTIFACT-UNVERIFIED`, and the record says `artifactsVerified:
false`.

## Warnings

- `NYL-PROMOTE-EVIDENCE-OVERRIDE`: `--evidence published` took a state below
  the path's level, or an `--ignore-attestation` entry set nothing aside.
- `NYL-PROMOTE-ARTIFACT-UNVERIFIED`: an artifact's kind cannot verify it.
- `NYL-PROMOTE-REBUILT`: the target builds a unit and also binds a value that
  unit supplies, so one result is built and the other used. Only a target that
  does both is warned about.

## Exit codes

| Exit | When |
| --- | --- |
| 0 | The record was written, or the path's record already names the chosen state (`unchanged`), or the dry run found a promotion to make. |
| 1 | Configuration error: an invalid path or selector, a refused or unknown `--value`, a target with `record: source` (`NYL-PROMOTE-UNSUPPORTED`), a missing artifact (`NYL-PROMOTE-ARTIFACT-MISSING`), a form M6 does not support (`NYL-PROMOTE-UNSUPPORTED`), or an unreadable commit. |
| 2 | Nothing was written because evidence is missing, stale, or failing (`NYL-PROMOTE-NO-EVIDENCE`, the fix says what is missing), or the target's lease is held (`NYL-LEASE-HELD`). |

Run [`nyl reconcile`](/nyl/reference/cli/reconcile/) on the target afterwards:
an environment that takes its source from a promotion exits 2 with
`NYL-PROMOTION-MISSING` until its path has a record.
