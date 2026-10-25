---
title: 'Example: team-owned AWS accounts'
description: 'A walkthrough of planned domain resources, from a type definition to removing an access grant.'
---

:::note[Planned]
This walkthrough shows the planned design; none of it runs yet. Field names
and commands may change before the
[slices on the roadmap](/nyl/domain-resources/roadmap/) ship.
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

## 7. Remove access, keep the account

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
