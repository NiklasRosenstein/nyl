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
  with the real Git `StateStore` on the local bare repositories and a scripted
  fake Argo CD observer that reports, per step, that Applications exist, are
  gone, or cannot be read. Drivers are the real ones where they exist and fakes
  otherwise, see [Fake kinds](#fake-kinds).
- **Tier 2, the real tools.** The same steps run the real `nyl` binary as
  separate processes, with the real `tofu` and `docker buildx` against the local
  registry. Tier 2 scenarios compile only with the `tier2` Cargo feature, and
  with it a missing `tofu`, `docker buildx`, or registry fails the test instead
  of skipping it, so a passing tier 2 always ran. A dedicated CI job with the
  tools installed (OpenTofu through mise, Docker with buildx, and `registry:2`
  as a service container) runs `cargo test --features tier2` and is required
  for merging from M4 on. Tests never read environment variables to decide
  whether to run.

### Fake kinds

Test-only kinds in `units.test.nyl/v1` stand in for built-in kinds that do not
exist yet in a milestone, so each milestone runs the whole platform scenario.
Each implements exactly the driver trait and capabilities of the kind it stands
in for:

| Fake kind | Stands in for | Behaves like |
| --- | --- | --- |
| `FakeImage` | `OciImage` | Returns a digest derived from the execution key; publishes a `ContainerImage` artifact; declares `build` |
| `FakeInfra` | `OpenTofu` | Keeps resources and outputs in a JSON file per backend key; reports plan and destroy digests for `bind: plan`; tears down |
| `FakePublication` | `KubernetesPublication` | Writes a tree with an ownership index to the deploy branch and runs both teardown phases against the fake observer |

The scenario files name the real kinds; the harness maps them to fakes for the
milestones that need it. Tier 1 keeps the fakes for images and OpenTofu even
after the real drivers exist, so it never needs the tools.

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

The example uses a condensed layout: one file per environment, one for shared
resources, and the native source files next to them.

```text
examples/platform/
  nyl.toml
  nyl/
    platform.yaml            # GitRepository platform; ArgoCDInstances dev and prod; PromotionPath dev-to-prod; units
    dev.yaml                 # Cluster dev, DeploymentTarget dev, Environment dev (follows its worktree)
    prod.yaml                # Cluster prod, DeploymentTarget prod, Environment prod (source fromPromotion)
    preview.yaml             # EnvironmentTemplate preview: dev cluster, shared catalog, ttl 7d
    demo.yaml                # DeploymentTarget demo: static inputs, no orchestration
  infra/network/main.tf
  infra/database/main.tf     # uses ../modules/postgres
  infra/modules/postgres/
  services/web/Dockerfile    # FROM scratch, COPY index.html: builds without pulling
  services/web/index.html
  applications/web/group.yaml
  applications/web/release.yaml   # inputs: image (string), database (object)
  .github/workflows/
    plan.yaml                # pull request: nyl validate; nyl plan -e dev -e prod
    reconcile.yaml           # push to main: reconcile dev; promote dev-to-prod and reconcile prod behind an approval gate
    preview.yaml             # pull request opened/updated: reconcile -e pr-<n> --template preview --param pr=<n>; closed: state delete --teardown
    previews-maintenance.yaml   # nightly: nyl reconcile --template preview
```

| Unit | dev | prod | preview |
| --- | --- | --- | --- |
| `network` | ✓ | ✓ | reads dev's |
| `database` | ✓ | ✓ | ✓ (its own) |
| `web-image` | ✓ | reuses dev's through promotion | ✓ (the pull request's) |
| `seed` | ✓ | | |
| `kubernetes` | ✓ | ✓ | ✓ |

Directories and files carry no meaning: discovery follows Git visibility, and a
file may hold any number of resources. The documentation suggests an expanded
layout for larger projects, with one resource per file:

```text
config/repository.yaml   config/clusters/<name>.yaml   config/targets/<name>.yaml
config/environments/<name>.yaml   config/promotion/<path>.yaml   units/<unit>.yaml
```

Key excerpts:

```yaml
# dev.yaml
apiVersion: gitops.nyl/v1
kind: Environment
metadata: {name: dev}
spec:
  unitSelector: {matchLabels: {dev: 'true'}}
  values:
    target: dev
    cidr: 10.0.0.0/16
    backendDir: <temporary directory>/tofu
    approval: auto                        # dev applies without gates; prod requires reviewed plans
---
# prod.yaml: prod runs definitions dev proved, and reuses dev's image
apiVersion: gitops.nyl/v1
kind: Environment
metadata: {name: prod}
spec:
  source: {fromPromotion: {path: dev-to-prod}}
  unitSelector: {matchLabels: {prod: 'true'}}      # no web-image
  values:
    target: prod
    cidr: 10.1.0.0/16
    approval: {mode: manual, bind: plan, requireDigest: true}
---
apiVersion: k8s.gitops.nyl/v1
kind: DeploymentTarget
metadata: {name: prod}
spec:
  argocdRef: {name: prod}
  releaseInputs:
    web/web:
      image: {fromPromotion: {path: dev-to-prod, value: webImage}}
      database: {fromUnit: {unit: database, output: connection}}
---
# platform.yaml: one Argo CD instance per cluster with teardown-ready catalog
# defaults; every target, inline ones included, names its instance
apiVersion: k8s.gitops.nyl/v1
kind: ArgoCDInstance
metadata: {name: dev}
spec:
  clusterRef: {name: dev}
  catalogApplicationDefaults:
    syncPolicy: {automated: {prune: true}}
---
# platform.yaml (prod's ArgoCDInstance is the same for Cluster prod)
apiVersion: gitops.nyl/v1
kind: PromotionPath
metadata: {name: dev-to-prod}
spec:
  from: {environment: dev}
  to: {environment: prod}
  evidence: healthy
  values:
    webImage: {select: {unit: web-image, artifact: image, pointer: /reference}}
---
# applications/web/group.yaml: names and namespaces per target, so previews never collide
apiVersion: k8s.gitops.nyl/v1
kind: ApplicationGroup
metadata: {name: web, labels: {app: web}}
spec:
  applicationNamespace: argocd
  applicationNameTemplate: '${ target.metadata.name }-${ release.metadata.name }'   # a template value, expanded per Release
  destinationNamespace: '{{ values.namespace | default("web") }}'
  projectTemplate:
    name: '{{ target.metadata.name }}-web'
    destinationNamespaces: ['{{ values.namespace | default("web") }}']
---
# platform.yaml: database (block-style envelope, because the document is
# not valid YAML before rendering)
apiVersion: units.gitops.nyl/v1
kind: OpenTofu
metadata:
  name: database
  labels:
    dev: 'true'
    prod: 'true'
    preview: 'true'
spec:
  source: {path: infra/database}
  backend: {path: '{{ values.backendDir }}/{{ environment.name }}-database.tfstate'}
  variables:
    vpc_id: {fromUnit: {unit: network, output: vpcId}}   # previews: from environment dev, see below
  outputs:
    connection: {type: object}    # host, port, secretName; never the password
  approval: {{ values.approval | tojson }}
---
# platform.yaml: kubernetes, one unit for every environment
apiVersion: units.gitops.nyl/v1
kind: KubernetesPublication
metadata:
  name: kubernetes
  labels:
    dev: 'true'
    prod: 'true'
    preview: 'true'
spec:
  target: {{ values.target | tojson }}    # dev, prod, or the preview template's inline target
  mode: publish                           # teardownWait defaults to manual
  {% if values.teardownWait %}teardownWait: {{ values.teardownWait | tojson }}{% endif %}
---
# dev.yaml: DeploymentTarget dev (excerpt)
spec:
  releaseInputs:
    web/web:
      image: {fromUnit: {unit: web-image, artifact: image, pointer: /reference}}
      database: {fromUnit: {unit: database, output: connection}}
---
# preview.yaml
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
        argocdRef: {name: dev}
        catalogApplication:
          shared: {pathPrefix: previews, name: previews}   # one self-managing catalog for every instance
        applicationGroupSelector: {matchLabels: {app: web}}
        publication: {repositoryRef: {name: platform}, revision: previews, pathPrefix: '{{ environment.name }}'}
        values: {namespace: '{{ environment.name }}-web'}   # its own namespace; names come from the target name
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
  The GitRepository in `nyl/platform.yaml` names the repository's own URL, which is also what
  Nyl uses without any GitRepository. Splitting the deploy branches or the
  state into other repositories changes only that resource; teams do it to give
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
  and namespace `deletePolicy: Automatic`. The ApplicationGroup derives
  Application and AppProject names from the target's name, and each preview
  sets its own namespace, so no preview can collide with dev or delete its
  namespace.
- Target `demo` binds the Release's inputs with plain `value` bindings and
  belongs to no environment. `render-tree --target demo` renders the same
  Release without orchestration, while `render-tree --target dev` fails and
  names `nyl build kubernetes -e dev`, because dev's bindings resolve only in
  its environment.
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
| `main` | People, through pull requests | Nyl: dev runs from its worktree; `protectedRefs` default |
| `pr-<n>` | People | Nyl: each preview instance runs from its pull request job's worktree, recorded in state (`allowUnprotectedSource`) |
| `refs/nyl/keep/<instance>` | Nyl, on every instance reconcile; removed by `state delete` | Nyl: teardown after the branch is gone |
| `deploy` | Nyl: `kubernetes` publishes `dev/` and `prod/` | Argo CD: the `dev` and `prod` catalog Applications, applied once by an operator |
| `previews` | Nyl: each instance publishes `<instance>/` and its entries in `previews/_nyl/catalog/` | Argo CD: the shared `previews` catalog Application, applied once by an operator |
| `nyl/dev/desired`, `nyl/dev/observed`, same for `prod` | Nyl: one transition commit per operation | Nyl; people reviewing history; promotion pull requests target the desired ref (M6) |
| `nyl/previews` | Nyl: every instance's state under its `state.path` | Nyl |
| `nyl/<env>/lease`, `nyl/<env>/runs/<run-id>`, `nyl/<env>/signals/<run-id>` | Nyl, for the duration of a run | Nyl: `status`, takeover, confirmations |
| `refs/nyl/local/<env>/*` | Nyl `--local` runs, in the developer's clone only | Nyl |

Branch protection: `main` requires review; `deploy`, `previews`, and the state
refs accept pushes from the CI identity and the operator group only, with force
pushes and deletion disabled, except that Nyl creates and deletes the lease,
run, and signal refs of each environment. Preview credentials can push
`previews`, `nyl/previews`, keep refs, and the lease, run, and signal refs of
preview instances (`nyl/pr-*/…`), and nothing else.

## Scenario 1: platform environment

| Step | Action | Proves |
| --- | --- | --- |
| 1 | `nyl validate` → 0 | The project is valid, including that the preview template's inline target generates different Argo CD names per instance |
| 2 | `nyl reconcile -e dev` → 1 | No state is created implicitly; the error names `state init` |
| 3 | `nyl state init -e dev` → 0 | `state.yaml` exists on dev's state refs in `project.git` |
| 4 | `nyl plan -e dev` → 2 | `network`, `web-image`, and `seed` are plannable; `database` and `kubernetes` are reported blocked on missing receipts, so the plan is incomplete |
| 5 | `nyl reconcile -e dev` → 0 | Wave 1 runs `network`, `web-image`, `seed`; wave 2 runs `database` with `network`'s `vpcId`; wave 3 runs `kubernetes`, which publishes `dev/` with the image's digest reference and the database connection. One desired and one observed commit |
| 6 | `nyl status -e dev` → 0 | Every unit is current; the status names the publication commit and the image reference |
| 7 | `nyl get output database/connection -e dev --pointer /host`, `nyl get artifact web-image/image -e dev --pointer /reference` | Value forms resolve like `fromUnit` |
| 8 | `nyl reconcile -e dev` → 0 | A repeated run executes nothing and writes no transition or publication commit; only its lease and run refs come and go |
| 9 | Commit a change to `services/web/index.html`; reconcile → 0 | Exactly `web-image` and `kubernetes` execute |
| 10 | Commit a comment-only change to `infra/network/main.tf`; reconcile → 0 | `network` executes; its outputs are unchanged, so `database` and `kubernetes` stay current |
| 11 | Commit a change to the Release template only; reconcile → 0 | Only `kubernetes` executes, because its key covers the render's inputs |
| 12 | Branch `typo`; commit a selector typo in dev's Environment; pull request job: `nyl plan -e dev` → 0, and with `--fail-on-leaving` → 1 | The plan's first section lists all five units as leaving, deselected because `dve: 'true'` matches nothing: `network`, `database`, `kubernetes` would need `--allow-teardown`; `web-image` and `seed` would be dropped and re-created as new incarnations if they return |
| 13 | Merge it anyway; reconcile → 2 | `network`, `database`, and `kubernetes` are `pending-teardown`; nothing is destroyed and the `deploy` branch is unchanged, because `kubernetes` still owns target `dev` while it is deleting; `web-image` and `seed` are dropped from state |
| 14 | Revert the typo; reconcile → 0 | The pending units return with their uids and receipts and do not run; `web-image` and `seed` run again as new incarnations, the cost the plan warned about; `kubernetes` republishes only if the rebuilt image's digest differs |
| 15 | `nyl plan -e dev --teardown --all` → 0 | Before anything is requested, the preview lists what decommissioning would remove, with each OpenTofu unit's destroy plan |
| 16 | `nyl state delete -e dev --teardown` → 2 | `seed`, which has no teardown step and no dependency path to `kubernetes`, is released and held at once; `kubernetes` publishes phase 1 (catalog without workload Applications) and waits (`manual`); `database`, `network`, and `web-image` wait for it |
| 17 | A CI push meanwhile: `nyl reconcile -e dev` → 2 | Nothing is recreated: every unit is held or tearing down, so the image is not rebuilt and `seed` does not run again |
| 18 | `nyl teardown -e dev --unit kubernetes --confirm-removed --reason "checked in Argo CD"` → 0 | Phase 2 removes only index-owned files; `dev/state/notes.txt` survives; the commit records the operator's confirmation |
| 19 | `nyl state delete -e dev --teardown` → 0 | The command resumes: `database` is destroyed, then `network`; `web-image`'s image is left in the registry; dev's state is removed |
| 20 | `nyl reconcile -e dev` → 1 | A pipeline still running dev fails visibly; the message names removing the Environment or `state init --fresh` |
| 21 | Commit removing dev's Environment; `nyl validate` → 1 | Validation names what still needs dev: PromotionPath `dev-to-prod` and the preview template's reference to dev's `network`. Decommissioning an environment others depend on means rewiring them first |

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
- **Manual approvals.** The realistic project gates only prod, which needs
  M6. To exercise approvals from M3 on, this variant sets dev's `approval`
  value to `{mode: manual, bind: plan}`: step 5 exits 2 with `database`
  awaiting approval and `kubernetes` blocked on it; `nyl plan -e dev --unit
  database --output json` prints the digest, and `--approve database=<digest>`
  applies it and runs `kubernetes` in a later wave; step 16 needs the destroy
  digest from `plan --teardown`; `--approve-all` approves without a digest and
  is recorded as unreviewed, and with `requireDigest: true` it skips
  `database` and exits 2.
- **Crash after an effect.** The run is killed after `network`'s effect and
  before its checkpoint. After the clock passes the lease deadline, the next
  run takes over the expired lease, marks
  `network` uncertain, converges, and lists it under `recovered`.
- **Cancellation.** A SIGTERM during `database`'s apply stops the tool,
  checkpoints finished units, marks `database` uncertain, and releases the
  lease within the grace the harness allows; the next run starts at once
  and converges `database`.
- **Broken lease.** After a forced kill, `nyl lease break -e dev --reason
  "runner lost"` lets the next run take over before the deadline.
- **Busy instance.** While a pull request job holds `pr-124`'s lease, the
  fleet job skips it as `busy` and exits by the other instances' results.
- **Close job waiting.** `state delete -e pr-123 --teardown --wait-lease 10m`
  waits for a running reconcile of `pr-123` to finish instead of exiting 2.
- **Lost lease.** A run whose lease was taken over pushes nothing and exits 4;
  the new run imports its checkpoints.

## Scenario 2: preview closed with its pull request

Starts after scenario 1's step 6, so dev's `network` has a current receipt.

| Step | Action | Proves |
| --- | --- | --- |
| 1 | Branch `pr-123` from `main`; commit a change to `index.html` | A pull request is a branch |
| 2 | CI job: `nyl reconcile -e pr-123 --template preview --param pr=123` → 0 | The instance is created: its `state.yaml` exists under `pr-123/` in the shared ref `nyl/previews`, recording `pr-123`'s commit as its source and `expiresAt` now plus 7 days |
| 3 | The same run continues | `web-image` and `database` run (the latter reading dev's `vpcId`); `kubernetes` publishes to the `previews` branch: workload trees under `pr-123/` and its Applications and AppProject in `previews/_nyl/catalog/`, writing the shared catalog manifest because it is the first instance; keep ref `refs/nyl/keep/pr-123` points at `pr-123`'s tip |
| 4 | Branch `pr-124`; commit; CI job: `nyl reconcile -e pr-124 --template preview --param pr=124` → 0 | Two instances share one state ref and one deploy branch without conflicts; their Argo CD names differ |
| 5 | Commit another change to `pr-123`; CI job: the same command for `pr-123` → 0 | The expiry is extended; only the changed units execute |
| 6 | Squash-merge `pr-123` into `main` and delete the branch | The instance's recorded source commit is no longer on any branch |
| 7 | Close job at `main`: `nyl state delete -e pr-123 --teardown` → 0 | Source is fetched through the keep ref; `kubernetes` publishes phase 1, waits its 2 minutes on the clock, and removes its files, including its entries in the shared catalog; then `database` is destroyed and `web-image` dropped; the `pr-123/` state directory and the keep ref are removed |
| 8 | From the `main` checkout: `nyl reconcile -e pr-124` → 0 | The run renders `pr-124` at its recorded commit, not at `main`, and says so; nothing executes |
| 9 | `nyl get environments` | Lists `dev`, `prod`, and `pr-124`; `pr-123` is gone |

Tier 1 variants:

- **No keep ref.** With `keepSource: false`, step 7 fails and suggests
  `--source`; `nyl state delete -e pr-123 --teardown --source main` warns about
  the substituted source, records it, and succeeds.
- **Name collision.** With an `applicationNameTemplate` that ignores the
  target's name, `nyl validate` rejects the template because two instances
  would generate the same Argo CD names.

## Scenario 3: preview expiry

Starts after scenario 2's step 4, with `pr-123` and `pr-124` live at time T.

| Step | Action | Proves |
| --- | --- | --- |
| 1 | Clock T+3d; CI job for a new `pr-123` commit: `nyl reconcile -e pr-123 --template preview --param pr=123` → 0 | Activity extends `pr-123` to T+10d; `pr-124` keeps T+7d |
| 2 | Clock T+8d; scheduled job: `nyl reconcile --template preview` → 0 | `pr-124` has expired and is removed, a `teardown --all` then `state delete` authorized by `allowTeardown`, with `kubernetes`'s delay wait; `pr-123` is reconciled as maintenance and its expiry stays T+10d |
| 3 | `nyl get environments` | `pr-124` is gone |
| 4 | Clock T+9d; `nyl state init -e pr-125 --template preview --param pr=125`, then `pr-126` → the second exits 2 | `maxInstances: 2` counts every instance until it is removed; nothing is expired, so the message names no instance to remove |
| 5 | Clock T+11d; `nyl state init -e pr-126 --template preview --param pr=126` → 2 | At the limit, `pr-126`'s job does not remove the expired `pr-123`; the message names it and suggests the scheduled job or `--make-room` |
| 6 | Same clock; `nyl state init -e pr-126 --template preview --param pr=126 --make-room` → 0 | With the opt-in, the expired `pr-123` is removed first, then `pr-126` is created |
| 7 | Clock T+19d; `nyl reconcile -e pr-125 --no-extend` → 0 | Any reconcile that finds an instance expired removes it, even one named explicitly |
| 8 | Clock T+19d; `nyl reconcile -e pr-126 --renew` → 0 | `--renew` keeps an expired instance: its expiry moves to T+26d and it reconciles at the commit `state init --template` recorded |

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
| M3 | Scenario 1 in tier 1 with fake kinds for images, OpenTofu, and the publication: waves, `bind: plan` approvals and destroy digests, repeat no-op, selector typo, decommissioning with teardown order and `--confirm-removed`, crash, cancellation, and lease variants |
| M4 | Scenario 1 in tier 2 with the real `OciImage` and `OpenTofu` drivers and `FakePublication`, plus `nyl build` for an image |
| M5 | All three scenarios in both tiers, with the real `KubernetesPublication` |
| M6 | A fourth scenario promotes dev's source commit and image digest to prod, including prod's reviewed-plan approvals with `requireDigest`, a rollback, and a value-only path |

The reference project grows with the milestones. A milestone's fixture omits
resources whose kinds it does not have yet: M3 and M4 run scenario 1 without
the preview template, the PromotionPath, and prod's promoted source, so step
1's per-instance name check and step 21 join in M5 and M6. In tier 2, the
`test-kinds` Cargo feature registers the fake kinds in the `nyl` binary, which
is how M4 runs `FakePublication` through the real binary.

A scenario's steps change together with the contract rules they prove, as the
implementation architecture requires for walkthroughs.
