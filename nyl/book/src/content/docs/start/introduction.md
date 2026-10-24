---
title: 'Introduction'
---

**Nyl** turns configuration in Git into deployed systems. It works at three
levels, each building on the one before. Use only as much as you need.

## Render and apply Kubernetes manifests

Nyl generates plain Kubernetes YAML from Helm charts, reusable components,
remote manifests, and Jinja2-compatible templates. `nyl render`, `nyl diff`,
and `nyl apply` work directly against a cluster.

Use this level to bootstrap a cluster, debug a change, drive a test cluster,
or make a controlled manual change, with fast local feedback and no extra
moving parts.

Start with the [rendering pipeline](/nyl/kubernetes/pipeline/) and the
[component system](/nyl/kubernetes/components/overview/).

## Publish rendered manifests for GitOps

Nyl compiles each deployment target into an ordinary directory of Kubernetes
manifests in a Git revision, with Argo CD Applications and least-privilege
AppProjects. Argo CD, Flux, or another reconciler syncs the published YAML; Nyl
runs in CI and never in the cluster's reconciliation path. Reviewers see the
exact manifests a change produces.

Use this level when Kubernetes workloads are deployed through GitOps and you
want what is deployed to be reviewable, reproducible YAML.

Start with the [rendered manifest pattern](/nyl/rendered-gitops/).

## Orchestrate environments

Nyl also grows into a Git-driven reconciler for whole environments. Units such
as commands, infrastructure configurations, container images, and Kubernetes
publications run in dependency order, record their results in Git, and pass
their outputs to the units that depend on them. Kubernetes is then one kind of
unit, published through rendered GitOps, and an environment does not need
Kubernetes at all.

Orchestration is in development; the
[roadmap](https://github.com/NiklasRosenstein/nyl/blob/develop/ROADMAP.md)
describes its milestones.

## What Nyl provides

- **Helm-backed resources** through `HelmChart` and component shortcuts.
- **Reusable local components** with chart lookup and schema validation.
- **Jinja2-compatible templating** via MiniJinja.
- **Clusters and targets** for deterministic capabilities, deployment values, and publication coordinates.
- **Remote manifests** for including HTTPS-hosted YAML and JSON.
- **Manifest validation** against Kubernetes schemas and Kyverno policies.
- **Release metadata and revision tracking** through `Release`.
- **A CI rendering image** with Nyl, Helm, Git, SOPS, and Kyverno CLI.
