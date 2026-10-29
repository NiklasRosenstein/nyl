# OciImage → OpenTofu on local Docker

Nyl builds an nginx image, pushes it to a local registry, and deploys it with
OpenTofu's Docker provider at <http://127.0.0.1:8080>. The `fromUnit` reference
in `units.yaml` passes the image digest and orders the two units.

## Prerequisites

Nyl, Git, OpenTofu, and Docker with Buildx on `PATH`; a running local daemon
with the default `docker` builder; free ports 5000 and 8080; and internet access.

`OciImage` reconciliation always pushes, so the example needs a local registry.

## Run

`environment.yaml` stores Nyl state in `/tmp/nyl-oci-image-opentofu/state.git`
and OpenTofu state in `/tmp/nyl-oci-image-opentofu/terraform.tfstate`.
`nyl state init` creates the bare Git repository automatically and publishes
state there instead of to the source checkout's `origin`. Keep both stores
until teardown; choose durable paths in `environment.yaml` to keep the demo.

From the Nyl repository root, in one shell:

```bash
cd examples/oci-image-opentofu
export DOCKER_CONFIG="${DOCKER_CONFIG:-$HOME/.docker}"
export DOCKER_HOST="$(docker context inspect --format '{{.Endpoints.docker.Host}}')"
docker run -d --name nyl-example-registry -p 127.0.0.1:5000:5000 registry:3

nyl validate
nyl state init -e local
nyl reconcile -e local --worktree
curl --fail http://127.0.0.1:8080
nyl verify -e local
```

The exports let both tools use your selected Docker daemon and give Buildx
access to your Docker configuration.

Edit `web/index.html` and rerun `nyl reconcile -e local --worktree` to rebuild
and replace the container. The flag snapshots uncommitted files; state retains
them for recovery and teardown. An unchanged run executes neither unit.

## Clean up

```bash
nyl teardown -e local --all --reason "Finished local Docker example"
docker rm -f -v nyl-example-registry
```

After teardown succeeds, delete `/tmp/nyl-oci-image-opentofu` to discard both
state stores. Buildx's cache and builder image tags remain.
