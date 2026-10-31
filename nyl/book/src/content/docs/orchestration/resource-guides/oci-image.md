---
title: 'OciImage'
---

An `OciImage` builds with Docker Buildx and records an immutable
`ContainerImage` artifact named `image`. Consumers reference its digest instead
of a moving tag:

```yaml
apiVersion: units.gitops.nyl/v1
kind: OciImage
metadata:
  name: web-image
  labels: {stage: dev}
spec:
  context: {path: ../services/web}
  repository: registry.example.com/team/web
  platforms: [linux/amd64]
  tags: [dev]
```

The runner needs Docker with Buildx and access to its builder and registry.
`reconcile` always pushes. In addition to `tags`, it adds
`nyl-<environment>-<first 16 execution-key hex digits>` so another environment
never moves its execution tag. The artifact contains `repository`, `digest`,
`reference`, `platforms`, and `tags`; its digest comes from Buildx's metadata
file, never from build logs.

```yaml
values:
  image:
    fromUnit: {unit: web-image, artifact: image, pointer: /reference}
```

## Contexts and execution keys

The main `context` and named directory contexts use the common Git source
shape, with optional repository, revision, and commit lock. Each source field
gets its own checkout, so named contexts can use different commits or repositories.
An image context can consume another unit's artifact:

```yaml
spec:
  context: {path: ../services/web}
  contexts:
    shared: {path: ../libs/shared}
    base:
      image: {fromUnit: {unit: base-image, artifact: image, pointer: /reference}}
  dockerfile: Dockerfile
  target: runtime
  repository: registry.example.com/team/web
  buildArgs:
    VERSION: '1.2.3'
```

The Dockerfile consumes named contexts with `FROM base` or `COPY --from=shared`.
Pin externally supplied image references by digest. A plan warns about named
image contexts supplied by mutable tags; changing the registry's tag alone
cannot change the execution key. The same limitation applies to tagged base
images in Dockerfiles.

Keys cover every admitted context file, empty directory, file mode, Dockerfile,
and image reference. `.dockerignore` rules, including exceptions and `**`,
filter directory inputs. A Dockerfile-specific ignore file takes precedence
over the context's root `.dockerignore`. Builds stage exactly those inputs;
Git's disposable `.git` administration is excluded. `builder` and `cache`
change how the build runs and are excluded from the key.

## Registry credentials

Tools start from an empty environment, with `PATH`, a temporary `HOME`, and the
unit's explicitly admitted environment. To use the runner's Docker credentials
and credential helpers directly, declare `env.passthrough: [DOCKER_CONFIG]`.
Alternatively, use project secrets:

```yaml
spec:
  registryAuth:
    registry.example.com:
      usernameSecret: registry-user
      passwordSecret: registry-token
```

The helper writes a private temporary Docker configuration containing only the
listed registry credentials. It preserves the runner's `buildx/`, `contexts/`,
and current context, so a named `builder` still works. Source credential values
and derived authentication strings are masked. The temporary credentials are
removed after the driver finishes; secret values never enter execution keys.

## Verification and deletion

`nyl verify` checks that the recorded digest reference still exists. A missing
manifest is drift; authentication and transport failures are errors. Reconciliation
can rebuild after drift or an interrupted execution because image builds declare
`converge` recovery.

Registry deletion is not supported. `deletionPolicy: Teardown` is rejected;
removing a unit leaves its image in the registry and releases its state after
its dependents leave.

For a local experiment without state writes, use [`nyl build`](/nyl/reference/cli/build/).
The image driver accepts its build arguments after `--`:

```bash
nyl build web-image -- --load --tag experiment
nyl build web-image -- --push --tag experiment
```

The driver validates these image options; they are not generic Nyl flags.
The full field reference is generated from the
[OciImage schema](/nyl/reference/resources/units.gitops.nyl/v1/oci-image/).
