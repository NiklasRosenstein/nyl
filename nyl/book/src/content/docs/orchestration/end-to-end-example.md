---
title: 'End-to-end example'
---

This page builds one environment from start to finish, first on a workstation
and then in GitHub Actions: a `dev` environment whose single reconcile builds a
container image, applies an OpenTofu configuration, and publishes Kubernetes
manifests that consume both the image digest and the OpenTofu output, and a
preview environment for every pull request that is removed when the pull
request closes or expires. The same project, the same commands, and the same
Git state serve both: a job in CI differs from a run on your laptop only in
the credentials it holds. GitLab pipelines arrive with the CI adapters
(M8); this page uses GitHub Actions, and every step is an ordinary `nyl`
invocation that any CI system can run.

:::note[Where the listings come from]
OpenTofu and Docker were not available where this page was written, so every
`console` listing comes from a test build of Nyl (`test-clock`, `test-kinds`)
that runs the project below with fake image and OpenTofu kinds and the built-in
`KubernetesPublication`. The image reference and the database host are therefore
the fakes' (a digest derived from the unit's execution key, not one a registry
returned), and times come from the test clock. The `nyl attest` listings come
from the same build reading a small local stand-in for the Argo CD API. Per-unit
progress lines on stderr are left out, and paths are shortened to `/tmp/shop`.
Everything else is Nyl's own output. The same project runs with real OpenTofu,
Docker Buildx, and a registry in
[the reference scenarios' second tier](https://github.com/NiklasRosenstein/nyl/blob/develop/design/reference-scenarios.md#tiers)
on every change.
:::

## The project

The project is `nyl/tests/scenarios/examples/end-to-end/project` in the Nyl
repository, which CI runs end to end. Besides the application it has two
directories: `infra/database`, an OpenTofu configuration, and `services/web`, a
Dockerfile.

```text
.gitignore                    # .nyl/, Nyl's local caches
nyl/nyl.toml
nyl/platform.yaml             # repository, cluster, Argo CD, targets, environment
nyl/units.yaml                # database, web-image, kubernetes
nyl/web.yaml                  # the ApplicationGroup
nyl/preview.yaml              # the EnvironmentTemplate
applications/web/release.yaml # the Release both kinds of environment deploy
infra/database/main.tf
services/web/Dockerfile
```

Add `.nyl/` to `.gitignore`: Nyl caches under it, and a build that publishes
refuses a worktree with untracked files it cannot attribute to a commit.

`nyl/platform.yaml` declares where things are published and which environment
runs. The `dev` DeploymentTarget publishes the `web` Release with an image
digest and a database host that come from units:

```yaml
apiVersion: gitops.nyl/v1
kind: GitRepository
metadata: {name: shop}
spec:
  repoURL: https://github.com/example/shop.git
  publishURL: https://github.com/example/shop.git
---
apiVersion: k8s.nyl/v1
kind: Cluster
metadata: {name: dev}
spec:
  destination: {server: https://kubernetes.default.svc}
  live: {context: dev-admin}      # for `nyl attest`: the kubeconfig context of the Argo CD cluster
  kubernetes:
    kubeVersion: 1.31.4
    apiVersions: [v1, apps/v1, v1/Namespace, apps/v1/Deployment]
    clusterScopedKinds: [core/Namespace]
---
apiVersion: k8s.gitops.nyl/v1
kind: ArgoCDInstance
metadata: {name: dev}
spec:
  clusterRef: {name: dev}
  namespace: argocd
  catalogApplicationDefaults:
    syncPolicy: {automated: {prune: true}}   # so a removed target's Applications disappear
---
apiVersion: k8s.gitops.nyl/v1
kind: DeploymentTarget
metadata: {name: dev}
spec:
  clusterRef: {name: dev}
  argocdRef: {name: dev}
  applicationGroupSelector: {matchLabels: {app: web}}
  publication:
    repositoryRef: {name: shop}
    revision: deploy
    pathPrefix: dev
  releaseInputs:
    web/web:
      image: {fromUnit: {unit: web-image, artifact: image, pointer: /reference}}
      databaseHost: {fromUnit: {unit: database, output: connection, pointer: /host}}
      namespace: {value: web}
---
apiVersion: gitops.nyl/v1
kind: Environment
metadata: {name: dev}
spec:
  unitSelector: {matchLabels: {stage: dev}}
  values:
    target: dev
    teardownWait: {strategy: manual}
    stateDir: /var/lib/shop/tofu
  state: {repositoryRef: {name: shop}}
  allowUnprotectedSource: true
```

`nyl/units.yaml` declares the three units. The publication depends on the other
two through the target's `fromUnit` bindings, so nothing says so twice:

```yaml
# Block-style labels, because the publication's document is not valid YAML
# before it renders.
apiVersion: units.gitops.nyl/v1
kind: OpenTofu
metadata:
  name: database
  labels:
    stage: dev
    preview: 'true'
spec:
  source: {path: ../infra/database}
  backend: {path: '{{ values.stateDir }}/{{ environment.name }}-database.tfstate'}
  variables: {name: '{{ environment.name }}'}
  outputs:
    connection: {type: object}
---
apiVersion: units.gitops.nyl/v1
kind: OciImage
metadata:
  name: web-image
  labels:
    stage: dev
    preview: 'true'
spec:
  env: {passthrough: [DOCKER_CONFIG]}
  context: {path: ../services/web}
  repository: registry.example.com/web
---
apiVersion: units.gitops.nyl/v1
kind: KubernetesPublication
metadata:
  name: kubernetes
  labels:
    stage: dev
    preview: 'true'
spec:
  target: {{ values.target | tojson }}      # dev, or the preview template's inline target
  mode: observe
  teardownWait: {{ values.teardownWait | tojson }}
  env: {passthrough: [KUBECONFIG]}       # the credential `nyl attest` reads Argo CD with
```

The OpenTofu unit's root module declares `backend "local" {}` and an output
`connection`, the image unit builds `services/web`, and the publication's
`mode: observe` is what lets `nyl attest` and `nyl observe` read Argo CD later
(see [Observing Argo CD](/nyl/orchestration/resource-guides/kubernetes-publication/#observing-argo-cd)).
A local OpenTofu backend suits a workstation; [CI needs a remote one](#reconcile-on-main).

`nyl/preview.yaml` is what makes every pull request an environment of its own.
Each instance publishes its own tree to the `previews` branch behind one shared
catalog Application, gets its own database and image, and expires after seven
days of inactivity:

```yaml
apiVersion: gitops.nyl/v1
kind: EnvironmentTemplate
metadata: {name: preview}
spec:
  parameters: {pr: {type: integer}}
  unitSelector: {matchLabels: {preview: 'true'}}
  values:
    stateDir: /var/lib/shop/tofu
    teardownWait: {strategy: delay, duration: 2m}   # previews expire unattended
    target:
      inline:
        clusterRef: {name: dev}
        argocdRef: {name: dev}
        catalogApplication:
          shared: {name: previews}      # one self-managing catalog for every instance
        applicationGroupSelector: {matchLabels: {app: web}}
        publication: {repositoryRef: {name: shop}, revision: previews, pathPrefix: '{{ environment.name }}'}
        releaseInputs:
          web/web:
            image: {fromUnit: {unit: web-image, artifact: image, pointer: /reference}}
            databaseHost: {fromUnit: {unit: database, output: connection, pointer: /host}}
            namespace: {value: '{{ environment.name }}-web'}
  state: {repositoryRef: {name: shop}, path: '{{ environment.name }}', desiredRef: nyl/previews, observedRef: nyl/previews}
  allowUnprotectedSource: true
  deletionPolicy: Teardown
  allowTeardown: true
  ttl: 7d
  maxInstances: 20
```

The ApplicationGroup in `nyl/web.yaml` names its Applications and AppProject
after the target, so no two targets generate the same Argo CD names
(`nyl validate` rejects the project otherwise), and it declares its labels in
block style because its spec uses template syntax:

```yaml
apiVersion: k8s.gitops.nyl/v1
kind: ApplicationGroup
metadata:
  name: web
  labels:
    app: web
spec:
  applicationNamespace: argocd
  source: {path: ../applications/web}
  applicationNameTemplate: '${ target.metadata.name }-${ release.metadata.name }'
  projectTemplate:
    name: '{{ target.metadata.name }}-web'
  namespace: {deletePolicy: Automatic}
```

The Release in `applications/web/release.yaml` is a plain Deployment whose image
is `{{ inputs.image }}` and whose `DATABASE_HOST` is `{{ inputs.databaseHost }}`.
Nothing in it knows about orchestration.

## Run it locally

You need Git, OpenTofu (`tofu`), and Docker with Buildx, and a registry the
image can be pushed to (`reconcile` always pushes). For an experiment, start a
throwaway one and point the image unit's `repository` at it:

```bash
docker run -d --rm -p 127.0.0.1:5000:5000 registry:2   # then repository: localhost:5000/web
export DOCKER_CONFIG=$HOME/.docker                     # the unit admits it, so it must exist
export KUBECONFIG=$HOME/.kube/config                   # likewise: the publication admits it
```

A unit sees only the environment variables it admits with `env`, and an
admitted variable that is missing fails the unit where it runs, so a job that
plans or reconciles a unit needs every variable that unit admits.
[Credentials](#credentials-and-the-variables-units-admit) lists them.

Commit the project, then check it and create dev's state, which Nyl never
creates implicitly:

```console
$ nyl validate
✓ Found project config: nyl/nyl.toml
✓ Helm chart search path exists: .
✓ GitOps configuration is valid (9 resource(s), 3 unit(s), 4 YAML file(s))
⚠ Components search path does not exist: components

✓ Validation passed with warnings
$ nyl state init -e dev
state-init dev: done
  desired: f486c453c0bdc2affdc96bdbe6f75e225eb7ad27
  observed: f486c453c0bdc2affdc96bdbe6f75e225eb7ad27
  location: file:///tmp/shop/shop.git (nyl/dev/desired and nyl/dev/observed, coordination refs/nyl/)
  stateLocalPath: /tmp/shop/shop.git
  stateRepository: file:///tmp/shop/shop.git
```

The state is two branches of the repository, `nyl/dev/desired` and
`nyl/dev/observed`. Planning reads it, runs the tools' own plans, and changes
nothing. The publication waits for the units it reads, so the plan is
incomplete (exit 2):

```console
$ nyl plan -e dev
UNIT        KIND                   STATE    RECEIPT  CHANGES  DETAIL
database    OpenTofu               ready    none     create   create
web-image   OciImage               ready    none     create   build and push a new image
kubernetes  KubernetesPublication  blocked  none     -        /releases/web~1web/databaseHost waits for database (no-current-receipt); /releases/web~1web/image waits for web-image (no-current-receipt)

  source:   b5e03f05bb4735244684cad4cdfdbf7fdcb58f6e
            file:///tmp/shop/shop.git
```

One reconcile builds the image, applies OpenTofu, and then publishes. The
second wave starts when the first has current receipts, and the publication reads
the image digest and the database host from them:

```console
$ nyl reconcile -e dev
UNIT        KIND                   RESULT    DETAIL
database    OpenTofu               executed  reason: no receipt
web-image   OciImage               executed  reason: no receipt
kubernetes  KubernetesPublication  executed  reason: no receipt

  run:      01a1b0c0-7820-760e-9a2d-77fbe94268e1
  source:   b5e03f05bb4735244684cad4cdfdbf7fdcb58f6e
            file:///tmp/shop/shop.git
  desired:  874b309b711a721899898456eb923c461e5cd48b
  observed: 5363ee9cea206432fb69dcbb0eaa082ab8cf01f4

warning  dev/kubernetes: the self-managing catalog Application and its AppProjects remain after teardown

  Action  let a parent Application or ApplicationSet own the target's catalog (catalogApplication.enabled: false) to remove them with the teardown

  NYL-TEARDOWN-CATALOG-REMAINS
```

The warning is the publication's teardown readiness: the catalog Application of
a target lives in the tree it manages and stays when the target is removed.
`status` shows the three receipts, and `get` resolves a value the way a
`fromUnit` binding does:

```console
$ nyl status -e dev
UNIT        KIND                   STATE    RECEIPT  LAST RUN                              DETAIL
database    OpenTofu               current  current  01a1b0c0-7820-760e-9a2d-77fbe94268e1  
kubernetes  KubernetesPublication  current  current  01a1b0c0-7820-760e-9a2d-77fbe94268e1  
web-image   OciImage               current  current  01a1b0c0-7820-760e-9a2d-77fbe94268e1  

state repository: file:///tmp/shop/shop.git
state local path: /tmp/shop/shop.git
  source:   b5e03f05bb4735244684cad4cdfdbf7fdcb58f6e
            file:///tmp/shop/shop.git
$ nyl get output database/connection -e dev --pointer /host
database.c90b3e4f44fd.internal
$ nyl get artifact web-image/image -e dev --pointer /reference
registry.example.com/web@sha256:8d118458f015c7d9b869e1e20d0f9f945288395b6f131b3c53cbf2a4d14556ea
```

The published tree carries both values. The Deployment's image is the
reference above and `DATABASE_HOST` the host above, rendered from the same
`web` Release as any other target:

```console
$ git ls-tree -r --name-only origin/deploy
dev/_nyl/catalog/application.argoproj.io/argocd/dev-catalog.yaml
dev/_nyl/catalog/application.argoproj.io/argocd/dev-web.yaml
dev/_nyl/catalog/appproject.argoproj.io/argocd/dev-web.yaml
dev/_nyl/index.json
dev/web/web/deployment.apps/web/web.yaml
dev/web/web/namespace/web.yaml
```

A repeated reconcile executes nothing and writes nothing but its lease:

```console
$ nyl reconcile -e dev
UNIT        KIND                   RESULT   DETAIL
database    OpenTofu               current  
web-image   OciImage               current  
kubernetes  KubernetesPublication  current  

  run:      01a1b0c0-7c42-7e51-9f19-bbc399b1916e
  source:   b5e03f05bb4735244684cad4cdfdbf7fdcb58f6e
            file:///tmp/shop/shop.git

warning  dev/kubernetes: the self-managing catalog Application and its AppProjects remain after teardown

  Action  let a parent Application or ApplicationSet own the target's catalog (catalogApplication.enabled: false) to remove them with the teardown

  NYL-TEARDOWN-CATALOG-REMAINS
```

### The same Release without orchestration

A Release does not know whether its inputs come from units. The project also
declares a `static` DeploymentTarget that no environment uses, with the image
and database host written out as `value` bindings:

```yaml
releaseInputs:
  web/web:
    image: {value: 'registry.example.com/web@sha256:0000…'}
    databaseHost: {value: shop.db.internal}
    namespace: {value: web}
```

[`nyl publish-tree`](/nyl/reference/cli/publish-tree/) renders and publishes it
executes no units and needs none of their tools:

```console
$ nyl publish-tree --target static
✓ Published deployment target static
  Repository: file:///tmp/shop/shop.git
  Branch: static
  Commit: 37c3e5b0b5612236c763caf471b0b6566eaa33a3
[1/1] Release web/web (/applications/web/release.yaml)

Render statistics
  Cache
    Target tree         rebuilt
    Release renders     1 rendered
  Sources
    Git repositories    1 cloned · 1 ref refreshed
$ git ls-tree -r --name-only origin/static
static/_nyl/catalog/application.argoproj.io/argocd/static-catalog.yaml
static/_nyl/catalog/application.argoproj.io/argocd/static-web.yaml
static/_nyl/catalog/appproject.argoproj.io/argocd/static-web.yaml
static/_nyl/index.json
static/web/web/deployment.apps/web/web.yaml
static/web/web/namespace/web.yaml
```

Only `fromUnit` bindings need orchestration; see
[Release inputs from other units](/nyl/orchestration/resource-guides/kubernetes-publication/#release-inputs-from-other-units).

### Apply the catalogs once

Argo CD runs what the branches contain once it manages their catalogs. Apply
each catalog Application once, by an operator, and Argo CD manages it from then
on: `dev/_nyl/catalog/application.argoproj.io/argocd/dev-catalog.yaml` from the
`deploy` branch, and `_nyl/shared/previews/catalog.yaml` from the `previews`
branch once the first preview has published it. See
[Kubernetes GitOps](/nyl/kubernetes-gitops/) for what the catalog contains.

## Run it in CI

Every job below runs the commands you ran locally from a fresh checkout. What
differs is who may push and which credentials the job holds. Nothing else
changes what Nyl renders or runs: it does not read the process environment
beyond the variables a unit admits (CI run metadata such as `GITHUB_RUN_ID` only
annotates what it records), the kube context, or files outside the repository.

### Credentials and the variables units admit

| What | Where it comes from | Which jobs need it |
| --- | --- | --- |
| Push access to the repository | `[[git.credentials]]` in `nyl.toml` naming the variable that holds a token or key, or Git's own helpers. Reaches `git` only, never a unit | Every job that writes: state, the `deploy` and `previews` branches, and keep refs when a template sets `keepSource: true` |
| `DOCKER_CONFIG` | `web-image` admits it with `env.passthrough`. Docker's login writes it | Every job that plans or runs `web-image`: reconcile, preview, close, fleet |
| `KUBECONFIG`, or `ARGOCD_AUTH_TOKEN` | `kubernetes` admits it, according to the ArgoCDInstance's `spec.access` (below). A read-only credential | Every job that plans or runs `kubernetes`, which includes the reconcile and preview jobs, and the attest and observe jobs |
| Credentials of the OpenTofu backend | `database` admits them with `env.passthrough` or `env.secrets` | Every job that plans or runs `database` |

Nyl names these variables in Git and reads their values from the job: a
variable that is not set fails the unit with `credentials: admitted variable
"…" is missing` instead of falling back to anything else. Declare
`env.secrets` instead of `passthrough` to take a value from the project's
[secrets provider](/nyl/reference/configuration/).

For pushes, a token reaches `git` through `nyl.toml`:

```toml
[[git.credentials]]
url = "https://github.com/example/"
token_env = "NYL_GIT_TOKEN"
```

A variable an entry names but the environment lacks fails the command before it
contacts the repository, so a workstation run sets it too, or relies on its
Git credential helper in a project without the entry.

The Argo CD credential depends on how Nyl reads Argo CD, which the
ArgoCDInstance declares once, in Git, for every target on it. With the default
the Kubernetes API of the instance's cluster is read through the Cluster's
`live.context`, as above; the job writes a kubeconfig with that context and
admits `KUBECONFIG`. To read the Argo CD API server instead, declare it and
admit a token:

```yaml
apiVersion: k8s.gitops.nyl/v1
kind: ArgoCDInstance
metadata: {name: dev}
spec:
  clusterRef: {name: dev}
  namespace: argocd
  access:
    method: api
    api:
      server: https://argocd.example.com
      tokenEnv: ARGOCD_AUTH_TOKEN      # the default
```

and in `units.yaml`, `env: {passthrough: [ARGOCD_AUTH_TOKEN]}` replaces
`KUBECONFIG`. Either way the credential needs only read access to Argo CD's
Applications. Both are described under
[Reading Argo CD](/nyl/kubernetes-gitops/resource-guides/argocd-instance/#reading-argo-cd).

Install Nyl and the tools in each job; the [Nyl CI image](https://github.com/NiklasRosenstein/nyl/tree/develop/docker)
has Nyl but not OpenTofu or Buildx. These steps repeat in every workflow
below, so they are shown once:

```yaml
env:
  NYL_VERSION: X.Y.Z                       # a release that includes orchestration
  DOCKER_CONFIG: ${{ runner.temp }}/docker
  NYL_GIT_TOKEN: ${{ github.token }}
  ARGOCD_AUTH_TOKEN: ${{ secrets.ARGOCD_READ_TOKEN }}     # or write KUBECONFIG from a secret
steps:
  - uses: actions/checkout@v4
    with: {fetch-depth: 0, persist-credentials: false}
  - uses: opentofu/setup-opentofu@v1
    with: {tofu_version: 1.13.2, tofu_wrapper: false}
  - uses: docker/setup-buildx-action@v3
  - uses: docker/login-action@v3
    with: {registry: registry.example.com, username: shop-ci, password: '${{ secrets.REGISTRY_TOKEN }}'}
  - run: curl --proto '=https' --tlsv1.2 -LsSf https://github.com/NiklasRosenstein/nyl/releases/download/v${NYL_VERSION}/nyl-installer.sh | sh
  - run: |
      git config user.name "shop-ci"
      git config user.email "shop-ci@users.noreply.github.com"
```

The identity is what Nyl commits state and trees as; without one a command that
writes fails with `NYL-COMMIT-IDENTITY-MISSING`. Set `NYL_VERSION` to a release
that includes orchestration.

### Reconcile on main

```yaml
# .github/workflows/reconcile.yaml
name: reconcile
on: {push: {branches: [main]}}
concurrency: {group: reconcile-dev}
permissions: {contents: write}
jobs:
  reconcile:
    runs-on: ubuntu-latest
    steps:
      # …the checkout, tools, and login steps above
      - run: nyl validate
      - run: nyl reconcile -e dev --wait-lease 10m
```

The job runs the command from your terminal. It exits 0 when every unit is
current, 2 when something waits, 3 when a unit failed, and 4 when one is
uncertain (see [`nyl reconcile`](/nyl/reference/cli/reconcile/#exit-codes)); the
pipeline fails on anything but 0. `--wait-lease` makes it queue behind a run
that holds the environment instead of exiting 2.

A workflow in CI cannot keep OpenTofu's state in a local file: a runner is
discarded after the job. Declare a remote backend on the unit and in the root
module, and admit its credentials:

```yaml
spec:
  backend: {bucket: shop-tofu-state, key: '{{ environment.name }}/database'}
  env: {passthrough: [AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY]}
```

with `terraform { backend "s3" {} }` in `infra/database/main.tf`. See
[Terraform and OpenTofu](/nyl/orchestration/resource-guides/terraform/).

### Attest and observe

Publishing is a receipt: the tree is on the branch. Whether Argo CD took it and
its Applications are healthy is a separate fact only Argo CD knows. A job that
holds the Argo CD read credential records it with `nyl attest`, and records the
live values a unit declares (a load balancer's address, say) with `nyl observe`:

```yaml
# .github/workflows/attest.yaml
name: attest
on:
  schedule: [{cron: '*/20 * * * *'}]
  workflow_run: {workflows: [reconcile], types: [completed]}
permissions: {contents: write}
jobs:
  attest:
    runs-on: ubuntu-latest
    steps:
      # …the checkout and Nyl install above, and the credential below
      - run: nyl attest -e dev --wait-lease 10m
      - run: nyl observe -e dev --wait-lease 10m
```

With the Kubernetes API access, the job writes the kubeconfig whose context is the
Cluster's `live.context` (`dev-admin`) and admits it as `KUBECONFIG`; with the API
server access, it sets `ARGOCD_AUTH_TOKEN` as above:

```yaml
      - run: |
          echo "$KUBECONFIG_DATA" > "$RUNNER_TEMP/kubeconfig"
          echo "KUBECONFIG=$RUNNER_TEMP/kubeconfig" >> "$GITHUB_ENV"
        env: {KUBECONFIG_DATA: '${{ secrets.ARGOCD_KUBECONFIG }}'}
```

Before Argo CD has the catalog Application, an attestation cannot say anything,
and it says so instead of failing:

```console
$ nyl attest -e dev
UNIT        KIND                   ATTESTATIONS  DETAIL
kubernetes  KubernetesPublication  unknown       the catalog Application argocd/dev-catalog is not in the reading, so the credential or context does not see this target's Applications and their absence would prove nothing (if the catalog was never applied, apply it first): check that the ArgoCDInstance's access reaches the Argo CD that runs this target and may read its Applications

  run:      01a1b0b7-ce2c-7f1f-a2bf-5f0a7df6f45c

warning  attestations of kubernetes in dev are unknown: the catalog Application argocd/dev-catalog is not in the reading, so the credential or context does not see this target's Applications and their absence would prove nothing (if the catalog was never applied, apply it first): check that the ArgoCDInstance's access reaches the Argo CD that runs this target and may read its Applications

  Action  fix the cause, such as a missing tool or credentials, and run `nyl attest` again

  NYL-ATTEST-UNKNOWN (kubernetes)
```

Once Argo CD runs the publication and reports it healthy, the attestations
pass. They record that each Application runs the tree the unit published, and
that it is healthy:

```console
$ nyl attest -e dev
UNIT        KIND                   ATTESTATIONS                              DETAIL
kubernetes  KubernetesPublication  attested (accepted: pass, healthy: pass)  

  run:      01a1b0b7-cf23-7306-990c-4b23287a9005
  observed: c82debd9f2a9e0e8de547b12fe5f58e8edf3bcfe
```

A degraded Application fails `healthy` (exit 3) and keeps `accepted`. An Argo CD
that cannot be read is never recorded as a failure: with the API server, a
rejected token is reported as expired or invalid, and the unit is `unknown`
(exit 2) until the credential is fixed.

```console
$ nyl attest -e dev
UNIT        KIND                   ATTESTATIONS                              DETAIL
kubernetes  KubernetesPublication  attested (accepted: pass, healthy: fail)  

  run:      01a1b0b7-d03c-7ccb-af8d-0e6b7b4b7f17
  observed: 629a967a027c4f066e74faa25a05cac340734732

warning  attestation healthy of kubernetes in dev failed

  Action  investigate the failing check; references that require the attestation stay unsatisfied until a newer one passes

  NYL-ATTESTATION-FAILED (kubernetes)
$ nyl attest -e dev
UNIT        KIND                   ATTESTATIONS  DETAIL
kubernetes  KubernetesPublication  unknown       Argo CD at http://127.0.0.1:18765/ rejected the API token (HTTP 401: invalid session: token has invalid claims: token is expired): it is invalid or has expired; issue a new one and update the secret or variable the unit admits

  run:      01a1b0b7-d636-73f9-a39e-14a5d2bf2b8d

warning  attestations of kubernetes in dev are unknown: Argo CD at http://127.0.0.1:18765/ rejected the API token (HTTP 401: invalid session: token has invalid claims: token is expired): it is invalid or has expired; issue a new one and update the secret or variable the unit admits

  Action  fix the cause, such as a missing tool or credentials, and run `nyl attest` again

  NYL-ATTEST-UNKNOWN (kubernetes)
```

The example declares no observed values, so there is nothing for `nyl observe`
to read:

```console
$ nyl observe -e dev
dev: no units
```

With them, as in [Observed values](/nyl/orchestration/runs/#observed-values),
the same job keeps them current and the next reconcile resolves their consumers
from the record. A unit that should run only against healthy workloads depends on
the attestation (`dependsOn: [{unit: kubernetes, attestations: [healthy]}]`) and
runs in the next reconcile without publishing again.

### A preview for every pull request

```yaml
# .github/workflows/preview.yaml
name: preview
on: {pull_request: {types: [opened, synchronize, reopened]}}
permissions: {contents: write}
# One job per pull request at a time, and never cancelled: a job that applies
# OpenTofu and pushes state must finish (the next one queues, and the close job
# shares the group).
concurrency: {group: 'preview-${{ github.event.number }}', cancel-in-progress: false}
jobs:
  preview:
    if: github.event.pull_request.head.repo.full_name == github.repository
    runs-on: ubuntu-latest
    steps:
      # …the checkout (of ${{ github.event.pull_request.head.sha }}), tools, and login steps above
      - run: >
          nyl reconcile -e "pr-${{ github.event.number }}" --template preview
          --param "pr=${{ github.event.number }}" --wait-lease 10m
```

The first run creates the instance, `pr-1` here, in the shared `nyl/previews`
ref: the template's name, its parameters, the pull request's commit as its
source, and an expiry of seven days from now. Every later push extends the
expiry and executes only what the push changed:

```console
$ nyl reconcile -e pr-1 --template preview --param pr=1
UNIT        KIND                   RESULT    DETAIL
database    OpenTofu               executed  reason: no receipt
web-image   OciImage               executed  reason: no receipt
kubernetes  KubernetesPublication  executed  reason: no receipt

  run:      01a1b0c0-807b-7fb2-bc85-52f63938395e
  source:   c8a542f5b63ba3671e578c7480b8cadffb8f1434
            file:///tmp/shop/shop.git
  desired:  5ee818570bed2eb4b2715f2629f624faf76644a4
  observed: 5ee818570bed2eb4b2715f2629f624faf76644a4
  instance: template preview, expires at 2026-10-01T11:00:00+00:00; expiry extended
```

A second pull request gets its own instance, database, image, and tree. Both
share one state ref and one deploy branch, and `nyl get environments` lists
them with their expiry:

```console
$ nyl reconcile -e pr-2 --template preview --param pr=2
UNIT        KIND                   RESULT    DETAIL
database    OpenTofu               executed  reason: no receipt
web-image   OciImage               executed  reason: no receipt
kubernetes  KubernetesPublication  executed  reason: no receipt

  run:      01a1b0c0-8661-77ea-bda4-1a019fba3984
  source:   5dd2d95ee5c0d43b2434263c8a67d9ae035d42e4
            file:///tmp/shop/shop.git
  desired:  459f51e197c71835fd335e865e531ada35cdb8d2
  observed: 459f51e197c71835fd335e865e531ada35cdb8d2
  instance: template preview, expires at 2026-10-01T12:00:00+00:00; expiry extended
$ nyl get environments
NAME  TEMPLATE  UNITS         STATE                                   EXPIRES               FILE
dev   -         stage=dev     shop@nyl/dev/desired                    -                     platform.yaml#document-7
pr-1  preview   preview=true  file:///tmp/shop/shop.git@nyl/previews  2026-10-01T11:00:00Z  -
pr-2  preview   preview=true  file:///tmp/shop/shop.git@nyl/previews  2026-10-01T12:00:00Z  -
```

Each instance's tree is an ordinary tree beneath its own prefix of the
`previews` branch, with the catalog directory of its Applications. Beside them,
`_nyl/shared/previews/` holds the one shared catalog Application that
manages them all, created by the first publication:

```console
$ git ls-tree -r --name-only origin/previews
_nyl/shared/previews/_nyl/index.json
_nyl/shared/previews/catalog.yaml
pr-1/_nyl/catalog/application.argoproj.io/argocd/pr-1-kubernetes-web.yaml
pr-1/_nyl/catalog/appproject.argoproj.io/argocd/pr-1-kubernetes-web.yaml
pr-1/_nyl/index.json
pr-1/web/web/deployment.apps/pr-1-web/web.yaml
pr-1/web/web/namespace/pr-1-web.yaml
pr-2/_nyl/catalog/application.argoproj.io/argocd/pr-2-kubernetes-web.yaml
pr-2/_nyl/catalog/appproject.argoproj.io/argocd/pr-2-kubernetes-web.yaml
pr-2/_nyl/index.json
pr-2/web/web/deployment.apps/pr-2-web/web.yaml
pr-2/web/web/namespace/pr-2-web.yaml
```

The shared catalog Application is a manifest like any other; an operator applies
it once:

```console
$ git show origin/previews:_nyl/shared/previews/catalog.yaml
apiVersion: argoproj.io/v1alpha1
kind: Application
metadata:
  annotations:
    argocd.argoproj.io/sync-options: Prune=confirm
  finalizers:
  - resources-finalizer.argocd.argoproj.io
  name: previews
  namespace: argocd
spec:
  destination:
    namespace: argocd
    server: https://kubernetes.default.svc
  project: default
  source:
    directory:
      include: "{_nyl/shared/previews/*.yaml,*/_nyl/catalog/*}"
      recurse: true
    path: '.'
    repoURL: >-
      file:///tmp/shop/shop.git
    targetRevision: previews
  syncPolicy:
    automated:
      prune: true
      selfHeal: false
    syncOptions:
    - ApplyOutOfSyncOnly=true
    - ServerSideApply=true
```

Close the pull request, and its job removes the preview. The job runs at the
default branch, because the pull request's own branch may be gone after the
squash-merge. It needs no keep ref and no override: Nyl renders the units from
the instance's recorded source commit, the pull request's last head, which a
checkout of the default branch does not have, so Nyl fetches it by its id from
the repository's `origin`. GitHub keeps `refs/pull/<n>/head` after the branch is
deleted, also after a squash merge, so the commit is still there (GitLab keeps
`refs/merge-requests/<n>/head`):

```yaml
# .github/workflows/preview-close.yaml
name: preview-close
on: {pull_request: {types: [closed]}}
permissions: {contents: write}
concurrency: {group: 'preview-${{ github.event.number }}', cancel-in-progress: false}
jobs:
  close:
    if: github.event.pull_request.head.repo.full_name == github.repository
    runs-on: ubuntu-latest
    steps:
      # The default branch, not the pull request's merge commit. Nyl fetches the
      # recorded commit from origin by its id, with the credentials of nyl.toml.
      - uses: actions/checkout@v4
        with: {ref: '${{ github.event.repository.default_branch }}', fetch-depth: 0, persist-credentials: false}
      # …the tools, login, and Nyl steps above
      - run: |
          pr="pr-${{ github.event.number }}"
          instances=$(nyl get environments)      # a failure fails the job
          if echo "$instances" | awk -v pr="$pr" '$1 == pr {found = 1} END {exit !found}'; then
            nyl state delete -e "$pr" --teardown --wait-lease 10m
          fi
```

The job reads the listing and removes only an instance that is in it, so a
pull request that never had a preview skips the removal, while a listing that
fails (an exit other than 0) fails the job instead of passing as "no preview". The
removal is `teardown --all` and then `state delete`: the publication removes
its Applications from the catalog first and waits two minutes (the template's
`delay`) for Argo CD, then its files. The database is destroyed, the image unit
is released (a registry image is never deleted), the instance's state is
removed, and the other preview is untouched:

```console
$ nyl state delete -e pr-1 --teardown
state-delete pr-1: done
  desired: a3c5be17fb7d7d0dbb664c0974f8f4bd95fe1c43
  observed: a3c5be17fb7d7d0dbb664c0974f8f4bd95fe1c43
  removed: ["database","kubernetes","web-image"]
  teardown: exit 0
$ git ls-tree -r --name-only origin/previews
_nyl/shared/previews/_nyl/index.json
_nyl/shared/previews/catalog.yaml
pr-2/_nyl/catalog/application.argoproj.io/argocd/pr-2-kubernetes-web.yaml
pr-2/_nyl/catalog/appproject.argoproj.io/argocd/pr-2-kubernetes-web.yaml
pr-2/_nyl/index.json
pr-2/web/web/deployment.apps/pr-2-web/web.yaml
pr-2/web/web/namespace/pr-2-web.yaml
```

### If the recorded commit cannot be fetched

The fetch needs a host that keeps the commit reachable and a job that may read
it. Without that, the removal fails before it changes anything and names the way
forward. This listing comes from a run in which nothing kept the commit after
its branch was deleted (the lines about the units that wait for `kubernetes` are
left out):

```console
$ nyl state delete -e pr-1 --teardown
state-delete pr-1: refused
  teardown: exit 1
error  cannot tear down unit kubernetes of pr-1: cannot check out commit a2d6fb749fe8601c374354698e56cb0a78908a34: commit a2d6fb749fe8601c374354698e56cb0a78908a34 does not exist in this repository

  Action  fetch the commit, or tear it down with another revision's files: `nyl teardown -e pr-1 --unit kubernetes --source <revision>`

  NYL-SOURCE-UNAVAILABLE

error  units of pr-1 still have an incarnation or a teardown in progress: database, kubernetes, web-image

  Action  tear them down first with `nyl state delete -e pr-1 --teardown` or `nyl teardown -e pr-1 --unit <u>`, or drop a retained or pending-teardown unit without touching its resources with `nyl state forget -e pr-1 --unit <u>`

  NYL-STATE-DELETE-INCARNATIONS
```

The fallback is `--source <default-branch>`, which takes the units' source files
from that branch instead of the recorded commit. Nyl warns
(`NYL-SOURCE-OVERRIDE`, once per unit) and records the substitution in the
transition commit:

```console
$ nyl state delete -e pr-1 --teardown --source main
state-delete pr-1: done
  desired: f17c524a3f898cc6be67b631da7b9c06a47a6bf7
  observed: f17c524a3f898cc6be67b631da7b9c06a47a6bf7
  removed: ["database","kubernetes","web-image"]
  teardown: exit 0
warning  unit kubernetes of pr-1 tears down with the files of 606d798d57ad621dc802aa201a7715f6fb4aaef3, not its recorded source commit a2d6fb749fe8601c374354698e56cb0a78908a34

  Action  check that the revision still holds what the unit needs to be destroyed; the substitution is recorded in the transition commit

  NYL-SOURCE-OVERRIDE

warning  unit database of pr-1 tears down with the files of 606d798d57ad621dc802aa201a7715f6fb4aaef3, not its recorded source commit a2d6fb749fe8601c374354698e56cb0a78908a34

  Action  check that the revision still holds what the unit needs to be destroyed; the substitution is recorded in the transition commit

  NYL-SOURCE-OVERRIDE
```

The override is safe when the default branch's configuration for those units can
still destroy what the instance recorded: the same providers and backend, no unit
configuration removed since, and every rename covered by a `moved` block. A squash
merge of the pull request itself is the best case, because the default branch
then holds exactly the configuration the preview ran. A pull request closed
without merging, or one that changed the units long before it closed, is the
worst: the teardown can fail at the unit or destroy something other than what
the preview created, so read the warning before relying on it.

### Scheduled maintenance

Nyl has no daemon, so expiry is work that people's commands do. Previews whose
pull requests were abandoned, or whose close job failed, are removed by a
scheduled job that runs the same fleet reconcile for every instance of the
template. It maintains the live instances at their own recorded commits without
extending any, and removes the expired ones (`allowTeardown: true` lets it
tear them down):

```yaml
# .github/workflows/previews-maintenance.yaml
name: previews-maintenance
on:
  schedule: [{cron: '17 3 * * *'}]
  workflow_dispatch: {}
permissions: {contents: write}
jobs:
  maintain:
    runs-on: ubuntu-latest
    steps:
      # …the checkout (of the default branch), tools, and login steps above
      - run: nyl reconcile --template preview
      # The one writer of the shared catalog, from the default branch: it
      # replaces the manifest after a Nyl upgrade or a change of its settings.
      - run: |
          instance=$(nyl get environments | awk '$2 == "preview" {print $1; exit}')
          if [ -n "$instance" ]; then
            nyl build kubernetes -e "$instance" -- --shared-catalog --push
          fi
```

More than seven days after `pr-2`'s last push, the fleet job removes it and exits 0. An instance
whose lease another run holds is reported as `busy`; a later run removes it:

```console
$ nyl reconcile --template preview
UNIT        KIND                   RESULT    DETAIL
kubernetes  KubernetesPublication  removed   phase 1 removed the workload Applications from the catalog of pr-2
database    OpenTofu               removed   
web-image   OciImage               released  

  run:      01a1b0c0-98ca-77c0-be64-c2afecdec1d6
  desired:  eed4fe60d448ac20daeb70be92b2fffae6a90b06
  observed: eed4fe60d448ac20daeb70be92b2fffae6a90b06
  instance: template preview, expired at 2026-10-01T12:00:00+00:00; removed

warning  unit web-image of pr-2 left its resources in place: ContainerImage image (sha256:c7103bb2278e8acc8bcc1f62964289cbff46505d430917a8281cc720fe9d6ad8)

  Action  remove the resources by hand when nothing needs them any more

  NYL-UNIT-RELEASED (web-image)
$ nyl get environments
NAME  TEMPLATE  UNITS      STATE                 EXPIRES  FILE
dev   -         stage=dev  shop@nyl/dev/desired  -        platform.yaml#document-7
```

The unit's `NYL-UNIT-RELEASED` warning is the image: Nyl cannot delete a
registry image, so it stays and its unit leaves state.

The writer renders the unit's whole target, so it needs a live instance (or a
declared environment) whose references resolve. It compares the branch's
manifest with the one the instance renders. `--diff` shows what would change and
`--push` publishes it, only from a committed source:

```console
$ nyl build kubernetes -e pr-2 -- --shared-catalog --diff
{
  "apiVersion": "artifacts.gitops.nyl/v1",
  "executionKey": "sha256:4a0c9658175ac8856cbd4af2f6b15d2ef291e2211f57f83059bef098e59ec213",
  "kind": "TreeDiff",
  "metadata": {
    "name": "diff",
    "unit": {
      "environment": "pr-2",
      "name": "kubernetes",
      "uid": "01a1b0c0-8baa-7c9f-899b-031fa0cc0993"
    }
  },
  "spec": {
    "base": "a6e00d4f772a0369e122bb5cfc0329e9fc0c0105",
    "branch": "previews",
    "changes": "none",
    "files": [],
    "owner": "catalog:previews",
    "pathPrefix": "_nyl/shared/previews",
    "repository": "file:///tmp/shop/shop.git",
    "target": "catalog:previews"
  }
}
```

Publications of the instances never overwrite the shared manifest, so pull
requests on different Nyl versions do not fight over it; one that renders
another manifest warns with `NYL-KUBERNETES-SHARED-CATALOG-DIFFERS` and reports
`differs` in the tree artifact (`nyl get artifact kubernetes/tree -e pr-2
--pointer /sharedCatalog/result`, which prints `current` here).
See [One catalog for every instance](/nyl/orchestration/resource-guides/environment-template/#one-catalog-for-every-instance).

### Keep refs, for previews that outlive their branches

The close job covers a preview that is removed when its pull request closes. A
preview can also expire, through the scheduled job below, long after its branch
is gone: an abandoned pull request, or a close job that failed. Where the host
may no longer serve the recorded commit by then, set `keepSource: true` on the
template. Every command that records an instance's source commit then first
pushes a ref of its own, `refs/nyl-keep/<keepId>/<commit>`, to the source
repository, so the commit stays fetchable until the instance's state is
deleted.

:::caution[`keepSource: true` needs a token that can push `refs/nyl-keep/*`]
`nyl reconcile --template` and `nyl state init --template` refuse
(`NYL-INSTANCE-KEEP-FAILED`, exit 1, nothing written) when the source repository
refuses the ref. GitHub's default read-only `GITHUB_TOKEN`, and the token of a
pull request from a fork, cannot push it. The workflows above ask for
`contents: write` because they push state and the deploy branch, and skip fork
pull requests for the same reason. Either give such a job a token that may push
the ref, or leave `keepSource` off and rely on the close job (or tear instances
down with `--source <revision>`). See
[Source keep refs](/nyl/orchestration/resource-guides/environment-template/#source-keep-refs).
:::

## Where each command is documented

| Step | Reference |
| --- | --- |
| `nyl validate`, `plan`, `reconcile`, `status`, `get` | [`validate`](/nyl/reference/cli/validate/), [`plan`](/nyl/reference/cli/plan/), [`reconcile`](/nyl/reference/cli/reconcile/), [`status`](/nyl/reference/cli/status/) |
| `nyl state init`, `state delete` | [`state`](/nyl/reference/cli/state/) |
| `nyl attest`, `nyl observe` | [`detect-drift` and `attest`](/nyl/reference/cli/detect-drift/), [`observe`](/nyl/reference/cli/observe/) |
| `nyl build -- --shared-catalog` | [`build`](/nyl/reference/cli/build/) |
| `nyl publish-tree` | [`publish-tree`](/nyl/reference/cli/publish-tree/) |
| Previews, expiry, and the shared catalog | [Using EnvironmentTemplate](/nyl/orchestration/resource-guides/environment-template/) |
| Publications, attestations, and Argo CD access | [Using KubernetesPublication](/nyl/orchestration/resource-guides/kubernetes-publication/) |
