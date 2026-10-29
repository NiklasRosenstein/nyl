---
title: 'Terraform and OpenTofu'
---

`Terraform` and `OpenTofu` units share one configuration and lifecycle, with
native state and locking. Choose `kind: Terraform` to run the installed
`terraform` executable or `kind: OpenTofu` to run `tofu`. Nyl does not download
either tool.

```yaml
apiVersion: units.gitops.nyl/v1
kind: OpenTofu
metadata:
  name: application
spec:
  source: {path: infra/application}
  files: ['infra/application/**', 'infra/modules/application/**']
  backend: {bucket: acme-state, key: production/application}
  variables:
    image: {fromUnit: {unit: web-image, artifact: image, pointer: /reference}}
  outputs:
    endpoint: {type: string}
  approval: {mode: manual, bind: plan, requireDigest: true}
```

The root module declares its backend type. Nyl passes `backend` through a
private configuration file and runs `init -lockfile=readonly`; commit a
`.terraform.lock.hcl`, including an empty file for configurations using only
builtin providers. Native tool data and plans live in private temporary
directories. A local backend needs an absolute persistent state path outside
the disposable source checkout. Declare all nonempty effective remote backend
settings on the unit, including settings authored in module HCL, so ownership
has one explicit identity. Empty native defaults do not change that identity.
Credentials enter through the common unit `env` admission, never `backend`.

`source` selects a pinned repository revision and directory. Relative local
modules use that same checkout. `files` defaults to `source.path/**`, excluding
`.terraform`; every local module must be covered, so changing a module changes
the execution key. `varFiles` names native variable files under the same local
path rule; their contents enter the key and their order is preserved. Explicit
`variables` override them. `fromUnit` references work at any depth:

```yaml
variables:
  vpc_id: {fromUnit: {unit: network, output: vpcId}}
  image: {fromUnit: {unit: web-image, artifact: image, pointer: /reference}}
```

The image reference can configure a container deployment through a native provider,
such as an ECS task, a container-based Lambda function, or a Docker container.
The infrastructure configuration declares how to use the image; `fromUnit`
supplies the built artifact reference and waits for its current receipt.

`version` optionally requires an exact installed version. Set project defaults
under `[units.Terraform]` or `[units.OpenTofu]`; an explicit unit version wins.
Without a pin, changing the installed tool does not force execution. Receipts
record the executable version that ran.

`nyl plan` and `nyl verify` run native plans without the backend lock and apply
nothing. Plans include a digest of resource, move, import, and output changes.
Sensitive values and known credentials are masked before summaries and digests;
native JSON never enters the transcript. `bind: plan` compares the reviewed
digest with the saved plan before applying. With `requireDigest: false`, explicit
unreviewed approval is also supported. Repair after `nyl exec` requires a reviewed
digest. No-change plans skip apply.

Only declared, correctly typed, non-sensitive outputs are recorded. A native
output marked sensitive must be declared sensitive; Nyl checks this before
apply. Teardown creates and applies a native destroy plan using the same
approval rules.

A missing backend state for a unit with a receipt fails with
`NYL-TOFU-STATE-MISSING`, instead of recreating resources. Restore it, or use
`nyl recover --retry --reason` to accept an intentionally empty state for that
ownership scope. The acknowledgement applies to the recorded unit incarnation,
backend, and workspace, and expires after successful reconciliation. Changing any
of those requires a fresh acknowledgement. A native state lock error is non-retryable; Nyl never breaks
native locks automatically. Use the held-unit repair workflow
for inspection, import, and recovery. The first execution after operator repair
requires approval of its newly generated plan.

Changing a unit between `Terraform` and `OpenTofu` is a kind change, so the
ordinary deletion policy and teardown gate apply. For migration without
destruction, first reconcile `deletionPolicy: Retain`, change the kind, and
follow the native state migration and repair workflow
before resuming.
