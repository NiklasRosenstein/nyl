//! Value sources of a Release bundle: `$fromInput` in every resource of the
//! bundle, and HelmChart `valuesFrom`.
//!
//! Contract: [Declarative value sources](../../../../design/declarative-value-sources.md),
//! the Release bundle context. Each resource is resolved on its [`Value`]
//! right after its file is parsed, before anything deserializes or inspects
//! it, so every reader (expansion, `lookup` detection, cache bypass
//! decisions, namespace defaults) sees the resolved document:
//!
//! - The literal-only fields (`apiVersion`, `kind`, `metadata.name`,
//!   `metadata.labels`) hold no source ([`literal_field_issues`]);
//!   `metadata.namespace` may.
//! - `$fromInput` and `$literal` resolve at any depth ([`resolve_document`]):
//!   in the typed fields of a HelmChart, a RemoteManifest, a Release, and a
//!   Component's envelope ([`Context::ReleaseBundle`], every `$` key
//!   reserved), and in free-form data: a HelmChart's `spec.values`, a
//!   Component's `spec`, and plain Kubernetes manifests
//!   ([`Context::ReleaseData`], foreign `$` keys such as `$ref` are data). A
//!   Release's input declarations are literal. The resolved document is then
//!   deserialized, which checks each value's type as a literal's.
//! - `valuesFrom` entries merge in order after the inline `values` resolved,
//!   then the inline `values` ([`merge_chart_values`]); the result replaces
//!   `spec.values` and the entries are removed. The merged values are data
//!   and are never walked; a values file that holds a source is an error
//!   with the fix.
//! - What rendering generates (a chart's output, a Component's, a remote
//!   manifest) holds no source ([`reject_sources_in_output`]).
//! - An input only orchestration fills (`$fromUnit`, `$fromPromotion`) holds a
//!   placeholder of its declared type in `nyl validate`. What a `pointer`
//!   selects in it, when its type is `object` or `array`, is unknown, like a
//!   blocked value: the pointer is not checked, and the selected value is a
//!   stand-in of the type its position takes ([`settle_unknown`]): in a typed
//!   field the type Nyl's schema gives it, in chart values the type the
//!   chart's `values.schema.json` gives it ([`settle_chart_values`]), else the
//!   placeholder string. A `valuesFrom` entry with a `pointer` into it adds
//!   nothing. The input's own type and every other check still apply.
//!
//! Every value read enters the dependency recorder at the layer that consumes
//! it: a values file is returned to the caller as a bundle input (the Release
//! and target cache keys, and the ownership index); Release inputs are part
//! of the template context the Release cache records and of the target cache
//! key; and the resolved documents are what the Helm cache key (chart values)
//! and the remote manifest cache (URLs) read.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use nyl_core::bindings::{
    literal_field_issues, resolve_document, sources_in, BindingIssue, Clause, Context, InputSource, Origin, Provided,
    Providers, Provision, Resolved, Scope, VALUE_SOURCES_DOCS,
};
use nyl_core::constants::API_VERSION;
use serde_json::Value;

use super::bundle::{shown_path, ProvenanceRoots};
use crate::gitops::values::{load_chart_values_file, ValuesFileError, ValuesFileRoot};
use crate::resources::{merge_chart_values, HelmValuesOrigin, HelmValuesSource, Release, RemoteManifest};
use crate::{NylError, Result};

/// Where the files of a Release bundle come from, for `valuesFrom` files.
#[derive(Debug, Clone, Copy)]
pub(crate) struct BundleFiles<'a> {
    /// The Nyl project root, whose `.nyl/cache` directory is never read.
    pub project_root: &'a Path,
    /// The root of a pinned checkout the bundle comes from, such as a remote
    /// ApplicationGroup source. Without it, files belong to the Git worktree
    /// that holds them.
    pub checkout: Option<&'a Path>,
}

impl BundleFiles<'_> {
    fn root(&self) -> ValuesFileRoot<'_> {
        match self.checkout {
            Some(checkout) => ValuesFileRoot::Checkout(checkout),
            None => ValuesFileRoot::Worktree {
                project_root: self.project_root,
            },
        }
    }
}

/// Resolve the value sources of one resource that `file` declares, in place.
/// `inputs` are the Release's resolved inputs, when the bundle's Release
/// declares any, and `unknown` names those whose value only orchestration
/// knows. Values files read are appended to `read`. Messages show paths as
/// provenance records them (`roots`).
///
/// Returns the JSON Pointers of the resource's free-form data (chart values,
/// a Component's spec) that hold a value not known before a run: what a
/// pointer selects in an unknown input. In the resource's typed fields such
/// a value takes the type Nyl's schema gives the field
/// ([`settle_unknown`]); in chart values, rendering gives it the type the
/// chart's `values.schema.json` gives it ([`settle_chart_values`]).
pub(crate) fn resolve_bundle_resource(
    resource: &mut Value,
    file: &Path,
    files: BundleFiles<'_>,
    roots: ProvenanceRoots<'_>,
    inputs: Option<&Value>,
    unknown: &BTreeSet<String>,
    read: &mut Vec<PathBuf>,
) -> Result<Vec<String>> {
    let subject = BundleResource::of(resource);
    let mut unknown_data = Vec::new();
    let mut issues = literal_field_issues(resource)
        .into_iter()
        .map(BindingIssue::new)
        .collect::<Vec<_>>();
    if issues.is_empty() {
        let provider = ReleaseInputs { inputs, unknown };
        let resolved = resolve_document("", "", resource, subject.scope(), &provider, &mut issues);
        for issue in &mut issues {
            if let Some(field) = issue.message.strip_prefix('.') {
                issue.message = field.to_owned();
            }
        }
        if issues.is_empty() {
            let unknown_at = resolved
                .provenance
                .iter()
                .filter(|(_, origin)| **origin == ReleaseInputOrigin::Unknown)
                .map(|(pointer, _)| pointer.clone())
                .collect::<Vec<_>>();
            *resource = resolved.value;
            unknown_data = subject.settle_unknown_fields(resource, unknown_at);
            if subject == BundleResource::HelmChart {
                merge_values_from(resource, file, files, roots, &provider, read, &mut issues)?;
            }
        }
    }
    if issues.is_empty() {
        return Ok(unknown_data);
    }
    let messages = issues.iter().map(|issue| issue.message.as_str()).collect::<Vec<_>>();
    Err(NylError::value_source(format!(
        "{} in {}: {}",
        describe(resource),
        shown_path(file, roots),
        messages.join("; ")
    )))
}

/// The resources of a bundle, by how their fields take sources.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BundleResource {
    HelmChart,
    RemoteManifest,
    Release,
    Component,
    /// A plain Kubernetes manifest: free-form data.
    Manifest,
}

impl BundleResource {
    fn of(resource: &Value) -> Self {
        let kind = resource.get("kind").and_then(Value::as_str);
        let api_version = resource.get("apiVersion").and_then(Value::as_str);
        if api_version == Some(API_VERSION) && kind == Some("HelmChart") {
            Self::HelmChart
        } else if RemoteManifest::is_remote_manifest(resource) {
            Self::RemoteManifest
        } else if Release::is_release(resource) {
            Self::Release
        } else if crate::resources::is_nyl_component(resource) {
            Self::Component
        } else {
            Self::Manifest
        }
    }

    /// The contexts of the resource's parts: the typed fields of a Nyl
    /// resource reserve every `$` key, its free-form data (chart values, a
    /// Component's spec) and a plain manifest keep foreign `$` keys as data,
    /// and a Release's input declarations are literal.
    fn scope(self) -> Scope<'static> {
        const CHART: &[(&str, Context)] = &[("/spec/values", Context::ReleaseData)];
        const COMPONENT: &[(&str, Context)] = &[("/spec", Context::ReleaseData)];
        const RELEASE: &[(&str, Context)] = &[("/spec/inputs", Context::Literal)];
        let (context, within) = match self {
            Self::HelmChart => (Context::ReleaseBundle, CHART),
            Self::Component => (Context::ReleaseBundle, COMPONENT),
            Self::Release => (Context::ReleaseBundle, RELEASE),
            Self::RemoteManifest => (Context::ReleaseBundle, &[][..]),
            Self::Manifest => (Context::ReleaseData, &[][..]),
        };
        Scope { context, within }
    }

    /// The JSON Schema of the resource's typed fields, when Nyl types it.
    fn schema(self) -> Option<Value> {
        Some(match self {
            Self::HelmChart => serde_json::json!(schemars::schema_for!(crate::resources::HelmChart)),
            Self::RemoteManifest => serde_json::json!(schemars::schema_for!(RemoteManifest)),
            Self::Release => crate::resources::generate_release_schema(),
            Self::Component => serde_json::json!(schemars::schema_for!(crate::resources::NylComponent)),
            Self::Manifest => return None,
        })
    }

    /// Give each unknown value at `unknown` in the resource's typed fields
    /// the type Nyl's schema gives its field, so the resource's checks skip
    /// it ([`settle_unknown`]). Returns the pointers in its free-form data,
    /// which only what reads the data can type.
    fn settle_unknown_fields(self, resource: &mut Value, unknown: Vec<String>) -> Vec<String> {
        let scope = self.scope();
        let (data, typed): (Vec<_>, Vec<_>) = unknown.into_iter().partition(|pointer| {
            scope.context == Context::ReleaseData
                || scope
                    .within
                    .iter()
                    .any(|(prefix, context)| *context == Context::ReleaseData && pointer_within(pointer, prefix))
        });
        if let (false, Some(schema)) = (typed.is_empty(), self.schema()) {
            settle_unknown(resource, &typed, &schema);
        }
        data
    }
}

/// Whether `pointer` is `prefix` or lies beneath it.
fn pointer_within(pointer: &str, prefix: &str) -> bool {
    pointer
        .strip_prefix(prefix)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// The pointers of `unknown` beneath `prefix`, relative to it: the unknown
/// values of chart values that a HelmChart's `/spec/values` or a Component's
/// `/spec` holds.
pub(crate) fn unknown_under(unknown: &[String], prefix: &str) -> Vec<String> {
    unknown
        .iter()
        .filter(|pointer| pointer_within(pointer, prefix))
        .map(|pointer| pointer[prefix.len()..].to_owned())
        .collect()
}

/// Replace the value at each of `unknown`, one not known before a run, with
/// a stand-in of the type the JSON Schema `schema` gives its position
/// ([`nyl_core::bindings::schema_placeholder`]): the placeholder string
/// where a string fits or the schema says nothing, else `false`, `0`, `{}`,
/// `[]`, or the first allowed value. A check of `document` against `schema`
/// then passes at those positions, as admission skips a value not yet
/// known, and still checks everything else.
pub(crate) fn settle_unknown(document: &mut Value, unknown: &[String], schema: &Value) {
    for pointer in unknown {
        if let Some(slot) = document.pointer_mut(pointer) {
            *slot = nyl_core::bindings::schema_placeholder(schema, pointer, slot);
        }
    }
}

/// The chart values `values` with each unknown value at `unknown` typed by
/// the chart's `values.schema.json` in `chart_dir`, when it has a readable
/// one ([`settle_unknown`]); without one, the values as they are. Helm then
/// validates the values against the schema everywhere else.
pub(crate) fn settle_chart_values<'a>(
    values: &'a Value,
    unknown: &[String],
    chart_dir: &Path,
) -> std::borrow::Cow<'a, Value> {
    if unknown.is_empty() {
        return std::borrow::Cow::Borrowed(values);
    }
    // A schema Helm cannot read fails the render there, with Helm's error.
    let Some(schema) = std::fs::read(chart_dir.join("values.schema.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
    else {
        return std::borrow::Cow::Borrowed(values);
    };
    let mut values = values.clone();
    settle_unknown(&mut values, unknown, &schema);
    std::borrow::Cow::Owned(values)
}

/// `HelmChart "web"`, for messages.
fn describe(resource: &Value) -> String {
    let kind = resource.get("kind").and_then(Value::as_str).unwrap_or("<unknown-kind>");
    let name = resource
        .pointer("/metadata/name")
        .and_then(Value::as_str)
        .unwrap_or("<unknown-name>");
    format!("{kind} {name:?}")
}

/// Merge a resolved HelmChart's `valuesFrom` entries in order, then its
/// inline `values`; the result replaces `spec.values` and the entries are
/// removed. The merged values are data and are never walked, and a values
/// file that holds a source is an error: it would read as data. A values file
/// the configuration may not read is an issue; one that cannot be read for a
/// reason outside the configuration, such as an I/O error, is returned as it is.
fn merge_values_from(
    resource: &mut Value,
    file: &Path,
    files: BundleFiles<'_>,
    roots: ProvenanceRoots<'_>,
    provider: &ReleaseInputs<'_>,
    read: &mut Vec<PathBuf>,
    issues: &mut Vec<BindingIssue>,
) -> Result<()> {
    let Some(spec) = resource.get_mut("spec").and_then(Value::as_object_mut) else {
        return Ok(());
    };
    let Some(entries) = spec.remove("valuesFrom") else {
        return Ok(());
    };
    let entries: Vec<HelmValuesSource> = match serde_json::from_value(entries) {
        Ok(entries) => entries,
        Err(error) => {
            issues.push(BindingIssue::new(format!("spec.valuesFrom: {error}")));
            return Ok(());
        }
    };
    let mut sources = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        let field = format!("spec.valuesFrom[{index}]");
        let origin = match entry.origin(&field) {
            Ok(origin) => origin,
            Err(error) => {
                issues.push(BindingIssue::new(error.to_string()));
                continue;
            }
        };
        let source = match origin {
            HelmValuesOrigin::File(path) => {
                let directory = file.parent().filter(|parent| !parent.as_os_str().is_empty());
                let shown = |path: &Path| shown_path(path, roots);
                match load_chart_values_file(files.root(), directory.unwrap_or_else(|| Path::new(".")), path, &shown) {
                    Ok((path, document)) => {
                        let what = format!("values file {}", shown_path(&path, roots));
                        read.push(path);
                        match sources_in("", &document).first() {
                            Some((at, source)) => Err(format!(
                                "{what} holds a {} source at {}; value sources are not resolved inside valuesFrom files, which are data; move it to the inline values (see {VALUE_SOURCES_DOCS})",
                                source.key(),
                                if at.is_empty() { "its root" } else { at }
                            )),
                            None => entry.select(&document, &what),
                        }
                    }
                    Err(ValuesFileError::Invalid(NylError::Config(message))) => Err(message),
                    Err(ValuesFileError::Invalid(error)) => Err(error.to_string()),
                    Err(ValuesFileError::Failed(error)) => return Err(error),
                }
            }
            HelmValuesOrigin::Input(name) => match provider.input_value(&format!("{field}.input"), name) {
                // What a pointer selects in an unknown input is unknown: the
                // entry contributes nothing, and the input's type still counts.
                Ok(value) if value.is_object() && provider.selects_unknown(name, entry.pointer.as_deref()) => continue,
                Ok(value) if value.is_object() => entry.select(value, &format!("Release input {name:?}")),
                Ok(value) => Err(format!(
                    "Release input {name:?} has type {}; valuesFrom reads only inputs of type object",
                    nyl_core::bindings::json_type_name(value)
                )),
                Err(issue) => {
                    issues.push(issue);
                    continue;
                }
            },
        };
        match source {
            Ok(source) => sources.push(source),
            Err(message) => issues.push(BindingIssue::new(format!("{field}: {message}"))),
        }
    }
    if !issues.is_empty() {
        return Ok(());
    }
    let inline = spec.get("values").cloned().unwrap_or(Value::Null);
    match merge_chart_values(sources, inline) {
        Ok(values) => {
            spec.insert("values".to_owned(), values);
        }
        Err(message) => issues.push(BindingIssue::new(message)),
    }
    Ok(())
}

/// The `$fromInput` source of a Release bundle: the Release's resolved inputs,
/// when its Release declares any.
struct ReleaseInputs<'a> {
    inputs: Option<&'a Value>,
    /// Inputs whose value only orchestration knows (`nyl validate`): each
    /// holds a placeholder of its declared type, so what a pointer selects in
    /// one is unknown and not checked.
    unknown: &'a BTreeSet<String>,
}

impl ReleaseInputs<'_> {
    /// Whether reading `pointer` of input `name` selects a value only
    /// orchestration knows: a non-empty pointer into an unknown input whose
    /// declared type is `object` or `array`, which its placeholder holds. A
    /// pointer into a scalar input selects nothing whatever its value, so it
    /// stays an error.
    fn selects_unknown(&self, name: &str, pointer: Option<&str>) -> bool {
        pointer.is_some_and(|pointer| !pointer.is_empty())
            && self.unknown.contains(name)
            && self
                .inputs
                .and_then(|inputs| inputs.get(name))
                .is_some_and(|value| value.is_object() || value.is_array())
    }

    /// The resolved value of input `name`, read at `field`.
    fn input_value(&self, field: &str, name: &str) -> std::result::Result<&Value, BindingIssue> {
        let Some(inputs) = self.inputs.and_then(Value::as_object) else {
            return Err(BindingIssue::new(format!(
                "{field} reads Release input {name:?}, but this resource is not rendered as part of a Release that declares inputs; Release inputs are read only in the bundle of a Release whose spec.inputs declares them"
            )));
        };
        inputs.get(name).ok_or_else(|| {
            let declared = inputs.keys().map(|key| format!("{key:?}")).collect::<Vec<_>>();
            BindingIssue::new(format!(
                "{field} reads Release input {name:?}, which the Release does not declare; it declares {}",
                if declared.is_empty() {
                    "none".to_owned()
                } else {
                    declared.join(", ")
                }
            ))
        })
    }
}

/// Where a resolved `$fromInput` source read its value: always the Release
/// input, because the source has no default of its own.
#[derive(Debug, Clone, PartialEq)]
enum ReleaseInputOrigin {
    /// The input's value, or what a pointer selects in it.
    Input,
    /// What a pointer selects in an input only orchestration fills: not
    /// known before a run, so it holds a stand-in.
    Unknown,
}

impl Origin for ReleaseInputOrigin {
    fn from_default() -> Self {
        Self::Input
    }
    fn from_override() -> Self {
        Self::Input
    }
    fn from_value() -> Self {
        Self::Input
    }
    fn describe(&self, field: &str) -> String {
        format!("{field} (from a Release input)")
    }
}

impl Providers for ReleaseInputs<'_> {
    type Origin = ReleaseInputOrigin;

    /// The input's value, or the value `pointer` selects in it. Its type is
    /// checked where the resolved resource is deserialized, as a literal's.
    fn input(&self, field: &str, source: &InputSource) -> Provided<ReleaseInputOrigin> {
        let field = format!("{field}.$fromInput");
        let input = self.input_value(&field, &source.name)?;
        if self.selects_unknown(&source.name, source.pointer.as_deref()) {
            // The placeholder's shape says nothing about the value
            // orchestration delivers; the value's type is unknown too.
            return Ok(Provision::Value(Resolved {
                value: Value::String(crate::gitops::inputs::ORCHESTRATED_PLACEHOLDER.to_owned()),
                origin: ReleaseInputOrigin::Unknown,
            }));
        }
        let pointer = source.pointer.as_deref().unwrap_or_default();
        let Some(value) = input.pointer(pointer) else {
            return Err(BindingIssue::new(format!(
                "{field} reads {pointer:?} of Release input {:?}, which holds nothing there",
                source.name
            )));
        };
        Ok(Provision::Value(Resolved {
            value: value.clone(),
            origin: ReleaseInputOrigin::Input,
        }))
    }

    fn missing(&self, field: &str, clause: &Clause) -> Option<BindingIssue> {
        let Clause::Input(source) = clause else {
            return None;
        };
        Some(BindingIssue::new(format!(
            "{field}.$fromInput reads Release input {:?}, which is null; bind a value or give the input a default",
            source.name
        )))
    }
}

/// Refuse `valuesFrom` in a HelmChart that rendering generated, such as one
/// a chart's output holds: it reads files and inputs only in the files of a
/// Release bundle. A value source in generated output is refused where it is
/// generated ([`reject_sources_in_output`]).
pub(crate) fn reject_generated_values_from(resource: &Value) -> Result<()> {
    if resource.pointer("/spec/valuesFrom").is_none() {
        return Ok(());
    }
    Err(NylError::value_source(format!(
        "{} was generated while rendering, and spec.valuesFrom is read only in the files of a Release bundle",
        describe(resource)
    )))
}

/// Refuse a value source in what `expanding` generated: a chart's output, a
/// Component's, or a remote manifest. Sources resolve only in the files of a
/// Release bundle, so the output of recursive expansion never reads one;
/// foreign `$` keys such as `$ref` stay data.
pub(crate) fn reject_sources_in_output(expanding: &str, generated: &[Value]) -> Result<()> {
    let mut issues = Vec::new();
    for resource in generated {
        for (field, source) in sources_in("", resource) {
            issues.push(format!(
                "{} {}",
                describe(resource),
                if field.is_empty() {
                    format!("is a {} source", source.key())
                } else {
                    format!("holds a {} source at {field}", source.key())
                }
            ));
        }
    }
    if issues.is_empty() {
        return Ok(());
    }
    Err(NylError::value_source(format!(
        "{expanding} generated value sources, which resolve only in the files of a Release bundle, never in what rendering generates: {}; write the value in the bundle's own files, such as the chart's values (see {VALUE_SOURCES_DOCS})",
        issues.join("; ")
    )))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::json;
    use tempfile::TempDir;

    use super::*;
    use crate::render::load_release_bundle;
    use crate::template::TemplateContext;

    fn context(inputs: Option<Value>) -> TemplateContext {
        TemplateContext {
            values: json!({}),
            secrets: json!({}),
            env: serde_json::Map::new(),
            cluster: None,
            target: None,
            inputs,
            unknown_inputs: BTreeSet::new(),
        }
    }

    /// A Git worktree with `files`, and `app.yaml` holding a Release and a
    /// HelmChart whose spec is `spec`.
    fn project(spec: &str, files: &[(&str, &str)]) -> TempDir {
        let temp = TempDir::new().unwrap();
        git2::Repository::init(temp.path()).unwrap();
        for (path, contents) in files {
            let path = temp.path().join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, contents).unwrap();
        }
        fs::create_dir_all(temp.path().join("apps")).unwrap();
        fs::write(
            temp.path().join("apps/app.yaml"),
            format!(
                "apiVersion: k8s.nyl/v1\nkind: Release\nmetadata: {{name: web, namespace: web}}\n---\napiVersion: k8s.nyl/v1\nkind: HelmChart\nmetadata: {{name: web}}\nspec:\n  chart: {{name: ./chart}}\n{spec}"
            ),
        )
        .unwrap();
        temp
    }

    fn load(temp: &TempDir, inputs: Option<Value>) -> Result<(Value, Vec<PathBuf>)> {
        let bundle = load_release_bundle(&temp.path().join("apps/app.yaml"), &context(inputs))?;
        let chart = bundle
            .resources
            .iter()
            .find(|resource| resource.value["kind"] == "HelmChart")
            .unwrap()
            .value
            .clone();
        Ok((chart, bundle.inputs))
    }

    fn error(temp: &TempDir, inputs: Option<Value>) -> String {
        load(temp, inputs).unwrap_err().to_string()
    }

    /// Catches `valuesFrom` merging in another order than written, a `key`
    /// or `pointer` ignored, inline `values` not winning, or a values file
    /// missing from the bundle's inputs, which the Release and target cache
    /// keys and the ownership index record.
    #[test]
    fn test_values_from_merges_files_and_inputs_in_order_then_inline_values() {
        let temp = project(
            "  valuesFrom:\n    - file: ../shared/base.yaml\n    - file: /apps/overlay.yaml\n      pointer: /web\n    - input: chartValues\n      key: mounted\n    - input: chartValues\n      pointer: /optional\n  values: {replicas: 3, nested: {inline: true}}\n",
            &[
                ("shared/base.yaml", "replicas: 1\nnested: {base: true, list: [1, 2]}\norder: base\n"),
                ("apps/overlay.yaml", "web: {nested: {list: [3]}, order: overlay}\nother: {ignored: true}\n"),
            ],
        );
        let inputs = json!({"chartValues": {"order": "input", "optional": {"feature": {"enabled": true}}}});
        let (chart, read) = load(&temp, Some(inputs)).unwrap();
        assert_eq!(
            chart["spec"]["values"],
            json!({
                "replicas": 3,
                "nested": {"base": true, "list": [3], "inline": true},
                "order": "overlay",
                "mounted": {"order": "input", "optional": {"feature": {"enabled": true}}},
                "feature": {"enabled": true},
            })
        );
        assert!(chart["spec"].get("valuesFrom").is_none());
        let root = temp.path().canonicalize().unwrap();
        assert!(read.contains(&root.join("shared/base.yaml")), "{read:?}");
        assert!(read.contains(&root.join("apps/overlay.yaml")), "{read:?}");
    }

    /// Catches merged values being walked for sources: in this release they
    /// are chart data, inline and from a values file.
    #[test]
    fn test_values_from_values_are_data() {
        let temp = project(
            "  valuesFrom: [{file: values.yaml}]\n  values: {inline: {$ref: '#/x'}}\n",
            &[("apps/values.yaml", "file: {$schema: x}\n")],
        );
        let (chart, _) = load(&temp, None).unwrap();
        assert_eq!(
            chart["spec"]["values"],
            json!({"file": {"$schema": "x"}, "inline": {"$ref": "#/x"}})
        );
    }

    /// Catches an `input` entry read outside a Release that declares it, or
    /// from an input that is not an object, passing silently.
    #[test]
    fn test_values_from_input_errors_name_the_chart_and_the_rule() {
        let temp = project("  valuesFrom: [{input: chartValues}]\n", &[]);
        let outside = error(&temp, None);
        assert!(outside.contains("HelmChart \"web\" in "), "{outside}");
        assert!(
            outside.contains("spec.valuesFrom[0].input reads Release input \"chartValues\", but this resource is not rendered as part of a Release that declares inputs"),
            "{outside}"
        );
        let undeclared = error(&temp, Some(json!({"replicas": 2})));
        assert!(
            undeclared.contains("which the Release does not declare; it declares \"replicas\""),
            "{undeclared}"
        );
        let scalar = error(&temp, Some(json!({"chartValues": "big"})));
        assert!(
            scalar
                .contains("Release input \"chartValues\" has type string; valuesFrom reads only inputs of type object"),
            "{scalar}"
        );

        let pointer = project("  valuesFrom: [{input: chartValues, pointer: /missing}]\n", &[]);
        let missing = error(&pointer, Some(json!({"chartValues": {"present": {}}})));
        assert!(
            missing.contains("pointer \"/missing\" selects nothing in Release input \"chartValues\""),
            "{missing}"
        );

        let both = project("  valuesFrom: [{input: chartValues, file: a.yaml}]\n", &[]);
        assert!(error(&both, None).contains("spec.valuesFrom[0] must set exactly one of file and input"));
    }

    /// Catches a values file read past the rules of Cluster `valuesFrom`: one
    /// Git ignores, or one that is not a mapping.
    #[test]
    fn test_values_from_files_follow_the_values_file_rules() {
        let ignored = project(
            "  valuesFrom: [{file: secret.yaml}]\n",
            &[("apps/secret.yaml", "a: 1\n"), (".gitignore", "secret.yaml\n")],
        );
        assert!(error(&ignored, None).contains("is missing or is not an eligible Git-visible regular file"));
        let list = project("  valuesFrom: [{file: list.yaml}]\n", &[("apps/list.yaml", "- a\n")]);
        assert!(error(&list, None).contains("spec.valuesFrom[0]"));
        let escaped = project("  valuesFrom: [{file: ../../outside.yaml}]\n", &[]);
        assert!(error(&escaped, None).contains("outside the Git worktree"));
        let missing = project("  valuesFrom: [{file: missing.yaml}]\n", &[]);
        for temp in [&ignored, &list, &escaped, &missing] {
            assert!(load(temp, None).unwrap_err().is_value_source_config());
        }
    }

    /// Catches a values file that cannot be read for a reason outside the
    /// configuration, such as a Release outside any Git repository, reported
    /// as the configuration's mistake (`NYL-UNIT-INVALID` in a run) instead
    /// of the failure it is.
    #[test]
    fn test_values_from_environment_failures_keep_their_error() {
        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join("values.yaml"), "a: 1\n").unwrap();
        fs::write(
            temp.path().join("app.yaml"),
            "apiVersion: k8s.nyl/v1\nkind: HelmChart\nmetadata: {name: web}\nspec:\n  chart: {name: ./chart}\n  valuesFrom: [{file: values.yaml}]\n",
        )
        .unwrap();
        if git2::Repository::discover(temp.path()).is_ok() {
            return; // The temporary directory lies in a Git worktree here.
        }
        let error = load_release_bundle(&temp.path().join("app.yaml"), &context(None)).unwrap_err();
        assert!(!error.is_value_source_config(), "{error}");
        assert!(
            error.to_string().contains("reads a file of the Git worktree, but"),
            "{error}"
        );
    }

    /// Catches `$fromInput` in a HelmChart's fields not resolving, a value of
    /// the wrong type passing, or a source the bundle cannot resolve, or one
    /// in a literal-only field, surviving as data.
    #[test]
    fn test_input_resolves_chart_fields() {
        let temp = project(
            "  includeCrds: {$fromInput: crds}\n  lookup: {$fromInput: {name: features, pointer: /lookup}}\n",
            &[],
        );
        let (chart, _) = load(&temp, Some(json!({"crds": false, "features": {"lookup": true}}))).unwrap();
        assert_eq!(chart["spec"]["includeCrds"], json!(false));
        assert_eq!(chart["spec"]["lookup"], json!(true));
        let parsed: crate::resources::HelmChart = serde_json::from_value(chart).unwrap();
        assert_eq!(parsed.spec.include_crds, Some(false));

        let (wrong, _) = load(&temp, Some(json!({"crds": "no", "features": {"lookup": true}}))).unwrap();
        assert!(serde_json::from_value::<crate::resources::HelmChart>(wrong).is_err());
        let outside = error(&temp, None);
        assert!(
            outside.contains("spec.includeCrds.$fromInput reads Release input \"crds\", but this resource is not rendered as part of a Release that declares inputs"),
            "{outside}"
        );

        let shorthand = project("  includeCrds: {$fromInput: /crds}\n", &[]);
        let shorthand = error(&shorthand, Some(json!({"crds": true})));
        assert!(
            shorthand.contains("spec.includeCrds.$fromInput takes a name; did you mean crds?"),
            "{shorthand}"
        );

        let unit = project(
            "  values: {}\n  version: {$fromUnit: {unit: chart, output: version}}\n",
            &[],
        );
        let unit = error(&unit, Some(json!({"chart": "x"})));
        assert!(
            unit.contains("spec.version holds a $fromUnit source")
                && unit.contains("declare a Release input and bind it on the target with $fromUnit"),
            "{unit}"
        );

        let named = project("  values: {}\n", &[]);
        fs::write(
            named.path().join("apps/app.yaml"),
            "apiVersion: k8s.nyl/v1\nkind: Release\nmetadata: {name: web, namespace: web}\n---\napiVersion: k8s.nyl/v1\nkind: HelmChart\nmetadata: {name: {$fromInput: name}}\nspec: {chart: {name: ./chart}}\n",
        )
        .unwrap();
        let named = load_release_bundle(
            &named.path().join("apps/app.yaml"),
            &context(Some(json!({"name": "x"}))),
        )
        .unwrap_err()
        .to_string();
        assert!(
            named.contains("metadata.name holds a $fromInput source, but metadata.name must be literal"),
            "{named}"
        );
    }

    /// Catches a chart that rendering generated reading files, and a value
    /// source in generated output passing as data.
    #[test]
    fn test_generated_output_takes_no_sources() {
        let chart = json!({"apiVersion": API_VERSION, "kind": "HelmChart", "metadata": {"name": "nested"},
            "spec": {"chart": {"name": "c"}, "valuesFrom": [{"file": "a.yaml"}]}});
        let error = reject_generated_values_from(&chart).unwrap_err().to_string();
        assert!(
            error.contains("HelmChart \"nested\" was generated while rendering, and spec.valuesFrom"),
            "{error}"
        );

        let output = [
            json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "a"}, "data": {"schema": {"$ref": "#/x"}}}),
            json!({"apiVersion": "apps/v1", "kind": "Deployment", "metadata": {"name": "web"},
                "spec": {"template": {"spec": {"containers": [{"image": {"$fromInput": "image"}}]}}}}),
        ];
        let error = reject_sources_in_output("k8s.nyl/v1 HelmChart web (chart: ./chart)", &output)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("k8s.nyl/v1 HelmChart web (chart: ./chart) generated value sources"),
            "{error}"
        );
        assert!(
            error.contains("Deployment \"web\" holds a $fromInput source at spec.template.spec.containers[0].image"),
            "{error}"
        );
        assert!(!error.contains("ConfigMap"), "{error}");
        assert!(reject_sources_in_output("x", &output[..1]).is_ok());
    }

    /// Catches `$fromInput` in chart values resolved after `valuesFrom` merged
    /// (a file would then override the input), a foreign `$` key in chart
    /// values refused, or a likely slip there passing as data.
    #[test]
    fn test_input_resolves_in_chart_values_before_values_from_merges() {
        let temp = project(
            "  valuesFrom: [{file: base.yaml}]\n  values:\n    image: {$fromInput: image}\n    nested: {replicas: {$fromInput: {name: sizes, pointer: /web}}}\n    schema: {$ref: '#/x'}\n    escaped: {$literal: {$fromInput: kept}}\n",
            &[("apps/base.yaml", "image: from-file\nnested: {replicas: 1, other: true}\n")],
        );
        let (chart, _) = load(&temp, Some(json!({"image": "web:1", "sizes": {"web": 3}}))).unwrap();
        assert_eq!(
            chart["spec"]["values"],
            json!({
                "image": "web:1",
                "nested": {"replicas": 3, "other": true},
                "schema": {"$ref": "#/x"},
                "escaped": {"$fromInput": "kept"},
            })
        );

        let slip = project("  values: {image: {$fromInputs: image}}\n", &[]);
        let slip = error(&slip, Some(json!({"image": "web:1"})));
        assert!(
            slip.contains("spec.values.image.$fromInputs is not a value source; did you mean $fromInput?"),
            "{slip}"
        );
    }

    /// Catches a source in a valuesFrom file read as data, where it would
    /// silently reach the chart as an object.
    #[test]
    fn test_a_source_in_a_values_from_file_is_an_error_with_the_fix() {
        let temp = project(
            "  valuesFrom: [{file: base.yaml}]\n",
            &[("apps/base.yaml", "web: {image: {$fromInput: image}}\n")],
        );
        let message = error(&temp, Some(json!({"image": "web:1"})));
        assert!(
            message.contains("spec.valuesFrom[0]: values file") && message.contains("holds a $fromInput source at web.image; value sources are not resolved inside valuesFrom files, which are data; move it to the inline values"),
            "{message}"
        );
    }

    /// Catches `nyl validate` failing a correct bundle that reads an object
    /// input by pointer, when a target binds the input with `$fromUnit`: the
    /// input holds a placeholder before a run, so what the pointer selects is
    /// unknown and unchecked, while the input's own type still is. Catches,
    /// too, a pointer into a known input left unchecked.
    #[test]
    fn test_a_pointer_into_an_input_only_orchestration_knows_is_unchecked() {
        let temp = project(
            "  valuesFrom: [{input: image, pointer: /extra}]
  values:
    image:
      repository: {$fromInput: {name: image, pointer: /repository}}
      digest: {$fromInput: {name: image, pointer: /digest}}
",
            &[],
        );
        let unknown = BTreeSet::from(["image".to_owned()]);
        let mut context = context(Some(json!({"image": {}})));
        context.unknown_inputs = unknown;
        let bundle = load_release_bundle(&temp.path().join("apps/app.yaml"), &context).unwrap();
        let chart = &bundle.resources[1].value;
        let placeholder = crate::gitops::inputs::ORCHESTRATED_PLACEHOLDER;
        assert_eq!(
            chart["spec"]["values"],
            json!({"image": {"repository": placeholder, "digest": placeholder}})
        );

        let known = error(&temp, Some(json!({"image": {}})));
        assert!(
            known.contains("reads \"/repository\" of Release input \"image\", which holds nothing there"),
            "{known}"
        );

        // A scalar input holds nothing a pointer selects, known or not.
        let mut context = super::tests::context(Some(json!({"image": placeholder})));
        context.unknown_inputs = BTreeSet::from(["image".to_owned()]);
        let scalar = load_release_bundle(&temp.path().join("apps/app.yaml"), &context)
            .unwrap_err()
            .to_string();
        assert!(
            scalar.contains("reads \"/repository\" of Release input \"image\", which holds nothing there"),
            "{scalar}"
        );
    }

    /// Catches a correct bundle failing `nyl validate` because what a pointer
    /// selects in an input only orchestration fills is a placeholder string:
    /// in a typed field it takes the type Nyl's schema gives the field, and in
    /// chart values the type the chart's `values.schema.json` gives it, while
    /// it stays the string where a string fits or nothing says otherwise.
    #[test]
    fn test_an_unknown_value_takes_the_type_of_where_it_lands() {
        let temp = project(
            "  includeCrds: {$fromInput: {name: settings, pointer: /crds}}
  lookup: {$fromInput: {name: settings, pointer: /lookup}}
  values:
    replicas: {$fromInput: {name: settings, pointer: /replicas}}
    feature: {enabled: {$fromInput: {name: settings, pointer: /enabled}}}
    name: {$fromInput: {name: settings, pointer: /name}}
",
            &[],
        );
        let mut context = context(Some(json!({"settings": {}})));
        context.unknown_inputs = BTreeSet::from(["settings".to_owned()]);
        let bundle = load_release_bundle(&temp.path().join("apps/app.yaml"), &context).unwrap();
        let resource = &bundle.resources[1];
        let chart: crate::resources::HelmChart = serde_json::from_value(resource.value.clone()).unwrap();
        assert_eq!(chart.spec.include_crds, Some(false));
        assert_eq!(chart.spec.lookup.map(|lookup| lookup.is_enabled()), Some(false));
        assert_eq!(
            resource.unknown,
            [
                "/spec/values/feature/enabled",
                "/spec/values/name",
                "/spec/values/replicas"
            ]
        );

        let unknown = unknown_under(&resource.unknown, "/spec/values");
        let placeholder = crate::gitops::inputs::ORCHESTRATED_PLACEHOLDER;
        let chart_dir = temp.path().join("chart");
        fs::create_dir_all(&chart_dir).unwrap();
        let untyped = settle_chart_values(&chart.spec.values, &unknown, &chart_dir);
        assert_eq!(*untyped, chart.spec.values);
        fs::write(
            chart_dir.join("values.schema.json"),
            r##"{"type": "object", "properties": {"replicas": {"type": "integer"}, "name": {"type": "string"},
                "feature": {"$ref": "#/$defs/feature"}}, "$defs": {"feature": {"properties": {"enabled": {"type": "boolean"}}}}}"##,
        )
        .unwrap();
        let typed = settle_chart_values(&chart.spec.values, &unknown, &chart_dir);
        assert_eq!(
            *typed,
            json!({"replicas": 0, "feature": {"enabled": false}, "name": placeholder})
        );
    }

    /// Catches a run's error naming the absolute path of its temporary
    /// checkout: the resource's file and a values file show as provenance
    /// records them, relative to the project or the worktree.
    #[test]
    fn test_errors_name_files_as_provenance_records_them() {
        let temp = project(
            "  valuesFrom: [{file: base.yaml}, {file: /shared/values.yaml}]\n",
            &[
                ("apps/base.yaml", "web: {image: {$fromInput: image}}\n"),
                ("shared/values.yaml", "host: {$fromInput: host}\n"),
            ],
        );
        let roots = ProvenanceRoots {
            root: Some(&temp.path().join("apps")),
            worktree: Some(temp.path()),
        };
        let files = BundleFiles {
            project_root: temp.path(),
            checkout: None,
        };
        let message = crate::render::bundle::load_release_bundle_with_root(
            &temp.path().join("apps/app.yaml"),
            &context(Some(json!({"image": "web:1", "host": "web"}))),
            roots,
            files,
        )
        .unwrap_err()
        .to_string();
        assert!(
            message.contains("HelmChart \"web\" in app.yaml: spec.valuesFrom[0]: values file base.yaml holds")
                && message.contains("spec.valuesFrom[1]: values file /shared/values.yaml holds"),
            "{message}"
        );
        // Files that are missing show the same way.
        let load_with_roots = |path: &Path| {
            crate::render::bundle::load_release_bundle_with_root(path, &context(None), roots, files)
                .unwrap_err()
                .to_string()
        };
        let absent = load_with_roots(&temp.path().join("apps/absent.yaml"));
        assert!(absent.contains("File not found: absent.yaml"), "{absent}");
        fs::write(
            temp.path().join("apps/app.yaml"),
            "apiVersion: k8s.nyl/v1\nkind: HelmChart\nmetadata: {name: web}\nspec:\n  chart: {name: ./chart}\n  valuesFrom: [{file: missing.yaml}, {file: /shared/missing.yaml}]\n",
        )
        .unwrap();
        let missing = load_with_roots(&temp.path().join("apps/app.yaml"));
        assert!(
            missing.contains("spec.valuesFrom[0]: Values file missing.yaml is missing")
                && missing.contains("spec.valuesFrom[1]: Values file /shared/missing.yaml is missing"),
            "{missing}"
        );
    }

    fn bundle(contents: &str, inputs: Option<Value>) -> Result<Vec<Value>> {
        let temp = TempDir::new().unwrap();
        git2::Repository::init(temp.path()).unwrap();
        fs::write(temp.path().join("app.yaml"), contents).unwrap();
        let bundle = load_release_bundle(&temp.path().join("app.yaml"), &context(inputs))?;
        Ok(bundle.resources.into_iter().map(|resource| resource.value).collect())
    }

    /// Catches `$fromInput` left unresolved in a Component's spec, a
    /// RemoteManifest, a plain manifest, or a `metadata.namespace`, and a
    /// source in a Release's input declarations read as a default.
    #[test]
    fn test_input_resolves_in_every_resource_of_a_bundle() {
        let resources = bundle(
            "apiVersion: k8s.nyl/v1\nkind: Release\nmetadata: {name: web, namespace: {$fromInput: namespace}}\n---\napiVersion: components.k8s.nyl/v1\nkind: web\nmetadata: {name: web}\nspec: {image: {$fromInput: image}, schema: {$ref: '#/x'}}\n---\napiVersion: k8s.nyl/v1\nkind: RemoteManifest\nmetadata: {name: crds}\nspec: {url: {$fromInput: crds}}\n---\napiVersion: apps/v1\nkind: Deployment\nmetadata: {name: web, namespace: {$fromInput: namespace}}\nspec:\n  replicas: {$fromInput: {name: sizes, pointer: /web}}\n  template: {spec: {containers: [{name: web, image: {$fromInput: image}}]}}\n",
            Some(json!({"namespace": "apps", "image": "web:1", "crds": "https://example.com/crds.yaml", "sizes": {"web": 2}})),
        )
        .unwrap();
        assert_eq!(resources[0]["metadata"]["namespace"], "apps");
        assert_eq!(
            resources[1]["spec"],
            json!({"image": "web:1", "schema": {"$ref": "#/x"}})
        );
        assert_eq!(resources[2]["spec"]["url"], "https://example.com/crds.yaml");
        assert_eq!(resources[3]["metadata"]["namespace"], "apps");
        assert_eq!(resources[3]["spec"]["replicas"], 2);
        assert_eq!(
            resources[3]["spec"]["template"]["spec"]["containers"][0]["image"],
            "web:1"
        );

        let labels = bundle(
            "apiVersion: v1\nkind: ConfigMap\nmetadata: {name: a, labels: {tier: {$fromInput: tier}}}\n",
            Some(json!({"tier": "web"})),
        )
        .unwrap_err()
        .to_string();
        assert!(
            labels.contains("metadata.labels.tier holds a $fromInput source, but metadata.labels.tier must be literal"),
            "{labels}"
        );
        let reserved = bundle(
            "apiVersion: k8s.nyl/v1\nkind: RemoteManifest\nmetadata: {name: crds}\nspec: {url: {$ref: x}}\n",
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(reserved.contains("spec.url.$ref is not a value source"), "{reserved}");
        let declaration = bundle(
            "apiVersion: k8s.nyl/v1\nkind: Release\nmetadata: {name: web, namespace: web}\nspec:\n  inputs:\n    image: {type: string, default: {$fromInput: other}}\n",
            Some(json!({"image": "x", "other": "y"})),
        )
        .unwrap_err()
        .to_string();
        assert!(
            declaration.contains("spec.inputs.image.default holds a $fromInput source"),
            "{declaration}"
        );
        let outside = bundle(
            "apiVersion: v1\nkind: ConfigMap\nmetadata: {name: a}\ndata: {image: {$fromInput: image}}\n",
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(
            outside.contains("ConfigMap \"a\" in ") && outside.contains("data.image.$fromInput reads Release input \"image\", but this resource is not rendered as part of a Release that declares inputs"),
            "{outside}"
        );
    }
}
