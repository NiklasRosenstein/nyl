//! Orchestration units as the source declares them: discovery, selection,
//! and per-environment rendering, and which DeploymentTargets environments
//! own.
//!
//! Contract: [Units](../../../../design/orchestration-core.md#units),
//! [Environment](../../../../design/orchestration-core.md#environment)
//! (values, group defaults), and the
//! [Kubernetes publication unit](../../../../design/orchestration-core.md#kubernetes-publication-unit).
//!
//! - Every document whose `apiVersion` is a unit group (`units.gitops.nyl/v1`)
//!   is a unit; one YAML document is one unit. Its envelope (`apiVersion`,
//!   `kind`, `metadata.name`, `metadata.labels`) is literal and read by the
//!   static envelope scanner, so a document that is not YAML before
//!   rendering needs a block-style envelope. Names are unique across kinds.
//! - An Environment's `unitSelector.matchLabels` selects on the literal
//!   labels before rendering; `{}` selects every unit.
//! - A selected unit's `spec` renders once per Environment with only
//!   `values` (group values merged beneath the Environment's, the
//!   Environment winning), the sanitized `environment`, and later `params`:
//!   never secrets or the process environment. A document that renders to
//!   nothing has no spec.
//! - A `KubernetesPublication` unit's rendered spec gains `releases`: for
//!   each `<group>/<release>` of its target's `releaseInputs`, the bindings
//!   orchestration resolves, as reference objects, so references at
//!   `/releases/<group>~1<release>/<input>` become graph edges.
//! - A target is placed into an environment by a publication unit of that
//!   environment naming it; a target placed by two environments is an error
//!   ([`Code::TargetEnvironmentConflict`](nyl_core::orchestration::codes::Code::TargetEnvironmentConflict)).
//!
//! Rendering never connects to anything and never learns about state.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use nyl_core::bindings::{InputBinding, InputDeclaration, InputType};
use nyl_core::orchestration::codes::Code;
use nyl_core::orchestration::ids::{CommitId, EnvironmentName, UnitName};
use nyl_core::orchestration::ports::{RenderedSource, RenderedUnit, SourceError, SourceRenderer, SourceResult};
use nyl_core::resources::environment::{effective_environment, Environment, EnvironmentGroup};
use nyl_core::resources::units::{is_builtin_unit_api_version, UnitEnvelope, UnitMetadata};
use nyl_core::template_syntax::has_template_syntax;
use serde_json::{Map, Value};

use super::discovery::{
    scan_static_envelope_fields, scan_static_metadata_labels, split_yaml_documents, top_level_value,
};
use super::GitOpsInventory;
use crate::resources::{GitOpsResource, GitOpsResourceKind};
use crate::template::TemplateEngine;
use crate::util::SourceContext;
use crate::{NylError, Result};

/// Kind name of the built-in publication unit, whose rendered spec gains `releases`.
pub const KIND_KUBERNETES_PUBLICATION: &str = "KubernetesPublication";

/// One unit document, before rendering.
#[derive(Debug, Clone, PartialEq)]
pub struct DiscoveredUnit {
    /// Path relative to the directory containing `nyl.toml`.
    pub source_path: PathBuf,
    /// One-based document index within the source file.
    pub document_index: usize,
    /// The document's text, rendered per environment.
    pub raw_document: String,
    pub envelope: UnitEnvelope,
}

impl DiscoveredUnit {
    fn location(&self) -> String {
        format!("{} document {}", self.source_path.display(), self.document_index)
    }

    fn is_publication(&self) -> bool {
        is_builtin_unit_api_version(&self.envelope.api_version) && self.envelope.kind == KIND_KUBERNETES_PUBLICATION
    }
}

/// Every unit document of the project, by name. Duplicate names across
/// kinds fail ([`Code::UnitNameDuplicate`](nyl_core::orchestration::codes::Code::UnitNameDuplicate)).
///
/// Discovery reads the Git-visible YAML files of the inventory, the files
/// control resources are discovered in.
pub fn discover_units(inventory: &GitOpsInventory) -> Result<BTreeMap<UnitName, DiscoveredUnit>> {
    let mut units: BTreeMap<UnitName, DiscoveredUnit> = BTreeMap::new();
    for relative_path in &inventory.yaml_files {
        let bytes = std::fs::read(inventory.project_root.join(relative_path))?;
        let contents = String::from_utf8_lossy(&bytes);
        for (index, document) in split_yaml_documents(&contents).into_iter().enumerate() {
            let Some(api_version) = top_level_value(document, "apiVersion") else {
                continue;
            };
            if !UnitEnvelope::is_unit_api_version(api_version) {
                continue;
            }
            let location = format!("{} document {}", relative_path.display(), index + 1);
            let envelope = scan_unit_envelope(document, &location)?;
            let name = envelope
                .name()
                .map_err(|error| unit_invalid(&location, &error.to_string()))?;
            let discovered = DiscoveredUnit {
                source_path: relative_path.clone(),
                document_index: index + 1,
                raw_document: document.to_owned(),
                envelope,
            };
            if let Some(previous) = units.get(&name) {
                return Err(NylError::config(format!(
                    "{}: unit {name} is declared twice, as {} in {} and as {} in {}; unit names are unique across \
                     kinds, so rename one of them",
                    Code::UnitNameDuplicate,
                    previous.envelope.kind,
                    previous.location(),
                    discovered.envelope.kind,
                    discovered.location(),
                )));
            }
            units.insert(name, discovered);
        }
    }
    Ok(units)
}

fn unit_invalid(location: &str, message: &str) -> NylError {
    NylError::config(format!("{}: unit in {location}: {message}", Code::UnitInvalid))
}

const LITERAL_LABELS: &str = "metadata.labels must be literal, because environments select units before rendering";

/// The literal envelope of a unit document: parsed as YAML when the document
/// is YAML before rendering, otherwise read by the static block-style
/// scanner. A templated `apiVersion`, `kind`, name, or label is an error.
fn scan_unit_envelope(document: &str, location: &str) -> Result<UnitEnvelope> {
    let parsed = crate::yaml::parse_yaml_documents_k8s_compatible(document)
        .ok()
        .filter(|documents| documents.len() == 1)
        .and_then(|mut documents| documents.pop());
    let envelope =
        if let Some(value) = parsed {
            let envelope = serde_json::json!({
                "apiVersion": value.get("apiVersion"),
                "kind": value.get("kind"),
                "metadata": value.get("metadata"),
            });
            serde_json::from_value::<UnitEnvelope>(envelope).map_err(|error| {
                unit_invalid(
                    location,
                    &format!("the envelope (apiVersion, kind, metadata.name, metadata.labels) is invalid: {error}"),
                )
            })?
        } else {
            let (api_version, kind, name) = scan_static_envelope_fields(document);
            let labels = scan_static_metadata_labels(document).map_err(|_| unit_invalid(location, LITERAL_LABELS))?;
            match (api_version, kind, name) {
                (Some(api_version), Some(kind), Some(name)) => UnitEnvelope {
                    api_version,
                    kind,
                    metadata: UnitMetadata { name, labels },
                },
                _ => return Err(unit_invalid(
                    location,
                    "it has no literal envelope; a document that is not YAML before rendering must write apiVersion, \
                     kind, and metadata.name in block style, one literal key per line",
                )),
            }
        };
    if has_template_syntax(&envelope.api_version) || has_template_syntax(&envelope.kind) {
        return Err(unit_invalid(location, "apiVersion and kind must be literal"));
    }
    if has_template_syntax(&envelope.metadata.name) {
        return Err(unit_invalid(
            location,
            "metadata.name must be literal, because it is the unit's identity",
        ));
    }
    if envelope
        .metadata
        .labels
        .iter()
        .any(|(key, value)| has_template_syntax(key) || has_template_syntax(value))
    {
        return Err(unit_invalid(location, LITERAL_LABELS));
    }
    Ok(envelope)
}

/// Renders an environment's units from a pinned worktree of its source
/// commit: the [`SourceRenderer`] of orchestration.
#[derive(Debug, Clone)]
pub struct UnitSourceRenderer {
    /// Worktree-rooted directory of the entry `nyl.toml`, tried first, then
    /// `[project] previous_paths`, as `locate_checkout_project` does.
    entry_project_dir: String,
    /// `[project] previous_paths` of the entry `nyl.toml`.
    previous_paths: Vec<String>,
}

impl UnitSourceRenderer {
    pub fn new(entry_project_dir: impl Into<String>) -> Self {
        Self {
            entry_project_dir: entry_project_dir.into(),
            previous_paths: Vec::new(),
        }
    }

    /// Also look for the project at the entry `nyl.toml`'s
    /// `[project] previous_paths`, for a source commit from before a move.
    #[must_use]
    pub fn with_previous_paths(mut self, previous_paths: Vec<String>) -> Self {
        self.previous_paths = previous_paths;
        self
    }

    fn render_source(
        &self,
        worktree: &Path,
        environment: &EnvironmentName,
        commit: &CommitId,
    ) -> Result<RenderedSource> {
        let same = self.entry_project_dir.trim_start_matches('/');
        let located =
            crate::util::project_path::locate_checkout_project(worktree, Path::new(same), &[], &self.previous_paths)?;
        let inventory = super::discover_gitops_inventory(&located.directory, None)?;
        let effective = resolve_effective_environment(&inventory, environment)?;
        let units = discover_units(&inventory)?;
        let mut rendered = render_environment_units(&inventory, &units, &effective)?;
        let units = units
            .into_iter()
            .map(|(name, unit)| {
                let (selected, spec) = match rendered.remove(&name) {
                    Some(spec) => (true, spec),
                    None => (false, None),
                };
                let rendered = RenderedUnit {
                    source_path: worktree_rooted(&inventory, &unit.source_path),
                    envelope: unit.envelope,
                    selected,
                    spec,
                };
                (name, rendered)
            })
            .collect();
        Ok(RenderedSource {
            source_commit: commit.clone(),
            project_dir: worktree_rooted(&inventory, Path::new("")),
            environment: effective,
            units,
        })
    }
}

impl SourceRenderer for UnitSourceRenderer {
    fn render(
        &self,
        worktree: &Path,
        environment: &EnvironmentName,
        commit: &CommitId,
    ) -> SourceResult<RenderedSource> {
        self.render_source(worktree, environment, commit).map_err(|error| {
            SourceError(format!(
                "Rendering the units of Environment {environment} at {commit} failed: {error}"
            ))
        })
    }
}

/// A project-relative path in its worktree-rooted form, such as `/nyl/units.yaml`.
fn worktree_rooted(inventory: &GitOpsInventory, project_relative: &Path) -> String {
    let absolute = inventory.project_root.join(project_relative);
    let relative = absolute
        .strip_prefix(&inventory.worktree_root)
        .expect("discovery keeps the project inside its worktree");
    let relative = relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/");
    format!("/{relative}")
}

/// Declared Environments and EnvironmentGroups of the inventory.
fn declared(inventory: &GitOpsInventory) -> (Vec<&Environment>, Vec<EnvironmentGroup>) {
    let mut environments = Vec::new();
    let mut groups = Vec::new();
    for discovered in inventory.resources.values() {
        match &discovered.resource {
            Some(GitOpsResource::Environment(environment)) => environments.push(environment.as_ref()),
            Some(GitOpsResource::EnvironmentGroup(group)) => groups.push(group.clone()),
            _ => {}
        }
    }
    (environments, groups)
}

/// The Environment `name` with its group defaults applied, through the one
/// group rule of nyl-core.
fn resolve_effective_environment(inventory: &GitOpsInventory, name: &EnvironmentName) -> Result<Environment> {
    let (environments, groups) = declared(inventory);
    let environment = environments
        .into_iter()
        .find(|environment| environment.metadata.name == name.as_str())
        .ok_or_else(|| {
            NylError::config(format!(
                "{}: Environment {name} is not declared in this project",
                Code::EnvironmentUnknown
            ))
        })?;
    effective_environment(environment, &groups).map_err(|finding| NylError::config(finding.to_string()))
}

/// Render the units `environment` selects, by name; a document that renders
/// to nothing maps to `None`. Unselected units are absent.
fn render_environment_units(
    inventory: &GitOpsInventory,
    units: &BTreeMap<UnitName, DiscoveredUnit>,
    environment: &Environment,
) -> Result<BTreeMap<UnitName, Option<Value>>> {
    let context = unit_template_context(environment);
    let selector = &environment.spec.unit_selector.match_labels;
    let mut rendered = BTreeMap::new();
    for (name, unit) in units {
        let selected = selector
            .iter()
            .all(|(key, value)| unit.envelope.metadata.labels.get(key) == Some(value));
        if !selected {
            continue;
        }
        let mut spec = render_unit(unit, &context)?;
        if let Some(spec) = &mut spec {
            if unit.is_publication() {
                expand_publication_releases(inventory, unit, spec)?;
            }
        }
        rendered.insert(name.clone(), spec);
    }
    Ok(rendered)
}

/// The only context unit templates see: the effective `values` and the
/// sanitized Environment (`name`, `labels`, `values`). Never secrets, state
/// locations, or the process environment.
fn unit_template_context(environment: &Environment) -> Value {
    let values = Value::Object(environment.spec.values.clone().into_iter().collect::<Map<_, _>>());
    serde_json::json!({
        "values": values,
        "environment": {
            "name": environment.metadata.name,
            "labels": environment.metadata.labels,
            "values": values,
        },
    })
}

/// Render one unit document. The rendered envelope must equal the literal
/// one, and only `spec` may follow it.
fn render_unit(unit: &DiscoveredUnit, context: &Value) -> Result<Option<Value>> {
    let location = unit.location();
    let rendered = TemplateEngine::new()
        .render_named(&unit.source_path.display().to_string(), &unit.raw_document, context)
        .map_err(|error| unit_invalid(&location, &format!("rendering failed: {error}")))?;
    let mut documents = SourceContext::new(unit.source_path.clone())
        .parse_yaml_documents(&rendered)
        .map_err(|error| unit_invalid(&location, &format!("the rendered document is not YAML: {error}")))?;
    if documents.is_empty() {
        return Ok(None);
    }
    if documents.len() != 1 {
        return Err(unit_invalid(
            &location,
            "it renders more than one document; one document declares one unit",
        ));
    }
    let Value::Object(mut document) = documents.remove(0) else {
        return Err(unit_invalid(&location, "it does not render to a mapping"));
    };
    let spec = match document.remove("spec") {
        None | Some(Value::Null) => Value::Object(Map::new()),
        Some(spec @ Value::Object(_)) => spec,
        Some(_) => return Err(unit_invalid(&location, "spec must render to a mapping")),
    };
    if let Some(key) = document
        .keys()
        .find(|key| !matches!(key.as_str(), "apiVersion" | "kind" | "metadata"))
    {
        return Err(unit_invalid(
            &location,
            &format!("unknown top-level field {key:?}; a unit has apiVersion, kind, metadata, and spec"),
        ));
    }
    let envelope = serde_json::from_value::<UnitEnvelope>(Value::Object(document))
        .map_err(|error| unit_invalid(&location, &format!("the rendered envelope is invalid: {error}")))?;
    if envelope != unit.envelope {
        return Err(unit_invalid(
            &location,
            "rendering changed its envelope; apiVersion, kind, metadata.name, and metadata.labels must stay literal",
        ));
    }
    Ok(Some(spec))
}

/// Fill a publication unit's `releases` with the target's bindings that only
/// orchestration resolves, as reference objects. Other binding kinds stay
/// with rendering (M3 status of the publication unit contract).
fn expand_publication_releases(inventory: &GitOpsInventory, unit: &DiscoveredUnit, spec: &mut Value) -> Result<()> {
    let location = unit.location();
    let target = publication_target(unit, spec)?;
    let spec = spec.as_object_mut().expect("rendered specs are mappings");
    if spec.contains_key("releases") {
        return Err(unit_invalid(
            &location,
            "spec.releases is filled from the target's releaseInputs and must not be written by hand",
        ));
    }
    let deployment_target = static_target(inventory, &target).map_err(|reason| {
        unit_invalid(
            &location,
            &format!("spec.target names DeploymentTarget {target:?}, which {reason}"),
        )
    })?;
    let mut releases = Map::new();
    for (release, bindings) in &deployment_target.spec.release_inputs {
        let mut inputs = Map::new();
        for (input, binding) in bindings {
            if binding_requires_orchestration(binding) {
                inputs.insert(input.clone(), serde_json::to_value(binding)?);
            }
        }
        if !inputs.is_empty() {
            releases.insert(release.clone(), Value::Object(inputs));
        }
    }
    spec.insert("releases".to_owned(), Value::Object(releases));
    Ok(())
}

/// The static DeploymentTarget `name`, or why there is none.
fn static_target<'a>(
    inventory: &'a GitOpsInventory,
    name: &str,
) -> std::result::Result<&'a crate::resources::DeploymentTarget, &'static str> {
    let discovered = inventory
        .get(GitOpsResourceKind::DeploymentTarget, name)
        .ok_or("does not exist")?;
    match &discovered.resource {
        Some(GitOpsResource::DeploymentTarget(target)) => Ok(target),
        _ => Err("is not static"),
    }
}

/// The static DeploymentTarget name of a rendered publication spec.
fn publication_target(unit: &DiscoveredUnit, spec: &Value) -> Result<String> {
    let location = unit.location();
    match spec.get("target") {
        Some(Value::String(name)) if !name.is_empty() => Ok(name.clone()),
        Some(Value::Object(target)) if target.contains_key("inline") => Err(unit_invalid(
            &location,
            "spec.target.inline is not supported yet; name a static DeploymentTarget",
        )),
        _ => Err(unit_invalid(
            &location,
            "spec.target must name a static DeploymentTarget",
        )),
    }
}

fn binding_requires_orchestration(binding: &InputBinding) -> bool {
    binding
        .kind("binding")
        .is_ok_and(nyl_core::bindings::BindingKind::requires_orchestration)
}

fn spec_enabled(spec: &Value) -> bool {
    spec.get("enabled") != Some(&Value::Bool(false))
}

/// Which environment places each DeploymentTarget, by rendering every
/// Environment's selected `KubernetesPublication` units' `spec.target`.
/// Targets no publication unit names are absent.
///
/// A publication unit places its target whenever it renders to a spec, even
/// with `enabled: false`, because a unit being deleted still owns its target
/// until its teardown completes.
pub fn target_placements(inventory: &GitOpsInventory) -> Result<BTreeMap<String, EnvironmentName>> {
    let (environments, _) = declared(inventory);
    if environments.is_empty() {
        return Ok(BTreeMap::new());
    }
    let units = discover_units(inventory)?;
    let mut placements: BTreeMap<String, (EnvironmentName, String)> = BTreeMap::new();
    for environment in environments {
        let name = environment.name()?;
        let effective = resolve_effective_environment(inventory, &name)?;
        let rendered = render_environment_units(inventory, &units, &effective)?;
        for (unit_name, spec) in &rendered {
            let unit = &units[unit_name];
            let Some(spec) = spec else { continue };
            if !unit.is_publication() {
                continue;
            }
            let target = publication_target(unit, spec)?;
            let placed_by = format!("unit {unit_name} of Environment {name}");
            if let Some((other, other_by)) = placements.get(&target) {
                if *other != name {
                    return Err(NylError::config(format!(
                        "{}: DeploymentTarget {target} is placed by {other_by} and by {placed_by}; a target belongs \
                         to at most one environment, so select its publication unit in only one of them",
                        Code::TargetEnvironmentConflict
                    )));
                }
                continue;
            }
            placements.insert(target, (name.clone(), placed_by));
        }
    }
    Ok(placements
        .into_iter()
        .map(|(target, (environment, _))| (target, environment))
        .collect())
}

/// Check the `fromUnit` bindings of a target placed into `environment`: each
/// names a selected, enabled unit of that environment and, for `output`, an
/// output it declares and does not mark sensitive. Problems are listed
/// together, each naming the binding.
///
/// An `artifact` reference is checked only for the unit's existence; the
/// artifacts a kind publishes are checked when the kind's driver resolves it.
pub fn check_unit_bindings(
    inventory: &GitOpsInventory,
    target: &str,
    environment: &EnvironmentName,
) -> Result<Vec<String>> {
    let deployment_target = static_target(inventory, target)
        .map_err(|reason| NylError::config(format!("DeploymentTarget {target:?} {reason}")))?;
    let effective = resolve_effective_environment(inventory, environment)?;
    let units = discover_units(inventory)?;
    let rendered = render_environment_units(inventory, &units, &effective)?;

    let mut issues = Vec::new();
    for (release, bindings) in &deployment_target.spec.release_inputs {
        for (input, binding) in bindings {
            let Some(source) = &binding.from_unit else { continue };
            let field = format!("spec.releaseInputs.{release:?}.{input}.fromUnit");
            if let Some(other) = source
                .environment
                .as_deref()
                .filter(|other| *other != environment.as_str())
            {
                issues.push(format!(
                    "{field} names Environment {other}, but the target belongs to Environment {environment}; \
                     references to another environment's units are not supported yet"
                ));
                continue;
            }
            let unit = &source.unit;
            let declared = UnitName::new(unit).ok().filter(|name| units.contains_key(name));
            let Some(name) = declared else {
                issues.push(format!("{field} names unit {unit}, which is not declared"));
                continue;
            };
            let Some(spec) = rendered.get(&name) else {
                issues.push(format!(
                    "{field} names unit {unit}, which Environment {environment} does not select"
                ));
                continue;
            };
            let Some(spec) = spec.as_ref().filter(|spec| spec_enabled(spec)) else {
                issues.push(format!(
                    "{field} names unit {unit}, which is not enabled in Environment {environment}"
                ));
                continue;
            };
            if let Some(output) = &source.output {
                match spec.get("outputs").and_then(|outputs| outputs.get(output)) {
                    None => issues.push(format!(
                        "{field} names output {output} of unit {unit}, which the unit does not declare"
                    )),
                    Some(declaration) if declaration.get("sensitive") == Some(&Value::Bool(true)) => {
                        issues.push(format!(
                            "{field} names output {output} of unit {unit}, which is sensitive and cannot be referenced"
                        ));
                    }
                    Some(_) => {}
                }
            }
        }
    }
    Ok(issues)
}

/// Placeholder overrides for [`super::inputs::OrchestratedInputs::Placeholders`]:
/// a value of each declared type for every binding orchestration resolves.
/// A declaration with `enum` takes its first allowed value.
pub(crate) fn placeholder_overrides(
    declarations: &BTreeMap<String, InputDeclaration>,
    bindings: Option<&BTreeMap<String, InputBinding>>,
) -> BTreeMap<String, Value> {
    let Some(bindings) = bindings else {
        return BTreeMap::new();
    };
    bindings
        .iter()
        .filter(|(_, binding)| binding_requires_orchestration(binding))
        .filter_map(|(input, _)| {
            let declaration = declarations.get(input)?;
            let value = declaration
                .allowed
                .as_ref()
                .and_then(|allowed| allowed.first().cloned())
                .unwrap_or_else(|| placeholder(declaration.input_type));
            Some((input.clone(), value))
        })
        .collect()
}

fn placeholder(input_type: InputType) -> Value {
    match input_type {
        InputType::String => Value::String("<from orchestration>".to_owned()),
        InputType::Integer | InputType::Number => Value::from(0),
        InputType::Boolean => Value::Bool(false),
        InputType::Object => Value::Object(Map::new()),
        InputType::Array => Value::Array(Vec::new()),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::json;
    use tempfile::TempDir;

    use super::*;

    const PLATFORM: &str = r"apiVersion: k8s.nyl/v1
kind: Cluster
metadata:
  name: dev
spec:
  destination:
    server: https://kubernetes.default.svc
  kubernetes:
    kubeVersion: 1.31.4
    apiVersions: [v1, apps/v1]
---
apiVersion: k8s.gitops.nyl/v1
kind: DeploymentTarget
metadata:
  name: dev
spec:
  clusterRef: {name: dev}
  publication:
    repository: {repoURL: 'https://example.invalid/deploy.git'}
    revision: deploy
    pathPrefix: dev
  releaseInputs:
    web/web:
      image: {fromUnit: {unit: web-image, artifact: image, pointer: /reference}}
      databaseHost: {fromUnit: {unit: database, output: host}}
      replicas: {value: 2}
---
apiVersion: gitops.nyl/v1
kind: Environment
metadata:
  name: dev
  labels: {stage: dev}
spec:
  unitSelector:
    matchLabels: {dev: 'true'}
  values:
    target: dev
    region: eu
";

    const UNITS: &str = r"apiVersion: units.gitops.nyl/v1
kind: OpenTofu
metadata:
  name: database
  labels: {dev: 'true'}
spec:
  source: {path: ../infra/database}
  outputs:
    host: {type: string}
    password: {type: string, sensitive: true}
---
apiVersion: units.gitops.nyl/v1
kind: KubernetesPublication
metadata:
  name: kubernetes
  labels: {dev: 'true'}
spec:
  target: '{{ values.target }}'
  mode: publish
";

    const IMAGE: &str = r#"{% set registry = "registry.invalid" %}
apiVersion: units.gitops.nyl/v1
kind: OciImage
metadata:
  name: web-image
  labels:
    dev: 'true'
spec:
  repository: {{ registry }}/web
  region: {{ values.region }}
"#;

    /// A Git worktree with the project in `nyl/` and `files` beneath it.
    fn project(files: &[(&str, &str)]) -> TempDir {
        let worktree = TempDir::new().unwrap();
        git2::Repository::init(worktree.path()).unwrap();
        let project = worktree.path().join("nyl");
        fs::create_dir_all(&project).unwrap();
        fs::write(project.join("nyl.toml"), "[project]\n").unwrap();
        for (path, contents) in files {
            let path = project.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, contents).unwrap();
        }
        worktree
    }

    fn inventory(worktree: &TempDir) -> GitOpsInventory {
        super::super::discover_gitops_inventory(&worktree.path().join("nyl"), None).unwrap()
    }

    fn render(worktree: &TempDir, environment: &str) -> SourceResult<RenderedSource> {
        UnitSourceRenderer::new("/nyl").render(
            worktree.path(),
            &EnvironmentName::new(environment).unwrap(),
            &CommitId::new("a".repeat(40)).unwrap(),
        )
    }

    fn unit<'a>(source: &'a RenderedSource, name: &str) -> &'a RenderedUnit {
        &source.units[&UnitName::new(name).unwrap()]
    }

    fn command_unit(name: &str, labels: &str, spec: &str) -> String {
        format!(
            "apiVersion: units.gitops.nyl/v1\nkind: Command\nmetadata:\n  name: {name}\n  labels: {labels}\nspec:\n{spec}"
        )
    }

    #[test]
    fn test_discover_units_reads_every_file_with_literal_and_block_envelopes() {
        let worktree = project(&[
            ("platform.yaml", PLATFORM),
            ("units.yaml", UNITS),
            ("images/web.yaml", IMAGE),
        ]);
        let units = discover_units(&inventory(&worktree)).unwrap();
        assert_eq!(
            units.keys().map(UnitName::as_str).collect::<Vec<_>>(),
            ["database", "kubernetes", "web-image"]
        );
        let kubernetes = &units[&UnitName::new("kubernetes").unwrap()];
        assert_eq!(kubernetes.source_path, Path::new("units.yaml"));
        assert_eq!(kubernetes.document_index, 2);
        assert_eq!(kubernetes.envelope.kind, "KubernetesPublication");
        let image = &units[&UnitName::new("web-image").unwrap()];
        assert_eq!(image.source_path, Path::new("images/web.yaml"));
        assert_eq!(image.envelope.metadata.labels["dev"], "true");
    }

    #[test]
    fn test_discover_units_rejects_duplicate_names_across_kinds() {
        let duplicate = command_unit("database", "{}", "  command: [./run.sh]\n");
        let worktree = project(&[("units.yaml", UNITS), ("more/seed.yaml", &duplicate)]);
        let error = discover_units(&inventory(&worktree)).unwrap_err().to_string();
        assert!(error.contains("NYL-UNIT-NAME-DUPLICATE:"), "{error}");
        assert!(error.contains("more/seed.yaml document 1"), "{error}");
        assert!(error.contains("units.yaml document 1"), "{error}");
    }

    #[test]
    fn test_discover_units_rejects_templated_labels_and_names() {
        let flow = command_unit("seed", "{tier: '{{ values.tier }}'}", "  command: [./run.sh]\n");
        let block = "{% if true %}\napiVersion: units.gitops.nyl/v1\nkind: Command\nmetadata:\n  name: seed\n  labels:\n    tier: '{{ values.tier }}'\nspec: {}\n{% endif %}\n";
        let name = "{% if true %}\napiVersion: units.gitops.nyl/v1\nkind: Command\nmetadata:\n  name: '{{ values.name }}'\nspec: {}\n{% endif %}\n";
        for (document, expected) in [
            (flow.as_str(), "metadata.labels must be literal"),
            (block, "metadata.labels must be literal"),
            (name, "no literal envelope"),
        ] {
            let worktree = project(&[("units.yaml", document)]);
            let error = discover_units(&inventory(&worktree)).unwrap_err().to_string();
            assert!(error.contains("NYL-UNIT-INVALID"), "{error}");
            assert!(error.contains(expected), "{error}");
            assert!(error.contains("units.yaml document 1"), "{error}");
        }
    }

    #[test]
    fn test_render_selects_units_by_literal_labels() {
        // An unselected unit never renders, so its template may need values
        // this environment lacks.
        let other = command_unit("seed", "{prod: 'true'}", "  command: ['{{ values.missing.field }}']\n");
        let worktree = project(&[
            ("platform.yaml", PLATFORM),
            ("units.yaml", UNITS),
            ("seed.yaml", &other),
        ]);
        let source = render(&worktree, "dev").unwrap();
        assert!(unit(&source, "database").selected);
        let seed = unit(&source, "seed");
        assert!(!seed.selected);
        assert_eq!(seed.spec, None);
        assert_eq!(source.project_dir, "/nyl");
        assert_eq!(unit(&source, "database").source_path, "/nyl/units.yaml");
        assert_eq!(source.source_commit.as_str(), "a".repeat(40));
    }

    #[test]
    fn test_render_selects_every_unit_with_an_empty_selector() {
        let platform = PLATFORM.replace("    matchLabels: {dev: 'true'}\n", "    matchLabels: {}\n");
        let other = command_unit("seed", "{}", "  command: [./run.sh]\n");
        let worktree = project(&[("platform.yaml", &platform), ("seed.yaml", &other)]);
        let source = render(&worktree, "dev").unwrap();
        assert!(unit(&source, "seed").selected);
    }

    #[test]
    fn test_render_templates_see_values_and_environment_but_not_process_env() {
        let probe = command_unit(
            "probe",
            "{dev: 'true'}",
            "  values:\n    region: '{{ values.region }}'\n    environment: '{{ environment.name }}/{{ environment.labels.stage }}/{{ environment.values.target }}'\n    processEnv: '{{ env is defined }}'\n    secrets: '{{ secrets is defined }}'\n    state: '{{ environment.spec is defined }}'\n",
        );
        let worktree = project(&[("platform.yaml", PLATFORM), ("probe.yaml", &probe)]);
        let source = render(&worktree, "dev").unwrap();
        assert_eq!(
            unit(&source, "probe").spec.as_ref().unwrap()["values"],
            json!({
                "region": "eu",
                "environment": "dev/dev/dev",
                "processEnv": "false",
                "secrets": "false",
                "state": "false"
            })
        );
    }

    #[test]
    fn test_render_document_that_renders_to_nothing_has_no_spec() {
        let empty = "{% if values.region == 'us' %}\napiVersion: units.gitops.nyl/v1\nkind: Command\nmetadata:\n  name: seed\n  labels:\n    dev: 'true'\nspec:\n  command: [./run.sh]\n{% endif %}\n";
        let worktree = project(&[
            ("platform.yaml", PLATFORM),
            ("seed.yaml", empty),
            ("images/web.yaml", IMAGE),
        ]);
        let source = render(&worktree, "dev").unwrap();
        let seed = unit(&source, "seed");
        assert!(seed.selected);
        assert_eq!(seed.spec, None);
        assert_eq!(
            unit(&source, "web-image").spec,
            Some(json!({"repository": "registry.invalid/web", "region": "eu"}))
        );
    }

    #[test]
    fn test_render_rejects_a_changed_envelope() {
        let changed = "apiVersion: units.gitops.nyl/v1\nkind: Command\nmetadata:\n  name: seed\n  labels:\n    dev: 'true'\n{% if true %}    extra: x\n{% endif %}\nspec: {}\n";
        let worktree = project(&[("platform.yaml", PLATFORM), ("seed.yaml", changed)]);
        let error = render(&worktree, "dev").unwrap_err().0;
        assert!(error.contains("rendering changed its envelope"), "{error}");
        assert!(error.contains("seed.yaml document 1"), "{error}");
    }

    #[test]
    fn test_render_merges_group_values_beneath_the_environment() {
        let group = "apiVersion: gitops.nyl/v1\nkind: EnvironmentGroup\nmetadata:\n  name: stages\nspec:\n  environmentSelector:\n    matchLabels: {stage: dev}\n  values:\n    region: us\n    tier: shared\n";
        let probe = command_unit(
            "probe",
            "{dev: 'true'}",
            "  values: {region: '{{ values.region }}', tier: '{{ values.tier }}'}\n",
        );
        let worktree = project(&[
            ("platform.yaml", PLATFORM),
            ("group.yaml", group),
            ("probe.yaml", &probe),
        ]);
        let source = render(&worktree, "dev").unwrap();
        assert_eq!(
            unit(&source, "probe").spec.as_ref().unwrap()["values"],
            json!({"region": "eu", "tier": "shared"})
        );
        assert_eq!(source.environment.spec.values["tier"], json!("shared"));
    }

    #[test]
    fn test_render_publication_releases_hold_orchestrated_bindings() {
        let worktree = project(&[("platform.yaml", PLATFORM), ("units.yaml", UNITS)]);
        let source = render(&worktree, "dev").unwrap();
        assert_eq!(
            unit(&source, "kubernetes").spec,
            Some(json!({
                "target": "dev",
                "mode": "publish",
                "releases": {
                    "web/web": {
                        "image": {"fromUnit": {"unit": "web-image", "artifact": "image", "pointer": "/reference"}},
                        "databaseHost": {"fromUnit": {"unit": "database", "output": "host"}}
                    }
                }
            }))
        );
    }

    #[test]
    fn test_render_rejects_authored_releases_and_inline_targets() {
        for (spec, expected) in [
            (
                "  target: dev\n  releases: {}\n",
                "spec.releases is filled from the target's releaseInputs",
            ),
            ("  target:\n    inline: {}\n", "spec.target.inline is not supported yet"),
            (
                "  target: missing\n",
                "DeploymentTarget \"missing\", which does not exist",
            ),
        ] {
            let publication = format!(
                "apiVersion: units.gitops.nyl/v1\nkind: KubernetesPublication\nmetadata:\n  name: kubernetes\n  labels: {{dev: 'true'}}\nspec:\n{spec}"
            );
            let worktree = project(&[("platform.yaml", PLATFORM), ("publication.yaml", &publication)]);
            let error = render(&worktree, "dev").unwrap_err().0;
            assert!(error.contains(expected), "{error}");
            assert!(error.contains("publication.yaml document 1"), "{error}");
        }
    }

    #[test]
    fn test_target_placements_map_targets_to_environments_and_reject_conflicts() {
        let worktree = project(&[("platform.yaml", PLATFORM), ("units.yaml", UNITS)]);
        let placements = target_placements(&inventory(&worktree)).unwrap();
        assert_eq!(
            placements,
            BTreeMap::from([("dev".to_owned(), EnvironmentName::new("dev").unwrap())])
        );

        let staging = "apiVersion: gitops.nyl/v1\nkind: Environment\nmetadata:\n  name: staging\nspec:\n  unitSelector:\n    matchLabels: {dev: 'true'}\n  values: {target: dev}\n";
        let worktree = project(&[
            ("platform.yaml", PLATFORM),
            ("units.yaml", UNITS),
            ("staging.yaml", staging),
        ]);
        let error = target_placements(&inventory(&worktree)).unwrap_err().to_string();
        assert!(error.contains("NYL-TARGET-ENVIRONMENT-CONFLICT:"), "{error}");
        assert!(error.contains("Environment dev"), "{error}");
        assert!(error.contains("Environment staging"), "{error}");
    }

    #[test]
    fn test_check_unit_bindings_names_each_invalid_binding() {
        let environment = EnvironmentName::new("dev").unwrap();
        let worktree = project(&[
            ("platform.yaml", PLATFORM),
            ("units.yaml", UNITS),
            ("images/web.yaml", IMAGE),
        ]);
        assert_eq!(
            check_unit_bindings(&inventory(&worktree), "dev", &environment).unwrap(),
            Vec::<String>::new()
        );

        let platform = PLATFORM.replace("output: host", "output: password");
        let worktree = project(&[("platform.yaml", &platform), ("units.yaml", UNITS)]);
        let issues = check_unit_bindings(&inventory(&worktree), "dev", &environment).unwrap();
        assert_eq!(
            issues,
            [
                "spec.releaseInputs.\"web/web\".databaseHost.fromUnit names output password of unit database, which \
                 is sensitive and cannot be referenced",
                "spec.releaseInputs.\"web/web\".image.fromUnit names unit web-image, which is not declared",
            ]
        );

        let platform = PLATFORM.replace("output: host", "output: port");
        let image = IMAGE.replace("    dev: 'true'", "    prod: 'true'");
        let worktree = project(&[
            ("platform.yaml", &platform),
            ("units.yaml", UNITS),
            ("images/web.yaml", &image),
        ]);
        let issues = check_unit_bindings(&inventory(&worktree), "dev", &environment).unwrap();
        assert_eq!(
            issues,
            [
                "spec.releaseInputs.\"web/web\".databaseHost.fromUnit names output port of unit database, which the \
                 unit does not declare",
                "spec.releaseInputs.\"web/web\".image.fromUnit names unit web-image, which Environment dev does not \
                 select",
            ]
        );
    }

    #[test]
    fn test_placeholder_overrides_follow_declared_types() {
        let declarations: BTreeMap<String, InputDeclaration> = serde_json::from_value(json!({
            "image": {"type": "string"},
            "replicas": {"type": "integer"},
            "flags": {"type": "array"},
            "size": {"type": "string", "enum": ["small", "large"]},
            "plain": {"type": "string"}
        }))
        .unwrap();
        let bindings: BTreeMap<String, InputBinding> = serde_json::from_value(json!({
            "image": {"fromUnit": {"unit": "web-image", "artifact": "image"}},
            "replicas": {"fromPromotion": {"value": "replicas"}},
            "flags": {"fromUnit": {"unit": "flags", "output": "list"}},
            "size": {"fromUnit": {"unit": "sizing", "output": "size"}},
            "plain": {"value": "kept"}
        }))
        .unwrap();
        assert_eq!(
            placeholder_overrides(&declarations, Some(&bindings)),
            BTreeMap::from([
                ("flags".to_owned(), json!([])),
                ("image".to_owned(), json!("<from orchestration>")),
                ("replicas".to_owned(), json!(0)),
                ("size".to_owned(), json!("small")),
            ])
        );
        assert!(placeholder_overrides(&declarations, None).is_empty());
    }
}
