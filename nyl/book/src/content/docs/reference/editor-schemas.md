---
title: 'Nyl Resource Schemas'
sidebar:
  label: 'Editor schemas'
---

The [resource catalog](/nyl/reference/resources/) provides version-qualified
references and schema downloads for every built-in Nyl resource.

Schemas are generated from Rust types, documentation comments, and example
annotations. The same schemas supply editor validation and the documentation's
purpose descriptions, examples, and field references.

```bash
nyl schema config
nyl schema resource HelmChart --api-version k8s.nyl/v1
nyl schema resource DeploymentTarget
nyl schema gitops
nyl schema all --output-dir book/public/reference/schemas
```

The [project schema](/nyl/reference/schemas/nyl.schema.json) describes `nyl.toml`.
The [GitOps aggregate](/nyl/reference/schemas/gitops-resource.schema.json) covers
shared GitRepository resources and Kubernetes GitOps resources. Individual
resource schemas live beneath their API group and version; flat resource schema
URLs resolve to their canonical schemas.

`all` also writes [resources.json](/nyl/reference/schemas/resources.json), the
catalog manifest used for resource pages and navigation. Rust tests verify the
committed artifacts; docs tests validate examples and schema rendering. Regenerate
these artifacts whenever the Rust resource contracts or documentation change.

## Value sources

Where a resource's fields accept [value sources](/nyl/reference/value-sources/),
the schema offers them next to each top-level `spec` field, from the context
table: a unit kind's fields accept `$environment`, `$unit`, `$promotion`, and
`$literal`, a HelmChart's and a RemoteManifest's accept `$input` and
`$literal`, and a DeploymentTarget's Release-input bindings accept the binding
sources. Sources are also valid deeper in those fields and in free-form data,
where the schemas do not offer them; an editor may flag such a source that Nyl
accepts.
