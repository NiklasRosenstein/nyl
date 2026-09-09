---
title: Manifest validation
---

Nyl runs configured validators against final Kubernetes manifests before writing
render output, calculating diffs, applying resources, or publishing a target.
The CI image includes the pinned kubeconform executable. For local development,
install the tools in the project mise configuration.

## Configure validation

Add a validator to `nyl.toml`:

```toml
[validation]
enabled = false

[validation.kubeconform]
strict = true
vendor_builtin_schemas = false
schema_locations = []
skip = []
timeout_seconds = 60
```

The kubeconform table selects that validator. `enabled = true` runs configured
validators automatically for `render`, `diff`, `apply`, `render-tree`,
`diff-tree`, and `publish-tree`. `--validate` enables them for one invocation;
`--no-validate` suppresses automatic validation. Requesting validation without a
configured validator is an error.

`strict = true` rejects unknown fields as well as duplicate YAML keys. Captured
CRD schemas retain both strict and permissive variants so this setting can change
without recapturing. Both variants validate declared constraints; permissive mode
allows extra fields where the schema does not explicitly constrain them. Strict
mode respects fields that explicitly preserve unknown properties.

```bash
nyl render-tree --target production --output-dir rendered --validate
nyl diff-tree --target production --validate
nyl publish-tree --target production --validate
nyl render app.yaml --offline --kube-version 1.31.4 --validate
```

Target-aware validation uses the Cluster's effective Kubernetes version.
Targetless validation requires `--kube-version`; Helm rendering also requires
`--kube-api-versions`. The schema lookup normalizes a leading `v`, distribution
suffixes, and a missing patch version, without falling back to another version.

Validation runs on cache hits, `render-tree --check`, and
`publish-tree --dry-run`. A filtered `diff-tree` validates the entire desired
tree, then selects the differences to display. Its baseline is allowed to
contain invalid resources so that corrections remain diffable.

`apply` validates the submitted set before writing resources or release state.
In append mode, `diff` validates its composed desired set, while `apply`
validates the resources submitted in that invocation.

Diagnostics and summaries go to stderr, preserving manifest and diff stdout.
Invalid resources, missing schemas, tool failures, and timeouts fail the command.
Explicit `skip` entries use `apiVersion/kind`, such as `example.com/v1/Widget`;
the summary reports skipped resources. Lists are expanded for schema discovery
and validation, with item paths retained in diagnostics. A skipped List excludes
its entire contents. Schema validation does not execute CEL,
admission webhooks, or conversion webhooks, and does not establish deployment
ordering or compatibility with resources outside the validated input.

## Capture cluster CRD schemas

CRD capture is an explicit live read. Pass `--crds` on each capture, or enable it
by default in `nyl.toml`:

```toml
[capture.cluster]
crds = true
```

```bash
nyl capture cluster staging
nyl capture cluster staging --crds --context staging-admin
nyl capture cluster staging --check
nyl capture cluster staging --no-crds
```

The command refreshes committed Kubernetes capabilities and, when enabled,
schemas for every served CRD version. `--check` compares the enabled capture
outputs with the live cluster without writing. `--no-crds` refreshes capabilities
while preserving schema files; it does not certify those schemas as current.
Capturing schemas requires permission to list CRDs.

A capture with CRDs replaces that Cluster's schema inventory with the served
CRD versions currently in the cluster, removing entries for deleted CRDs and
versions that are no longer served. Unreferenced schema blobs remain on disk
until `nyl vendor --prune`; blobs referenced by another Cluster are retained.

Snapshots use `vendor.path`, or `vendor/` when that setting is absent:

```text
vendor/
  clusters/staging/schemas.json
  schemas/blobs/<sha256>.json
```

The cluster inventory identifies served versions and references shared schema
blobs. Equal schema content is stored once across clusters. Volatile resource
metadata and status are excluded. Commit the inventories and blobs with the
Cluster configuration. Capture and vendoring generate `schemas/.gitattributes`
to treat blobs as binary in Git, suppressing text diffs and merges and preserving
the exact bytes required by their hashes. Blobs remain JSON in ordinary Git;
inventories remain diffable.

A snapshot includes its source capabilities fingerprint. Validation rejects a
mismatch and instructs recapture, including when a capture was interrupted
between updating the schema inventory and Cluster source.

## Borrow another Cluster's API contract

A production Cluster can use a committed staging contract when production is
inaccessible to the rendering pipeline:

```yaml
apiVersion: k8s.gitops.nyl/v1
kind: Cluster
metadata:
  name: production
spec:
  destination:
    server: https://production.example.com
  apiContractFrom:
    clusterRef:
      name: staging
    mode: all
```

`all` inherits Kubernetes version, API capabilities, and the CRD schema source.
Omit `spec.kubernetes` in this mode. `schemas` borrows only the CRD schema source
and requires production's own `spec.kubernetes` block. Rendering and validation
use the same effective capabilities.

Destinations, values, labels, and live connection settings remain local.
References may form acyclic chains. `nyl get clusters` reports effective versions
and borrowed provenance. Capture the source Cluster; capture of an inheriting
Cluster is rejected with the source name.

Borrowing asserts API compatibility; it is not an observation of production.
Validation reports the destination and the source of its captured schemas.
Generated Argo CD Applications and AppProjects use their ArgoCDInstance Cluster's
contract, independently of the workload destination.

## Validate an operator installation or upgrade

`--use-desired-crds` enables validation and asserts that the input includes each
desired CRD and **all custom resources affected by it**:

```bash
nyl publish-tree --target production --use-desired-crds
```

This assertion is per invocation. It cannot be combined with source/output kind
filters or `--append-release`. A complete file-based render can also make the
assertion. Nyl does not infer completeness from the presence of a CRD.

Schemas are resolved in this order, independently for each destination:

1. Desired CRDs, when explicitly admitted with `--use-desired-crds`.
2. Configured local schema locations.
3. The effective Cluster's captured CRD schemas.
4. The pinned registry for built-in Kubernetes APIs.

A desired CRD supersedes its entire group/kind definition. Removed and unserved
versions cannot fall back to captured schemas. Conflicting desired definitions
fail. Without the assertion, CRD objects themselves are validated but their
schemas are not used to validate custom resources.

For APIs managed elsewhere, commit schemas locally and configure a lookup:

```toml
[validation.kubeconform]
schema_locations = [
  'schemas/{{.Group}}/{{.ResourceKind}}_{{.ResourceAPIVersion}}.json',
]
```

Lookup templates support kubeconform's `Group`, `ResourceKind` (lowercase),
`ResourceAPIVersion`, `KindSuffix`, `NormalizedKubernetesVersion`, and
`StrictSuffix` fields. Paths and relative schema references must stay inside the
project and must not traverse symlinks. A location without a JSON extension uses
the standard Kubernetes schema directory layout. External HTTP references in
local schemas are not admitted; commit their dependencies locally.

## Reproducible and offline CI

Built-in schemas come from an immutable commit of
[yannh/kubernetes-json-schema](https://github.com/yannh/kubernetes-json-schema).
The effective Kubernetes version selects the version directory within that
registry snapshot. The kubeconform executable version and registry revision are
independent pins. A project can set `builtin_schema_revision` to a full commit
SHA when it needs a different registry snapshot.

CustomResourceDefinition schemas use the registry's shared definitions because
their recursive schemas are not available as standalone files. Their complete
reference graph uses the selected Kubernetes version and strictness policy.

By default, built-ins are downloaded into a disposable cache. To require
committed schemas during validation:

```toml
[vendor]
mode = "required"

[validation.kubeconform]
vendor_builtin_schemas = true
```

```bash
nyl vendor
nyl vendor --check
nyl diff-tree --target production --validate
nyl publish-tree --target production --validate
```

Vendoring materializes built-ins required by the selected target renders,
including catalog destinations and schema dependencies. In this mode validation
fails on missing vendored schemas and never downloads a fallback. A targetless
render must also have all of its required built-ins available locally.

`nyl vendor` repairs missing or corrupt builtin schema blobs from the cache or
pinned registry. `nyl vendor --refresh` fetches required builtins and their
dependencies again; validation and `--check` reject damaged blobs without repair.

`nyl vendor` does not access clusters or refresh captures. `--prune` preserves
blobs referenced by every captured Cluster, including inherited sources, and
aborts schema pruning if an inventory is malformed.

Publication loads validation policy, inheritance references, and schemas from
the source snapshot selected for the published artifact. Commit these inputs
before publishing; invocation flags apply to that selected snapshot.
