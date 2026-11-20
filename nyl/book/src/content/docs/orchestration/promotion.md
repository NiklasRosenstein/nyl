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
M6 delivers promotion in steps. `nyl promote`, `promotion` sources and `$promotion`
bindings, artifact verification, `nyl get states`, `promotion-candidates`,
and `promotions`, `status` lineage, `record: source`, several paths into
one environment with the supersede check and hotfixes, value-only paths
judged per deployment, and `coverage` work today.
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
A publication's `accepted` and `healthy` say what runs now, so they count
only for its current receipt ([below](#what-runs-now)).
`--revision` and `--state-revision` choose a state, and rolling back is
promoting an older one:

```bash
nyl promote dev-to-prod --revision 3e7b9c… --evidence published --reason "Roll back release 2"
```

## Types checked before promotion

`nyl validate`, `nyl plan`, `nyl reconcile`, `nyl promote`, and `nyl get
promotion-candidates` compare the type of each promoted value with every place
that reads it, before any promotion runs. A value's type comes from what its
selector reads: a rendered literal, a declared output, the kind's schema, or
an artifact's schema. A place is a field of a unit's spec, typed by its
kind's schema, or a Release input a DeploymentTarget's `releaseInputs` fills
with `$promotion`, typed by the Release's `spec.inputs.<name>.type`. A
mismatch is `NYL-PROMOTE-VALUE-TYPE`:

```yaml
releaseInputs:
  web/web:
    image: {$promotion: {value: webImage}}   # Release web declares image: {type: string}
```

The Release's declaration is read from the ApplicationGroups the target
selects, without rendering. A remote group source is fetched at its pinned
commit, or read from the Git cache when the fetch fails. `nyl validate
--offline` and `nyl reconcile --local` read the cache only, so a remote group
never fetched before leaves its inputs unchecked
(`NYL-PROMOTE-VALUE-TYPE-UNCHECKED`): an error in `nyl validate`, a warning in
the other commands. Resolution still checks the promoted value against the
declaration when the publication unit runs, so an unchecked input is never
deployed with the wrong type.

## What runs now

A KubernetesPublication that declares `observe: {}` attests `accepted` and
`healthy` by reading what Argo CD (or the direct reconciler) runs. Those
attestations describe the present, not one receipt's past, so promotion
counts them only for the publication's current receipt:

- An older state does not qualify through its publication attestations once
  a newer receipt of the publication exists, even if they passed.
- When the newest attestation fails, such as when deployments still run the
  previous publication or run different ones, `nyl promote` refuses with
  `NYL-PROMOTE-NO-EVIDENCE` and the driver's reason. It does not fall back to
  an older state that once passed.
- Environment-wide attestations, external ones recorded with
  `nyl attest --name`, and the attestations of other units name a state or a
  receipt, so for them the newest state that passed still wins.
- An explicitly chosen state, with `--state-revision` or `--revision`, is
  judged on the attestations recorded for it.

The cost: between publishing a new receipt and its deployments running it,
the state they still run cannot be promoted without flags. The refusal names
that state, and `--state-revision` promotes it:

```bash
nyl promote dev-to-prod --state-revision <desired commit> --reason "Release 5, still running"
```

A publication without `observe` has no reader: it declares no attestations,
its receipt is its evidence, and none of this applies.

## Value-only paths

A path whose target does not take its source from it promotes values only,
and takes each value from the state its consumer runs. A unit's driver says
which of the unit's deployments consumes an input: for a
KubernetesPublication, a value selected as a Release input, `{unit:
kubernetes, input: /releases/<group>~1<release>/image}`, is consumed by that
Release's deployment, its Argo CD Application (or the Release itself on a
direct target). Any other selector (an artifact, an output, another input)
is consumed by the selected unit itself.

For a value consumed by a deployment, `nyl promote` reads the unit's current
`accepted` and `healthy` per deployment: the value comes from the state
whose receipt published what that deployment runs, so deployments that were
synced to different publications by hand do not block, and the record names
a value's own `state` when it differs from `from`. Only the consuming
deployment's own findings block its value: it runs a revision no
publication explains (`unmatched`), a mix of revisions, nothing, or a recorded
`healthy` fails for it (even when the path requires only `accepted`, as any
failing attestation blocks). A path that promotes the target's source still
needs one state behind every value, so the same mix refuses it.

Following the consumer is how a value-only path picks a state, not an
evidence requirement: it applies at `evidence: published` and under
`--evidence published` too, whenever the publication's current receipt has a
recorded `accepted` or `healthy`. A state is matched by the publication commit
it published, which may be the commit the reading matched or the revision
the deployment reports running. When no recorded state published what a
deployment runs, such as after the history was cut short or a publication
outside orchestration, the promotion is refused with "no recorded state
published what <deployment> runs (<commit>)": promote a state explicitly
with `--state-revision`, or reconcile the source so it publishes again.

## Coverage

A publication unit has one receipt and one publication commit, but runs as
several deployments, each synced on its own. `coverage` says, per unit,
which of its deployments a promotion judges that unit's running-scoped
attestations (`accepted` and `healthy`) for. It never changes what moves.
Entries are keyed by the unit's name and name deployments and groups as the
unit's kind lists them:

```yaml
spec:
  coverage:
    kubernetes:                 # the publication unit
      groups: [web]             # source paths: count only these groups
      exclude: [web-batch]      # knowingly accepted as falling short; listed in each record
      require: [web-gateway]    # must meet the attestations at whatever they run
```

For a KubernetesPublication, a deployment is an Argo CD Application the
target generates, named as the attestation details name it (its name,
qualified by its namespace outside the Argo CD instance's), or a Release,
`<group>/<release>`, on a direct target; a group is the ApplicationGroup of
its Release. The catalog Application belongs to no group.

- `groups` and `exclude` narrow a source path's judgement of their unit: a
  failing `accepted` or `healthy` that only deployments outside them explain
  does not block. An entry counts only for the unit it is keyed by.
- `groups` applies only to source paths. A value-only path judges each value
  for its own consumer, so `nyl validate` refuses `groups` there
  (`NYL-PROMOTE-COVERAGE-INEFFECTIVE`); use `exclude` instead. Coverage of a
  unit that records nothing per deployment, such as a KubernetesPublication
  without `observe: {}`, is refused the same way.
- On a value-only path, an excluded consumer must still run a publication of
  the source, which decides its value's state, but need not be healthy.
- `require` adds deployments that contribute no value but must meet the
  required attestations at whatever publication they run.
- `nyl validate`, `nyl plan`, `nyl reconcile`, `nyl promote`, and `nyl get promotion-candidates` check every name against what the unit runs, as its kind lists
  it for the worktree (a KubernetesPublication renders its target): a unit
  the source does not declare, select, or enable, a unit whose kind runs no
  deployments, and a group or deployment the unit does not run are refused
  with `NYL-PROMOTE-COVERAGE-UNKNOWN`, which lists the names the unit has.
  When the names cannot be listed, such as when the source's target does not
  render, `nyl validate` fails with `NYL-PROMOTE-COVERAGE-UNCHECKED` and the
  render error, and the other commands warn with it and carry on.
- Coverage still never fails open when a promotion runs: a group no
  deployment of the attestation belongs to, a coverage that leaves no
  deployment to judge while the attestation fails, and a `require` name no
  attestation of the unit lists (also for a publication without `observe`,
  and on a path that requires none of its attestations) refuse the
  promotion and name the field.

## Reading what was promoted

- **A promoted source.** An Environment with
  `source: {promotion: {path: dev-to-prod}}` runs its units at the source
  commit of the newest record on its path (or `paths`). The record's path and
  sequence are written to the environment record as `sourceChosenBy` and
  `appliedPromotionSequence`. Before the first promotion `reconcile` and `plan`
  exit 2 with `NYL-PROMOTION-MISSING`; its fix names `nyl promote <path>`.
  With `record: source` the record lives in the Environment itself, as
  described [below](#keeping-the-record-in-source).
- **Bindings.** `{$promotion: {value: webImage}}` in a unit's spec, or in a
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
    promotion: {paths: [dev-to-prod, hotfix-to-prod]}
  protectedRefs: [main, 'release/*']
---
apiVersion: gitops.nyl/v1
kind: Environment
metadata: {name: prod-hotfix}
spec:
  source: {revision: release/prod}     # cut from what prod runs
  unitSelector: {matchLabels: {hotfix: 'true'}}   # the image and a publication to a target on a Cluster or Argo CD instance of its own
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
  A state whose publication attestations no longer describe what runs shows
  `not-running`.
  A value-only path is judged per value, as `nyl promote` takes each value
  from its own state. Artifacts are not verified here.
  `nyl promote dev-to-prod --dry-run` then shows the exact record.
- `nyl get promotions -e prod` lists prod's PromotionRecords, the one its
  last run applied, and every earlier promotion from prod's desired history.
- `nyl status -e prod` shows the lineage of what prod runs: the path and
  sequence of the applied record, the source commit, and the source state it
  came from, and a newer record waiting for the next reconcile. `nyl status
  -e dev` lists every attestation that fails for what dev runs
  (`NYL-ATTESTATION-FAILED`, a warning with the attestation's reason), so a
  failure that blocks promotion shows before anyone promotes.
- `nyl plan -e prod` warns with `NYL-PROMOTE-UNCHECKED` about each value two
  paths into prod promote that has no known source commit, such as a
  command's output: the supersede check cannot compare it.

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
    promotion:
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
  fast-forward only. Then, under prod's lease, it writes one marker commit
  into prod's desired state with the same message and no record file: it
  changes no document, so runs, `status`, and the lineage read nothing
  from it, but its summary reserves the sequence (a checkout of another
  branch, without the block, takes the next number) and lists the
  promotion in `nyl get promotions`. If the marker cannot be written after
  the push, the promotion stands, and `nyl promote` exits 1 with
  `NYL-PROMOTE-MARKER-UNWRITTEN`. Update the checkout to the pushed commit
  and run it again: while the block still names the state it would
  promote, it is `unchanged` and only the marker is written, carrying the
  re-runner's `Nyl-Requested-By` and `Nyl-Reason`; if a newer state
  qualifies by then, it promotes that instead, with a marker of its own. A
  marker is written for an existing block only when the checked-out branch
  is not ahead of its upstream and the Environment's file is unchanged, so
  the block is the one the upstream carries
  (`NYL-PROMOTE-SOURCE-UNWRITABLE` otherwise). Write
  `promotion` in the Environment itself, in block style, one key per
  line; the rest of the file stays as written. A detached HEAD, a branch
  ahead of its upstream, uncommitted changes to the Environment's file, a
  push the remote refuses for branch protection, or a push that never
  started (an unusable credential, a `git` that cannot run) exit 1 with
  `NYL-PROMOTE-SOURCE-UNWRITABLE`, each with its own fix. A push refused
  because the branch moved since the checkout writes nothing and exits 2
  with `NYL-PROMOTE-SOURCE-REJECTED`: update the checkout and promote again.
  A push that fails without a refusal, such as a network error, may have
  reached the remote: the commit stays on the local branch, no marker is
  written, since the record is not known to be published, and `nyl
  promote` exits 2 with `NYL-PROMOTE-SOURCE-UNCERTAIN`. Fetch the branch,
  check whether the commit arrived (push or drop it if not), and run it
  again: the re-run finds the block `unchanged` and writes the marker, or
  promotes again. A promotion is never forced. The [command
  page](/nyl/reference/cli/promote/#record-source) lists every case.
- **Reading.** Every command that resolves prod (`plan`, `reconcile`,
  `build`, and the multi-environment runs) reads the block as the entry
  worktree declares it, like the rest of `source`, and checks a block no run
  applied yet against its PromotionPath (its source environment, values, and
  selectors) and the state of dev it names (its run, source commit, and
  every value read again from that state). A block that does not match, or
  names a state dev's history does not hold, is refused with
  `NYL-PROMOTION-RECORD-INVALID`. That check reads dev's state, so a run of
  prod needs read access to dev's state repository, unless the block is
  trusted by its signature (below). `status` and `nyl get` show the block
  and refuse nothing. `source-locks` never touches the block.
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
  promotions` lists every promotion in its history through the markers.

### Trusting the block by its signature

Without more, a run trusts a new block only after reading dev's state. When
prod's runners should not read dev's state, or the check should rest on who
wrote the block, name a Git `allowed_signers` file:

```yaml
source:
  promotion:
    path: dev-to-prod
    record: source
    verify:
      allowedSigners: .nyl/allowed_signers   # a local path of the project
```

```text
# .nyl/allowed_signers
release-bot@example.com ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAA…
```

A run then finds the commit that last changed the block (`git log -L`
over the block's lines, from the checked-out commit; through a merge, the
side commit that changed them, not the merge) and verifies it with
`git -c gpg.ssh.allowedSignersFile=<file> verify-commit`. When the
signature holds, the block is checked against its PromotionPath and
trusted, and dev's state is not read.

- **Prerequisites.** Git 2.34 or newer and OpenSSH 8.2 or newer
  (`ssh-keygen`) on every runner that runs prod or promotes into it, and a
  full clone: a shallow clone cannot tell the commit that changed the block
  from its boundary commit, so it is refused (`fetch-depth: 0` with
  actions/checkout, or `git fetch --unshallow`). Replace objects are
  ignored.
- **Promoting.** The job that runs `nyl promote` signs its commits with a
  key the file lists (`git config commit.gpgsign true`, `gpg.format ssh`,
  `user.signingkey <key>`), since the block commit uses your Git
  configuration. `nyl promote` checks that signing is configured (also on
  `--dry-run`), and verifies its own commit before pushing it: a commit a
  run would not trust is undone and never pushed
  (`NYL-PROMOTE-SOURCE-UNWRITABLE`).
- **Refusals.** A run refuses with `NYL-PROMOTION-RECORD-UNSIGNED` (exit 1)
  when that commit is unsigned, signed by a key the file does not list, or
  signed with OpenPGP or X.509 (only an SSH signature header counts, and
  no keyring is consulted), and when the clone is shallow, the
  Environment's file differs from what the checked-out commit carries, the
  allowed signers file is missing, untracked, or differs from what the checked-out commit carries, or the tools are missing. A later hand
  edit inside the block is the block's last change and needs a signature
  of its own; an edit elsewhere in the file does not.
- **Key validity.** `valid-after` and `valid-before` options in the allowed
  signers file are checked against the commit's committer date, which the
  signer chose, so they do not stop a signer who backdates a commit. To
  withdraw a key, remove it from the file, or list it in a revocation file
  your Git configuration names (`gpg.ssh.revocationFile`), which
  `verify-commit` honours.
- **Only as strong as your branch protection.** A run reads the `verify`
  setting and the allowed signers file from the same commit it checks.
  Anyone who can push to that branch can add their own key to the file, or
  point `verify` at another file, and then sign a block of their choosing.
  So `verify` is only as strong as the protection on the branch: protect the
  Environment's file and the allowed signers file (for example with
  required reviews and a CODEOWNERS entry), or keep the default state check.
  Nyl never turns `verify` on by itself; without it the state check above
  applies.

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
