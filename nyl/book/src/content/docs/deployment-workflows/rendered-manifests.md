---
title: 'Rendered Manifest GitOps'
---

Rendered manifest GitOps is the recommended Nyl deployment model. Nyl compiles
trusted source configuration into ordinary Kubernetes YAML in a deployment Git
revision. Argo CD reads plain recursive directories and does not need the Nyl CMP.

## Responsibility and security boundaries

Nyl is a compiler and publisher. It validates the authoring contract, renders a
deterministic tree, and records file ownership and provenance. It is not the
ultimate admission boundary.

- The Git forge controls who may change source resources, target policy, and
  protected deployment revisions.
- Argo CD controls which repositories, projects, destinations, and sync options
  are accepted at reconciliation time.
- Kubernetes admission controls the resources that may enter a cluster.

Keep `GitOpsTarget`, `AppProjectDefinition`, `ApplicationGroup`, CI definitions,
and protected deployment revisions under platform-owner review. Use
`releaseCustomization.allowedPaths` and `deniedPaths` to constrain per-release
Argo CD Application overrides. Argo CD AppProject policy remains effective even
when a source repository is compromised.

## Project conventions

Discovery is project-wide and follows Git visibility. Tracked files and
non-ignored untracked YAML files are eligible, so the directories below are
conventions rather than mandatory lookup paths.

```text
nyl.toml
config/
  repositories/
    deploy.yaml
    workloads.yaml
  targets/
    production.yaml
  projects/
    workloads.yaml
  application-groups/
    workloads.yaml
applications/
  app1.yaml
  workloads/
    app2.yaml
components/
```

Set `project.gitops_scaffold_path` in `nyl.toml` when generated control resources
should live somewhere other than `config/`. This setting only changes scaffold
destinations; it does not restrict discovery.

Create the layout or individual resources with:

```bash
nyl new project platform
nyl new gitops repository deploy
nyl new gitops target production
nyl new gitops project workloads
nyl new gitops application-group workloads
```

An ApplicationGroup can also be colocated with its source:

```bash
nyl new gitops application-group workloads \
  --source applications/workloads \
  --colocate
```

The resulting `applications/workloads/_application-group.yaml` derives its
source directory from its location. A centrally stored group with no explicit
source derives `applications/<group-name>`. `spec.source.path` selects any other
project-relative location.

## Control resources

`GitRepository` gives a stable local name to credential-free Git coordinates.
`publishURL` can select a distinct write URL while Argo CD continues to use
`repoURL`.

```yaml
apiVersion: gitops.nyl.niklasrosenstein.github.com/v1
kind: GitRepository
metadata:
  name: deploy
spec:
  repoURL: https://git.example.com/platform/deploy.git
  publishURL: git@git.example.com:platform/deploy.git
```

`GitOpsTarget` is one independently rendered and published deployment slice. A
target selects a Nyl profile, adds target values and labels, and owns one
repository, revision, and path prefix.

```yaml
apiVersion: gitops.nyl.niklasrosenstein.github.com/v1
kind: GitOpsTarget
metadata:
  name: production
  labels:
    environment: production
spec:
  profile: production
  values:
    region: eu-central-1
  destination:
    repositoryRef:
      name: deploy
    revision: deploy/production
    pathPrefix: production
  projects: [workloads]
```

The model supports one target, several targets with disjoint prefixes on one
revision, a revision per target, and repositories that differ per target.
Prefixes may overlap only when repository or revision differs.

`AppProjectDefinition` assigns a stable local project identity. A `Rendered`
project is written into the generated catalog; an `External` project is
referenced by generated Applications but is not published by Nyl.

```yaml
apiVersion: gitops.nyl.niklasrosenstein.github.com/v1
kind: AppProjectDefinition
metadata:
  name: workloads
spec:
  management: Rendered
  manifest:
    apiVersion: argoproj.io/v1alpha1
    kind: AppProject
    metadata:
      name: workloads
      namespace: argocd
    spec:
      sourceRepos:
        - https://git.example.com/platform/deploy.git
      destinations:
        - server: https://kubernetes.default.svc
          namespace: '*'
```

`ApplicationGroup` selects targets and source releases, assigns an Argo CD
project, and defines the platform-owned Application and Namespace policy.

```yaml
apiVersion: gitops.nyl.niklasrosenstein.github.com/v1
kind: ApplicationGroup
metadata:
  name: workloads
spec:
  targetSelector:
    matchLabels:
      environment: production
  projectRef: workloads
  applicationNamespace: argocd
  source:
    path: applications/workloads
  destination:
    server: https://kubernetes.default.svc
  namespace:
    create: true
    prunePolicy: Confirm
    deletePolicy: Confirm
  applicationDeletionPolicy: Foreground
  releaseCustomization:
    allowedPaths:
      - metadata.annotations.**
    deniedPaths:
      - spec.project
      - spec.source.**
      - spec.destination.**
```

Application deletion uses Argo CD's foreground resources finalizer by default,
so deleting a generated Application cascades to its resources. `Background`
uses the background finalizer and `Orphan` omits the resources finalizer.

Missing destination Namespaces are created by default. The generated Namespace
requires explicit Argo CD confirmation for prune and delete. `Automatic` omits
the corresponding sync option; `Retain` writes `Prune=false` or `Delete=false`.
Each Namespace is owned by one dedicated generated Application, even when many
workload Applications share it. Conflicting project, destination, lifecycle, or
metadata policy for the same Namespace is rejected.
Workload releases may declare only their destination Namespace; any other
Namespace is rejected so its lifecycle cannot overlap another Application.

## Multiple clusters and conditional applications

Templates receive the selected profile values plus target overlays. Target
values win recursively, and the effective target is available as `target`:

```yaml
data:
  environment: '{{ target.labels.environment }}'
  region: '{{ values.region }}'
```

ApplicationGroup `targetSelector.matchLabels` can omit a whole group. Source
files may use Nyl templating to omit a `NylRelease` or individual resources for
a target. ApplicationGroup and AppProjectDefinition specs may also use
target-dependent structural templating; their API version, kind, and local name
remain static for discovery. Each target renders with its stored Kubernetes
version and API versions, so CI does not need cluster access.
Within one target, all ApplicationGroups identify clusters consistently with
either `destination.server` or `destination.name`. Mixing both representations
is rejected because Nyl cannot prove that two Argo CD cluster aliases are
distinct.

Remote ApplicationGroup sources have a human-readable mutable `revision` and an
authoritative full `commit` lock. Central renderer mode uses the platform
project configuration. Remote renderer mode loads the remote `nyl.toml`.
Rendering any remote source exposes neither a secrets provider nor process
environment variables. Remote checkouts and renderer paths may not contain
symbolic links or escape the checkout root.

```bash
nyl source update workloads
nyl source update --check
```

ApplicationGroups managed by `source update` keep their source coordinates and
commit lock in a complete statically parseable resource. Target-dependent
structural templating is intended for group selection and deployment policy.

## Rendered layout

For each release, Nyl writes:

```text
<target-prefix>/<group-output>/<release>/resources.yaml
<target-prefix>/<group-output>/<release>/crd/<crd-name>.yaml
<target-prefix>/_nyl/namespaces/<identity>/resources.yaml
<target-prefix>/_nyl/catalog/projects/<project-id>.yaml
<target-prefix>/_nyl/catalog/applications/<namespace>/<application>.yaml
<target-prefix>/_nyl/index.json
```

CRDs are split one per file. Other resources are ordered deterministically.
Managed Namespaces live outside workload directories and are referenced by one
dedicated Application per cluster/namespace identity. Resources may not be
owned by more than one workload Application.
Generated Argo CD Applications use `source.directory.recurse: true`, so nested
CRD directories are included. The versioned index records target identity,
source provenance, owned files, and SHA-256 hashes. Nyl preserves unowned files
and fails closed when an owned file was changed outside Nyl.

## CI commands

Validate and inspect targets:

```bash
nyl validate gitops
nyl target list
```

Render into a checked-out destination repository:

```bash
nyl render-tree --target production --output-dir deploy-worktree
```

Diff a pull request against the currently published deployment revision:

```bash
nyl diff-tree --target production --against published > rendered.diff
```

Diff against the tree produced from the source default branch. This remains
accurate when an earlier publication job has not updated the deployment branch:

```bash
nyl diff-tree \
  --target production \
  --against source \
  --source-ref main > rendered.diff
```

Forge-specific CI can post `rendered.diff` in a merge request comment and update
the same marker comment on later pipelines. Comment ownership and API calls stay
in forge-specific tooling; Nyl only produces the diff.

Publish directly with a clean, committed source worktree:

```bash
nyl publish-tree --target production
```

Publication clones the destination branch, reconciles only indexed files,
creates one commit, refreshes the remote branch, and performs a negotiated
compare-and-swap push. Interrupted local reconciliation resumes when installed
files match the intended generation, while unrelated modifications and symlink
ancestors fail closed. Protected-branch rules and Git credentials remain forge
configuration.
