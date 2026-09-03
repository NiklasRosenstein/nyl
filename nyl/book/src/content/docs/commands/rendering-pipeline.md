---
title: 'Rendering Pipeline'
---

`nyl render`, `nyl diff`, `nyl apply`, and the rendered-tree commands share one
manifest generation pipeline. Tree commands add target composition, policy,
layout, and whole-target caching after each Release bundle has passed through
that pipeline.

## Shared Pipeline Steps

1. Load project configuration and, when requested, resolve the target and its Cluster.
2. Build the template context. Direct commands load project secrets and admitted `NYL_*` environment variables; tree commands require `--allow-secret-inputs` for those inputs.
3. Load the input manifest file and render Jinja templates.
4. Apply `--only-source-kind` filtering on top-level input resources (before expansion).
5. Expand resources recursively (`HelmChart`, `Component`, `RemoteManifest`, aliases) with `--max-depth`.
6. Apply Kyverno policies (Global scope currently supported).
7. Deduplicate final manifests (last occurrence wins).
8. Apply post-render kind filtering with `--only-kind` / `--exclude-kind`.

Bundle expansion and parsed Helm output are cached by their complete observed
inputs. `--refresh` bypasses cache reads and replaces successful entries;
`--no-cache` disables persistent reads and writes. `diff` and `apply` always
perform their live-cluster work even when desired-manifest rendering is reused.

## Namespace Resolution (Online Mode)

In online mode, Nyl connects to Kubernetes and resolves missing `metadata.namespace` for namespaced resources.

Fallback order:
1. Existing `metadata.namespace`
2. Release namespace hint (`Release.metadata.namespace` or `--namespace` for release commands)
3. Kube context default namespace
4. Error if no namespace can be determined

When `--offline` is used (`render` only), namespace resolution is skipped.

## Online vs Offline

- `render --offline`:
  - does not connect to the cluster for API discovery during rendering;
  - uses `--kube-version` / `--kube-api-versions` or committed `nyl.toml` Kubernetes target metadata;
  - skips namespace resolution.
- `render` (without `--offline`), `diff`, and `apply`:
  - require cluster connectivity;
  - initialize Kubernetes discovery once per command run and reuse it for scope checks.

## Command-Specific Behavior After Rendering

- `render`: outputs rendered YAML to stdout.
- `diff`: compares desired state with live cluster state and prints differences.
- `apply`: applies resources and manages release tracking/pruning.

## Source Filter vs Post-Render Filters

- `--only-source-kind` filters only top-level resources in the input file, before expansion.
- `--only-kind` / `--exclude-kind` filter the final rendered manifests, after expansion.
