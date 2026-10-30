---
title: 'Rendering Pipeline'
---

`nyl render`, `nyl diff`, `nyl apply`, and the rendered-tree commands share one
manifest generation pipeline, so a local render and a CI render of the same
declaration take [one path](/nyl/start/introduction/#one-declaration-one-path). Tree commands add target composition, policy,
layout, and whole-target caching after each Release bundle has passed through
that pipeline.

## Shared Pipeline Steps

1. Load project configuration and, when requested, resolve the target and its Cluster.
2. Build the template context. Project secrets and admitted `NYL_*` environment variables enter it only with `--allow-secret-inputs`, for direct and tree commands alike.
3. Load the input manifest file and render Jinja templates.
4. Apply `--only-source-kind` filtering on top-level input resources (before expansion).
5. Expand resources recursively (`HelmChart`, `Component`, `RemoteManifest`, aliases) with `--max-depth`.
6. Apply Kyverno policies (Global scope currently supported).
7. Deduplicate final manifests (last occurrence wins).
8. Take the Release's resources as its Argo CD Application deploys them
   ([below](#the-releases-application)).
9. Apply post-render kind filtering with `--only-kind` / `--exclude-kind`.

Bundle expansion and parsed Helm output are cached by their complete observed
inputs. `--refresh` bypasses cache reads and replaces successful entries;
`--no-cache` disables persistent reads and writes. `diff` and `apply` always
perform their live-cluster work even when desired-manifest rendering is reused.

## YAML Data Preservation

Nyl preserves manifest keys, values, and types when its YAML output is parsed
again. Comments, quoting style, and whitespace outside string values are not
preserved. Multiline manifest values use literal blocks when their contents can
be preserved safely; values requiring escaping use quoted strings. Long
single-line strings are not folded.

Quote strings that resemble YAML booleans, numbers, or null, such as `"no"`,
`"123"`, and `"null"`. Mapping keys are strings and retain their spelling.
Duplicate mapping keys are rejected. YAML aliases are expanded within parser
resource limits. Manifest streams apply complexity limits per document, so
combining many resources does not impose a fixed document-count ceiling. Empty
and null documents are omitted from manifest streams.

## The Release's Application

With `--target`, a Release that one of the target's enabled ApplicationGroups
renders is taken from the compiled target, the tree `render-tree` publishes. The
direct commands therefore work on exactly the resources its Argo CD Application
deploys:

- Resources without `metadata.namespace` keep none; the Application's
  destination namespace (the group's `destinationNamespace`, else the Release
  namespace) gives them one.
- The destination and `spec.additionalNamespaces` get managed Namespace objects
  by the group's `namespace` policy, unless `sharedNamespaces` gives a namespace
  to another owner or it is a Kubernetes bootstrap namespace such as `default`.
- Namespace scope rules and shared-namespace ownership across the target's
  Releases are checked as `render-tree` checks them.

Compiling the target renders its other Releases too; the render cache makes
repeated runs cheap. `--release-only` settles the Release alone instead: it
applies the same per-Release rules with its group's policy, but cannot see
conflicts with other Releases, such as a namespace two groups use without a
`sharedNamespaces` declaration. A Release with `--input`, `--inputs`, or
`--defaults-only`, outside every group of the target, or rendered without a
target is settled alone the same way, without a group's policy where it has
none.

## Namespaces in diff and apply

`render` never connects to Kubernetes and leaves missing namespaces unset, as
the published tree does. `diff` and `apply` fill them in for namespaced kinds,
as Argo CD does when it syncs:

1. the resource's own `metadata.namespace`;
2. the destination namespace of the Release's Application;
3. `--namespace`, for a file without a Release;
4. otherwise an error naming the resource.

The kube context's default namespace is never used, so the result depends only
on the declaration.

## Pruning and deletion

`apply` prunes resources the new revision no longer renders, and
`release delete` purges a release's resources, by the Argo CD sync options each
live resource carries:

| `argocd.argoproj.io/sync-options` | `apply` pruning | `release delete` |
| --- | --- | --- |
| `Prune=false` / `Delete=false` | kept and no longer tracked | kept |
| `Prune=confirm` / `Delete=confirm` | kept and still tracked, unless `--confirm-prune` | kept, unless `--confirm-delete` |
| neither | deleted | deleted |

Managed Namespaces carry `Prune=confirm` and `Delete=confirm` by default (the
group's `namespace.prunePolicy` and `deletePolicy`), because deleting a
Namespace deletes everything in it. `diff` lists kept resources separately and
accepts `--confirm-prune` to preview a confirmed apply.

## Command-Specific Behavior After Rendering

- `render`: outputs rendered YAML to stdout.
- `diff`: compares desired state with live cluster state and prints differences.
- `apply`: applies resources and manages release tracking/pruning.

## Source Filter vs Post-Render Filters

- `--only-source-kind` filters only top-level resources in the input file, before expansion.
- `--only-kind` / `--exclude-kind` filter the final rendered manifests, after expansion.
