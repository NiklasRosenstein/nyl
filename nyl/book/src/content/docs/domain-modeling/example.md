---
title: 'Example: team-owned AWS accounts'
description: 'A walkthrough of planned domain resources, from a type definition to removing an access grant.'
---

:::note[Planned]
This walkthrough shows the planned design; none of it runs yet. Field names
and commands may change before the
[slices on the roadmap](/nyl/domain-modeling/roadmap/) ship.
:::

A platform team manages AWS accounts for every product team. Each team
declares its accounts in its own folder, and one OpenTofu configuration,
owned by the platform, creates them.

## 1. Define the type

The platform describes what an account declaration may contain. The schema is
strict, so a typo fails validation instead of reaching AWS:

```yaml
# platform/definitions/aws.yaml
apiVersion: gitops.nyl/v1
kind: ResourceDefinition
metadata:
  name: awsaccounts.domain.acme.example
spec:
  group: domain.acme.example
  kind: AwsAccount
  version: v1
  ownership: project
  schema:
    type: object
    required: [email, ou]
    additionalProperties: false
    properties:
      email: {type: string, format: email}
      ou: {type: string, enum: [workloads, sandbox, security]}
      budgetUsd: {type: integer, minimum: 0, default: 100}
```

`nyl.toml` names the only paths that definitions and policies may come from,
so the platform's CODEOWNERS entry for those paths controls them:

```toml
[resources]
definitions = ["/platform/definitions/**"]
policies = ["/platform/policies/**"]
```

## 2. Declare an account

The payments team adds a file to its own folder. The file is literal YAML,
never a template, so reviewers and policies see exactly what the
implementation receives:

```yaml
# teams/payments/aws.yaml
apiVersion: domain.acme.example/v1
kind: AwsAccount
metadata:
  name: payments-sandbox
  labels: {team: payments}
spec:
  email: aws+payments-sandbox@acme.example
  ou: sandbox
```

## 3. Limit what a folder may declare

CODEOWNERS decides who reviews `teams/payments/`. A policy decides what that
folder may declare at all. Nyl gives the policy the declaration's source path
as an annotation that authors cannot set:

```yaml
# platform/policies/accounts.yaml
apiVersion: policies.kyverno.io/v1
kind: ValidatingPolicy
metadata: {name: teams-declare-sandbox-accounts}
spec:
  matchConstraints:
    resourceRules:
      - apiGroups: [domain.acme.example]
        apiVersions: [v1]
        resources: [awsaccounts]
        operations: [CREATE]
  validations:
    - expression: >-
        object.metadata.annotations['source.gitops.nyl/path']
          .startsWith('/platform/')
        || object.spec.ou == 'sandbox'
      message: Teams may only declare sandbox accounts.
```

This policy admits `payments-sandbox`. The same file with `ou: workloads`
under `teams/` is rejected, and the units that consume it apply nothing until
the declaration is fixed. `nyl validate` in the pull request reports the
rejection before it is merged. Under `platform/accounts/`, where only the
platform team can approve changes, the same declaration is admitted.

## 4. Implement the type with a unit

The platform's OpenTofu unit collects every admitted account and maps the
collection to a variable:

```yaml
# platform/aws-accounts/unit.yaml
apiVersion: units.gitops.nyl/v1
kind: OpenTofu
metadata: {name: aws-accounts, labels: {tier: org}}
spec:
  source: {path: /platform/aws-accounts}
  backend: {bucket: acme-tofu-state, key: org/aws-accounts}
  collections:
    accounts:
      type: {apiVersion: domain.acme.example/v1, kind: AwsAccount}
      paths: ['/teams/**', '/platform/accounts/**']
  variables:
    accounts: {fromCollection: {collection: accounts, each: /spec}}
```

`fromCollection` produces a map from each account's name to its `spec`, which
is the shape `for_each` expects:

```hcl
# platform/aws-accounts/main.tf
variable "accounts" {
  type = map(object({
    email     = string
    ou        = string
    budgetUsd = number
  }))
}

resource "aws_organizations_account" "this" {
  for_each  = var.accounts
  name      = each.key
  email     = each.value.email
  parent_id = local.ou_ids[each.value.ou]
  lifecycle { prevent_destroy = true }
}
```

## 5. Know what re-runs

The unit's execution key covers the collected content, not where it came
from. Where a declaration lives matters only for membership and admission:

| Change | Does `aws-accounts` run again? |
| --- | --- |
| A team adds or edits an account | Yes |
| A schema default changes an account's effective value | Yes |
| A declaration moves to another folder under `teams/` | No |
| A declaration moves into or out of the collection's `paths` | Yes: it joins or leaves the collection |
| A declaration moves across a folder the policy treats differently | Yes: admission changes, so the unit is blocked or unblocked |
| Another team changes its GitHub declarations | No |

## 6. One configuration per team

When each team should have its own state and blast radius, the unit declares
`forEach` and is instantiated once per declaration of a team type. The
declaration is available to the template as `resource`:

```yaml
apiVersion: units.gitops.nyl/v1
kind: OpenTofu
metadata: {name: team-aws, labels: {tier: org}}
forEach:
  type: {apiVersion: domain.acme.example/v1, kind: TeamWorkspace}
  paths: ['/teams/**']
spec:
  source: {path: /platform/team-aws}
  backend:
    bucket: acme-tofu-state
    key: 'teams/{{ resource.metadata.name }}'
  collections:
    accounts:
      type: {apiVersion: domain.acme.example/v1, kind: AwsAccount}
      selector: {matchLabels: {team: '{{ resource.metadata.name }}'}}
```

A team with a `TeamWorkspace` named `payments` gets the unit
`team-aws-payments`. Nyl knows the instances before rendering anything,
because `forEach` and the declarations are both literal.

## 7. Guard sensitive changes

Some fields may be set once but not changed lightly. Changing an AWS
account's email replaces the account, so the definition makes that change
wait for an approval, and forbids moving a workload account back to the
sandbox OU:

```yaml
# platform/definitions/aws.yaml, beneath spec.schema
properties:
  email:
    type: string
    x-nyl-validations:
      - rule: self == oldSelf
        message: Changing an account email replaces the account.
        effect: approve
        reason: email-change
  ou:
    type: string
    x-nyl-validations:
      - rule: "!(oldSelf == 'workloads' && self == 'sandbox')"
        message: Workload accounts cannot move back to the sandbox OU.
```

A rule that uses `oldSelf` compares a declaration with its previous version:

- In a pull request, `nyl validate --base origin/main` compares with the merge
  base and reports both rules before anything is merged.
- When `aws-accounts` runs, Nyl compares with what the unit last applied. An
  email change makes the unit wait for `--approve aws-accounts`, and the plan
  shows what the provider would do to the account.

## 8. Use your own pipeline

Orchestration is optional. When Terraform or OpenTofu already runs in a CI
pipeline, Nyl prepares its inputs, and the pipeline plans and applies as
before:

```sh
# In the pull request
nyl exec --unit aws-accounts --base origin/main -- plan -out=tfplan

# After the merge
nyl exec --unit aws-accounts --base refs/nyl-applied/aws-accounts -- plan -out=tfplan
nyl exec --unit aws-accounts --advance refs/nyl-applied/aws-accounts -- apply tfplan
```

`nyl exec` without an environment prepares OpenTofu exactly as an
orchestrated run would, with the backend, the variables, and its own working
copy, and then runs `tofu` with the arguments after `--`. The pipeline stays
in charge of when to plan and who approves the apply.

Without orchestration, nothing records what was last applied, so every run
says what to compare with:

- `--base origin/main` in a pull request compares with where the branch
  forked from `main`.
- After the merge, comparing `main` with itself would detect nothing.
  `--advance` names a ref that points at the commit last applied: Nyl
  compares with it, and moves it to the current commit once `tofu apply`
  succeeds. Before running the tool, Nyl checks that it may push the ref and
  that no newer commit was applied already. If the push still fails, it says
  so, prints the commands to fix it, and the next run simply checks the
  changes since the older commit again.
- The ref is optional and can be a custom ref, a tag, or a branch. The first
  apply, before the ref exists, passes `--bootstrap` to run once without a
  comparison and create it.
- `--no-base` skips the comparison deliberately, for local development.
  Leaving out a base entirely is an error, so a pipeline never skips
  transition rules by accident.

`nyl exec` refuses to run uncommitted changes to the files a unit reads
unless you pass `--worktree`, so a local apply never uses inputs that no
commit records. `--source HEAD` runs the committed version instead.

To hand the inputs to a tool invocation of your own instead, write them to a
directory:

```sh
nyl prepare --unit aws-accounts --native --base origin/main --output-dir out/   # in a pull request
tofu -chdir=platform/aws-accounts init -backend-config=../../out/native/backend.tfbackend.json
tofu -chdir=platform/aws-accounts plan -var-file=../../out/native/variables.tfvars.json
```

The unit document stays the same, and no Environment selects it. Validation,
policies, and transition rules still apply. An approval-guarded change, such
as the email change above, stops `exec` and `prepare` unless the pipeline passes
`--allow-change AwsAccount/payments-prod:email-change` after its own
approval. Selecting the unit from an Environment later changes nothing about
the inputs it receives.

## 9. Remove access, keep the account

Removing a declaration changes the implementing unit's input. What happens next
is up to the implementation:

- **Revoking an access grant** is the everyday case. The team deletes the grant's
  declaration, the plan shows the grant leaving the collection and its destroy,
  and an approval applies it. Removals wait for approval by default.
- **Retiring an account** is deliberately harder. `prevent_destroy` makes the
  plan fail, so an account is never closed by deleting a file. The platform
  holds the unit, removes the account from state with `nyl exec … -- state rm`,
  deletes the declaration, and resumes the unit.

A hold on a unit protects the unit, not the native resources its next apply
removes. Protecting individual resources is the implementation's job.
