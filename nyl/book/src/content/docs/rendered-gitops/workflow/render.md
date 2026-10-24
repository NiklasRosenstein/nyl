---
title: 'Render and Validate'
---

Render a [`DeploymentTarget`](/nyl/reference/resources/k8s.gitops.nyl/v1/deployment-target/)
into a checkout of its publication repository to inspect the
[rendered layout](/nyl/rendered-gitops/rendered-layout/) before publishing it.

## Validate and render

List the targets and where each one publishes:

```console
$ nyl get targets
NAME        CLUSTER  PUBLICATION      PATH-PREFIX  FILE
production  primary  platform@deploy  production   config/targets/production.yaml#document-1
staging     staging  platform@deploy  staging      config/targets/staging.yaml#document-1
```

Then validate the project and render one target into a checkout of its
publication repository:

```bash
nyl validate
nyl render-tree --target production --output-dir deploy-worktree
```

`--output-dir` names the checked-out destination repository root; Nyl appends
the target's `pathPrefix`. Use `nyl render-tree --check` to render and validate
without writing.

Validation failures return a nonzero exit status and leave the current rendered
files at the output path for inspection. Normal rendering reconciles these files
even when validation fails; ownership and local-edit protections still apply.
`--check` leaves existing output untouched.

Tree commands display an updating Release progress bar on an attended stderr.
In CI they instead print one line before processing each Release, keeping stdout
available for rendered diffs. Select the behavior explicitly with
`--progress bar`, `--progress plain`, or `--progress off`.

If an indexed rendered file was deleted or edited locally, repair the worktree
from the current render explicitly:

```bash
nyl render-tree --target production --output-dir deploy-worktree --force
```

Nyl warns for every repaired owned file. Unowned files, ownership boundaries,
and symbolic-link protections remain enforced.

## Render cache and source freshness

Nyl caches three expensive results: complete target trees, individual rendered
Release bundles, and parsed local Helm output. Complete target trees apply only
to `render-tree`, `diff-tree`, and `publish-tree`; bundle and Helm artifacts are
also shared by `render`, `diff`, and `apply`. Cache entries are
content-addressed and accompanied by dependency records. Nyl verifies effective
selected control resources by identity, exact ApplicationGroup candidate
membership, Release entrypoints and includes, resolved local chart directories,
target context, Kubernetes capabilities, admitted `NYL_*` environment, secrets
through a cache-local keyed fingerprint, and renderer tool versions before
reuse. Formatting, comments, and unrelated resources colocated in the same YAML
file do not invalidate a target artifact. Selected control-resource locations
remain dependencies because the published ownership index records physical
source provenance. Nyl does not fingerprint the whole project checkout. A
target miss can still reuse unchanged Releases, while target-wide ownership,
namespace, project, catalog, and layout validation runs again.

The cache lives under `$NYL_CACHE_DIR/gitops/v1`. Without `NYL_CACHE_DIR`, the
project-local `.nyl/cache/gitops/v1` directory is used. Artifacts can contain
the same rendered secret data as the publication tree and use private file
permissions. CI caches should therefore have the same access restrictions as
deployment artifacts.

Normal commands reuse rendered Releases addressed by their declared inputs.
This includes RemoteManifest URLs and remote Helm charts with an explicit
version. An unversioned remote chart remains uncacheable because its coordinate
does not identify stable content.

Remote downloads have a separate exact source cache under
`$NYL_CACHE_DIR/sources/v1`, or `.nyl/cache/sources/v1` by default. It stores
original RemoteManifest bytes, Helm chart archives, and deterministic Git
source archives. A render-cache miss can therefore rerun Helm or Nyl rendering
without pulling the same exact source again.

Use `--refresh` on a rendering command to bypass both render and exact source
cache reads, retrieve the inputs again, rerender, and replace successful cache
records. Use `--no-cache` to perform no disposable cache reads or writes;
temporary storage is deleted with the command. The two options are mutually
exclusive. An enabled vendor snapshot remains authoritative for both flags;
refresh it explicitly with `nyl vendor --refresh`.

The `_nyl/index.json` ownership index is always verified when reading a
published tree. Its hashes are publication integrity and provenance data, not
a complete render cache key.

## Render statistics

Rendering commands print rendering statistics on standard error. The Cache
section reports work as reused, rebuilt, rendered, or avoided. Avoided work was
not consulted because a higher-level artifact supplied the result. The Sources
section reports external operations that the invocation actually reached,
including manifest downloads, chart pulls, and Git repository, ref, and
worktree operations. Inputs that cannot be cached retain their reason next to
the affected layer. Debug logging records the underlying miss, invalidation,
refresh and store events and names changed dependencies; secret and environment
values remain represented only by keyed fingerprints.

A complete target reuse reports the work that it short-circuits instead of
claiming reuse at lower-level caches that were never consulted:

```text
Render statistics
  Cache
    Target tree         reused
    Release renders     38 avoided
    Helm renders        65 avoided
```

A partial rebuild attributes lower-level work to the Release artifacts that
short-circuited it:

```text
Render statistics
  Cache
    Target tree         rebuilt
    Release renders     37 reused · 1 rendered
    Helm renders        65 avoided
  Sources
    Remote manifests    2 downloaded
    Helm charts         3 pulled
    Git repositories    2 reused · 2 refs refreshed
    Git worktrees       2 reused
```

Zero-valued rows and the empty Sources section are omitted. Automatic color is
enabled for terminals and conventional CI environments. `NO_COLOR`,
`CLICOLOR=0`, `TERM=dumb`, and `--color never` disable styling; `--color always`
and a non-zero `CLICOLOR_FORCE` enable it explicitly.

## Vendored remote inputs

Projects can commit every remote renderer input needed by one or more targets.
This includes RemoteManifest response bytes, packaged Helm repository and OCI
charts, deterministic Git snapshots used by Git charts and remote
ApplicationGroups, and the files `fromGit` Release input bindings lock.
Configure the project-wide
policy in [`nyl.toml`](/nyl/reference/configuration/#remote-artifact-vendoring), then run:

```bash
nyl vendor
nyl vendor --check
```

`nyl vendor` scans all DeploymentTargets by default. Repeat `--target` to
refresh a subset while retaining entries owned by other targets. It resolves
existing vendor entries first, then the exact source cache, then the remote
origin. `--refresh` forces upstream retrieval; when an old extracted Helm cache
contains the exact chart, Nyl can package and import it before contacting the
origin.

`nyl vendor --check` performs no network access except refreshing the
publication branch of targets with `fromPublication` inputs, which falls back
to the cached head offline. It compiles the selected targets
through the normal rendering pipeline, verifies every required coordinate and
digest, reports unreferenced lock entries, and verifies the managed
`.gitattributes`. A matching but missing, corrupt, or unmaterialized Git LFS
artifact is an error rather than permission to fall back to a remote source.

The directory backend is the current storage implementation. Its
`lock.yaml`, artifacts, and generated `.gitattributes` are committed together.
Helm and Git archives are assigned to Git LFS; large RemoteManifest files are
assigned according to `vendor.lfs_threshold_bytes`. Run `nyl vendor --prune` to
remove artifact files not referenced by the lock.

See [`render-tree`](/nyl/reference/cli/render-tree/) and
[`vendor`](/nyl/reference/cli/vendor/) for all flags.
