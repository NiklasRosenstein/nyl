---
title: 'Orchestration'
---

:::note
Orchestration is in development in milestone M3. Environments, their state in
Git, the [`nyl state` and `nyl lease`](/nyl/reference/cli/state/) commands,
[Command](/nyl/orchestration/resource-guides/command/) units, one
environment's runs with [`nyl plan`](/nyl/reference/cli/plan/),
[`nyl reconcile`](/nyl/reference/cli/reconcile/) (including `--local` runs),
and [`nyl status`](/nyl/reference/cli/status/), and recovery with
[`nyl recover`](/nyl/reference/cli/recover/) and
[`nyl verify`](/nyl/reference/cli/verify/) exist today. Teardown, holds,
approvals, and runs over several environments follow. The
[roadmap](https://github.com/NiklasRosenstein/nyl/blob/develop/ROADMAP.md)
describes the milestones.
:::

Orchestration makes Nyl a Git-driven reconciler for whole environments. Units
such as commands, infrastructure configurations, container images, and
Kubernetes publications run in dependency order, record their results in Git,
and pass their outputs to the units that depend on them. Like everything in
Nyl, a run takes [one path](/nyl/start/introduction/#one-declaration-one-path)
whether it starts on a workstation or in CI.

- An [Environment](/nyl/reference/resources/gitops.nyl/v1/environment/) selects
  the units it runs and names where its state lives.
- An [EnvironmentGroup](/nyl/reference/resources/gitops.nyl/v1/environment-group/)
  gives several environments shared defaults.
- [Environments and state](/nyl/orchestration/environments-and-state/) explains
  how state is stored, protected by a lease, and supported by Git hosts.
- [Runs](/nyl/orchestration/runs/) explains how a run chooses its source
  commit, resolves references between units, decides what executes, and
  records the result.
- [Recovery](/nyl/orchestration/recovery/) explains how a run takes over from
  one that died, what an uncertain unit is, and how to recover it.
- [Git storage](/nyl/reference/git-storage/) lists every repository and ref
  orchestration reads and writes.
