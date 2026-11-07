//! Cleanup permission analysis over two target-owned rendered snapshots.
//!
//! Argo CD filters cascading deletion through `AppProject.IsResourcePermitted`:
//! <https://github.com/argoproj/argo-cd/blob/master/controller/appcontroller.go>.
//! This transition check has no live deletion evidence; operators must verify
//! completion before releasing retained scope.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde_json::{json, Value};

use super::tree::ScopeResolver;
use super::{DesiredFiles, CATALOG_DIRECTORY};
use crate::kubernetes::prune::{delete_decision, prune_decision, PruneDecision, SYNC_OPTIONS_ANNOTATION};
use crate::resources::{ClusterKubernetesCapabilities, ResourceScope};
use crate::{NylError, Result};

use nyl_core::constants::legacy::ANNOTATION_PUBLICATION_CONTEXT as LEGACY_CONTEXT_ANNOTATION;
pub(crate) use nyl_core::constants::ANNOTATION_PUBLICATION_CONTEXT as CONTEXT_ANNOTATION;

pub(crate) fn annotate(manifest: &mut Value, context: &Value) -> Result<()> {
    if manifest["metadata"].get("annotations").is_none() {
        manifest["metadata"]["annotations"] = json!({});
    }
    manifest["metadata"]["annotations"][CONTEXT_ANNOTATION] = serde_json::to_string(context)?.into();
    Ok(())
}

/// Explicit escape hatch for a reviewed cleanup-scope contraction.
#[derive(clap::Args, Debug, Default)]
pub struct ProjectScopeArgs {
    /// Ignore AppProject cleanup permission contractions (can orphan resources).
    #[arg(long)]
    pub ignore_project_scope_contraction: bool,
}

/// One contraction, aggregated across Applications sharing the same cleanup scope.
#[derive(Debug)]
pub struct ProjectScopeContraction {
    /// Argo CD namespace/name identities requiring cleanup.
    pub applications: BTreeSet<String>,
    /// Owning group, from recorded catalog provenance where available.
    pub application_group: String,
    /// Source configuration location, or an explicit unavailable marker.
    pub source: String,
    /// AppProject namespace/name identity.
    pub project: String,
    /// Target cluster and its recorded Argo CD destination.
    pub cluster: String,
    /// Workload namespace, the name of a managed Namespace, or empty for
    /// other cluster-scoped resources.
    pub namespace: String,
    /// Effective permissions that cleanup loses.
    pub permissions: BTreeSet<String>,
    /// Additive retention and any further project permissions required.
    pub remedies: BTreeSet<String>,
}

/// Fail by default, or explicitly warn when the operator ignores the findings.
pub fn check_project_scope(
    previous: &DesiredFiles<'_>,
    proposed: &DesiredFiles<'_>,
    previous_prefix: &str,
    proposed_prefix: &str,
    capabilities: Option<&ClusterKubernetesCapabilities>,
    ignore: bool,
) -> Result<()> {
    let findings = project_scope_contractions(previous, proposed, previous_prefix, proposed_prefix, capabilities)?;
    if findings.is_empty() {
        return Ok(());
    }
    let mut message = String::from("[NYL_PROJECT_SCOPE_CONTRACTION] AppProject cleanup permissions would contract:\n");
    for finding in findings {
        use std::fmt::Write;
        writeln!(
            message,
            "- Application(s) {}; ApplicationGroup {:?} ({}); AppProject {}; target cluster {}; {}: {}. {}",
            finding.applications.into_iter().collect::<Vec<_>>().join(", "),
            finding.application_group,
            finding.source,
            finding.project,
            finding.cluster,
            if finding.namespace.is_empty() {
                "cluster-scoped resources".to_owned()
            } else {
                format!("namespace {:?}", finding.namespace)
            },
            finding.permissions.into_iter().collect::<Vec<_>>().join(", "),
            finding.remedies.into_iter().collect::<Vec<_>>().join(" ")
        )
        .unwrap();
    }
    message.push_str("Argo CD skips resources outside the project's permitted scope during cascading deletion, potentially leaving them orphaned. Publish retained permissions before manually pruning the retiring Application from the catalog, including required deletion confirmations. Verify live cascading deletion has completed before removing retention entries: this check only compares publications and cannot establish live deletion completion. Use --ignore-project-scope-contraction only after reviewing this risk.");
    if ignore {
        tracing::warn!("{message}");
        Ok(())
    } else {
        Err(NylError::config(message))
    }
}

struct Snapshot<'a> {
    files: &'a DesiredFiles<'a>,
    applications: BTreeMap<String, Value>,
    projects: BTreeMap<String, Value>,
}

fn field<'a>(value: &'a Value, pointer: &str) -> &'a str {
    value.pointer(pointer).and_then(Value::as_str).unwrap_or_default()
}

fn identity(value: &Value) -> String {
    format!(
        "{}/{}",
        field(value, "/metadata/namespace"),
        field(value, "/metadata/name")
    )
}

fn context(value: &Value) -> Result<Value> {
    // Baselines published before the `.nyl` annotation carry the legacy key.
    let annotations = &value["metadata"]["annotations"];
    let annotation = annotations[CONTEXT_ANNOTATION]
        .as_str()
        .or_else(|| annotations[LEGACY_CONTEXT_ANNOTATION].as_str());
    match annotation {
        Some(text) => {
            let invalid = |detail: &str| {
                NylError::config(format!("Invalid publication context on {}: {detail}", identity(value)))
            };
            let context: Value = serde_json::from_str(text).map_err(|error| invalid(&error.to_string()))?;
            if !context.is_object() {
                return Err(invalid("expected an object"));
            }
            for key in ["group", "source", "projectNamespace", "cluster"] {
                if context.get(key).is_some_and(|value| !value.is_string()) {
                    return Err(invalid(key));
                }
            }
            for key in ["generated", "namespaceCreate"] {
                if context.get(key).is_some_and(|value| !value.is_boolean()) {
                    return Err(invalid(key));
                }
            }
            if let Some(destination) = context.get("destination") {
                let destination: crate::resources::ClusterDestination =
                    serde_json::from_value(destination.clone()).map_err(|error| invalid(&error.to_string()))?;
                destination.validate().map_err(|error| invalid(&error.to_string()))?;
            }
            Ok(context)
        }
        None => Ok(Value::Null),
    }
}

/// Every resource a rendered file holds. Earlier publications wrote whole
/// Release streams to one file, so a baseline may hold several documents.
fn parse(path: &Path, bytes: &[u8]) -> Result<Vec<Value>> {
    let text = std::str::from_utf8(bytes)
        .map_err(|error| NylError::config(format!("Invalid rendered manifest {}: {error}", path.display())))?;
    let documents = crate::yaml::parse_yaml_documents_k8s_compatible(text)
        .map_err(|error| NylError::config(format!("Invalid rendered manifest {}: {error}", path.display())))?;
    for document in &documents {
        crate::kubernetes::ResourceKey::from_json_value(document)
            .map_err(|error| NylError::config(format!("Invalid rendered resource {}: {error}", path.display())))?;
    }
    Ok(documents)
}

fn validate_catalog_manifest(manifest: &Value) -> Result<()> {
    let invalid = |detail: &str| NylError::config(format!("Invalid catalog {}: {detail}", identity(manifest)));
    for pointer in ["/metadata/name", "/metadata/namespace"] {
        if field(manifest, pointer).is_empty() {
            return Err(invalid(pointer));
        }
    }
    context(manifest)?;
    match field(manifest, "/kind") {
        "Application" => {
            if let Some(finalizers) = manifest["metadata"].get("finalizers").filter(|value| !value.is_null()) {
                if !finalizers
                    .as_array()
                    .is_some_and(|values| values.iter().all(Value::is_string))
                {
                    return Err(invalid("metadata.finalizers must be a list of strings"));
                }
            }
            for pointer in ["/spec/project", "/spec/source/path", "/spec/destination/namespace"] {
                if field(manifest, pointer).is_empty() {
                    return Err(invalid(pointer));
                }
            }
            if field(manifest, "/spec/destination/server").is_empty()
                && field(manifest, "/spec/destination/name").is_empty()
            {
                return Err(invalid("spec.destination requires a cluster server or name"));
            }
        }
        "AppProject" => {
            if !manifest["spec"].is_object() {
                return Err(invalid("spec must be an object"));
            }
            if let Some(destinations) = manifest["spec"].get("destinations").filter(|value| !value.is_null()) {
                let destinations: Vec<crate::resources::AppProjectDestination> =
                    serde_json::from_value(destinations.clone())
                        .map_err(|_| invalid("spec.destinations must contain destination patterns"))?;
                for destination in destinations {
                    for pattern in destination
                        .server
                        .iter()
                        .chain(destination.name.iter())
                        .chain(std::iter::once(&destination.namespace))
                    {
                        glob(pattern.trim_start_matches('!'), "")?;
                    }
                }
            }
            for key in [
                "clusterResourceWhitelist",
                "clusterResourceBlacklist",
                "namespaceResourceWhitelist",
                "namespaceResourceBlacklist",
            ] {
                if let Some(patterns) = manifest["spec"].get(key).filter(|v| !v.is_null()) {
                    let patterns: Vec<crate::resources::AppProjectResourcePattern> =
                        serde_json::from_value(patterns.clone())
                            .map_err(|_| invalid(&format!("spec.{key} must contain resource patterns")))?;
                    for pattern in patterns {
                        for value in std::iter::once(&pattern.group)
                            .chain(std::iter::once(&pattern.kind))
                            .chain(pattern.name.iter())
                        {
                            glob(value, "")?;
                        }
                    }
                }
            }
        }
        _ => {}
    }
    Ok(())
}

impl<'a> Snapshot<'a> {
    fn new(files: &'a DesiredFiles<'a>) -> Result<Self> {
        let mut snapshot = Self {
            files,
            applications: BTreeMap::new(),
            projects: BTreeMap::new(),
        };
        for (path, bytes) in files.iter().filter(|(path, _)| path.starts_with(CATALOG_DIRECTORY)) {
            for manifest in parse(path, bytes)? {
                if field(&manifest, "/apiVersion") != "argoproj.io/v1alpha1" {
                    continue;
                }
                validate_catalog_manifest(&manifest)?;
                let collection = match field(&manifest, "/kind") {
                    "Application" => &mut snapshot.applications,
                    "AppProject" => &mut snapshot.projects,
                    _ => continue,
                };
                if collection.insert(identity(&manifest), manifest).is_some() {
                    return Err(NylError::config(format!(
                        "Duplicate catalog identity in {}",
                        path.display()
                    )));
                }
            }
        }
        Ok(snapshot)
    }

    fn project(&self, app: &Value) -> Result<Option<&Value>> {
        let name = field(app, "/spec/project");
        let ctx = context(app)?;
        let project = if let Some(namespace) = ctx["projectNamespace"].as_str() {
            self.projects.get(&format!("{namespace}/{name}"))
        } else {
            // Without context the catalog must identify the project unambiguously.
            let mut matches = self
                .projects
                .values()
                .filter(|project| field(project, "/metadata/name") == name);
            let project = matches.next();
            if matches.next().is_some() {
                return Err(NylError::config(format!("Cannot identify AppProject namespace for Application {}; baseline has multiple projects named {name:?}. Re-render the baseline with publication context.", identity(app))));
            }
            project
        };
        if project.is_none() && ctx["generated"] == true {
            return Err(NylError::config(format!("Invalid rendered catalog: Application {} references missing generated AppProject {name:?}; restore the project's catalog manifest and ownership record before comparing or publishing", identity(app))));
        }
        Ok(project)
    }

    /// Resources leaving `app`, each with the namespace Argo CD manages it in.
    /// Resources Argo CD intentionally leaves in place need no cleanup permission:
    /// `Delete=false` when the Application goes, `Prune=false` when it stays.
    fn retiring_resources(
        &self,
        app: &Value,
        prefix: &str,
        next: &Snapshot<'_>,
        next_prefix: &str,
        scopes: &Scopes<'_>,
    ) -> Result<Vec<Value>> {
        let new_app = next.applications.get(&identity(app));
        let same_cluster = new_app.is_some_and(|new| {
            new["spec"]["destination"]["server"] == app["spec"]["destination"]["server"]
                && new["spec"]["destination"]["name"] == app["spec"]["destination"]["name"]
        });
        let remaining = if same_cluster {
            next.resources(new_app.unwrap(), next_prefix, scopes)?
        } else {
            BTreeMap::new()
        };
        let mut resources = self.resources(app, prefix, scopes)?;
        // An empty Application still has declared destination scope.
        if resources.is_empty() && !same_cluster {
            resources.insert(String::new(), json!({"apiVersion":"v1", "kind":"", "metadata":{"namespace": app["spec"]["destination"]["namespace"]}}));
        }
        // A surviving Application on the same cluster prunes; otherwise Argo CD deletes.
        let retained = |resource: &Value| {
            let annotations = resource["metadata"]["annotations"][SYNC_OPTIONS_ANNOTATION]
                .as_str()
                .map(|options| BTreeMap::from([(SYNC_OPTIONS_ANNOTATION.to_owned(), options.to_owned())]));
            let decision = if same_cluster { prune_decision } else { delete_decision };
            decision(annotations.as_ref(), true) == PruneDecision::Retain
        };
        Ok(resources
            .into_iter()
            .filter(|(key, resource)| !remaining.contains_key(key) && !retained(resource))
            .map(|(_, resource)| resource)
            .collect())
    }

    /// Rendered resources of `app` by identity in their effective namespace.
    /// A namespaced resource without `metadata.namespace` lives in the
    /// Application's destination namespace; one of unknown scope is compared
    /// under both readings, so neither can hide a contraction.
    fn resources(&self, app: &Value, prefix: &str, scopes: &Scopes<'_>) -> Result<BTreeMap<String, Value>> {
        let source = field(app, "/spec/source/path");
        nyl_core::resources::validate_relative_path("catalog Application spec.source.path", source, false, false)?;
        let root = Path::new(source).strip_prefix(prefix).map_err(|_| {
            NylError::config(format!(
                "Application {} source path {source:?} is outside target prefix {prefix:?}",
                identity(app)
            ))
        })?;
        let mut resources = BTreeMap::new();
        for (path, bytes) in self.files.iter().filter(|(path, _)| path.starts_with(root)) {
            for resource in parse(path, bytes)? {
                if field(&resource, "/metadata/namespace").is_empty() && !is_namespace(&resource) {
                    let scope = scopes.scope(api_group(&resource), field(&resource, "/kind"));
                    if scope != Some(ResourceScope::Namespaced) {
                        resources.insert(resource_identity(&resource), resource.clone());
                    }
                    if scope != Some(ResourceScope::Cluster) {
                        let mut namespaced = resource;
                        namespaced["metadata"]["namespace"] = app["spec"]["destination"]["namespace"].clone();
                        resources.insert(resource_identity(&namespaced), namespaced);
                    }
                    continue;
                }
                resources.insert(resource_identity(&resource), resource);
            }
        }
        Ok(resources)
    }
}

fn is_namespace(resource: &Value) -> bool {
    field(resource, "/apiVersion") == "v1" && field(resource, "/kind") == "Namespace"
}

/// Resource scope for the comparison: CRDs either snapshot renders, then the
/// target Cluster's recorded capabilities, then built-in Kubernetes kinds.
struct Scopes<'a> {
    resolver: ScopeResolver<'a>,
}

impl<'a> Scopes<'a> {
    fn new(snapshots: [&DesiredFiles<'_>; 2], capabilities: Option<&'a ClusterKubernetesCapabilities>) -> Self {
        const CRD_MARKER: &[u8] = b"CustomResourceDefinition";
        // Only CRDs carry scope; skip parsing every other manifest.
        let crds = snapshots
            .into_iter()
            .flat_map(|files| files.iter())
            .filter(|(_, bytes)| bytes.windows(CRD_MARKER.len()).any(|window| window == CRD_MARKER))
            // An unreadable file leaves its kinds of unknown scope, which are
            // checked both ways, so skipping it cannot hide a contraction.
            .filter_map(|(path, bytes)| parse(path, bytes).ok())
            .flatten()
            .filter(|resource| field(resource, "/kind") == "CustomResourceDefinition")
            .collect::<Vec<_>>();
        Self {
            resolver: ScopeResolver::with_capabilities(capabilities, crds.iter()),
        }
    }

    fn scope(&self, group: &str, kind: &str) -> Option<ResourceScope> {
        self.resolver
            .scope(group, kind)
            .or_else(|| crate::kubernetes::builtin_scope(group, kind))
    }
}

/// The API group of a resource's `apiVersion`, `""` for the core group.
fn api_group(resource: &Value) -> &str {
    field(resource, "/apiVersion")
        .split_once('/')
        .map_or("", |(group, _)| group)
}

fn resource_identity(resource: &Value) -> String {
    format!(
        "{}/{}/{}",
        api_group(resource),
        field(resource, "/kind"),
        identity(resource)
    )
}

/// Compare effective cleanup permission for resources leaving each Application.
/// Only catalog directory sources establish ownership; workload labels never do.
/// Missing optional publication context is supported when catalog manifests are unambiguous.
pub fn project_scope_contractions(
    previous: &DesiredFiles<'_>,
    proposed: &DesiredFiles<'_>,
    previous_prefix: &str,
    proposed_prefix: &str,
    capabilities: Option<&ClusterKubernetesCapabilities>,
) -> Result<Vec<ProjectScopeContraction>> {
    let scopes = Scopes::new([previous, proposed], capabilities);
    let previous = Snapshot::new(previous)?;
    let proposed = Snapshot::new(proposed)?;
    let mut findings: BTreeMap<(String, String, String, String), ProjectScopeContraction> = BTreeMap::new();
    for (id, app) in &previous.applications {
        // The catalog's own source contains control resources, not workload ownership.
        if Path::new(field(app, "/spec/source/path")) == Path::new(previous_prefix).join(CATALOG_DIRECTORY) {
            continue;
        }
        if !cascading(app) {
            continue;
        }
        let Some(old_project) = previous.project(app)? else {
            // Independently managed external scope is unobservable.
            continue;
        };
        let ctx = context(app)?;
        let new_app = proposed.applications.get(id);
        if new_app.is_some_and(|app| !cascading(app)) {
            continue;
        }
        let new_project = match new_app {
            Some(app) => proposed.project(app)?,
            None => proposed.projects.get(&identity(old_project)),
        };
        let project_id = identity(new_project.unwrap_or(old_project));
        let project_ctx = context(old_project)?;
        let new_context = context(new_project.unwrap_or(old_project))?;
        let group = ctx["group"]
            .as_str()
            .or(project_ctx["group"].as_str())
            .or(new_context["group"].as_str())
            .unwrap_or(field(app, "/spec/project"));
        let source = ctx["source"]
            .as_str()
            .or(project_ctx["source"].as_str())
            .or(new_context["source"].as_str())
            .unwrap_or("source location unavailable in baseline");
        let cluster = ctx["cluster"]
            .as_str()
            .or(project_ctx["cluster"].as_str())
            .unwrap_or("baseline target");
        let destination = ctx
            .get("destination")
            .filter(|v| !v.is_null())
            .unwrap_or(&app["spec"]["destination"]);
        let destination_label = serde_json::to_string(destination)?;
        for resource in previous.retiring_resources(app, previous_prefix, &proposed, proposed_prefix, &scopes)? {
            // Empty for cluster-scoped kinds other than Namespace, which Argo CD
            // permits by cluster resource lists alone.
            let namespace = cleanup_namespace(&resource);
            let before = denied_permissions(old_project, &resource, destination)?;
            if !before.is_empty() {
                continue;
            }
            let after = new_project.map_or_else(
                || Ok(BTreeSet::from(["AppProject is absent".to_owned()])),
                |p| denied_permissions(p, &resource, destination),
            )?;
            if after.is_empty() {
                continue;
            }
            let remedy = retention_remedy(new_project, &new_context, &resource, destination, group, source)?;
            let entry = findings
                .entry((
                    project_id.clone(),
                    group.to_owned(),
                    destination_label.clone(),
                    namespace.to_owned(),
                ))
                .or_insert_with(|| ProjectScopeContraction {
                    applications: BTreeSet::new(),
                    application_group: group.to_owned(),
                    source: source.to_owned(),
                    project: project_id.clone(),
                    cluster: format!("{cluster} {destination_label}"),
                    namespace: namespace.to_owned(),
                    permissions: BTreeSet::new(),
                    remedies: BTreeSet::new(),
                });
            entry.remedies.insert(remedy);
            entry.applications.insert(id.clone());
            entry.permissions.extend(after);
        }
    }
    Ok(findings.into_values().collect())
}

fn cascading(app: &Value) -> bool {
    app["metadata"]["finalizers"].as_array().is_some_and(|finalizers| {
        finalizers.iter().any(|value| {
            matches!(
                value.as_str(),
                Some("resources-finalizer.argocd.argoproj.io" | "resources-finalizer.argocd.argoproj.io/background")
            )
        })
    })
}

fn cleanup_namespace(resource: &Value) -> &str {
    if is_namespace(resource) {
        field(resource, "/metadata/name")
    } else {
        field(resource, "/metadata/namespace")
    }
}

/// The additive retention settings that restore `resource`'s cleanup permission
/// in a group-owned generated project, and whatever they cannot restore.
fn retention_remedy(
    project: Option<&Value>,
    context: &Value,
    resource: &Value,
    destination: &Value,
    group: &str,
    source: &str,
) -> Result<String> {
    let Some(project) = project.filter(|_| context["generated"] == true) else {
        return Ok("Retain the AppProject and its listed destination/resource permissions in the owning project declaration; restore the owning ApplicationGroup if needed. The retained* ApplicationGroup settings apply only to group-owned generated projects.".to_owned());
    };
    let group = context["group"].as_str().unwrap_or(group);
    let source = context["source"].as_str().unwrap_or(source);
    let namespace = cleanup_namespace(resource);
    let mut retained = project.clone();
    let mut steps = Vec::new();
    // Namespace retention restores the destination and, with namespace
    // creation, the generated Namespace permission.
    let namespace_create = context["namespaceCreate"] == true;
    let retain_namespace = if is_namespace(resource) {
        namespace_create && !denied_permissions(project, resource, destination)?.is_empty()
    } else {
        !namespace.is_empty() && !destination_permitted(project, destination, namespace)?
    };
    if retain_namespace {
        let mut destinations = list(project, "destinations").to_vec();
        let mut retained_destination = context["destination"].clone();
        retained_destination["namespace"] = namespace.into();
        destinations.push(retained_destination);
        retained["spec"]["destinations"] = destinations.into();
        if is_namespace(resource) {
            push_pattern(
                &mut retained,
                "clusterResourceWhitelist",
                json!({"group":"", "kind":"Namespace", "name":namespace}),
            );
        }
        steps.push(format!("add {namespace:?} to spec.retainedProjectNamespaces"));
    }
    // Kind retention restores a whitelist entry; it cannot lift a blacklist.
    let kind = field(resource, "/kind");
    let cluster_scoped = field(resource, "/metadata/namespace").is_empty();
    let (whitelist, setting) = if cluster_scoped {
        ("clusterResourceWhitelist", "retainedClusterResourceWhitelist")
    } else {
        ("namespaceResourceWhitelist", "retainedNamespaceResourceWhitelist")
    };
    let denied = denied_permissions(&retained, resource, destination)?;
    if !kind.is_empty() && !denied.is_empty() && retained["spec"][whitelist].is_array() {
        let mut pattern = json!({"group": api_group(resource), "kind": kind});
        if cluster_scoped {
            pattern["name"] = field(resource, "/metadata/name").into();
        }
        let mut candidate = retained.clone();
        push_pattern(&mut candidate, whitelist, pattern.clone());
        if denied_permissions(&candidate, resource, destination)?.len() < denied.len() {
            retained = candidate;
            steps.push(format!("add {pattern} to spec.{setting}"));
        }
    }
    let remaining = denied_permissions(&retained, resource, destination)?;
    let extra = if remaining.is_empty() {
        String::new()
    } else {
        format!(" Retention settings cannot restore: {}. Lift the deny rule or keep the original cluster destination in projectTemplate until cleanup completes.", remaining.into_iter().collect::<Vec<_>>().join(", "))
    };
    if steps.is_empty() {
        return Ok(format!("On ApplicationGroup {group:?} ({source}):{extra}"));
    }
    Ok(format!(
        "On ApplicationGroup {group:?} ({source}), {} (append to any existing entries).{extra}",
        steps.join(" and ")
    ))
}

fn push_pattern(project: &mut Value, list_name: &str, pattern: Value) {
    let mut patterns = list(project, list_name).to_vec();
    patterns.push(pattern);
    project["spec"][list_name] = patterns.into();
}

fn list<'a>(project: &'a Value, name: &str) -> &'a [Value] {
    project["spec"][name].as_array().map_or(&[], Vec::as_slice)
}

fn denied_permissions(project: &Value, resource: &Value, destination: &Value) -> Result<BTreeSet<String>> {
    let mut denied = BTreeSet::new();
    let namespace = field(resource, "/metadata/namespace");
    let kind = field(resource, "/kind");
    let group = api_group(resource);
    if !kind.is_empty() {
        let (allow, deny) = if namespace.is_empty() {
            ("clusterResourceWhitelist", "clusterResourceBlacklist")
        } else {
            ("namespaceResourceWhitelist", "namespaceResourceBlacklist")
        };
        let matches = |pattern: &Value| -> Result<bool> {
            let name_matches = if namespace.is_empty() {
                match pattern.get("name").and_then(Value::as_str) {
                    Some(pattern) => glob(pattern, field(resource, "/metadata/name"))?,
                    None => true,
                }
            } else {
                true
            };
            Ok(glob(field(pattern, "/group"), group)? && glob(field(pattern, "/kind"), kind)? && name_matches)
        };
        let permitted = if !namespace.is_empty() && project["spec"].get(allow).is_none_or(Value::is_null) {
            true
        } else {
            list(project, allow)
                .iter()
                .map(matches)
                .collect::<Result<Vec<_>>>()?
                .contains(&true)
        };
        if !permitted
            || list(project, deny)
                .iter()
                .map(matches)
                .collect::<Result<Vec<_>>>()?
                .contains(&true)
        {
            let resource = if namespace.is_empty() {
                format!("{group}/{kind} {:?}", field(resource, "/metadata/name"))
            } else {
                format!("{group}/{kind}")
            };
            denied.insert(format!("{allow}/{deny} no longer permits {resource}"));
        }
    }
    if !namespace.is_empty() && !destination_permitted(project, destination, namespace)? {
        denied.insert("destination namespace permission revoked".to_owned());
    }
    Ok(denied)
}

fn destination_permitted(project: &Value, destination: &Value, namespace: &str) -> Result<bool> {
    let mut permitted = false;
    for pattern in list(project, "destinations") {
        let server = field(destination, "/server");
        let name = field(destination, "/name");
        let ps = field(pattern, "/server");
        let pn = field(pattern, "/name");
        let ns = field(pattern, "/namespace");
        // A universal pattern admits the cluster even when only its other identifier is declared.
        let cluster_matches = |pattern: &str, value: &str| -> Result<bool> {
            if value.is_empty() {
                Ok(!pattern.is_empty() && !pattern.starts_with('!') && glob(pattern, "")?)
            } else {
                match_negated(pattern, value)
            }
        };
        let server_match = cluster_matches(ps, server)?;
        let name_match = cluster_matches(pn, name)?;
        let namespace_match = match_negated(ns, namespace)?;
        if (server_match || name_match) && namespace_match {
            permitted = true;
        } else if ((!name_match && pn.starts_with('!')) || (!server_match && ps.starts_with('!'))) && namespace_match
            || !namespace_match && ns.starts_with('!') && server_match
        {
            return Ok(false);
        }
    }
    Ok(permitted)
}

fn match_negated(pattern: &str, value: &str) -> Result<bool> {
    match pattern.strip_prefix('!') {
        Some(pattern) => Ok(!glob(pattern, value)?),
        None => glob(pattern, value),
    }
}

fn glob(pattern: &str, value: &str) -> Result<bool> {
    // Argo CD globs treat '/' as an ordinary character and support brace alternatives.
    if let Some(start) = pattern.find('{') {
        let mut depth = 1;
        let mut ends = Vec::new();
        let mut closing = None;
        for (offset, character) in pattern[start + 1..].char_indices() {
            let index = start + 1 + offset;
            match character {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        closing = Some(index);
                        ends.push(index);
                        break;
                    }
                }
                ',' if depth == 1 => ends.push(index),
                _ => {}
            }
        }
        let closing = closing.ok_or_else(|| NylError::config(format!("Invalid AppProject glob {pattern:?}")))?;
        let mut alternative_start = start + 1;
        let mut matched = false;
        for end in ends {
            matched |= glob(
                &format!(
                    "{}{}{}",
                    &pattern[..start],
                    &pattern[alternative_start..end],
                    &pattern[closing + 1..]
                ),
                value,
            )?;
            alternative_start = end + 1;
        }
        return Ok(matched);
    }
    glob::Pattern::new(pattern)
        .map(|pattern| pattern.matches(value))
        .map_err(|error| NylError::config(format!("Invalid AppProject glob {pattern:?}: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn project(namespaces: &[&str]) -> Value {
        let mut project = json!({"apiVersion":"argoproj.io/v1alpha1", "kind":"AppProject",
            "metadata":{"name":"workloads","namespace":"control"},
            "spec":{"destinations": namespaces.iter().map(|ns| json!({"server":"https://cluster", "namespace":ns})).collect::<Vec<_>>(),
                "namespaceResourceWhitelist":[{"group":"", "kind":"ConfigMap"}],
                "clusterResourceWhitelist":[{"group":"", "kind":"Namespace", "name":"retired"}]}});
        annotate(&mut project, &json!({"group":"workloads", "source":"config/group.yaml", "projectNamespace":"control",
            "cluster":"production", "destination":{"server":"https://cluster"}, "generated":true,"namespaceCreate":true})).unwrap();
        project
    }

    fn app(name: &str) -> Value {
        let mut app = json!({"apiVersion":"argoproj.io/v1alpha1", "kind":"Application",
            "metadata":{"name":name,"namespace":"apps", "finalizers":["resources-finalizer.argocd.argoproj.io"]},
            "spec":{"project":"workloads", "destination":{"server":"https://cluster", "namespace":"retired"},
                "source":{"repoURL":"https://repo", "path":"production/workloads/api", "targetRevision":"deploy"}}});
        annotate(
            &mut app,
            &json!({"group":"workloads", "source":"config/group.yaml", "projectNamespace":"control",
            "cluster":"production", "destination":{"server":"https://cluster"}}),
        )
        .unwrap();
        app
    }

    fn resource(namespace: &str) -> Value {
        json!({"apiVersion":"v1", "kind":"ConfigMap", "metadata":{"name":"example", "namespace":namespace}})
    }

    fn tree(project: Value, app: Option<Value>, resources: &[Value]) -> BTreeMap<PathBuf, Vec<u8>> {
        let mut files = BTreeMap::new();
        files.insert(
            PathBuf::from("_nyl/catalog/project.yaml"),
            serde_json::to_vec(&project).unwrap(),
        );
        if let Some(app) = app {
            files.insert(
                PathBuf::from("_nyl/catalog/app.yaml"),
                serde_json::to_vec(&app).unwrap(),
            );
            for (i, resource) in resources.iter().enumerate() {
                files.insert(
                    PathBuf::from(format!("workloads/api/{i}.yaml")),
                    serde_json::to_vec(resource).unwrap(),
                );
            }
        }
        files
    }

    fn borrowed(files: &BTreeMap<PathBuf, Vec<u8>>) -> DesiredFiles<'_> {
        files
            .iter()
            .map(|(path, bytes)| (path.as_path(), bytes.as_slice()))
            .collect()
    }

    fn findings(old: &BTreeMap<PathBuf, Vec<u8>>, new: &BTreeMap<PathBuf, Vec<u8>>) -> Vec<ProjectScopeContraction> {
        project_scope_contractions(&borrowed(old), &borrowed(new), "production", "production", None).unwrap()
    }

    #[test]
    fn test_removal_detects_contraction_and_additive_remedy() {
        let old = tree(project(&["retired"]), Some(app("example")), &[resource("retired")]);
        let new = tree(project(&["active"]), None, &[]);
        let found = findings(&old, &new);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].applications, BTreeSet::from(["apps/example".to_owned()]));
        let error = check_project_scope(
            &borrowed(&old),
            &borrowed(&new),
            "production",
            "production",
            None,
            false,
        )
        .unwrap_err()
        .to_string();
        for expected in [
            "NYL_PROJECT_SCOPE_CONTRACTION",
            "workloads",
            "control/workloads",
            "production",
            "retired",
            "config/group.yaml",
            "append to any existing entries",
            "skips resources",
            "orphaned",
            "Verify live cascading deletion",
        ] {
            assert!(error.contains(expected), "{expected}: {error}");
        }
        check_project_scope(&borrowed(&old), &borrowed(&new), "production", "production", None, true).unwrap();
        let retained = tree(project(&["active", "retired"]), None, &[]);
        assert!(findings(&old, &retained).is_empty());
    }

    #[test]
    fn test_baseline_with_legacy_context_annotation_names_the_group() {
        let mut old = tree(project(&["retired"]), Some(app("example")), &[resource("retired")]);
        for bytes in old.values_mut() {
            let text = String::from_utf8(std::mem::take(bytes)).unwrap();
            *bytes = text.replace(CONTEXT_ANNOTATION, LEGACY_CONTEXT_ANNOTATION).into_bytes();
        }
        let new = tree(project(&["active"]), None, &[]);
        let found = findings(&old, &new);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].applications, BTreeSet::from(["apps/example".to_owned()]));
        assert_eq!(found[0].application_group, "workloads");
        assert_eq!(found[0].source, "config/group.yaml");
    }

    #[test]
    fn test_cluster_scoped_retirement_needs_cluster_resource_permission() {
        let cluster_role =
            json!({"apiVersion":"rbac.authorization.k8s.io/v1", "kind":"ClusterRole", "metadata":{"name":"reader"}});
        let mut wide = project(&["retired"]);
        wide["spec"]["clusterResourceWhitelist"] = json!([{"group":"rbac.authorization.k8s.io", "kind":"ClusterRole"}]);
        let old = tree(wide.clone(), Some(app("example")), &[cluster_role]);
        // The surviving project keeps the namespace but derives no ClusterRole permission.
        let found = findings(&old, &tree(project(&["retired"]), None, &[]));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].namespace, "");
        let error = check_project_scope(
            &borrowed(&old),
            &borrowed(&tree(project(&["retired"]), None, &[])),
            "production",
            "production",
            None,
            false,
        )
        .unwrap_err()
        .to_string();
        for expected in [
            "cluster-scoped resources",
            "rbac.authorization.k8s.io/ClusterRole \"reader\"",
            r#"add {"group":"rbac.authorization.k8s.io","kind":"ClusterRole","name":"reader"} to spec.retainedClusterResourceWhitelist"#,
        ] {
            assert!(error.contains(expected), "{expected}: {error}");
        }
        assert!(findings(&old, &tree(wide, None, &[])).is_empty());
    }

    #[test]
    fn test_equivalent_permissions_and_unchanged_scope_are_safe() {
        let old = tree(project(&["retired"]), Some(app("example")), &[resource("retired")]);
        for ns in ["retired", "*", "ret*", "{active,retired}", "{active,{old,retired}}"] {
            assert!(findings(&old, &tree(project(&[ns]), None, &[])).is_empty(), "{ns}");
        }
        let mut project_scoped = project(&["retired"]);
        project_scoped["spec"]["permitOnlyProjectScopedClusters"] = true.into();
        assert!(findings(&old, &tree(project_scoped, None, &[])).is_empty());
        let mut other_cluster = project(&["retired"]);
        other_cluster["spec"]["destinations"]
            .as_array_mut()
            .unwrap()
            .push(json!({"server":"https://elsewhere", "namespace":"active"}));
        assert!(findings(&old, &tree(other_cluster, None, &[])).is_empty());
    }

    #[test]
    fn test_universal_cluster_permissions_match_named_and_server_destinations() {
        let old = tree(project(&["retired"]), Some(app("example")), &[resource("retired")]);
        let mut by_name = project(&["*"]);
        by_name["spec"]["destinations"] = json!([{"name":"*", "namespace":"*"}]);
        assert!(findings(&old, &tree(by_name, None, &[])).is_empty());
        let mut named_app = app("example");
        named_app["spec"]["destination"] = json!({"name":"production", "namespace":"retired"});
        let mut ctx = context(&named_app).unwrap();
        ctx["destination"] = json!({"name":"production"});
        annotate(&mut named_app, &ctx).unwrap();
        let mut named_project = project(&["retired"]);
        named_project["spec"]["destinations"] = json!([{"name":"production", "namespace":"retired"}]);
        let old = tree(named_project, Some(named_app), &[resource("retired")]);
        let mut by_server = project(&["*"]);
        by_server["spec"]["destinations"] = json!([{"server":"*", "namespace":"*"}]);
        assert!(findings(&old, &tree(by_server, None, &[])).is_empty());
    }

    #[test]
    fn test_rename_destination_change_and_secondary_namespace_retirement() {
        let old = tree(
            project(&["retired", "secondary"]),
            Some(app("example")),
            &[resource("retired"), resource("secondary")],
        );
        let renamed = tree(project(&["active"]), Some(app("replacement")), &[resource("retired")]);
        assert_eq!(findings(&old, &renamed).len(), 2);
        let secondary_removed = tree(project(&["retired"]), Some(app("example")), &[resource("retired")]);
        assert_eq!(findings(&old, &secondary_removed)[0].namespace, "secondary");
        let mut moved = app("example");
        moved["spec"]["destination"]["server"] = "https://new-cluster".into();
        let mut moved_project = project(&["retired", "secondary"]);
        for destination in moved_project["spec"]["destinations"].as_array_mut().unwrap() {
            destination["server"] = "https://new-cluster".into();
        }
        let new = tree(
            moved_project,
            Some(moved),
            &[resource("retired"), resource("secondary")],
        );
        assert_eq!(findings(&old, &new).len(), 2);
    }

    #[test]
    fn test_surviving_application_uses_its_current_project_for_retiring_resources() {
        let old = tree(project(&["retired"]), Some(app("example")), &[resource("retired")]);
        let mut reassigned = app("example");
        reassigned["spec"]["project"] = "replacement-project".into();
        let mut equivalent = project(&["retired"]);
        equivalent["metadata"]["name"] = "replacement-project".into();
        assert!(findings(&old, &tree(equivalent, Some(reassigned.clone()), &[])).is_empty());
        let mut narrowed = project(&["active"]);
        narrowed["metadata"]["name"] = "replacement-project".into();
        let found = findings(&old, &tree(narrowed, Some(reassigned), &[]));
        assert_eq!(found[0].project, "control/replacement-project");
    }

    #[test]
    fn test_orphan_deletion_and_baseline_boundary() {
        let mut orphan = app("example");
        orphan["metadata"]["finalizers"] = json!([]);
        let old = tree(project(&["retired"]), Some(orphan), &[resource("retired")]);
        let new = tree(project(&["active"]), None, &[]);
        assert!(findings(&old, &new).is_empty());
        assert!(findings(&BTreeMap::new(), &new).is_empty());
        // No Application in the previous publication means no evidence of live deletion completion.
        let retained = tree(project(&["retired"]), None, &[]);
        assert!(findings(&retained, &new).is_empty());
    }

    #[test]
    fn test_namespace_management_and_resource_denials_need_complete_remedies() {
        let namespace = json!({"apiVersion":"v1", "kind":"Namespace", "metadata":{"name":"retired"}});
        let old = tree(project(&["retired"]), Some(app("example")), &[namespace]);
        let mut narrowed = project(&["retired"]);
        narrowed["spec"]["clusterResourceWhitelist"] = json!([]);
        assert_eq!(findings(&old, &tree(narrowed.clone(), None, &[])).len(), 1);
        narrowed["spec"]["clusterResourceBlacklist"] = json!([{"group":"", "kind":"Namespace", "name":"retired"}]);
        let found = findings(&old, &tree(narrowed, None, &[]));
        assert!(found[0].remedies.iter().any(|r| r.contains("retainedProjectNamespaces")
            && r.contains("cannot restore")
            && r.contains("clusterResourceBlacklist")));
        let old = tree(project(&["retired"]), Some(app("example")), &[resource("retired")]);
        let mut denied = project(&["*"]);
        denied["spec"]["destinations"]
            .as_array_mut()
            .unwrap()
            .push(json!({"server":"https://cluster", "namespace":"!retired"}));
        denied["spec"]["namespaceResourceWhitelist"] = json!([]);
        let found = findings(&old, &tree(denied, None, &[]));
        assert!(found[0].remedies.iter().any(|r| r.contains("retainedProjectNamespaces")
            && r.contains(r#"add {"group":"","kind":"ConfigMap"} to spec.retainedNamespaceResourceWhitelist"#)
            && r.contains("cannot restore: destination namespace permission revoked")));
    }

    #[test]
    fn test_surviving_namespace_losing_a_kind_needs_only_the_kind_permission() {
        let old = tree(project(&["retired"]), Some(app("example")), &[resource("retired")]);
        let mut narrowed = project(&["retired"]);
        narrowed["spec"]["namespaceResourceWhitelist"] = json!([]);
        let found = findings(&old, &tree(narrowed, None, &[]));
        assert_eq!(found.len(), 1);
        assert!(found[0]
            .remedies
            .iter()
            .all(|r| !r.contains("retainedProjectNamespaces")
                && r.contains(r#"add {"group":"","kind":"ConfigMap"} to spec.retainedNamespaceResourceWhitelist"#)));
    }

    #[test]
    fn test_unnamespaced_resources_resolve_to_the_destination_namespace() {
        let config_map = json!({"apiVersion":"v1", "kind":"ConfigMap", "metadata":{"name":"example"}});
        let old = tree(
            project(&["retired"]),
            Some(app("example")),
            std::slice::from_ref(&config_map),
        );
        // Retirement: the implicit namespace is the Application's destination.
        let found = findings(&old, &tree(project(&["active"]), None, &[]));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].namespace, "retired");
        // A destination move retires the resource from its old namespace.
        let mut moved = app("example");
        moved["spec"]["destination"]["namespace"] = "active".into();
        let new = tree(project(&["active"]), Some(moved), &[config_map]);
        let found = findings(&old, &new);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].namespace, "retired");
    }

    #[test]
    fn test_unknown_scope_is_checked_under_both_readings() {
        let widget = json!({"apiVersion":"example.com/v1", "kind":"Widget", "metadata":{"name":"example"}});
        let mut wide = project(&["retired"]);
        wide["spec"]["clusterResourceWhitelist"] = json!([{"group":"example.com", "kind":"Widget"}]);
        let old = tree(wide, Some(app("example")), std::slice::from_ref(&widget));
        // Unknown scope: the cluster reading the old project permitted contracts.
        let found = findings(&old, &tree(project(&["retired"]), None, &[]));
        assert!(found.iter().any(|f| f.namespace.is_empty()), "{found:?}");
        // A CRD in either snapshot settles the scope as namespaced, which the
        // retained namespace and permissive namespaced whitelist still permit.
        let crd = json!({"apiVersion":"apiextensions.k8s.io/v1", "kind":"CustomResourceDefinition",
            "metadata":{"name":"widgets.example.com"},
            "spec":{"group":"example.com", "scope":"Namespaced", "names":{"kind":"Widget"}}});
        let mut retained = project(&["retired"]);
        retained["spec"]["namespaceResourceWhitelist"] = json!([{"group":"*", "kind":"*"}]);
        let mut new = tree(retained, None, &[]);
        new.insert(PathBuf::from("crds/widget.yaml"), serde_json::to_vec(&crd).unwrap());
        let found = findings(&old, &new);
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn test_multi_document_baseline_files_are_read_whole() {
        // Earlier publications wrote a Release's stream, CRDs included, to one file.
        let crd = json!({"apiVersion":"apiextensions.k8s.io/v1", "kind":"CustomResourceDefinition",
            "metadata":{"name":"widgets.example.com"},
            "spec":{"group":"example.com", "scope":"Namespaced", "names":{"kind":"Widget"}}});
        let widget = json!({"apiVersion":"example.com/v1", "kind":"Widget", "metadata":{"name":"example"}});
        let mut wide = project(&["retired"]);
        wide["spec"]["namespaceResourceWhitelist"] = json!([{"group":"*", "kind":"*"}]);
        wide["spec"]["clusterResourceWhitelist"] = json!([{"group":"*", "kind":"*"}]);
        let mut old = tree(wide.clone(), Some(app("example")), &[]);
        let stream = [&resource("retired"), &crd, &widget]
            .map(|document| crate::yaml::serialize_yaml_value(document).unwrap())
            .join("---\n");
        old.insert(PathBuf::from("workloads/api/resources.yaml"), stream.into_bytes());
        // The CRD in the stream makes Widget namespaced, so dropping cluster
        // permission for every other kind loses nothing.
        let mut retained = wide;
        retained["spec"]["clusterResourceWhitelist"] =
            json!([{"group":"apiextensions.k8s.io", "kind":"CustomResourceDefinition"}]);
        let found = findings(&old, &tree(retained.clone(), None, &[]));
        assert!(found.is_empty(), "{found:?}");
        // Every namespaced document of the stream still detects a contraction.
        retained["spec"]["namespaceResourceWhitelist"] = json!([]);
        let found = findings(&old, &tree(retained, None, &[]));
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].namespace, "retired");
        for kind in ["ConfigMap", "Widget"] {
            assert!(found[0].permissions.iter().any(|p| p.contains(kind)), "{found:?}");
        }
    }

    #[test]
    fn test_builtin_kinds_are_checked_only_in_their_own_scope() {
        // A permissive cluster whitelist narrowed to nothing must not flag a
        // namespaced built-in kind as a cluster-scoped contraction.
        let config_map = json!({"apiVersion":"v1", "kind":"ConfigMap", "metadata":{"name":"example"}});
        let mut wide = project(&["retired", "active"]);
        wide["spec"]["clusterResourceWhitelist"] = json!([{"group":"*", "kind":"*"}]);
        let old = tree(wide, Some(app("example")), &[config_map]);
        let mut narrowed = project(&["retired", "active"]);
        narrowed["spec"]["clusterResourceWhitelist"] = json!([]);
        let found = findings(&old, &tree(narrowed, None, &[]));
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn test_resources_argo_cd_retains_need_no_cleanup_permission() {
        let mut kept = resource("retired");
        kept["metadata"]["annotations"] = json!({SYNC_OPTIONS_ANNOTATION: "ServerSideApply=true, Delete=false"});
        let old = tree(project(&["retired"]), Some(app("example")), &[kept.clone()]);
        assert!(findings(&old, &tree(project(&["active"]), None, &[])).is_empty());
        // A surviving Application prunes, so only Prune=false keeps it.
        let narrowed = || {
            let mut narrowed = project(&["retired"]);
            narrowed["spec"]["namespaceResourceWhitelist"] = json!([]);
            narrowed
        };
        assert_eq!(findings(&old, &tree(narrowed(), Some(app("example")), &[])).len(), 1);
        kept["metadata"]["annotations"] = json!({SYNC_OPTIONS_ANNOTATION: "Prune=false"});
        let old = tree(project(&["retired"]), Some(app("example")), &[kept]);
        assert!(findings(&old, &tree(narrowed(), Some(app("example")), &[])).is_empty());
    }

    #[test]
    fn test_legacy_context_and_project_namespace_identity() {
        let mut old_app = app("example");
        old_app["metadata"].as_object_mut().unwrap().remove("annotations");
        let mut legacy_project = project(&["retired"]);
        legacy_project["metadata"]
            .as_object_mut()
            .unwrap()
            .remove("annotations");
        let old = tree(legacy_project, Some(old_app), &[resource("retired")]);
        assert_eq!(findings(&old, &tree(project(&["active"]), None, &[])).len(), 1);
        let mut other_namespace = project(&["retired"]);
        other_namespace["metadata"]["namespace"] = "other-control".into();
        assert_eq!(findings(&old, &tree(other_namespace, None, &[])).len(), 1);
        let mut unrelated = tree(project(&["active"]), None, &[]);
        unrelated.insert(PathBuf::from("unowned-not-a-manifest.txt"), b"arbitrary state".to_vec());
        assert_eq!(findings(&old, &unrelated).len(), 1);
    }

    #[test]
    fn test_background_cascade_and_current_orphan_policy() {
        let mut background = app("example");
        background["metadata"]["finalizers"] = json!(["resources-finalizer.argocd.argoproj.io/background"]);
        let old = tree(project(&["retired"]), Some(background), &[resource("retired")]);
        assert_eq!(findings(&old, &tree(project(&["active"]), None, &[])).len(), 1);
        let mut orphan = app("example");
        orphan["metadata"]["finalizers"] = json!([]);
        assert!(findings(&old, &tree(project(&["active"]), Some(orphan), &[])).is_empty());
    }

    #[test]
    fn test_invalid_catalog_is_actionable_even_with_ignore() {
        let mut old = tree(project(&["retired"]), Some(app("example")), &[resource("retired")]);
        old.insert(PathBuf::from("_nyl/catalog/project.yaml"), b"not: [valid".to_vec());
        let error = check_project_scope(
            &borrowed(&old),
            &DesiredFiles::new(),
            "production",
            "production",
            None,
            true,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("Invalid rendered manifest _nyl/catalog/project.yaml"));
    }
    #[test]
    fn test_missing_generated_project_cannot_disable_the_guard() {
        let mut application = app("example");
        let mut provenance = context(&application).unwrap();
        provenance["generated"] = true.into();
        annotate(&mut application, &provenance).unwrap();
        let mut old = tree(project(&["retired"]), Some(application), &[resource("retired")]);
        old.remove(Path::new("_nyl/catalog/project.yaml"));
        let error = check_project_scope(
            &borrowed(&old),
            &DesiredFiles::new(),
            "production",
            "production",
            None,
            true,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("missing generated AppProject"), "{error}");
    }

    #[test]
    fn test_publisher_checks_exact_baseline_before_commit_author_or_push() {
        use super::super::publication::{prepare_publication, PublicationAuthor, TreePublication};
        use super::super::{ReconcileOptions, RenderIndex, RenderIndexPublication};
        use chrono::TimeZone;
        let directory = tempfile::TempDir::new().unwrap();
        let remote = git2::Repository::init_bare(directory.path().join("publication.git")).unwrap();
        let url = remote.path().to_str().unwrap();
        let auth = nyl_git::GitAuth::default();
        let index = RenderIndex::new(
            "production".into(),
            "production".into(),
            RenderIndexPublication {
                repository: "deploy".into(),
                revision: "deploy".into(),
                path_prefix: "production".into(),
            },
            None,
            false,
            BTreeMap::new(),
        );
        let old = tree(project(&["retired"]), Some(app("example")), &[resource("retired")]);
        let old_files = borrowed(&old);
        let mut request = TreePublication {
            push_url: url,
            branch: "deploy",
            path_prefix: "production",
            desired: &old_files,
            index,
            options: ReconcileOptions::default(),
            publication_base: None,
            auth: &auth,
            ignore_project_scope_contraction: false,
            cluster_kubernetes: None,
        };
        let baseline = prepare_publication(&request, "baseline", || {
            Ok(PublicationAuthor {
                identity: nyl_core::orchestration::ports::CommitIdentity {
                    name: "Tests".into(),
                    email: "tests@example.invalid".into(),
                },
                time: chrono::Utc.timestamp_opt(1, 0).unwrap(),
            })
        })
        .unwrap();
        baseline.push(&auth).unwrap();
        let head = remote.find_reference("refs/heads/deploy").unwrap().target().unwrap();
        let new = tree(project(&["active"]), None, &[]);
        let new_files = borrowed(&new);
        request.desired = &new_files;
        let error = prepare_publication(&request, "unsafe contraction", || {
            panic!("guard must run before commit author")
        })
        .err()
        .expect("unsafe contraction must fail")
        .to_string();
        assert!(error.contains("NYL_PROJECT_SCOPE_CONTRACTION"), "{error}");
        assert_eq!(
            remote.find_reference("refs/heads/deploy").unwrap().target().unwrap(),
            head
        );
    }
}
