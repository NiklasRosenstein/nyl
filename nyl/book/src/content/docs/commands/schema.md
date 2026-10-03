---
title: 'schema'
---

Generate JSON schemas for Nyl configuration and resources.

```bash
nyl schema config
nyl schema resource DeploymentTarget
nyl schema resource Cluster
nyl schema resource Release
nyl schema resource HelmChart
nyl schema resource RemoteManifest
nyl schema resource Component
nyl schema gitops
nyl schema all --output-dir book/public/reference/schemas
```

`config`, `resource`, and `gitops` print one schema to stdout. `all` writes the
complete published schema set to the selected directory.

Each resource kind currently belongs to one API version, so the kind alone
selects its schema. `--api-version` is optional: it checks that the selected
resource uses the expected group and version, and reports an error on a mismatch.
For example, this produces the same schema as `nyl schema resource HelmChart`:

```bash
nyl schema resource HelmChart --api-version k8s.nyl/v1
```

`Component` selects the dynamic-kind component envelope schema; it does not require a
literal `kind: Component`.

`all` writes version-qualified resource schemas, stable flat schema aliases,
the GitOps aggregate, the project schema, and `resources.json`. The generated
manifest drives the [resource catalog](/nyl/reference/resources/) and navigation.
Resource purposes, field descriptions, and examples come from the Rust models.

## Editor schema comments

`nyl schema annotate` points each YAML document in the project at its schema
with a [yaml-language-server](https://github.com/redhat-developer/yaml-language-server)
comment, so editors validate and complete it:

```bash
nyl schema annotate
nyl schema annotate --check
```

```yaml
# yaml-language-server: $schema=../.nyl/schemas/nyl/k8s.gitops.nyl/v1/cluster.schema.json
apiVersion: k8s.gitops.nyl/v1
kind: Cluster
---
# yaml-language-server: $schema=../.nyl/schemas/builtins/v1.31.4/configmap-v1.json
apiVersion: v1
kind: ConfigMap
```

Each document after a `---` gets its own comment, so files mixing kinds validate
document by document. The command replaces an existing schema comment at the
top of a document and leaves every other byte unchanged. A document gets a
comment when Nyl knows its schema:

- **Nyl resources** use the schemas of the running Nyl version.
- **Custom resources** use the strict schema of a CRD vendored by
  [`nyl capture cluster --crds`](/nyl/commands/manifest-validation/).
- **Kubernetes built-ins** use kubeconform's schema for the newest
  `kubeVersion` among the project's Clusters. A
  [vendored copy](/nyl/commands/manifest-validation/) is used when one
  exists: the strict variant, or the non-strict one that validation vendors
  when `strict = false`. Otherwise `local` mode downloads the strict schema
  and `vendored` mode references its pinned URL.

Documents of other kinds, files that are not valid YAML, Helm chart directories,
and the vendor directory are left untouched.

The schemas Nyl generates accept a `{{ … }}` template expression wherever a
scalar is expected, so Helm values and structurally templated Nyl resources such
as `enabled: '{{ values.enabled }}'` do not raise false errors. Built-in
schemas referenced by URL are served unchanged; vendor built-in schemas to get
the same leniency in `vendored` mode.

`[editor] schemas` in `nyl.toml` chooses where the generated schemas live:

```toml
[editor]
schemas = "local"     # default: .nyl/schemas, ignored by Git
# schemas = "vendored"  # vendor/schemas/editor, committed with the project
```

With `local`, each checkout runs `nyl schema annotate` once to generate the
schemas, and `--check` verifies only the comments. Every comment then points
into `.nyl/schemas`, so comments are identical across checkouts and `--check`
never needs the network. A built-in schema that is not vendored is downloaded
through the validation schema cache in `.nyl/cache`; later runs reuse it and work
offline. When the download fails, the comment is still written and the command
warns. With `vendored`, the
schemas are committed and `--check` also reports stale or missing schema files.
`--check` writes nothing and fails when anything is out of date, so CI can
enforce current comments.
