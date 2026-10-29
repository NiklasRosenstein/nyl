---
title: 'Git storage'
---

Nyl keeps everything it records in Git: the configuration it renders from, the
trees it publishes, and the state orchestration writes. This page lists every
repository and ref involved, who writes it, and when to use which layout. The
guides explain the concepts: [Rendered GitOps](/nyl/rendered-gitops/) for
publication, and [Environments and state](/nyl/orchestration/environments-and-state/),
[Runs](/nyl/orchestration/runs/), and [Recovery](/nyl/orchestration/recovery/)
for orchestration.

## Repositories involved

| Repository | Holds | Written by |
| --- | --- | --- |
| Source repository | `nyl.toml` and every resource Nyl renders: releases, targets, environments, and units | People, through commits and pull requests. Nyl reads it, and orchestration renders units from a fresh checkout of the run's source commit. |
| Publication destinations | Rendered Kubernetes trees for Argo CD | [`nyl publish-tree`](/nyl/reference/cli/publish-tree/); from M5 (planned), KubernetesPublication units |
| State repositories | Each environment's desired and observed state and its coordination refs | Orchestration commands such as `reconcile`, `recover`, `verify`, and `nyl state` |

All three can be one repository.

### Publication destinations

A [DeploymentTarget](/nyl/reference/resources/k8s.gitops.nyl/v1/deployment-target/)
publishes to a [GitRepository](/nyl/reference/resources/gitops.nyl/v1/git-repository/),
a branch or revision, and a path prefix:

- The tree beneath the prefix carries an ownership index, `_nyl/index.json`,
  that records which files Nyl owns. Publication removes only owned files and
  fails when an owned file was changed outside Nyl
  ([Rendered layout](/nyl/rendered-gitops/rendered-layout/#ownership-index)).
- One target owns one repository, revision, and prefix. Targets sharing a
  revision must use disjoint prefixes, which Nyl checks, including through
  URL aliases.
- A GitRepository's `repoURL` is the credential-free read URL that renderers
  and the generated Argo CD Applications use. `publishURL`, optional, is the
  write URL Nyl pushes through; without it, pushes use `repoURL`.

## State

An [Environment](/nyl/reference/resources/gitops.nyl/v1/environment/)'s
`state` field says where its state lives:

| Field | Meaning | Default |
| --- | --- | --- |
| `repositoryRef` / `repository` | A GitRepository reference or an inline repository | The EnvironmentGroup's, else the source checkout's `origin`, else the local repository itself |
| `desiredRef`, `observedRef` | The refs holding desired and observed state; they may be the same ref, with the same layout | `nyl/<environment>/desired`, `nyl/<environment>/observed` |
| `path` | A directory within those refs, so several environments share one ref, each with its own `state.yaml`, `desired/`, and `observed/` | The root |
| `coordinationRefPrefix` | Where leases and run refs live, see [Coordination refs](#coordination-refs) | `refs/nyl/` |
| `admissionRef` (M8, planned) | The ref holding the CI deployment admission | The desired ref |

A short ref name is a branch, the recommended form: it can be protected
against force pushes and deletion, its history shows in the forge, and a
promotion pull request needs a branch as its base. A full `refs/…` name keeps
state out of the branch list but gives up that protection, so anyone with
push access can rewrite it; validation warns with `NYL-STATE-UNPROTECTED`.
Nyl never force-pushes a state ref.

Inside the refs, `state.yaml` records the location and its history,
`desired/` the resolved environment and each unit's desired document, and
`observed/` each unit's receipt, condition, artifacts, observations, and
attestations. Each operation writes at most one commit per state ref.

[`nyl state delete`](/nyl/reference/cli/state/#nyl-state-delete) keeps the
refs: its commit removes the environment's `desired/` and `observed/` files
and leaves `state.yaml`, whose last event `state-deleted` marks the location
deleted. History keeps the removed state, and every later command for the
environment fails with `NYL-STATE-DELETED` until the Environment is removed
from source or `nyl state init --fresh` starts it over.

### The clone's local branches

Every command that reads remote state keeps the clone's own branches of the
state refs, `nyl/<environment>/desired` and `nyl/<environment>/observed` by
default, fast-forwarded to the remote and never pushed:

- Without a remote, these branches are the state.
  [`nyl state move -e <env> --from local`](/nyl/reference/cli/state/#nyl-state-move)
  publishes them once a remote exists.
- [`nyl reconcile --local`](/nyl/orchestration/environments-and-state/#local-runs)
  commits to them with a `Nyl-Local: true` trailer. It needs state refs that
  are branches, because full `refs/…` names outside `refs/heads/` have no
  local branch.
- [`nyl state reset -e <env> --local`](/nyl/reference/cli/state/#nyl-state-reset)
  drops local commits, and [`nyl state restore`](/nyl/reference/cli/state/#nyl-state-restore)
  repairs a remote whose tip is a `Nyl-Local` commit pushed by hand.

### Worktree snapshots

[`reconcile --worktree`](/nyl/orchestration/runs/#worktree-inputs) creates local
source refs `refs/nyl-worktrees/<base>/<tree>` without changing HEAD or the
staging index. They reuse identical snapshots. The snapshot and its source
ancestry are retained as parents of state and pre-effect run commits, so state
moves and fresh-clone recovery preserve the inputs. State history inspection
stops at the snapshot boundary. With `--local`, snapshot history stays in this
clone; otherwise it is published to the configured state repository.

## Coordination refs

Refs under `coordinationRefPrefix`, in the state repository, coordinate runs.
They are created and deleted on every run, so they stay out of the state
history:

| Ref | Content | Created by | Deleted by |
| --- | --- | --- | --- |
| `<prefix><env>/lease` | One `Lease` (`lease.yaml`): run, runner, operation, units executing, deadline | Every state-writing operation, with compare-and-swap | The holder when it ends; replaced by a run that takes over an expired lease or by `nyl lease break` |
| `<prefix><env>/runs/<run>` | `run.yaml`, the `Run` record with the same deadline and units; `created/<unit>.yaml`, the desired document and uid of each unit the run created, written before it starts; `checkpoints/<n>-<unit>/`, added in one commit per finished unit, with the desired document it executed, its receipt or condition, and its artifacts | `reconcile` | The run after its transition commit lands; otherwise the run that takes it over, which retires it to an evidence ref when an uncertain unit still needs it |
| `<prefix><env>/signals/<run>` | `Signal` records other invocations send to a running run: a teardown confirmation (`nyl teardown --confirm-removed`) or a hold (`nyl hold`) | The sending command | Together with the run ref; by the sender when the run finished before reading it; by `nyl state delete` |
| `<prefix><env>/evidence/<run>` | A retired run ref, kept because its checkpoints are evidence for an `uncertain` unit | A run that takes over | A later takeover or `nyl recover`, once no uncertain unit needs it; `nyl state delete` |
| `<prefix><env>/keep` (M5, planned) | Keeps a preview instance's latest source commit fetchable, in the source repository | Every reconcile of a template instance | `nyl state delete` |

While a run executes, its lease and run ref look like this:

```console
$ git for-each-ref refs/nyl
cf1ba2b1c65ed47fea947d08bf09a0f5bb8bee5b commit	refs/nyl/dev/lease
dea88c9f5d771d6d36ce713136a4994999523ba5 commit	refs/nyl/dev/runs/01a1732a-a59c-760b-aebe-f293aff659d9
$ git show refs/nyl/dev/runs/01a1732a-a59c-760b-aebe-f293aff659d9:run.yaml
apiVersion: gitops.nyl/v1
kind: Run

  run:      01a1732a-a59c-760b-aebe-f293aff659d9
runner: unknown@localhost
operation: reconcile
sourceCommit: 829c3b4f2faa421fc9498168330f1e92df8772a5
readDesired: fcfbb6fb6d03c2c3a539c4ef19787d50a9614d43
readObserved: a86335501a1f0e3b0dd7e244d382ebd411adf6fd
deadline: "2026-10-25T13:20:37.228489095Z"
units:
- migrate
```

The default `refs/nyl/` namespace keeps these refs out of the branch list and
out of push-triggered CI. On a host that rejects refs outside `refs/heads/`,
set `coordinationRefPrefix: refs/heads/nyl-run/`;
[Git host compatibility](/nyl/orchestration/environments-and-state/#git-host-compatibility)
lists the verified hosts. Changing the prefix goes through
[`nyl state move --coordination-refs`](/nyl/reference/cli/state/#nyl-state-move).

Forges cannot protect refs outside branches and tags, so anyone with push
access can delete a coordination ref. State stays safe: every change reaches
the state refs in one atomic push with a compare-and-swap of the holder's
lease.

## Credentials

Remote Git operations run the `git` command line, so Nyl authenticates as
your `git` does. Runners without SSH keys or credential helpers name
dedicated credentials per repository URL prefix in
[`[[git.credentials]]`](/nyl/reference/configuration/#git-credentials) of
`nyl.toml`, for state pushes and `publish-tree` alike. Pushes go through a
GitRepository's `publishURL` when it has one, so the read URL Argo CD sees can
stay credential-free.

## When to use what

| Layout | Use it when |
| --- | --- |
| Everything in the source repository | You start out, or one team owns source and deployments. It is the simplest setup: state defaults to the checkout's `origin`. |
| A separate state repository | State needs other permissions or branch protection than source, such as pushes only by the CI identity and operators, or you want state commits out of the source history. Name it with `state.repositoryRef`. |
| One shared state ref with `path` | Many short-lived environments, such as previews, would otherwise create a pair of branches each. Deleting one empties its directory, keeping only its `state.yaml` marker, instead of leaving a pair of branches behind. |
| Separate publication repositories per target | Targets need access boundaries, such as a production repository that only the production Argo CD reads and only release pipelines write. |
