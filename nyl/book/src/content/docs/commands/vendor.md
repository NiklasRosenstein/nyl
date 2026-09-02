---
title: 'vendor'
---

Manage the project-wide snapshot of remote inputs consumed by Nyl rendering.
The commands require a `[vendor]` section in `nyl.toml`.

## `nyl vendor sync`

Discover remote inputs through the authoritative DeploymentTarget rendering
pipeline and materialize them into the configured directory:

```bash
nyl vendor sync
nyl vendor sync --target staging --target production
nyl vendor sync --refresh
```

With no `--target`, sync replaces the lock with the artifacts required by all
configured targets. A targeted sync updates the selected targets while
preserving other lock entries. Normal sync prefers an existing valid vendor
entry, then the exact source cache, then its remote origin. `--refresh` skips
the existing vendor entry and source cache and retrieves upstream again.

Progress options match the tree commands: `--progress bar`, `plain`, or `off`.

## `nyl vendor check`

Verify that the snapshot completely covers the selected targets:

```bash
nyl vendor check
nyl vendor check --target production
```

The check is network-free. It validates artifact digests and sizes, detects Git
LFS pointer files that have not been materialized, and checks the generated
`.gitattributes` file. An untargeted check also reports lock entries that are
not referenced by any configured target; a targeted check leaves entries for
other targets alone.

## `nyl vendor prune`

Delete artifact files that are not referenced by `vendor/lock.yaml`:

```bash
nyl vendor prune
```

Prune does not change the lock. Use an untargeted `vendor sync` first when the
lock should represent only currently configured targets.

See [Remote artifact vendoring](/nyl/configuration/#remote-artifact-vendoring)
for the modes and [Rendering, Diffing, and Publishing](/nyl/deployment-workflows/rendered-manifests/rendering-and-publishing/#vendored-remote-inputs)
for resolution and Git LFS behavior.
