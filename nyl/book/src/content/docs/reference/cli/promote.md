---
title: 'promote'
---

`nyl promote` records what one environment proved as a `PromotionRecord` in
another environment's desired state, or with `record: source` in that
Environment's source, along a
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

## `record: source`

When the path promotes the source of a target whose Environment says
`record: source`, the record is written into that Environment's
`source.fromPromotion.promoted` block instead, as one commit on the branch
the entry worktree has checked out, with the same message a state commit
would carry (kept verbatim) and `--author` as its author and committer. Your
Git configuration otherwise applies, so a configured commit signature signs
it. The commit carries only the Environment's file and is pushed to the
branch's upstream (`branch.<name>.remote` and `branch.<name>.merge`)
fast-forward only; the push is never forced. The report lists the commit
under `commits.source`. After the push, under the target's lease, one
marker commit goes into the target's desired state, listed under
`commits.desired`: the same message, and no record file. It reserves the
sequence and lists the promotion in
[`nyl get promotions`](/nyl/reference/cli/get-promotions/#get-promotions),
and runs read nothing from it. When the marker cannot be written, the
promotion stands (the block was pushed): the report says `done`, and the
command exits 1 with `NYL-PROMOTE-MARKER-UNWRITTEN`. Update the checkout
and run it again: while the block still names the state it would promote,
it is `unchanged` and only the marker is written, under the re-runner's
`Nyl-Requested-By` and `Nyl-Reason` (if a newer state qualifies by then, it
promotes that instead). A marker is written for an existing block only
when the branch is not ahead of its upstream and the Environment's file is
unchanged; otherwise `NYL-PROMOTE-SOURCE-UNWRITABLE`. With
`source.fromPromotion.verify`, the commit must be signed with SSH by a key
the allowed signers file lists: `nyl promote` (and `--dry-run`) checks that
commit signing is configured, and verifies the new commit before the push,
undoing it with `NYL-PROMOTE-SOURCE-UNWRITABLE` when a run would not trust
it ([Trusting the block by its signature](/nyl/orchestration/promotion/#trusting-the-block-by-its-signature)). The
block in the checkout is the path's current record for
[Unchanged](#unchanged); records in state on that path never count. A block
a run would refuse, invalid or older than what prod applied, refuses the
promotion the same way (`NYL-PROMOTION-RECORD-INVALID`,
`NYL-PROMOTION-SEQUENCE-OLDER`): update the checkout first.
`--dry-run` checks everything below without committing. The
[promotion guide](/nyl/orchestration/promotion/#keeping-the-record-in-source)
explains how runs read the block.

Each refusal exits 1 with `NYL-PROMOTE-SOURCE-UNWRITABLE` and its own fix,
except a moved branch:

- a detached HEAD: check out the branch the record belongs on;
- a branch without an upstream: set one with `git branch --set-upstream-to`;
- a branch with commits its upstream lacks, which the push would publish
  with the record: push or drop them first;
- an untracked Environment file, or uncommitted changes to it;
- a `fromPromotion` the Environment inherits (such as from its group), or
  one not in block style: declare it in the Environment, one key per line;
- an Environment no file declares, such as a template's instance;
- a push the remote refuses for another reason, such as branch protection or
  a hook: the commit is undone; promoting through a pull request
  (`changeGate: pullRequest`) comes in a later milestone;
- a push that never started, because a `[[git.credentials]]` entry cannot
  be used or `git` cannot run: the commit is undone; fix the credential or
  the tool.

A push refused because the branch moved since the checkout undoes the local
commit and exits 2 with `NYL-PROMOTE-SOURCE-REJECTED`: update the checkout
(`git pull --ff-only`) and run `nyl promote` again. The undo moves the branch
back only while it still points at the promotion commit, and restores the
file only while it holds what was written; an undo that cannot finish is
reported with the commit to drop.

A push that fails without a refusal, such as a network error after the
upload, may or may not have reached the remote. The commit stays on the
local branch, no marker is written into the target's state, and the command
exits 2 with `NYL-PROMOTE-SOURCE-UNCERTAIN`: nothing is known to have
failed, and running it again is safe. Fetch the branch, check whether the
commit reached the remote, and run `nyl promote <path>` again. When it did,
the block is `unchanged` and only the marker is written; when it did not,
push the commit (`git push`) or drop it (`git reset --keep <commit>~1`)
first, since a branch ahead of its upstream is refused.

## Options

| Option | Meaning |
| --- | --- |
| `--dry-run` | Take no lease and write nothing; report the chosen state and the record it would write, under a top-level `record` in `-o json`, with `outcome: done` and `dryRun: true` (`unchanged` when nothing would be written). Exits as the promotion would. |
| `--value <name>` | Promote only this value (repeatable) of a path that promotes values; the others stay at their recorded lineage, each naming the state it came from. A path that promotes the target's source moves its source commit and every value together, so it refuses `--value` with `NYL-PROMOTE-SELECTOR-INVALID`, as it does a name the path does not declare. |
| `--reason <text>` | Why, recorded as `requested.reason` and the commit's `Nyl-Reason`. Name the cause of a rollback or an override. |
| `--revision <commit>` | Take the newest qualifying state of the source that ran this source commit: a full 40-character commit ID, which is matched against the states as given and not fetched (the commit is fetched when the target reconciles at it), or a branch or tag name this clone resolves. A short SHA is rejected. |
| `--state-revision <commit>` | Take exactly this recorded state, by its desired state commit, as [`nyl get states -e <source>`](/nyl/reference/cli/get-promotions/#get-states) lists it. |
| `--evidence published` | Accept a state below the path's `attested` level, such as to roll back. The record carries an override and the command warns with `NYL-PROMOTE-EVIDENCE-OVERRIDE`. It never clears a failing attestation. |
| `--ignore-attestation <scope>/<name>=<reason>` | Set a failing attestation aside (repeatable); `<scope>` is a unit name or `environment`. The record keeps the reason and who gave it. |
| `--supersede [<repository>:]<commit or range>[=<reason>]` | Accept that the promotion replaces missing commits of another path's record (repeatable); see [Superseding](#superseding). An entry without its own reason takes `--reason`; one with neither exits 1 with `NYL-PROMOTE-SUPERSEDE-REASON`. |
| `--no-verify-artifacts` | Do not check that promoted artifacts still exist, for runners without registry access; the record says `artifactsVerified: false`. A path's `verifyArtifacts: false` does the same. |
| `--wait-lease <duration>` | Wait up to this long for another run's lease on the target. |
| `--author "Name <email>"` | Author of the state commit, or of the source commit with `record: source`; defaults to Git's `user.name` and `user.email`. |
| `-o json` | The operation's typed `PromoteReport`. |

## Which units the target selects

The whole-source rule covers every unit the target environment also selects.
A target whose source is promoted along the path runs the chosen state's
source commit, so `nyl promote` reads that set from the target's own
definitions at each candidate state's source commit, whatever the entry
worktree has checked out. It reads them newest state first and stops once
the choice is settled: each source commit it needs is checked out and the
target rendered there once, so promoting the newest state renders one
commit, not one per recorded state. A commit at which the target's definitions do not
render, such as one from before the target was declared, keeps its states
out, and the refusal names why. So does a commit this clone cannot check out
or fetch, such as one of a deleted branch; but when that passes over a state
newer than the one `nyl promote` would take, the promotion is refused with
`NYL-SOURCE-UNAVAILABLE` instead, so an unreadable commit never promotes an
older state. A target that
runs its own source, such as one that only binds promoted values, is judged
by the units its entry worktree selects.

## Which state

Without `--revision` or `--state-revision`, the state is the newest whose
evidence reaches the path's level. A publication's `accepted` and `healthy`
count only for its current receipt: once a newer receipt is published, an
older state no longer qualifies through them, and a newest one that fails
refuses the promotion without falling back to an older state that passed
(the [promotion guide](/nyl/orchestration/promotion/#what-runs-now) explains
why). The refusal names the older state that would otherwise qualify, and
`--state-revision` promotes it on the attestations recorded for it. On a
path that promotes only values, each value comes from the state its
consuming Application runs, as the publication's current attestations show
it ([Value-only paths](/nyl/orchestration/promotion/#value-only-paths)), and
`coverage` narrows or extends the Applications judged
([Coverage](/nyl/orchestration/promotion/#coverage)).

## Unchanged

When the chosen state and values are what the path's record already names, and
that record is the newest on the target, `nyl promote` writes nothing and exits
0 with `outcome: unchanged`, so a promote job can be repeated. After another
path recorded in between, or when the source has a newer state, a promotion is
a promotion like any other and takes the next sequence.

## Superseding

An environment can take its source, and values, from several paths
(`source.fromPromotion.paths`), such as `dev-to-prod` and a hotfix path.
Before it writes, `nyl promote` checks that what it promotes keeps what the
other paths promoted: for the source commit and each value, every record
another path wrote into the target since this path last promoted it must be
contained in the promoted commit. A commit contains another when every
commit of the other's history it lacks, merge commits aside, has a commit
with the same changes in its own history since their merge base, as
`git cherry` decides: an ancestor counts, and so do clean cherry-picks and a
squash of a single commit; a squash of several commits counts as none of
them. Commits are compared within their own repository, paired by the
source field they came from: a value's source commits are those its
producing unit resolved, such as an `OciImage`'s contexts. The records are
the target's current ones and those in its desired history; with
`record: source` only the current block counts.

A promotion that would drop commits writes nothing and exits 2 with
`NYL-PROMOTE-SUPERSEDES`, so a promote job succeeds once the commits are
merged. The message names each missing commit by subject, oldest first, and
the `--supersede` entries to copy, with full commit IDs; `-o json` lists
them under `missing` (`commits`, and `supersede` with the entries). Merge the
commits into the source and promote a state that runs them, or accept the
loss with `--supersede`:

```bash
nyl promote dev-to-prod --supersede '<oldest missing>^..<record commit>=Squash-merged into main' --reason "Release 3"
```

An entry is a commit or a Git range, resolved when the promotion runs; a
commit ID may be abbreviated and take `^` and `~` suffixes. Commits it names
that are not missing are ignored, and a missing commit no entry covers keeps
the promotion refused. An unqualified entry belongs to the repository whose
missing commits it names; when it names missing commits in several, qualify
it with the repository: `--supersede web:<range>=<reason>`. A repository is
named by the GitRepository whose URL matches its URL, else by its normalized
URL (no credentials, scheme, or trailing `.git`; `git@host:org/r` reads
`host/org/r`); the project's own repository by its `origin` URL, or
`project` without one. The record lists every superseded commit on its own,
with that repository name, the reason, and who gave it, never the range.
Once a path has promoted, its next promotion is no longer checked against
the records it superseded.

The check needs the whole history: in a shallow clone it exits 1 with
`NYL-PROMOTE-SHALLOW-CLONE`; fetch with `git fetch --unshallow`, or
`fetch-depth: 0` with `actions/checkout`. Commits the clone lacks are fetched
like a recorded source commit. A record whose commit cannot be fetched, such
as from a deleted branch, is not a refusal: the promotion warns with
`NYL-PROMOTE-UNCHECKED`, naming the record, and `--supersede <full commit
ID>=<reason>` naming that commit records that the promotion replaces it. A
value without a known source commit, such as an image built outside Nyl or a
command output, cannot be checked either and warns the same way; `nyl plan`
of the target lists such values before any promotion, for each value two
paths into it promote.

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
- `NYL-PROMOTE-UNCHECKED`: a value, or another path's record of it, has no
  known source commit, or a record's commit cannot be fetched, so the
  [supersede check](#superseding) cannot check it.

## Exit codes

| Exit | When |
| --- | --- |
| 0 | The record was written, or the path's record already names the chosen state (`unchanged`), or the dry run found a promotion to make. |
| 1 | A `record: source` commit was pushed but its marker in the target's state was not (`NYL-PROMOTE-MARKER-UNWRITTEN`; the promotion stands, run the command again). Configuration error: an invalid path or selector, a refused or unknown `--value`, a missing artifact (`NYL-PROMOTE-ARTIFACT-MISSING`), a form M6 does not support (`NYL-PROMOTE-UNSUPPORTED`), a `--supersede` entry with neither its own reason nor `--reason` (`NYL-PROMOTE-SUPERSEDE-REASON`), a shallow clone the supersede check cannot use (`NYL-PROMOTE-SHALLOW-CLONE`), a `record: source` block a run would refuse (`NYL-PROMOTION-RECORD-INVALID`, `NYL-PROMOTION-SEQUENCE-OLDER`), an unreadable commit or a `--supersede` entry that names none (`NYL-SOURCE-UNAVAILABLE`, also when a newer source state's commit cannot be checked out to read which units the target selects), or a `record: source` block that cannot be committed or pushed (`NYL-PROMOTE-SOURCE-UNWRITABLE`, see [`record: source`](#record-source)). |
| 2 | Nothing was written to the target's state because evidence is missing, stale, failing, or no longer describes what runs (`NYL-PROMOTE-NO-EVIDENCE`, the fix says what is missing), the promotion would drop commits another path promoted (`NYL-PROMOTE-SUPERSEDES`, see [Superseding](#superseding)), the target's lease is held (`NYL-LEASE-HELD`), the remote refused the push of a `record: source` commit because the branch moved (`NYL-PROMOTE-SOURCE-REJECTED`), or that push failed without a refusal and may have reached the remote (`NYL-PROMOTE-SOURCE-UNCERTAIN`; no marker was written, run the command again once you know). |

Run [`nyl reconcile`](/nyl/reference/cli/reconcile/) on the target afterwards:
an environment that takes its source from a promotion exits 2 with
`NYL-PROMOTION-MISSING` until its path has a record.
