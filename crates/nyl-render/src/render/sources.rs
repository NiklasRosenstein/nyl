//! Value sources of the Nyl resources in a Release bundle: HelmChart
//! `valuesFrom`, and `$input` in a HelmChart's fields.
//!
//! Contract: [Declarative value sources](../../../../design/declarative-value-sources.md),
//! the Release bundle context. Each resource is resolved on its [`Value`]
//! right after its file is parsed, before anything deserializes or inspects
//! it, so every reader (expansion, `lookup` detection, cache bypass
//! decisions) sees the resolved document:
//!
//! - The literal-only fields (`apiVersion`, `kind`, `metadata.name`,
//!   `metadata.labels`) hold no source ([`literal_field_issues`]).
//! - `$input` sources in a HelmChart's fields, outside its chart `values`,
//!   resolve against the Release's resolved inputs ([`resolve_document`] in
//!   the [`Context::ReleaseBundle`]); the resolved document is then
//!   deserialized, which checks each value's type as a literal's.
//! - `valuesFrom` entries merge in order, then the inline `values`
//!   ([`merge_chart_values`]); the result replaces `spec.values` and the
//!   entries are removed. The merged values are data and are never walked.
//! - The other Nyl resources of a bundle hold no source outside their
//!   free-form data.
//!
//! Every value read enters the dependency recorder at the layer that consumes
//! it: a values file is returned to the caller as a bundle input (the Release
//! and target cache keys, and the ownership index), Release inputs are part of
//! the template context the Release cache records, and the merged values are
//! a parameter of the Helm cache key.

use std::path::{Path, PathBuf};

use nyl_core::bindings::{
    find_clauses, literal_field_issues, resolve_document, BindingIssue, Clause, Context, InputSource, Origin, Provided,
    Providers, Provision, Resolved,
};
use nyl_core::constants::API_VERSION;
use serde_json::Value;

use crate::gitops::values::{load_chart_values_file, ValuesFileRoot};
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
/// declares any. Values files read are appended to `read`.
pub(crate) fn resolve_bundle_resource(
    resource: &mut Value,
    file: &Path,
    files: BundleFiles<'_>,
    inputs: Option<&Value>,
    read: &mut Vec<PathBuf>,
) -> Result<()> {
    let Some(subject) = BundleResource::of(resource) else {
        return Ok(());
    };
    let mut issues = literal_field_issues(resource)
        .into_iter()
        .map(BindingIssue::new)
        .collect::<Vec<_>>();
    if issues.is_empty() {
        if subject == BundleResource::HelmChart {
            resolve_chart(resource, file, files, inputs, read, &mut issues);
        } else {
            reject_sources(resource, subject, &mut issues);
        }
    }
    if issues.is_empty() {
        return Ok(());
    }
    let messages = issues.iter().map(|issue| issue.message.as_str()).collect::<Vec<_>>();
    Err(NylError::config(format!(
        "{} in {}: {}",
        describe(resource),
        crate::util::path_for_display(file).display(),
        messages.join("; ")
    )))
}

/// The Nyl resources of a bundle whose typed fields are checked for sources.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BundleResource {
    HelmChart,
    RemoteManifest,
    Release,
    Component,
}

impl BundleResource {
    fn of(resource: &Value) -> Option<Self> {
        let kind = resource.get("kind").and_then(Value::as_str);
        let api_version = resource.get("apiVersion").and_then(Value::as_str);
        if api_version == Some(API_VERSION) && kind == Some("HelmChart") {
            Some(Self::HelmChart)
        } else if RemoteManifest::is_remote_manifest(resource) {
            Some(Self::RemoteManifest)
        } else if Release::is_release(resource) {
            Some(Self::Release)
        } else if crate::resources::is_nyl_component(resource) {
            Some(Self::Component)
        } else {
            None
        }
    }

    /// The free-form subtree that is data, never resolved or checked.
    fn data_pointer(self) -> Option<&'static str> {
        match self {
            Self::HelmChart => Some("/spec/values"),
            Self::Component => Some("/spec"),
            Self::Release => Some("/spec/inputs"),
            Self::RemoteManifest => None,
        }
    }
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

/// `resource` without its free-form data.
fn typed_part(resource: &Value, subject: BundleResource) -> Value {
    let mut typed = resource.clone();
    if let Some(pointer) = subject.data_pointer() {
        let (parent, key) = pointer.rsplit_once('/').expect("a data pointer names a field");
        if let Some(Value::Object(parent)) = typed.pointer_mut(parent) {
            parent.remove(key);
        }
    }
    typed
}

/// Report every value source in a resource that resolves none, outside its
/// free-form data.
fn reject_sources(resource: &Value, subject: BundleResource, issues: &mut Vec<BindingIssue>) {
    if let Err(found) = find_clauses("", "", &typed_part(resource, subject), Context::Literal.into()) {
        issues.extend(
            found
                .into_iter()
                .map(|message| BindingIssue::new(message.strip_prefix('.').map(str::to_owned).unwrap_or(message))),
        );
    }
}

fn resolve_chart(
    resource: &mut Value,
    file: &Path,
    files: BundleFiles<'_>,
    inputs: Option<&Value>,
    read: &mut Vec<PathBuf>,
    issues: &mut Vec<BindingIssue>,
) {
    let provider = ReleaseInputs { inputs };
    // The chart values are data: they leave the walk and come back after it.
    let values = resource
        .get_mut("spec")
        .and_then(Value::as_object_mut)
        .and_then(|spec| spec.remove("values"));
    let resolved = resolve_document("", "", resource, Context::ReleaseBundle.into(), &provider, issues);
    for issue in issues.iter_mut() {
        if let Some(field) = issue.message.strip_prefix('.') {
            issue.message = field.to_owned();
        }
    }
    *resource = resolved.value;
    let Some(spec) = resource.get_mut("spec").and_then(Value::as_object_mut) else {
        return;
    };
    if let Some(values) = values {
        spec.insert("values".to_owned(), values);
    }
    if !issues.is_empty() {
        return;
    }
    let Some(entries) = spec.remove("valuesFrom") else {
        return;
    };
    let entries: Vec<HelmValuesSource> = match serde_json::from_value(entries) {
        Ok(entries) => entries,
        Err(error) => {
            issues.push(BindingIssue::new(format!("spec.valuesFrom: {error}")));
            return;
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
                match load_chart_values_file(files.root(), directory.unwrap_or_else(|| Path::new(".")), path) {
                    Ok((path, document)) => {
                        let what = format!("values file {}", crate::util::path_for_display(&path).display());
                        read.push(path);
                        entry.select(&document, &what)
                    }
                    Err(NylError::Config(message)) => Err(message),
                    Err(error) => Err(error.to_string()),
                }
            }
            HelmValuesOrigin::Input(name) => match provider.input_value(&format!("{field}.input"), name) {
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
        return;
    }
    let inline = spec.get("values").cloned().unwrap_or(Value::Null);
    match merge_chart_values(sources, inline) {
        Ok(values) => {
            spec.insert("values".to_owned(), values);
        }
        Err(message) => issues.push(BindingIssue::new(message)),
    }
}

/// The `$input` source of a Release bundle: the Release's resolved inputs,
/// when its Release declares any.
struct ReleaseInputs<'a> {
    inputs: Option<&'a Value>,
}

impl ReleaseInputs<'_> {
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

/// Where a resolved `$input` source read its value: always the Release
/// input, because the source has no default of its own.
#[derive(Debug, Clone, PartialEq)]
struct ReleaseInputOrigin;

impl Origin for ReleaseInputOrigin {
    fn from_default() -> Self {
        Self
    }
    fn from_override() -> Self {
        Self
    }
    fn from_value() -> Self {
        Self
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
        let field = format!("{field}.$input");
        let input = self.input_value(&field, &source.name)?;
        let pointer = source.pointer.as_deref().unwrap_or_default();
        let Some(value) = input.pointer(pointer) else {
            return Err(BindingIssue::new(format!(
                "{field} reads {pointer:?} of Release input {:?}, which holds nothing there",
                source.name
            )));
        };
        Ok(Provision::Value(Resolved {
            value: value.clone(),
            origin: ReleaseInputOrigin,
        }))
    }

    fn missing(&self, field: &str, clause: &Clause) -> Option<BindingIssue> {
        let Clause::Input(source) = clause else {
            return None;
        };
        Some(BindingIssue::new(format!(
            "{field}.$input reads Release input {:?}, which is null; bind a value or give the input a default",
            source.name
        )))
    }
}

/// Refuse value sources in a HelmChart that rendering generated, such as one
/// a chart's output holds: sources resolve only in the files of a Release
/// bundle, so chart output never reads a file or an input.
pub(crate) fn reject_generated_sources(resource: &Value) -> Result<()> {
    let mut issues = Vec::new();
    if resource.pointer("/spec/valuesFrom").is_some() {
        issues.push(BindingIssue::new("spec.valuesFrom is set"));
    }
    reject_sources(resource, BundleResource::HelmChart, &mut issues);
    if issues.is_empty() {
        return Ok(());
    }
    let messages = issues.iter().map(|issue| issue.message.as_str()).collect::<Vec<_>>();
    Err(NylError::config(format!(
        "{} was generated while rendering, and value sources resolve only in the files of a Release bundle: {}",
        describe(resource),
        messages.join("; ")
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
    }

    /// Catches `$input` in a HelmChart's fields not resolving, a value of
    /// the wrong type passing, or a source the bundle cannot resolve, or one
    /// in a literal-only field, surviving as data.
    #[test]
    fn test_input_resolves_chart_fields() {
        let temp = project(
            "  includeCrds: {$input: crds}\n  lookup: {$input: {name: features, pointer: /lookup}}\n",
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
            outside.contains("spec.includeCrds.$input reads Release input \"crds\", but this resource is not rendered as part of a Release that declares inputs"),
            "{outside}"
        );

        let shorthand = project("  includeCrds: {$input: /crds}\n", &[]);
        let shorthand = error(&shorthand, Some(json!({"crds": true})));
        assert!(
            shorthand.contains("spec.includeCrds.$input takes a name; did you mean crds?"),
            "{shorthand}"
        );

        let unit = project(
            "  values: {}\n  version: {$unit: {unit: chart, output: version}}\n",
            &[],
        );
        let unit = error(&unit, Some(json!({"chart": "x"})));
        assert!(
            unit.contains("spec.version holds a $unit source")
                && unit.contains("declare a Release input and bind it on the target with $unit"),
            "{unit}"
        );

        let named = project("  values: {}\n", &[]);
        fs::write(
            named.path().join("apps/app.yaml"),
            "apiVersion: k8s.nyl/v1\nkind: Release\nmetadata: {name: web, namespace: web}\n---\napiVersion: k8s.nyl/v1\nkind: HelmChart\nmetadata: {name: {$input: name}}\nspec: {chart: {name: ./chart}}\n",
        )
        .unwrap();
        let named = load_release_bundle(
            &named.path().join("apps/app.yaml"),
            &context(Some(json!({"name": "x"}))),
        )
        .unwrap_err()
        .to_string();
        assert!(
            named.contains("metadata.name holds a $input source, but metadata.name must be literal"),
            "{named}"
        );
    }

    /// Catches a chart that rendering generated reading files or inputs.
    #[test]
    fn test_generated_charts_take_no_sources() {
        let chart = json!({"apiVersion": API_VERSION, "kind": "HelmChart", "metadata": {"name": "nested"},
            "spec": {"chart": {"name": "c"}, "valuesFrom": [{"file": "a.yaml"}], "includeCrds": {"$input": "crds"}}});
        let error = reject_generated_sources(&chart).unwrap_err().to_string();
        assert!(
            error.contains("HelmChart \"nested\" was generated while rendering"),
            "{error}"
        );
        assert!(error.contains("spec.valuesFrom is set"), "{error}");
        assert!(error.contains("spec.includeCrds holds a $input source"), "{error}");
    }
}
