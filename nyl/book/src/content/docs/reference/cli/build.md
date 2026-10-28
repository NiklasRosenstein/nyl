---
title: 'build'
---

Builds an artifact from a unit without creating a receipt, taking a lease, or
writing orchestration state. The unit's driver must declare the `build`
capability. Infrastructure applies and Command executions use `reconcile`
instead.

```bash
nyl build web-image
nyl build web-image -e dev
nyl build web-image -- --push --tag experiment
nyl build web-image --source main -- --load
```

The default uses a private snapshot of Git-visible current worktree files,
including uncommitted and untracked files. Native tools cannot modify the
operator's checkout. `--source` uses that revision instead. Explicitly pinned
unit source fields retain their own commits and repositories in either mode.

With `-e`, templates read the environment's effective values and references
resolve read-only from current receipts. Without it, environment-dependent
values and references fail with an explanation naming `-e`. State need not
exist when the unit uses no environment.

Arguments after `--` are passed unchanged to the selected unit driver. The
driver defines and validates these options; Nyl has no generic Docker flags.

For `OciImage`, a build loads into the local Docker engine by default. Its `--push`
pushes only the spec's tags and `--tag` values; it requires at least one tag and
never creates the execution tag reserved for recorded reconciliations. Each driver enforces its own publishing policy; a kind that cannot safely
publish outside orchestration refuses such arguments and names `reconcile` instead.

Artifacts are printed as JSON: one document for a single artifact, or an array
for several. This output is local build information, not evidence of a recorded
execution.

## Options

| Option | Meaning |
| --- | --- |
| `<UNIT>` | Explicit unit name. |
| `-e`, `--environment <name>` | Environment values and current receipt inputs. |
| `--source <revision>` | Build committed source instead of the dirty worktree. |
| `-- <ARGS>...` | Kind-specific arguments passed unchanged to the driver. |
| `[PATH]` | Project directory or a path beneath it, default `.`. |

The `OciImage` driver accepts `--load` (the default), `--push`, and repeatable
`--tag <tag>` arguments after `--`. It rejects conflicting load/push options
and unsupported arguments. Other kinds may define different arguments.

Exit 0 means the build completed. Exit 1 means preparation, capability checks,
execution, or artifact admission failed. An interrupted push may already have
published an image; a direct build records no receipt to adopt afterwards.
