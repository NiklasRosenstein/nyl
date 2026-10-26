---
title: 'Domain resources'
description: 'Team-owned, typed declarations that orchestration units implement. Planned for milestone M9.'
---

:::note[Planned]
Domain resources are planned for milestone M9 and nothing in this section
exists yet. The [roadmap](/nyl/domain-resources/roadmap/) tracks progress,
and the [design](https://github.com/NiklasRosenstein/nyl/blob/develop/design/domain-resources.md)
holds the full contract.
:::

Domain resources let teams declare what they need, such as an AWS account, a
GitHub repository, or a Bao policy, as typed YAML in the folders they own. The
platform implements those declarations with ordinary
[orchestration](/nyl/orchestration/) units, and the native tools keep their
own state.

## The case

Infrastructure is usually organised by the tool that implements it. A team
that needs an AWS account, a repository with the right access, and a secrets
policy opens pull requests against three configurations owned by someone else:
one for AWS, one for GitHub, and one for Bao. That causes three problems:

- **Ownership is scattered.** Nothing in the repository says what the payments
  team has. Its accounts, repositories, and policies sit inside other teams'
  code, among everyone else's.
- **Review boundaries don't fit.** CODEOWNERS can protect a folder, but the
  folder belongs to the implementation, so every change needs the platform
  team's review, even for a routine new sandbox account.
- **Nothing limits what a team may ask for.** Rules such as "teams declare
  sandbox accounts, only the platform declares production" live in reviewers'
  heads instead of in enforced policy.

Domain resources organise the project by ownership and intent instead:

```text
platform/
  definitions/   # domain types and their schemas     @platform
  policies/      # what each folder may declare       @platform
  aws-accounts/  # OpenTofu unit for AwsAccount
  github/        # OpenTofu unit for GitHubRepository
teams/
  payments/      # aws.yaml, github.yaml, bao.yaml    @payments
  search/
```

The handles on the right are each folder's CODEOWNERS.

A team edits only its own folder. Its owners review the change, policy decides
whether that folder may declare it, and the platform's units pick it up on the
next reconcile.

## The fourth layer

Each layer of Nyl builds on the one before, and a project uses only as many as
it needs:

| Layer | You declare | Nyl does |
| --- | --- | --- |
| [Kubernetes](/nyl/kubernetes/pipeline/) | Releases, Helm charts, components | Renders, diffs, and applies manifests |
| [Rendered GitOps](/nyl/rendered-gitops/) | Clusters and deployment targets | Publishes reviewable manifests for a reconciler |
| [Orchestration](/nyl/orchestration/) | Environments and units | Runs units in dependency order and records their evidence in Git |
| Domain resources | Typed declarations in team folders | Validates them, admits them by policy, and feeds them to the units that implement them |

The implementation layer stays as it is. A domain type needs no new driver: an
OpenTofu, Terraform, or Command unit consumes the declarations like any other
input.

## How it works

1. A platform author defines a type, such as `AwsAccount`, with a JSON Schema.
   Definitions load only from paths that `nyl.toml` lists.
2. Teams write instances of the type as literal YAML. Each has a project-wide
   identity, and Nyl takes its source path from Git, never from the document.
3. Nyl validates each declaration against its schema and admits it through
   Kyverno policies, which receive the trusted source path as context.
4. A unit declares the collections it consumes: a type, plus label and path
   filters. Membership is explicit, and nothing is linked by matching names.
5. Nyl resolves references to other units' outputs and builds one tool-neutral
   bundle for the unit. The bundle is part of the unit's execution key.
6. The unit's driver turns the bundle into native input, such as a Terraform
   variable that `for_each` iterates over.

A unit can also declare `forEach` and be instantiated once per declaration,
for example one OpenTofu configuration per team, each with its own state.

## What to expect

- **Declarations are data, not units.** Editing a declaration changes the inputs
  of the units that consume it, never their identity, so it is never a
  replacement.
- **Only relevant changes re-run.** Editing a member re-runs its consumers.
  Another team editing its own declarations re-runs nothing, and so does
  moving a file, unless the move changes which collections include it or
  which policies admit it.
- **Failures stay local.** A declaration that fails its schema or a policy
  blocks only the units that consume it, before anything is applied.
- **Sensitive changes are guarded.** A definition can forbid a change, such as
  moving a workload account back to the sandbox OU, or require approval for
  it, such as changing an account's email.
- **Orchestration is optional.** In an existing Terraform or OpenTofu
  pipeline, `nyl exec` runs the tool with the collected inputs, or Nyl
  writes the inputs for the pipeline's own commands. Either way they are the
  same bytes an orchestrated run would pass, and nothing is recorded.
- **Removing a declaration is an input change.** The implementing unit plans
  and applies the removal like any other change, and removals wait for
  approval by default. A hold on the unit protects the unit, not the native
  resources its next apply removes.
- **There is no second state database.** Terraform, OpenTofu, or the command
  keeps the state of what it manages, as in [orchestration](/nyl/orchestration/).

## Next steps

- The [example](/nyl/domain-resources/example/) walks through team-owned AWS
  accounts from definition to removal.
- The [roadmap](/nyl/domain-resources/roadmap/) shows what ships in which slice.
