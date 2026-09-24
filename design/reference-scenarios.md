# Reference scenarios

**Status:** draft acceptance scenarios for M3–M5. See [ROADMAP.md](../ROADMAP.md),
the [orchestration core contract](orchestration-core.md), the
[infrastructure units contract](infrastructure-units.md), and the
[implementation architecture](implementation-architecture.md).

Three scenarios describe the workflows orchestration exists for, end to end:

1. **Platform environment:** OpenTofu, an OCI image, and a Kubernetes
   publication consuming both, reconciled, changed, and decommissioned.
2. **Preview closed with its pull request:** an instance created for a pull
   request, updated, and removed when the pull request is squash-merged.
3. **Preview expiry:** instances that are abandoned are removed by scheduled
   maintenance, while active ones survive.

Each scenario is a sequence of Git operations, clock changes, and `nyl`
invocations with expected results. Pull requests, CI jobs, and hosted
repositories are stand-ins: a pull request is a branch, a CI job is one `nyl`
invocation in a fresh clone at a commit, and every repository is a local bare
repository. Orchestration only ever sees commits, refs, and commands, so the
scenarios prove the workflows without external services.

The scenarios are acceptance criteria, not tutorials: each step names the
user-visible result it proves. They also become the worked examples in the
documentation.

## Test world

Everything lives in one per-test temporary directory, as the existing
`render_tree_test.rs` tests do with local bare repositories:

| Real world | Stand-in |
| --- | --- |
| Source repository | Bare `source.git` plus a working clone |
| Pull request | Branch `pr-<n>`; an update is a commit on it |
| Merge | Squash commit on `main`, then deleting the branch |
| CI job | One `nyl` invocation in a fresh clone of `source.git` at the job's commit |
| State repository | Bare `state.git`, named by a GitRepository with `file://` URLs |
| Deploy repository | Bare `deploy.git`, likewise |
| Container registry | Tier 1: fake. Tier 2: a `registry:2` container on an ephemeral port |
| Terraform backend | OpenTofu's `local` backend with a path inside the temporary directory |
| Cloud resources | Only the built-in `terraform_data` resource and outputs, so no provider is downloaded |
| Argo CD | Tier 1: a scripted fake observer. Publish mode needs none |
| Time | An injected clock (see [Clock](#clock)) |

### Tiers

The same scenario files run in two tiers:

- **Tier 1, always in CI.** The scenario harness runs orchestration in-process
  with the real Git `StateStore` on the local bare repositories, the real
  `KubernetesPublication` driver publishing to `deploy.git`, and fake
  `OciImage` and `OpenTofu` drivers. The fake image driver returns a digest
  derived from the execution key; the fake OpenTofu driver keeps its resources
  and outputs in a JSON file per backend key, so teardown and re-execution are
  observable. A scripted fake Argo CD observer reports, per step, that
  Applications exist, are gone, or cannot be read.
- **Tier 2, gated on tools.** The same steps run the real `nyl` binary as
  separate processes, with the real `tofu` and `docker buildx` against the local
  registry. Tests skip with a clear message when `tofu`, `docker`, or the
  registry is unavailable, like the other tool-gated tests.

Observe mode against a real Argo CD needs a cluster and stays a manual check;
tier 1's fake observer covers its decisions.

### Clock

Expiry and `delay` waits depend on time, and tests must not sleep or read the
global clock.

- Tier 1 passes a fake `Clock` to orchestration. A `delay` wait advances the
  fake clock instead of sleeping.
- Tier 2 builds `nyl` with a `test-clock` Cargo feature. Only with that feature,
  `nyl-cli` reads the current time from `NYL_TEST_NOW` and turns waits into
  clock advances. Tests set the variable per child process, so they stay
  parallel-safe. Release builds have no clock override.

### Scenario files

Each scenario is a directory under `nyl/tests/scenarios/reference/`:

```text
platform/
  project/                 # the source tree committed as the first commit of main
  scenario.yaml            # steps
  patches/                 # file changes applied by `commit` steps
```

```yaml
steps:
  - commit: {branch: main, message: "Change the web page", apply: patches/web-page}
  - nyl: [reconcile, -e, dev]
    exit: 0
    expect:
      executed: [web-image, kubernetes]
      current: [network, database]
      deploy: {branch: main, contains: ['dev/web/**'], unchanged: ['dev/_nyl/catalog/**']}
  - clock: {advance: 8d}
```

- Step kinds: `commit`, `branch`, `squash-merge`, `delete-branch`, `clock`,
  and `nyl`.
- Expectations cover only user-visible results: the exit code, per-unit results
  from the transition commit summary, `nyl get … -o json` documents, files and
  commits in `deploy.git` and `state.git`, and in tier 2 the registry's
  manifests and `tofu output`.
- Both tiers read the same file. A step that only one tier can run, such as a
  scripted observer response, is marked `tier: 1`.

## Shared project

All three scenarios use one project:

```text
nyl.toml
config/
  repositories.yaml        # GitRepository source, state, deploy
  cluster.yaml             # Cluster dev-cluster
  targets/dev.yaml         # DeploymentTarget dev with releaseInputs for web/web
  environments/dev.yaml    # Environment dev
  templates/preview.yaml   # EnvironmentTemplate preview
units/
  network.yaml             # OpenTofu                labels: tier: platform
  database.yaml            # OpenTofu, manual approval  tier: platform
  web-image.yaml           # OciImage                tier: platform, preview: 'true'
  announce.yaml            # Command, no dependencies   tier: platform
  kubernetes.yaml          # KubernetesPublication, static target dev   tier: platform
  preview-db.yaml          # OpenTofu, per instance     preview: 'true'
  preview-site.yaml        # KubernetesPublication, inline target       preview: 'true'
infra/network/main.tf
infra/database/main.tf
services/web/Dockerfile    # FROM scratch, COPY index.html: builds without pulling
services/web/index.html
applications/web/group.yaml
applications/web/release.yaml   # inputs: image (string), database (object)
```

Key excerpts:

```yaml
# config/environments/dev.yaml
apiVersion: gitops.nyl/v1
kind: Environment
metadata: {name: dev}
spec:
  unitSelector: {matchLabels: {tier: platform}}
  values: {cidr: 10.0.0.0/16, backendDir: <temporary directory>/tofu}
  state:
    repositoryRef: {name: state}
---
# units/database.yaml
apiVersion: units.gitops.nyl/v1
kind: OpenTofu
metadata: {name: database, labels: {tier: platform}}
spec:
  source: {path: infra/database}
  backend: {path: '{{ values.backendDir }}/{{ environment.name }}-database.tfstate'}
  variables:
    vpc_id: {fromUnit: {unit: network, output: vpcId}}
  outputs:
    host: {type: string}
  approval: {mode: manual, bind: plan}
---
# units/kubernetes.yaml
apiVersion: units.gitops.nyl/v1
kind: KubernetesPublication
metadata: {name: kubernetes, labels: {tier: platform}}
spec:
  target: dev
  mode: publish            # teardownWait defaults to manual
---
# config/targets/dev.yaml (excerpt)
spec:
  releaseInputs:
    web/web:
      image: {fromUnit: {unit: web-image, artifact: image, pointer: /reference}}
      database:
        value: {port: 5432}
---
# config/templates/preview.yaml
apiVersion: gitops.nyl/v1
kind: EnvironmentTemplate
metadata: {name: preview}
spec:
  parameters: [{name: pr, type: integer}]
  unitSelector: {matchLabels: {preview: 'true'}}
  state:
    repositoryRef: {name: state}
    desiredRef: nyl/previews
    observedRef: nyl/previews
    path: '{{ environment.name }}'
  allowUnprotectedSource: true
  deletionPolicy: Teardown
  allowTeardown: true
  ttl: 7d
  maxInstances: 2
---
# units/preview-site.yaml
apiVersion: units.gitops.nyl/v1
kind: KubernetesPublication
metadata: {name: preview-site, labels: {preview: 'true'}}
spec:
  mode: publish
  teardownWait: {strategy: delay, duration: 2m}   # unattended removal
  target:
    inline:
      clusterRef: {name: dev-cluster}
      applicationGroupSelector: {matchLabels: {app: web}}
      publication:
        repositoryRef: {name: deploy}
        revision: previews
        pathPrefix: '{{ environment.name }}'
      values: {nameSuffix: '-{{ environment.name }}'}   # unique Argo CD names per instance
      releaseInputs:
        web/web:
          image: {fromUnit: {unit: web-image, artifact: image, pointer: /reference}}
          database: {fromUnit: {unit: preview-db, output: connection}}
```

- `infra/network/main.tf` stores `var.cidr` in a `terraform_data` resource and
  outputs `vpcId` derived from it; `infra/database/main.tf` does the same with
  `vpc_id` and outputs `host`. A comment-only change leaves every output
  unchanged.
- `preview-db` reads dev's network with a cross-environment reference,
  `{fromUnit: {environment: dev, unit: network, output: vpcId}}`, and uses a
  backend path per instance.
- The Cluster, ApplicationGroup, and catalog settings are teardown-ready:
  catalog `syncPolicy.automated` with `prune: true`, `Foreground` deletion,
  and namespace `deletePolicy: Automatic`.
- `announce` is a Command with no dependencies and no dependents. It shows that
  a teardown wait never holds back unrelated units.
- The harness writes the temporary directory's absolute paths into the fixture
  when it commits it: the repository URLs and `values.backendDir` of dev and
  of the preview template, so
  OpenTofu state outlives each execution's worktree.
- Before the first reconcile, a setup step commits an unowned file,
  `dev/state/notes.txt`, into `deploy.git` under dev's prefix. Teardown must
  preserve it.

## Scenario 1: platform environment

| Step | Action | Proves |
| --- | --- | --- |
| 1 | `nyl validate` → 0 | The project is valid, including that the preview template's inline target generates different Argo CD names per instance |
| 2 | `nyl reconcile -e dev` → 1 | No state is created implicitly; the error names `state init` |
| 3 | `nyl state init -e dev` → 0 | `state.yaml` exists on dev's refs in `state.git` |
| 4 | `nyl plan -e dev` → 2 | `network`, `web-image`, and `announce` are plannable; `database` and `kubernetes` are reported blocked on missing receipts, so the plan is incomplete |
| 5 | `nyl reconcile -e dev` → 2 | Wave 1 runs `network`, `web-image`, `announce`; `database` waits for approval (`bind: plan`); `kubernetes` is blocked on it. One desired and one observed commit |
| 6 | `nyl plan -e dev --unit database --output json` → 0, then `nyl reconcile -e dev --approve database=<digest>` → 0 | The approved digest is applied; `kubernetes` runs in a later wave of the same run and publishes `dev/` with the image's digest reference and the database host; the approval is in the receipt |
| 7 | `nyl get output database/host -e dev`, `nyl get artifact web-image/image -e dev --pointer /reference` | Value forms resolve like `fromUnit` |
| 8 | `nyl reconcile -e dev` → 0 | A repeated run executes nothing and writes no commit to `state.git` or `deploy.git` |
| 9 | Commit a change to `services/web/index.html`; reconcile → 0 | Exactly `web-image` and `kubernetes` execute |
| 10 | Commit a comment-only change to `infra/network/main.tf`; reconcile → 0 | `network` executes; its outputs are unchanged, so `database` and `kubernetes` stay current |
| 11 | Commit a change to the Release template only; reconcile → 0 | Only `kubernetes` executes, because its key covers the render's inputs |
| 12 | Commit a selector typo in `environments/dev.yaml`; reconcile → 2 | Every unit is `pending-teardown`; nothing is destroyed and `deploy.git` is unchanged |
| 13 | Revert the typo; reconcile → 0 | The units return with their uids and receipts; nothing executes |
| 14 | `nyl teardown -e dev --all` → 2 | `announce`, a Command without a teardown step and with no dependency path to `kubernetes`, is released at once; `kubernetes` publishes phase 1 (catalog without workload Applications) and waits (`manual`); `database`, `network`, and `web-image` wait for it |
| 15 | `nyl teardown -e dev --unit kubernetes --confirm-removed --reason "checked in Argo CD"` → 0 | Phase 2 removes only index-owned files; `dev/state/notes.txt` survives; the commit records the operator's confirmation |
| 16 | `nyl teardown -e dev --all --approve database=<destroy digest>` → 0 | `database` is destroyed with its approved destroy-plan digest, then `network`; `web-image`'s image is left in the registry |
| 17 | `nyl get units -e dev`, then `nyl reconcile -e dev` → 2 | Every unit is `held` without an incarnation, because `--all` holds units that are still selected; reconcile recreates nothing and reports the holds |

Tier 1 variants:

- **Observe mode.** With `mode: observe`, step 14 waits on the fake observer.
  The observer first reports `observation-failed`: the teardown is uncertain
  with that reason, and `status` suggests fixing the observer or confirming.
  `--confirm-removed` then completes it as in step 15.
- **Crash after an effect.** The run is killed after `network`'s effect and
  before its checkpoint. The next run takes over the expired lease, marks
  `network` uncertain, converges, and lists it under `recovered`.
- **Lost lease.** A run whose lease was taken over pushes nothing and exits 4;
  the new run imports its checkpoints.

## Scenario 2: preview closed with its pull request

Starts after scenario 1's step 6, so dev's `network` has a current receipt.

| Step | Action | Proves |
| --- | --- | --- |
| 1 | Branch `pr-123` from `main`; commit a change to `index.html` | A pull request is a branch |
| 2 | CI job at `pr-123`'s head: `nyl state init -e pr-123 --template preview --param pr=123` → 0 | The instance's `state.yaml` exists under `pr-123/` in the shared ref `nyl/previews`, with `expiresAt` now plus 7 days |
| 3 | Same job: `nyl reconcile -e pr-123` → 0 | `web-image` and `preview-db` run (the latter reading dev's `vpcId`); `preview-site` publishes to `deploy.git` branch `previews` under `pr-123/` as target `pr-123-preview-site`; keep ref `refs/nyl/keep/pr-123` points at the job's commit |
| 4 | Branch `pr-124`; commit; CI job: `state init --template preview --param pr=124`, then `reconcile -e pr-124` → 0 | Two instances share one state ref and one deploy branch without conflicts; their Argo CD names differ |
| 5 | Commit another change to `pr-123`; CI job: `state init …` then `reconcile -e pr-123` → 0 | The expiry is extended; only the changed units execute |
| 6 | Squash-merge `pr-123` into `main` and delete the branch | The instance's recorded source commit is no longer on any branch |
| 7 | Close job at `main`: `nyl state delete -e pr-123 --teardown` → 0 | Source is fetched through the keep ref; `preview-site` publishes phase 1, waits its 2 minutes on the clock, and removes its files; then `preview-db` is destroyed and `web-image` dropped; the `pr-123/` state directory and the keep ref are removed |
| 8 | `nyl reconcile -e pr-124` → 0 | The other instance is untouched: nothing executes |
| 9 | `nyl get environments` | Lists `dev` and `pr-124` only |

Tier 1 variants:

- **No keep ref.** With `keepSource: false`, step 7 fails and suggests
  `--source`; `nyl state delete -e pr-123 --teardown --source main` warns about
  the substituted source, records it, and succeeds.
- **Name collision.** Without `nameSuffix`, `nyl validate` rejects the template
  because two instances would generate the same Argo CD names.

## Scenario 3: preview expiry

Starts after scenario 2's step 4, with `pr-123` and `pr-124` live at time T.

| Step | Action | Proves |
| --- | --- | --- |
| 1 | Clock T+3d; CI job for a new `pr-123` commit: `state init …` and `reconcile -e pr-123` → 0 | Activity extends `pr-123` to T+10d; `pr-124` keeps T+7d |
| 2 | Clock T+8d; scheduled job: `nyl reconcile --template preview` → 0 | `pr-124` has expired and is removed, a `teardown --all` then `state delete` authorized by `allowTeardown`, with `preview-site`'s delay wait; `pr-123` is reconciled as maintenance and its expiry stays T+10d |
| 3 | `nyl get environments` | `pr-124` is gone |
| 4 | Clock T+9d; `nyl state init -e pr-125 --template preview --param pr=125`, then `pr-126` → the second refuses | `maxInstances: 2` counts live instances; nothing is expired to make room |
| 5 | Clock T+11d; `nyl state init -e pr-126 --template preview --param pr=126` → 0 | At the limit, the expired `pr-123` is removed first, then `pr-126` is created |
| 6 | Clock T+19d; `nyl reconcile -e pr-125 --no-extend` → 0 | Any reconcile that finds an instance expired removes it, even one named explicitly |
| 7 | Clock T+19d; `nyl reconcile -e pr-126 --renew` → 0 | `--renew` keeps an expired instance: its expiry moves to T+26d and it reconciles |

Tier 1 variants:

- **Not teardown-ready.** With the default namespace `deletePolicy: Confirm`,
  `state init --template` warns, and step 2 reports `pr-124` as pending
  teardown with the reason and exits 2, until a run passes
  `--allow-incomplete`.
- **Manual wait.** With `preview-site` on the `manual` default, `state init
  --template` warns that instances cannot expire unattended, and step 2 stops
  after phase 1 with exit 2.

## Coverage by milestone

| Milestone | Runs |
| --- | --- |
| M3 | Scenario 1 with `Command` units standing in for every kind, tier 1: waves, approval, repeat no-op, selector typo, teardown order, crash, and lease variants |
| M4 | Scenario 1 steps 1–10 with the real `OciImage` and `OpenTofu` drivers in tier 2 |
| M5 | All three scenarios in both tiers |
| M6 | A fourth scenario extends scenario 1 with a `staging` environment and a PromotionPath for the image digest and the network source commit |

A scenario's steps change together with the contract rules they prove, as the
implementation architecture requires for walkthroughs.
