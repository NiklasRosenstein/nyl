---
title: 'Orchestration'
---

:::note
Orchestration is in development in milestone M3. Environments, their state in
Git, and the [`nyl state` and `nyl lease`](/nyl/reference/cli/state/) commands
exist today; units and the run commands such as `nyl plan` and `nyl reconcile`
follow. The [roadmap](https://github.com/NiklasRosenstein/nyl/blob/develop/ROADMAP.md)
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
