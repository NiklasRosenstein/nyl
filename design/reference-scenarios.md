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
| The project's repository | One bare `project.git` holding `main`, pull request branches, the deploy branches, and the state refs, plus a working clone |
| Pull request | Branch `pr-<n>`; an update is a commit on it |
| Merge | Squash commit on `main`, then deleting the branch |
| CI job | One `nyl` invocation in a fresh clone of `project.git` at the job's commit |
| Container registry | Tier 1: fake. Tier 2: a `registry:2` container on an ephemeral port |
| Terraform backend | OpenTofu's `local` backend with a path inside the temporary directory |
| Cloud resources | Only the built-in `terraform_data` resource and outputs, so no provider is downloaded |
| Argo CD | Tier 1: a scripted fake observer. Publish mode needs none |
| Time | An injected clock (see [Clock](#clock)) |

### Tiers

The same scenario files run in two tiers:

- **Tier 1, always in CI.** The scenario harness runs orchestration in-process
  with the real Git `StateStore` on the local bare repositories, the real
  `KubernetesPublication` driver publishing to the `deploy` and `previews` branches, and fake
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
      deploy: {branch: deploy, contains: ['dev/web/**'], unchanged: ['dev/_nyl/catalog/**']}
  - clock: {advance: 8d}
```

- Step kinds: `commit`, `branch`, `squash-merge`, `delete-branch`, `clock`,
  and `nyl`.
- Expectations cover only user-visible results: the exit code, per-unit results
  from the transition commit summary, `nyl get … -o json` documents, files and
  commits on the deploy branches and the state refs, and in tier 2 the registry's
  manifests and `tofu output`.
- Both tiers read the same file. A step that only one tier can run, such as a
  scripted observer response, is marked `tier: 1`.

## Reference project

All three scenarios run against one project, which also lives at
`examples/platform/` as the documented starting point for a project with
dev, prod, and preview environments. Same files in both places, so the
example can never drift from what is tested.

```text
examples/platform/
  nyl.toml
  config/
    repository.yaml          # GitRepository platform: this repository; deploy branches and state refs live in it
    clusters/dev.yaml        # Cluster dev
    clusters/prod.yaml       # Cluster prod
    targets/dev.yaml         # DeploymentTarget dev  → branch deploy, prefix dev/
    targets/prod.yaml        # DeploymentTarget prod → branch deploy, prefix prod/
    environments/dev.yaml    # Environment dev:  tier: platform
    environments/prod.yaml   # Environment prod: tier: platform, requireDigest approvals
    environments/preview.yaml   # EnvironmentTemplate preview: dev cluster, shared catalog, ttl 7d
  units/
    network.yaml             # OpenTofu               tier: platform
    database.yaml            # OpenTofu               tier: platform, preview: 'true'
    web-image.yaml           # OciImage               tier: platform, preview: 'true'
    seed.yaml                # Command, not idempotent   tier: platform
    kubernetes.yaml          # KubernetesPublication, target from values   tier: platform, preview: 'true'
  infra/network/main.tf
  infra/database/main.tf     # uses ../modules/postgres
  infra/modules/postgres/
  services/web/Dockerfile    # FROM scratch, COPY index.html: builds without pulling
  services/web/index.html
  applications/web/group.yaml
  applications/web/release.yaml   # inputs: image (string), database (object)
  .github/workflows/
    plan.yaml                # pull request: nyl validate; nyl plan -e dev -e prod
    reconcile.yaml           # push to main: reconcile dev, then prod behind an approval gate
    preview.yaml             # pull request opened/updated: state init --template + reconcile; closed: state delete --teardown
    previews-maintenance.yaml   # nightly: nyl reconcile --template preview
```

Directories carry no meaning: discovery follows Git visibility, so the
template sits under `environments/` next to the Environments it resembles, and
a condensed layout with everything under one `nyl/` directory works the same.

Key excerpts:

```yaml
# config/environments/dev.yaml
apiVersion: gitops.nyl/v1
kind: Environment
metadata: {name: dev}
spec:
  unitSelector: {matchLabels: {tier: platform}}
  values:
    target: dev
    cidr: 10.0.0.0/16
    backendDir: <temporary directory>/tofu
    approval: auto
---
# config/environments/prod.yaml differs only in values:
#   target: prod, cidr: 10.1.0.0/16, approval: {mode: manual, bind: plan, requireDigest: true}
---
# units/database.yaml
apiVersion: units.gitops.nyl/v1
kind: OpenTofu
metadata: {name: database, labels: {tier: platform, preview: 'true'}}
spec:
  source: {path: infra/database}
  backend: {path: '{{ values.backendDir }}/{{ environment.name }}-database.tfstate'}
  variables:
    vpc_id: {fromUnit: {unit: network, output: vpcId}}   # previews: from environment dev, see below
  outputs:
    connection: {type: object}    # host, port, secretName; never the password
  approval: {{ values.approval | tojson }}
---
# units/kubernetes.yaml: one unit for every environment
apiVersion: units.gitops.nyl/v1
kind: KubernetesPublication
metadata: {name: kubernetes, labels: {tier: platform, preview: 'true'}}
spec:
  target: {{ values.target | tojson }}    # dev, prod, or the preview template's inline target
  mode: publish                           # teardownWait defaults to manual
  {% if values.teardownWait %}teardownWait: {{ values.teardownWait | tojson }}{% endif %}
---
# config/targets/dev.yaml (excerpt)
spec:
  releaseInputs:
    web/web:
      image: {fromUnit: {unit: web-image, artifact: image, pointer: /reference}}
      database: {fromUnit: {unit: database, output: connection}}
---
# config/environments/preview.yaml
apiVersion: gitops.nyl/v1
kind: EnvironmentTemplate
metadata: {name: preview}
spec:
  parameters: [{name: pr, type: integer}]
  unitSelector: {matchLabels: {preview: 'true'}}
  values:
    backendDir: <temporary directory>/tofu
    approval: auto
    networkFrom: dev                      # database reads dev's vpcId
    teardownWait: {strategy: delay, duration: 2m}   # unattended removal
    target:
      inline:
        clusterRef: {name: dev}
        catalogApplication:
          shared: {pathPrefix: previews, name: previews}   # one self-managing catalog for every instance
        applicationGroupSelector: {matchLabels: {app: web}}
        publication: {repositoryRef: {name: platform}, revision: previews, pathPrefix: '{{ environment.name }}'}
        values: {nameSuffix: '-{{ environment.name }}'}   # unique Argo CD names and namespace per instance
        releaseInputs:
          web/web:
            image: {fromUnit: {unit: web-image, artifact: image, pointer: /reference}}
            database: {fromUnit: {unit: database, output: connection}}
  state: {path: '{{ environment.name }}', desiredRef: nyl/previews, observedRef: nyl/previews}
  allowUnprotectedSource: true
  deletionPolicy: Teardown
  allowTeardown: true
  ttl: 7d
  maxInstances: 2
```

- One repository holds everything: the source on `main`, the rendered
  manifests on the `deploy` and `previews` branches, and the state refs.
  `config/repository.yaml` names the repository's own URL, which is also what
  Nyl uses without any GitRepository. Splitting the deploy branches or the
  state into other repositories changes only that file; teams do it to give
  Argo CD or preview jobs narrower credentials, or to keep publication commits
  out of the source repository's history.
- Every preview gets its own database, because preview images commonly run
  migrations on startup, which must not touch dev's data. `database` carries
  both labels and keys its backend by environment name, so previews need no
  separate unit. Its `vpc_id` variable reads
  `{fromUnit: {environment: '{{ values.networkFrom }}', unit: network, …}}`
  when `networkFrom` is set, so previews use dev's network without running
  `network` themselves.
- `network` and `database` store their inputs in `terraform_data` resources and
  derive their outputs from them, so a comment-only change leaves every output
  unchanged. `database` stores the password in a secret store and outputs only
  its name; the Release reads the secret by name.
- The Cluster, ApplicationGroup, and catalog settings are teardown-ready:
  catalog `syncPolicy.automated` with `prune: true`, `Foreground` deletion,
  and namespace `deletePolicy: Automatic`. The ApplicationGroup templates
  Application, AppProject, and namespace names with `values.nameSuffix`.
- `seed` is a Command that loads fixture data into a store of its own. It is
  not `idempotent`, has no teardown step, and has no dependencies or
  dependents: it shows that a teardown wait never holds back unrelated units,
  and what dropping a unit's state costs.
- The harness writes the temporary directory's absolute paths into the fixture
  when it commits it: the repository URL and `values.backendDir`, so OpenTofu
  state outlives each execution's worktree.
- Tier 1's fake observer treats the shared preview catalog as applied, so
  instances' Applications appear on publish and disappear as teardown removes
  them.
- Before the first reconcile, a setup step commits an unowned file,
  `dev/state/notes.txt`, on the `deploy` branch under dev's prefix. Teardown
  must preserve it.

### Refs

Everything Nyl reads or writes in the reference project, and who else uses it:

| Ref | Written by | Read by |
| --- | --- | --- |
| `main` | People, through pull requests | Nyl: source commit of dev and prod runs; `protectedRefs` default |
| `pr-<n>` | People | Nyl: source commit of preview instance runs (`allowUnprotectedSource`) |
| `refs/nyl/keep/<instance>` | Nyl, on every instance reconcile; removed by `state delete` | Nyl: teardown after the branch is gone |
| `deploy` | Nyl: `kubernetes` publishes `dev/` and `prod/` | Argo CD: the `dev` and `prod` catalog Applications, applied once by an operator |
| `previews` | Nyl: each instance publishes `<instance>/` and its entries in `previews/_nyl/catalog/` | Argo CD: the shared `previews` catalog Application, applied once by an operator |
| `nyl/dev/desired`, `nyl/dev/observed`, same for `prod` | Nyl: one transition commit per operation | Nyl; people reviewing history; promotion pull requests target the desired ref (M6) |
| `nyl/previews` | Nyl: every instance's state under its `state.path` | Nyl |
| `nyl/<env>/lease`, `nyl/<env>/runs/<run-id>`, `nyl/<env>/signals/<run-id>` | Nyl, for the duration of a run | Nyl: `status`, takeover, confirmations |
| `refs/nyl/local/<env>/*` | Nyl `--local` runs, in the developer's clone only | Nyl |

Branch protection: `main` requires review; `deploy`, `previews`, and the state
refs accept pushes from the CI identity and the operator group only, with force
pushes and deletion disabled; preview credentials can push `previews`,
`nyl/previews`, and keep refs, and nothing else.

## Scenario 1: platform environment

| Step | Action | Proves |
| --- | --- | --- |
| 1 | `nyl validate` → 0 | The project is valid, including that the preview template's inline target generates different Argo CD names per instance |
| 2 | `nyl reconcile -e dev` → 1 | No state is created implicitly; the error names `state init` |
| 3 | `nyl state init -e dev` → 0 | `state.yaml` exists on dev's state refs in `project.git` |
| 4 | `nyl plan -e dev` → 2 | `network`, `web-image`, and `seed` are plannable; `database` and `kubernetes` are reported blocked on missing receipts, so the plan is incomplete |
| 5 | `nyl reconcile -e dev` → 2 | Wave 1 runs `network`, `web-image`, `seed`; `database` waits for approval (`bind: plan`); `kubernetes` is blocked on it. One desired and one observed commit |
| 6 | `nyl plan -e dev --unit database --output json` → 0, then `nyl reconcile -e dev --approve database=<digest>` → 0 | The approved digest is applied; `kubernetes` runs in a later wave of the same run and publishes `dev/` with the image's digest reference and the database host; the approval is in the receipt |
| 7 | `nyl get output database/host -e dev`, `nyl get artifact web-image/image -e dev --pointer /reference` | Value forms resolve like `fromUnit` |
| 8 | `nyl reconcile -e dev` → 0 | A repeated run executes nothing and writes no transition or publication commit; only its lease and run refs come and go |
| 9 | Commit a change to `services/web/index.html`; reconcile → 0 | Exactly `web-image` and `kubernetes` execute |
| 10 | Commit a comment-only change to `infra/network/main.tf`; reconcile → 0 | `network` executes; its outputs are unchanged, so `database` and `kubernetes` stay current |
| 11 | Commit a change to the Release template only; reconcile → 0 | Only `kubernetes` executes, because its key covers the render's inputs |
| 12 | Branch `typo`; commit a selector typo in `environments/dev.yaml`; pull request job: `nyl plan -e dev` → 0, and with `--fail-on-leaving` → 1 | The plan's first section lists all five units as leaving, deselected because `tier: platfrom` matches nothing: `network`, `database`, `kubernetes` would need `--allow-teardown`; `web-image` and `seed` would be dropped and re-created as new incarnations if they return |
| 13 | Merge it anyway; reconcile → 2 | `network`, `database`, and `kubernetes` are `pending-teardown`; nothing is destroyed and the `deploy` branch is unchanged, because `kubernetes` still owns target `dev` while it is deleting; `web-image` and `seed` are dropped from state |
| 14 | Revert the typo; reconcile → 0 | The pending units return with their uids and receipts and do not run; `web-image` and `seed` run again as new incarnations, the cost the plan warned about; `kubernetes` republishes only if the rebuilt image's digest differs |
| 15 | `nyl plan -e dev --teardown --all --output json` → 0 | Before anything is requested, the preview lists what decommissioning would remove, with `database`'s destroy-plan digest |
| 16 | `nyl state delete -e dev --teardown --approve database=<digest>` → 2 | `seed`, which has no teardown step and no dependency path to `kubernetes`, is released and held at once; `kubernetes` publishes phase 1 (catalog without workload Applications) and waits (`manual`); `database`, `network`, and `web-image` wait for it |
| 17 | A CI push meanwhile: `nyl reconcile -e dev` → 2 | Nothing is recreated: every unit is held or tearing down, so the image is not rebuilt and `seed` does not run again |
| 18 | `nyl teardown -e dev --unit kubernetes --confirm-removed --reason "checked in Argo CD"` → 0 | Phase 2 removes only index-owned files; `dev/state/notes.txt` survives; the commit records the operator's confirmation |
| 19 | `nyl state delete -e dev --teardown --approve database=<digest from step 15>` → 0 | The command resumes: `database` is destroyed with the previewed destroy plan, then `network`; `web-image`'s image is left in the registry; dev's state is removed |
| 20 | `nyl reconcile -e dev` → 1 | A pipeline still running dev fails visibly; the message names removing the Environment or `state init --fresh` |
| 21 | Commit removing `environments/dev.yaml` and target `dev`; `nyl validate` → 0 | Decommissioning ends in source; the units stay declared and are selected by no environment |

Tier 1 variants:

- **Observe mode.** With `mode: observe`, step 16 waits on the fake observer.
  The observer first reports `observation-failed`: the teardown is uncertain
  with that reason, and `status` suggests fixing the observer or confirming.
  `--confirm-removed` then completes it as in step 18.
- **Confirmation from a workstation.** While scenario 2's close job
  (step 7) waits out `kubernetes`'s delay, a second invocation runs `nyl teardown
  -e pr-123 --unit kubernetes --confirm-removed`. It delivers a signal to the
  waiting run and exits 0; the run ends its wait early and records the
  confirmation. A signal naming another phase 1 commit is rejected.
- **Blanket approval.** `nyl state delete -e dev --teardown --approve-all`
  destroys `database` without a digest and records the approval as
  unreviewed; with `requireDigest: true` on `database`, the same command skips
  it and exits 2.
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
| 3 | Same job: `nyl reconcile -e pr-123` → 0 | `web-image` and `database` run (the latter reading dev's `vpcId`); `kubernetes` publishes to the `previews` branch: workload trees under `pr-123/` and its Applications and AppProject in `previews/_nyl/catalog/`, writing the shared catalog manifest because it is the first instance; keep ref `refs/nyl/keep/pr-123` points at the job's commit |
| 4 | Branch `pr-124`; commit; CI job: `state init --template preview --param pr=124`, then `reconcile -e pr-124` → 0 | Two instances share one state ref and one deploy branch without conflicts; their Argo CD names differ |
| 5 | Commit another change to `pr-123`; CI job: `state init …` then `reconcile -e pr-123` → 0 | The expiry is extended; only the changed units execute |
| 6 | Squash-merge `pr-123` into `main` and delete the branch | The instance's recorded source commit is no longer on any branch |
| 7 | Close job at `main`: `nyl state delete -e pr-123 --teardown` → 0 | Source is fetched through the keep ref; `kubernetes` publishes phase 1, waits its 2 minutes on the clock, and removes its files, including its entries in the shared catalog; then `database` is destroyed and `web-image` dropped; the `pr-123/` state directory and the keep ref are removed |
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
| 2 | Clock T+8d; scheduled job: `nyl reconcile --template preview` → 0 | `pr-124` has expired and is removed, a `teardown --all` then `state delete` authorized by `allowTeardown`, with `kubernetes`'s delay wait; `pr-123` is reconciled as maintenance and its expiry stays T+10d |
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
- **Manual wait.** With `kubernetes` on the `manual` default, `state init
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
