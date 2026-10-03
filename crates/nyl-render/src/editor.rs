//! Editor schema comments: `# yaml-language-server: $schema=…` per YAML document.
//!
//! `nyl schema annotate` points every document Nyl can describe at a JSON
//! Schema: Nyl's own resource schemas, vendored CRD schemas, and Kubernetes
//! built-in schemas (vendored copies, or kubeconform's URLs otherwise). Local
//! schemas accept `{{ … }}` template expressions wherever a scalar is expected,
//! so Helm and structurally templated Nyl files validate in editors.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::gitops::{resolve_cluster_contract, GitOpsInventory};
use crate::resources::schema::ResourceKind;
use crate::resources::GitOpsResource;
use crate::validation::{resolve, store, KubeconformSettings};
use crate::{NylError, Result};

/// The comment prefix yaml-language-server reads in each document.
pub const MODELINE_PREFIX: &str = "# yaml-language-server: $schema=";

/// Where generated editor schemas live.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum EditorSchemas {
    /// Generate them into `.nyl/schemas`, ignored by Git; each checkout runs `nyl schema annotate`.
    #[default]
    Local,
    /// Commit them under the vendor directory in `schemas/editor`.
    Vendored,
}

/// Editor integration settings.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct EditorSettings {
    /// Where `nyl schema annotate` writes the schemas its comments reference.
    pub schemas: EditorSchemas,
}

/// The outcome of annotating a project.
#[derive(Debug, Default)]
pub struct AnnotationReport {
    /// Files whose comments were, or with `check` would be, changed, relative to the project.
    pub changed_files: Vec<PathBuf>,
    /// Generated schema files that were, or would be, written or removed.
    pub changed_schemas: Vec<PathBuf>,
    /// Documents that carry a schema comment.
    pub annotated_documents: usize,
}

impl AnnotationReport {
    pub fn is_current(&self) -> bool {
        self.changed_files.is_empty() && self.changed_schemas.is_empty()
    }
}

/// Annotate every YAML document of a project, or with `check` only report what would change.
///
/// Local schemas are compared in `check` mode only when they are vendored; a
/// local `.nyl/schemas` directory is a per-checkout cache.
pub fn annotate_project(inventory: &GitOpsInventory, check: bool) -> Result<AnnotationReport> {
    let project_root = &inventory.project_root;
    let vendor = store::vendor_root(project_root, &inventory.project_config)?;
    let mode = inventory.project_config.config.editor.schemas;
    let schema_root = match mode {
        EditorSchemas::Local => store::safe_path(project_root, Path::new(".nyl/schemas"))?,
        EditorSchemas::Vendored => store::safe_path(&vendor, Path::new("schemas/editor"))?,
    };
    let catalog = SchemaCatalog::load(inventory, &vendor)?;

    let mut report = AnnotationReport::default();
    let mut generated = BTreeMap::new();
    let skip = [schema_root.clone(), vendor.clone(), project_root.join(".nyl")];
    for relative in &inventory.yaml_files {
        let path = project_root.join(relative);
        if skip.iter().any(|root| path.starts_with(root)) || in_helm_chart(project_root, &path) {
            continue;
        }
        let Ok(original) = std::fs::read_to_string(&path) else {
            continue;
        };
        let directory = path.parent().unwrap_or(project_root);
        let mut annotated_documents = 0;
        let updated = annotate_text(&original, |manifest| {
            let target = catalog.resolve(manifest)?;
            annotated_documents += 1;
            Some(match target {
                SchemaTarget::Remote(url) => url,
                SchemaTarget::Local(file, schema) => {
                    let location = schema_root.join(&file);
                    let reference = relative_reference(directory, &location);
                    generated.entry(file).or_insert(schema);
                    reference
                }
            })
        });
        report.annotated_documents += annotated_documents;
        if updated != original {
            report.changed_files.push(relative.clone());
            if !check {
                store::atomic_write(&path, updated.as_bytes())?;
            }
        }
    }

    if !(check && mode == EditorSchemas::Local) {
        report.changed_schemas = sync_schemas(&schema_root, &generated, check)?;
    }
    if !check && mode == EditorSchemas::Local {
        store::atomic_write(&schema_root.join(".gitignore"), b"*\n")?;
    }
    Ok(report)
}

/// Where a document's schema comment points.
enum SchemaTarget {
    /// A schema served elsewhere, referenced by URL.
    Remote(String),
    /// A generated schema at a path beneath the editor schema directory.
    Local(PathBuf, Value),
}

/// The schemas a project can reference, keyed by document identity.
struct SchemaCatalog {
    /// Nyl resource schemas by apiVersion and kind; Components match any kind of their apiVersion.
    nyl: BTreeMap<(String, String), (PathBuf, Value)>,
    components: Option<(String, PathBuf, Value)>,
    /// Vendored CRD strict schemas by apiVersion and kind.
    crds: BTreeMap<(String, String), Value>,
    /// The newest Kubernetes version among the project's Clusters.
    kube_version: Option<String>,
    builtins: BTreeMap<String, String>,
    settings: KubeconformSettings,
    vendor: PathBuf,
}

impl SchemaCatalog {
    fn load(inventory: &GitOpsInventory, vendor: &Path) -> Result<Self> {
        let mut nyl = BTreeMap::new();
        let mut components = None;
        for kind in ResourceKind::ALL {
            let file = PathBuf::from("nyl").join(kind.schema_path());
            let schema = allow_templates(kind.schema());
            if kind == ResourceKind::Component {
                components = Some((kind.api_version().to_owned(), file, schema));
            } else {
                nyl.insert((kind.api_version().to_owned(), kind.name().to_owned()), (file, schema));
            }
        }

        let mut crds = BTreeMap::new();
        let mut versions = Vec::new();
        for discovered in inventory.resources.values() {
            let Some(GitOpsResource::Cluster(cluster)) = &discovered.resource else {
                continue;
            };
            let name = &cluster.metadata.name;
            if let Some(version) = resolve_cluster_contract(inventory, name)
                .ok()
                .and_then(|effective| effective.cluster.spec.kubernetes)
                .and_then(|capabilities| capabilities.kube_version)
                .and_then(|version| resolve::normalize_version(&version).ok())
            {
                versions.push(version);
            }
            let Some(index) = store::read_cluster_index(vendor, name).ok().flatten() else {
                continue;
            };
            for (crd_name, crd) in &index.crds {
                let Ok(definition) = store::read_definition(vendor, crd_name, &crd.definition) else {
                    continue;
                };
                for (version, variants) in definition.versions {
                    let key = (format!("{}/{version}", crd.group), crd.kind.clone());
                    crds.entry(key).or_insert_with(|| allow_templates(variants.strict));
                }
            }
        }
        versions.sort_by_key(|version| version_key(version));
        Ok(Self {
            nyl,
            components,
            crds,
            kube_version: versions.pop(),
            builtins: store::read_builtins(vendor)
                .map(|index| index.schemas)
                .unwrap_or_default(),
            // Editors get the strict variant whatever validation enforces.
            settings: KubeconformSettings {
                strict: true,
                ..inventory
                    .project_config
                    .config
                    .validation
                    .kubeconform
                    .clone()
                    .unwrap_or_default()
            },
            vendor: vendor.to_path_buf(),
        })
    }

    fn resolve(&self, manifest: &Value) -> Option<SchemaTarget> {
        let api_version = manifest.get("apiVersion")?.as_str()?;
        let kind = manifest.get("kind")?.as_str()?;
        let key = (api_version.to_owned(), kind.to_owned());
        if let Some((file, schema)) = self.nyl.get(&key) {
            return Some(SchemaTarget::Local(file.clone(), schema.clone()));
        }
        if let Some((components, file, schema)) = &self.components {
            if components == api_version {
                return Some(SchemaTarget::Local(file.clone(), schema.clone()));
            }
        }
        if let Some(schema) = self.crds.get(&key) {
            let (group, version) = api_version.split_once('/')?;
            let file = PathBuf::from("crds").join(group).join(format!("{kind}_{version}.json"));
            return Some(SchemaTarget::Local(file, schema.clone()));
        }
        let group = api_version.split_once('/').map_or("", |(group, _)| group);
        if !resolve::is_builtin_group(group) {
            return None;
        }
        let version = self.kube_version.as_deref()?;
        let url = resolve::builtin_url(&self.settings, &format!("{api_version}/{kind}"), version).ok()?;
        // Vendored copies are local, so they can accept template expressions too.
        if let Some(schema) = self
            .builtins
            .get(&url)
            .and_then(|digest| store::read_blob(&self.vendor, digest).ok())
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        {
            let name = url.rsplit('/').next()?;
            let file = PathBuf::from("builtins").join(format!("v{version}")).join(name);
            return Some(SchemaTarget::Local(file, allow_templates(schema)));
        }
        Some(SchemaTarget::Remote(url))
    }
}

/// Write the generated schemas and remove stale ones, returning what changed.
fn sync_schemas(root: &Path, generated: &BTreeMap<PathBuf, Value>, check: bool) -> Result<Vec<PathBuf>> {
    let mut changed = Vec::new();
    let mut expected = BTreeSet::new();
    for (file, schema) in generated {
        let path = store::safe_path(root, file)?;
        expected.insert(path.clone());
        let bytes = store::json_bytes(schema)?;
        if std::fs::read(&path).ok().as_deref() != Some(bytes.as_slice()) {
            changed.push(file.clone());
            if !check {
                store::atomic_write(&path, &bytes)?;
            }
        }
    }
    if root.exists() {
        for entry in walkdir::WalkDir::new(root).follow_links(false) {
            let entry = entry.map_err(|error| NylError::config(error.to_string()))?;
            let path = entry.path();
            if entry.file_type().is_file()
                && path.extension().is_some_and(|extension| extension == "json")
                && !expected.contains(path)
            {
                changed.push(path.strip_prefix(root).unwrap_or(path).to_path_buf());
                if !check {
                    std::fs::remove_file(path)?;
                }
            }
        }
    }
    Ok(changed)
}

/// Whether a file belongs to a Helm chart, whose templates Nyl must not edit.
fn in_helm_chart(project_root: &Path, path: &Path) -> bool {
    path.ancestors()
        .skip(1)
        .take_while(|directory| directory.starts_with(project_root))
        .any(|directory| directory.join("Chart.yaml").is_file())
}

/// Set each document's schema comment to what `schema_for` returns, keeping
/// every other byte. Documents it returns `None` for, or that do not parse,
/// are left unchanged.
pub fn annotate_text(text: &str, mut schema_for: impl FnMut(&Value) -> Option<String>) -> String {
    let mut output = String::with_capacity(text.len());
    for (separator, body) in split_documents(text) {
        output.push_str(separator);
        let manifest = serde_saphyr::from_str::<Value>(body).ok().filter(Value::is_object);
        match manifest.as_ref().and_then(&mut schema_for) {
            Some(schema) => output.push_str(&with_modeline(body, &format!("{MODELINE_PREFIX}{schema}"))),
            None => output.push_str(body),
        }
    }
    output
}

/// Split YAML text into `(separator line, document body)` pairs. The first
/// document has an empty separator unless the text starts with `---`.
fn split_documents(text: &str) -> Vec<(&str, &str)> {
    let mut documents = Vec::new();
    let mut separator = (0, 0);
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let content = line.trim_end_matches(['\n', '\r']);
        let is_separator = content == "---" || content.starts_with("--- ") || content.starts_with("---\t");
        if is_separator {
            if offset > 0 || separator != (0, 0) {
                documents.push((&text[separator.0..separator.1], &text[separator.1..offset]));
            }
            separator = (offset, offset + line.len());
        }
        offset += line.len();
    }
    documents.push((&text[separator.0..separator.1], &text[separator.1..]));
    documents
}

/// Replace a document's leading schema comment, or insert one as its first line.
fn with_modeline(body: &str, modeline: &str) -> String {
    let newline = if body.contains("\r\n") { "\r\n" } else { "\n" };
    let mut lines = body.split_inclusive('\n').collect::<Vec<_>>();
    // Only the leading comment block belongs to the document's header.
    let header = lines
        .iter()
        .take_while(|line| {
            let trimmed = line.trim();
            trimmed.is_empty() || trimmed.starts_with('#')
        })
        .count();
    let replacement = format!("{modeline}{newline}");
    if let Some(index) = lines[..header]
        .iter()
        .position(|line| line.trim_start().starts_with("# yaml-language-server:") && line.contains("$schema="))
    {
        lines[index] = &replacement;
        lines.concat()
    } else {
        format!("{replacement}{body}")
    }
}

/// A `/`-separated path from `from` (a directory) to `to`, starting with `./` or `../`.
fn relative_reference(from: &Path, to: &Path) -> String {
    let from = from.components().collect::<Vec<_>>();
    let to = to.components().collect::<Vec<_>>();
    let common = from.iter().zip(&to).take_while(|(left, right)| left == right).count();
    let mut parts = vec![".".to_owned()];
    parts.extend(std::iter::repeat_n("..".to_owned(), from.len() - common));
    if parts.len() > 1 {
        parts.remove(0);
    }
    parts.extend(to[common..].iter().filter_map(|component| match component {
        Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
        _ => None,
    }));
    parts.join("/")
}

fn version_key(version: &str) -> Vec<u64> {
    version.split('.').map(|part| part.parse().unwrap_or(0)).collect()
}

/// Accept a `{{ … }}` template expression wherever the schema expects a scalar.
pub fn allow_templates(schema: Value) -> Value {
    relax(schema)
}

const SCHEMA_MAPS: [&str; 5] = [
    "properties",
    "patternProperties",
    "definitions",
    "$defs",
    "dependentSchemas",
];
const SCHEMA_LISTS: [&str; 4] = ["allOf", "anyOf", "oneOf", "prefixItems"];
const SCHEMA_VALUES: [&str; 8] = [
    "items",
    "additionalProperties",
    "additionalItems",
    "not",
    "if",
    "then",
    "else",
    "contains",
];

fn relax(schema: Value) -> Value {
    let Value::Object(mut object) = schema else {
        return schema;
    };
    for key in SCHEMA_MAPS {
        if let Some(Value::Object(members)) = object.get_mut(key) {
            for member in members.values_mut() {
                *member = relax(member.take());
            }
        }
    }
    for key in SCHEMA_LISTS {
        if let Some(Value::Array(members)) = object.get_mut(key) {
            for member in members.iter_mut() {
                *member = relax(member.take());
            }
        }
    }
    for key in SCHEMA_VALUES {
        if let Some(member) = object.get_mut(key) {
            *member = match member.take() {
                Value::Array(members) => Value::Array(members.into_iter().map(relax).collect()),
                value => relax(value),
            };
        }
    }
    let scalar_type = |value: &Value| matches!(value.as_str(), Some("boolean" | "integer" | "number"));
    let constrained = object.get("type").is_some_and(|value| match value {
        Value::Array(types) => types.iter().any(scalar_type),
        value => scalar_type(value),
    }) || ["enum", "const", "pattern", "format"]
        .iter()
        .any(|keyword| object.contains_key(*keyword));
    if !constrained {
        return Value::Object(object);
    }
    // Annotations stay on the outer schema so editors keep showing them.
    let mut outer = serde_json::Map::new();
    for keyword in ["description", "title", "default", "examples", "$schema", "$id"] {
        if let Some(value) = object.remove(keyword) {
            outer.insert(keyword.to_owned(), value);
        }
    }
    outer.insert(
        "anyOf".to_owned(),
        json!([Value::Object(object), {"type": "string", "pattern": "\\{\\{[\\s\\S]*\\}\\}"}]),
    );
    Value::Object(outer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_annotate_text_sets_one_comment_per_document_and_is_idempotent() {
        let text = "# license\napiVersion: v1\nkind: ConfigMap\n---\n# yaml-language-server: $schema=old.json\napiVersion: example.com/v1\nkind: Widget\n--- # trailing\nnot: [valid\n---\nplain: scalar-free\n";
        let schema_for = |manifest: &Value| {
            manifest
                .get("kind")
                .and_then(Value::as_str)
                .map(|kind| format!("{kind}.json"))
        };
        let annotated = annotate_text(text, schema_for);
        assert_eq!(
            annotated,
            "# yaml-language-server: $schema=ConfigMap.json\n# license\napiVersion: v1\nkind: ConfigMap\n---\n# yaml-language-server: $schema=Widget.json\napiVersion: example.com/v1\nkind: Widget\n--- # trailing\nnot: [valid\n---\nplain: scalar-free\n"
        );
        assert_eq!(annotate_text(&annotated, schema_for), annotated);
    }

    #[test]
    fn test_relative_reference_walks_up_from_the_document() {
        assert_eq!(
            relative_reference(Path::new("/p/apps/web"), Path::new("/p/.nyl/schemas/nyl/a.json")),
            "../../.nyl/schemas/nyl/a.json"
        );
        assert_eq!(
            relative_reference(Path::new("/p"), Path::new("/p/.nyl/schemas/a.json")),
            "./.nyl/schemas/a.json"
        );
    }

    #[test]
    fn test_allow_templates_accepts_expressions_for_scalars_only() {
        let schema = allow_templates(json!({
            "type": "object",
            "properties": {
                "enabled": {"type": "boolean", "description": "Toggle."},
                "replicas": {"type": ["integer", "null"]},
                "name": {"type": "string"},
                "mode": {"enum": ["a", "b"]},
                "items": {"type": "array", "items": {"type": "integer"}}
            }
        }));
        let template = json!({"type": "string", "pattern": "\\{\\{[\\s\\S]*\\}\\}"});
        assert_eq!(
            schema["properties"]["enabled"],
            json!({"description": "Toggle.", "anyOf": [{"type": "boolean"}, template]})
        );
        assert_eq!(schema["properties"]["replicas"]["anyOf"][1], template);
        assert_eq!(schema["properties"]["mode"]["anyOf"][1], template);
        assert_eq!(schema["properties"]["items"]["items"]["anyOf"][1], template);
        // Plain strings already accept expressions.
        assert_eq!(schema["properties"]["name"], json!({"type": "string"}));
    }
}
