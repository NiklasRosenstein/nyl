# Infrastructure units

**Status:** draft M1 contract for M4. See [ROADMAP.md](../ROADMAP.md) and the
[orchestration core contract](orchestration-core.md), which defines the common
unit fields, execution, approval, credentials, and recovery used here.

M4 adds three built-in kinds in `units.gitops.nyl/v1`: `OciImage` for container
images, and `Terraform` and `OpenTofu` for infrastructure configurations. The
two infrastructure kinds share one implementation and differ only in the tool
they run.

## OciImage

```yaml
apiVersion: units.gitops.nyl/v1
kind: OciImage
metadata:
  name: web-image
  labels: {tier: platform}
spec:
  context: {path: services/web}      # or a repository source
  dockerfile: Dockerfile             # relative to the context
  target: runtime                    # optional build stage
  repository: registry.example.com/web
  platforms: [linux/amd64, linux/arm64]
  buildArgs:
    VERSION: '{{ values.version }}'
    API_URL: {fromUnit: {unit: api, output: url}}
  tags: ['{{ environment.name }}']   # extra tags; see Tags
  builder: ci-buildkit               # optional buildx builder
  cache:
    from: ['type=registry,ref=registry.example.com/web:cache']
    to: ['type=registry,ref=registry.example.com/web:cache,mode=max']
  registryAuth:                      # typed credential helper
    registry.example.com:
      usernameSecret: registry-user
      passwordSecret: registry-token
```

### Execution

The driver runs `docker buildx build --push --metadata-file <file>` in a
worktree at the effective source revision, with the platforms, target, build
arguments, cache options, and tags from the spec, and `--builder` when
`builder` is set. The image is always pushed: a digest reference is only useful
to consumers when the registry holds it.

- **Artifact:** the unit publishes a `ContainerImage` artifact named `image`
  with `repository`, `digest`, `reference`
  (`registry.example.com/web@sha256:…`), `platforms`, and `tags`, taken from
  the metadata file, never from parsing build output. The kind declares no
  outputs; consumers reference the artifact, for example
  `{fromUnit: {unit: web-image, artifact: image, pointer: /reference}}`.
- **Tags:** every push carries the tag `nyl-<first 16 hex digits of the
  execution key>`, so a pushed image can be found from its desired document.
  `tags` adds further tags, which move; consumers use the artifact's
  `reference`.
- **Credentials:** `registryAuth` writes a temporary Docker configuration with
  the listed registries' credentials from the secrets provider and points
  `DOCKER_CONFIG` at it. Without it, `env.passthrough: [DOCKER_CONFIG]` reuses
  the runner's configuration, including credential helpers.

### Execution key

The key follows the core rule: the resolved spec without the common fields that
do not affect what runs, plus the files in the context that the build can see
(respecting `.dockerignore`) and the Dockerfile. `OciImage` declares `builder`
and `cache` as fields that change how an image is built but not what it
contains, so they are excluded as well.

Base images referenced by tag (`FROM node:22`) can change without any change to
the key, so a rebuild is not triggered by a new base image. Pin base images by
digest and let a dependency updater raise pull requests; each updated digest
then changes the Dockerfile and rebuilds.

### Lifecycle

| Capability | Behavior |
| --- | --- |
| plan | Reports whether the execution key changed and what would be built; no build runs |
| reconcile | Build and push; record outputs and the artifact |
| verify | Checks that the artifact's `reference` still exists in the registry (`docker buildx imagetools inspect`); a missing image is drift |
| inspect | Not supported; recovery is `converge` |
| teardown | Not supported: registry deletion differs between registries and may break consumers. `deletionPolicy: Teardown` is rejected for this kind; an EnvironmentTemplate's forced `Teardown` skips it, so the image is retained when an instance is removed |
| recovery | `converge`. A rebuild after an uncertain execution pushes again; a non-reproducible build may produce a different digest, which consumers then pick up |

## Terraform and OpenTofu

```yaml
apiVersion: units.gitops.nyl/v1
kind: OpenTofu                         # or Terraform
metadata:
  name: database
  labels: {tier: platform}
spec:
  source: {path: infra/database}       # or a repository source; see Source
  files: ['infra/database/**', 'infra/modules/postgres/**']
  version: 1.9.5                       # optional exact version; see Tool version
  backend:
    bucket: acme-tofu-state
    key: '{{ values.stateKey }}'
  variables:
    vpc_id: {fromUnit: {unit: network, output: vpcId}}
    instance_class: '{{ values.dbClass }}'
  varFiles: ['environments/{{ environment.name }}.tfvars']
  outputs:
    host: {type: string}
    port: {type: integer}
    password: {type: string, sensitive: true}
  approval: {mode: manual, bind: plan}
  env:
    passthrough: [AWS_REGION, AWS_ROLE_ARN, AWS_WEB_IDENTITY_TOKEN_FILE]
```

`Terraform` runs the `terraform` binary and `OpenTofu` runs `tofu`. Everything
else in this section applies to both.

Switching a unit between the two kinds is a kind change and gets no special
handling: the old incarnation is deleted under its deletion policy and the new
kind starts a new incarnation. To switch without destroying anything, first set
`deletionPolicy: Retain` and reconcile, then change the kind; the new
incarnation converges against the same backend, after any state migration the
tools require has been done by hand. A teardown caused by a kind change always
requires `--allow-teardown`.

### Execution

1. `init -input=false -lockfile=readonly` with `backend` passed as a temporary
   backend configuration file. A missing or outdated `.terraform.lock.hcl` fails
   the execution instead of being rewritten on the runner.
2. `plan -input=false -out=<planfile>` with `variables` in a temporary
   variables file and `varFiles` in order. The plan file stays in the runner's
   temporary directory and is deleted after the execution.
3. For `approval: {mode: manual, bind: plan}`, compute the change digest (see
   below) and compare it with the approved digest. A mismatch ends the execution
   without effects, and the unit waits for a new approval.
4. `apply -input=false <planfile>`.
5. `output -json`. Each declared output must exist and match its type.
   Undeclared outputs are ignored, whether sensitive or not.

Output sensitivity is checked before any effect: after step 2, the driver reads
the plan's outputs, and a declared output that the tool marks sensitive but
the unit does not declare `sensitive` fails the execution before `apply`. A
secret can therefore never be recorded by accident, and the check never leaves
changes applied without a receipt.

A plan with no changes skips `apply` and records the receipt directly.

### Change digest

The change digest identifies what an apply would do, so an approval can be
bound to it:

- It is computed from `show -json <planfile>`: every resource change whose
  actions are not `no-op`, with its address, actions, and before and after
  values, plus changed outputs.
- Values the plan marks sensitive are replaced by a fixed marker before
  digesting, so no secret is part of the digest or of anything printed next to
  it.
- The digest is SHA-256 over the canonical JSON of that document. `nyl plan`
  prints it with a change summary; `--output json` includes it per unit as
  `changeDigest`.

### Source

- `source.path` is read from the worktree at the effective source revision.
  With a repository (`repositoryRef` or `repository`, `revision`, locked
  `commit`), the worktree is that repository at the locked commit, and
  `nyl update source-locks` refreshes it. A promoted revision replaces the
  commit, subject to the reachability rule in the core contract.
- `files` lists the repository-relative globs that enter the execution key.
  The default is `<source.path>/**`, excluding `.terraform/`.
- Relative local modules resolve inside the same worktree, so pinning the
  revision pins them too. After `init`, the driver reads the tool's module
  manifest; a local module directory outside `files` fails the execution with a
  message naming the glob to add, so a module change can never go unnoticed.

### Tool version

- The binary comes from `PATH`; Nyl never downloads tools. Tool managers such as
  mise install them.
- `version` sets an exact version. The driver fails the execution when the binary
  reports another version. `version` is part of the execution key, so changing
  it re-plans.
- Without `version`, any installed version runs, and upgrading the tool does
  not re-plan. The receipt always records the version that ran.
- `nyl.toml` may set a project default per kind:

  ```toml
  [units.OpenTofu]
  version = "1.9.5"
  ```

### Execution key

The key follows the core rule: the kind, the resolved spec without the common
fields that do not affect what runs (so `source`, `backend`, `variables`,
`varFiles`, declared `outputs`, `version`, and the names in `env` all count),
plus the files matched by `files` and the contents of `varFiles`. The source
commit itself is excluded; the matched bytes stand for it.

### Lifecycle

| Capability | Behavior |
| --- | --- |
| plan | `init` and `plan`; reports the change summary and change digest; nothing is applied or recorded |
| reconcile | Execution steps above |
| verify | `plan -detailed-exitcode` against the current desired document. Exit 0 is clean; exit 2 means applying would change something, reported as drift with the change summary |
| inspect | Not supported; recovery is `converge` |
| teardown | `plan -destroy -out=<planfile>`, then `apply`. For manual units, the approval binds to the destroy plan's digest |
| recovery | `converge`: the backend's state lock guards concurrent effects, and a new plan after an uncertain execution shows what is still missing |

A state lock left behind by a lost runner makes the next execution fail with the
lock ID, as a non-retryable failure. The operator releases it with the tool's
`force-unlock` and then runs `nyl recover --retry`; Nyl never releases locks
itself.

## Terraform to Terraform outputs

A unit consumes another unit's outputs with `fromUnit`, for example a network
configuration's `vpc_id` in a database configuration:

```yaml
apiVersion: units.gitops.nyl/v1
kind: OpenTofu
metadata: {name: network, labels: {tier: platform}}
spec:
  source: {path: infra/network}
  backend: {bucket: acme-tofu-state, key: '{{ environment.name }}/network'}
  outputs:
    vpcId: {type: string}
---
apiVersion: units.gitops.nyl/v1
kind: OpenTofu
metadata: {name: database, labels: {tier: platform}}
spec:
  source: {path: infra/database}
  backend: {bucket: acme-tofu-state, key: '{{ environment.name }}/database'}
  variables:
    vpc_id: {fromUnit: {unit: network, output: vpcId}}
  outputs:
    host: {type: string}
```

`database` runs after `network` has a current receipt. A `network` change that
leaves `vpcId` unchanged re-runs `network` but not `database`, because
`database`'s execution key only sees the value.

## Remaining questions

| Question | Needed by |
| --- | --- |
| Additional image build backends (`buildctl`, Buildah) as options of `OciImage` | After M4 |
| Registry-specific image deletion for `OciImage` teardown | After M4 |
