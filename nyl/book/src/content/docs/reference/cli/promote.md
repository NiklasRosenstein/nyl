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
| `--dry-run` | Take no lease and write nothing; report the chosen state and the record it would write, under a top-level `record` in `-o json`. Exits as the promotion would. |
| `--value <name>` | Promote only this value (repeatable) of a path that promotes values; the others stay at their recorded lineage. A path that promotes the target's source moves its source commit and every value together, so it refuses `--value`. |
| `--reason <text>` | Why, recorded as `requested.reason` and the commit's `Nyl-Reason`. Name the cause of a rollback or an override. |
| `--revision <commit>` | Take the newest qualifying state of the source that ran this source commit: a full commit ID, or a revision this clone can resolve. A commit the clone lacks is fetched like a recorded source commit. |
| `--state-revision <commit>` | Take exactly this recorded state, by its desired state commit (`git log` of the source's desired ref). |
| `--evidence published` | Accept a state below the path's `attested` level, such as to roll back. The record carries an override and the command warns with `NYL-PROMOTE-EVIDENCE-OVERRIDE`. It never clears a failing attestation. |
| `--ignore-attestation <scope>/<name>=<reason>` | Set a failing attestation aside (repeatable); `<scope>` is a unit name or `environment`. The record keeps the reason and who gave it. |
| `--no-verify-artifacts` | Do not check that promoted artifacts still exist, for runners without registry access; the record says `artifactsVerified: false`. A path's `verifyArtifacts: false` does the same. |
| `--wait-lease <duration>` | Wait up to this long for another run's lease on the target. |
| `--author "Name <email>"` | Author of the state commit; defaults to Git's `user.name` and `user.email`. |
| `-o json` | The operation's typed `PromoteReport`. |

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
- `NYL-PROMOTE-REBUILT`: the target also selects a unit whose result a promoted
  value replaces, so it builds its own and the binding overrides it.

## Exit codes

| Exit | When |
| --- | --- |
| 0 | The record was written, or the path's record already names the chosen state (`unchanged`), or the dry run found a promotion to make. |
| 1 | Configuration error: an invalid path or selector, an unknown `--value`, a missing artifact (`NYL-PROMOTE-ARTIFACT-MISSING`), a form M6 does not support (`NYL-PROMOTE-UNSUPPORTED`), or an unreadable commit. |
| 2 | Nothing was written because evidence is missing, stale, or failing (`NYL-PROMOTE-NO-EVIDENCE`, the fix says what is missing), or the target's lease is held (`NYL-LEASE-HELD`). |

Run [`nyl reconcile`](/nyl/reference/cli/reconcile/) on the target afterwards:
an environment that takes its source from a promotion exits 2 with
`NYL-PROMOTION-MISSING` until its path has a record.
