# nyl-e2e: the cluster end-to-end suite

One scenario, run against a real Kubernetes cluster with each reconciler:

1. `nyl reconcile` builds an image (`OciImage`), applies an OpenTofu
   configuration (`OpenTofu`), and publishes a Release that consumes the image
   digest and the OpenTofu output (`KubernetesPublication` with
   `observe: {waitHealthy: {timeout: 5m}}` and two `observations`). The reconcile
   waits until the reconciler reports the workload healthy and records the
   `accepted` and `healthy` attestations. A direct target is applied by the
   reconcile itself; for Argo CD the workload sync is manual, so the suite runs
   the reconcile on a thread while it applies the catalog Application and syncs
   the workload Application, the way an operator would.
2. The suite waits until the workload runs the image from the local registry.
3. It reads the attestations the reconcile recorded in the observed state
   (`accepted` and `healthy`, source `reconcile`) and cross-checks them with a
   single `nyl attest`. `nyl observe` reads the live values (the Service's
   cluster IP and the ready replicas) and the suite compares them with the cluster.
4. An input changes (the page the image serves and the OpenTofu output), the
   suite reconciles again, and the new revision runs and is attested. A third
   reconcile executes nothing.
5. `nyl teardown --all` waits (`teardownWait: observe`) for the reconciler to
   remove the workload; the suite checks the Namespace is gone, the published
   tree is removed, and the state holds the publication without an incarnation.

| Variant | Reconciler | Status |
| --- | --- | --- |
| `argocd-kubernetes` | Argo CD, read through the Kubernetes API of its Cluster (`KUBECONFIG`) | runs |
| `argocd-api` | Argo CD, read through its API server with a token (`ARGOCD_AUTH_TOKEN`) | runs |
| `direct` | Nyl's direct reconciler: `DeploymentTarget.spec.reconciler: {direct: {}}`, applied through `nyl apply`'s mechanics, with a Cluster of its own that names the kind API server | runs |

The suite drives the real `nyl` binary as a subprocess, with an empty
environment of its own and only what the project's units admit, so it also
checks that nothing reaches Nyl any other way. It depends on no Nyl crate.

## Prerequisites

- Docker, with the daemon running and Buildx (the image unit builds with it).
  Linux with Docker Engine: the host must reach container addresses on the
  `kind` network, which Docker Desktop's VM does not allow.
- [kind](https://kind.sigs.k8s.io/), `kubectl`, [OpenTofu](https://opentofu.org/)
  (`tofu`), `git`, and `curl`.
- Network access to Docker Hub (the kind node image, `registry`, `busybox`),
  `quay.io` and `ghcr.io` (Argo CD), and `raw.githubusercontent.com` (Argo CD's
  install manifest).

The suite checks these up front and lists everything that is missing.

## Running it

```sh
mise run e2e                       # builds nyl and nyl-e2e, runs every variant
E2E_VARIANT=argocd-api mise run e2e

# or by hand
cargo build -p nyl -p nyl-e2e      # without test-kinds: the real drivers
target/debug/nyl-e2e run --variant argocd-kubernetes
target/debug/nyl-e2e run --variant all --nyl path/to/nyl --keep
```

`--nyl` (or `NYL_E2E_NYL`) names the binary; the default is the `nyl` beside
`nyl-e2e`. `--work-dir` (or `NYL_E2E_WORK_DIR`) is where each variant keeps its
kubeconfig, checkout, and logs; the default is `nyl-e2e` in the temporary
directory. Each variant creates and removes its own cluster and containers
(`nyl-e2e-<variant>`), one after the other. The exit code is 1 when a variant
failed, 3 when only variants that cannot run yet were requested (none today),
and 0 otherwise.

To debug, keep the infrastructure and poke at it:

```sh
nyl-e2e setup --variant argocd-kubernetes   # cluster, registry, Git server, Argo CD; prints how to reach them
nyl-e2e run --variant argocd-kubernetes --keep   # reuses it, resets the scenario's leftovers, leaves it running
nyl-e2e teardown --variant all
```

On failure the suite prints diagnostics (nodes, pods, events, the workload,
Argo CD's Applications and logs, the containers, `nyl status`) and writes
everything it ran to `<work-dir>/<variant>/logs/transcript.log`.

## What it sets up

- **kind** `v0.33.0`, node image `kindest/node:v1.36.4` pinned by digest, with
  containerd reading per-registry `hosts.toml` (the standard kind local
  registry pattern).
- **A registry** (`registry:2.8.3`) on the `kind` network, published on a free
  loopback port. The host's Docker pushes to `localhost:<port>`; each node's
  containerd maps `localhost:<port>` to the container's address.
- **A Git server**: `git http-backend` behind lighttpd (Alpine), built locally
  from a ten-line Dockerfile, on the `kind` network. The host pushes to its
  published loopback port; Argo CD's repo-server reads the same repository by
  the container's address on the `kind` network. Argo CD speaks HTTP and SSH,
  not `git://`, and plain HTTP needs no credentials, TLS, or DNS. The project's
  `GitRepository` declares both URLs (`repoURL`, `publishURL`).
- **Argo CD** `v3.5.7` from its version-pinned upstream `install.yaml`, applied
  server-side, with a 15-second reconciliation poll, the API server serving
  plain HTTP behind a NodePort that kind publishes on the loopback, and dex
  and the notifications controller scaled to zero.
- **An API token** (`argocd-api`): the suite signs in as `admin` with the
  initial password, creates a token for an `e2e` account with `apiKey`
  capability and the built-in `role:readonly`, and hands it to `nyl` only as
  the variable the unit admits.
- **A kubeconfig** written to the work directory with the context `e2e-admin`,
  which `Cluster.spec.live.context` names, passed to `nyl` only as the
  admitted `KUBECONFIG`.

## Layout

| Module | Owns |
| --- | --- |
| `cmd`, `wait`, `log` | Running programs with captured output and timeouts; polling with backoff and timeout messages that name the last status; CI-friendly output |
| `infra` | kind, the registry, the Git server, Argo CD and its token, reuse and reset |
| `fixture` + `fixture/project` | The project, rendered per variant from one template |
| `nyl`, `checks` | Running `nyl` with only the admitted environment; the pure decisions over what `nyl` and the cluster report |
| `reconciler` | What differs between variants: declaration, how the cluster runs a published tree, what to check after a teardown |
| `scenario`, `runner`, `diag`, `cli` | The steps, running variants, diagnostics, the command line |

`cargo test -p nyl-e2e` runs the unit tests (also part of
`cargo test --workspace`); they cover the logic only: the waiter, the fixture
rendering, the decisions over reports, the environment `nyl` sees. The cluster
scenarios run only when the binary is invoked.
