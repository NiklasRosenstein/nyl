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

## See also

- [EnvironmentGroup](/nyl/reference/resources/gitops.nyl/v1/environment-group/)
- [GitRepository](/nyl/reference/resources/gitops.nyl/v1/git-repository/)
