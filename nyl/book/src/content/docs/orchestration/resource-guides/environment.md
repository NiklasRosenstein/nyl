---
title: 'Using Environment'
---

See the [Environment resource reference](/nyl/reference/resources/gitops.nyl/v1/environment/) for the API, example, and field definitions.

## Selecting units

An Environment is one place orchestration reconciles units into. Its
`unitSelector` matches literal unit `metadata.labels`; an empty selector
selects every unit. Templates of the selected units see `spec.values` as
`values`.

```yaml
apiVersion: gitops.nyl/v1
kind: Environment
metadata:
  name: production
spec:
  unitSelector:
    matchLabels:
      stage: production
  values:
    target: production
  state:
    repositoryRef:
      name: deploy
```

## Where state lives

`spec.state` names the Git repository and refs the environment's state lives
on. Without a repository, the environment uses its EnvironmentGroup's state
repository, then the project's `origin` remote. A project without a remote
keeps state on this clone's own branches. The refs default to
`nyl/<environment>/desired` and `nyl/<environment>/observed`.

Create the state once with `nyl state init`. After changing `spec.state`, move
existing state to the new location with `nyl state move`. See
[`nyl state`](/nyl/reference/cli/state/).

## Source and protected refs

`spec.source` decides the commit each run renders units at: omitted, the
commit of the worktree you run from; with `revision`, that ref's tip; with
`revision` and `commit`, the pinned commit. Runs check that commit against
`spec.protectedRefs`, by default the repository's default branch, and
`allowUnprotectedSource: true` lets an environment such as dev run from any
commit. An EnvironmentGroup supplies these fields when the Environment omits
them. See [Runs](/nyl/orchestration/runs/#source-commit).

## See also

- [EnvironmentGroup](/nyl/reference/resources/gitops.nyl/v1/environment-group/)
- [GitRepository](/nyl/reference/resources/gitops.nyl/v1/git-repository/)
