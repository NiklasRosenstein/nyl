//! Kubernetes-shaped configuration resources for rendered GitOps workflows.
//!
//! These resources are compiler inputs. They are not installed in a Kubernetes
//! cluster. Their `apiVersion`, `kind`, and local `metadata.name` form a static
//! envelope so that discovery does not need to evaluate templates.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path};

use clap::ValueEnum;
use schemars::{schema_for, JsonSchema};
use serde::{Deserialize, Serialize};

use crate::constants::API_VERSION_GITOPS;
use crate::{NylError, Result};

pub const KIND_GIT_REPOSITORY: &str = "GitRepository";
pub const KIND_CLUSTER: &str = "Cluster";
pub const KIND_GITOPS_TARGET: &str = "GitOpsTarget";
pub const KIND_APP_PROJECT_DEFINITION: &str = "AppProjectDefinition";
pub const KIND_APPLICATION_GROUP: &str = "ApplicationGroup";

/// Metadata shared by local GitOps compiler resources.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GitOpsResourceMetadata {
    pub name: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
}

/// A reusable, credential-free Git repository identity.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GitRepository {
    #[serde(rename = "apiVersion")]
    pub api_version: String,
    pub kind: String,
    pub metadata: GitOpsResourceMetadata,
    pub spec: GitRepositorySpec,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GitRepositorySpec {
    #[serde(rename = "repoURL")]
    pub repo_url: String,
    #[serde(rename = "publishURL", skip_serializing_if = "Option::is_none")]
    pub publish_url: Option<String>,
}

/// A concrete Kubernetes cluster and the deterministic facts used to render for it.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Cluster {
    #[serde(rename = "apiVersion")]
    pub api_version: String,
    pub kind: String,
    pub metadata: GitOpsResourceMetadata,
    pub spec: ClusterSpec,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ClusterSpec {
    pub destination: ClusterDestination,
    pub kubernetes: ClusterKubernetesCapabilities,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub values: BTreeMap<String, serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub live: Option<ClusterLiveConfiguration>,
}

/// Argo CD's identity for a concrete cluster.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ClusterDestination {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// Kubernetes discovery information used for deterministic offline rendering.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ClusterKubernetesCapabilities {
    #[serde(rename = "kubeVersion", skip_serializing_if = "Option::is_none")]
    pub kube_version: Option<String>,
    #[serde(default, rename = "apiVersions", skip_serializing_if = "Vec::is_empty")]
    pub api_versions: Vec<String>,
}

/// Local-only connection settings for live operations against a cluster.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ClusterLiveConfiguration {
    pub context: String,
}

/// A named, independently renderable and publishable deployment slice.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct GitOpsTarget {
    #[serde(rename = "apiVersion")]
    pub api_version: String,
    pub kind: String,
    pub metadata: GitOpsResourceMetadata,
    pub spec: GitOpsTargetSpec,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct GitOpsTargetSpec {
    #[serde(rename = "clusterRef")]
    pub cluster_ref: LocalReference,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub values: BTreeMap<String, serde_json::Value>,
    pub publication: GitPublication,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub projects: Vec<String>,
}

/// Git coordinates used as a rendered output and Argo CD source.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GitPublication {
    #[serde(rename = "repositoryRef", skip_serializing_if = "Option::is_none")]
    pub repository_ref: Option<LocalReference>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repository: Option<InlineGitRepository>,
    pub revision: String,
    #[serde(default, rename = "pathPrefix", skip_serializing_if = "String::is_empty")]
    pub path_prefix: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LocalReference {
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InlineGitRepository {
    #[serde(rename = "repoURL")]
    pub repo_url: String,
    #[serde(rename = "publishURL", skip_serializing_if = "Option::is_none")]
    pub publish_url: Option<String>,
}

/// A stable local identity wrapping an Argo CD AppProject manifest or contract.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AppProjectDefinition {
    #[serde(rename = "apiVersion")]
    pub api_version: String,
    pub kind: String,
    pub metadata: GitOpsResourceMetadata,
    pub spec: AppProjectDefinitionSpec,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AppProjectDefinitionSpec {
    pub management: AppProjectManagement,
    pub manifest: serde_json::Value,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub enum AppProjectManagement {
    Rendered,
    External,
}

/// Policy and source declaration for a set of NylRelease resources.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ApplicationGroup {
    #[serde(rename = "apiVersion")]
    pub api_version: String,
    pub kind: String,
    pub metadata: GitOpsResourceMetadata,
    pub spec: ApplicationGroupSpec,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ApplicationGroupSpec {
    #[serde(rename = "targetSelector", skip_serializing_if = "Option::is_none")]
    pub target_selector: Option<TargetSelector>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<ApplicationGroupSource>,
    #[serde(rename = "projectRef")]
    pub project_ref: String,
    #[serde(rename = "applicationNamespace")]
    pub application_namespace: String,
    #[serde(rename = "destinationNamespace", skip_serializing_if = "Option::is_none")]
    pub destination_namespace: Option<String>,
    #[serde(rename = "outputPath", skip_serializing_if = "Option::is_none")]
    pub output_path: Option<String>,
    #[serde(rename = "applicationNameTemplate", skip_serializing_if = "Option::is_none")]
    pub application_name_template: Option<String>,
    #[serde(rename = "syncPolicy", skip_serializing_if = "Option::is_none")]
    pub sync_policy: Option<GitOpsSyncPolicy>,
    #[serde(rename = "applicationDeletionPolicy", default)]
    pub application_deletion_policy: ApplicationDeletionPolicy,
    #[serde(default)]
    pub namespace: ManagedNamespacePolicy,
    #[serde(rename = "releaseCustomization", default)]
    pub release_customization: GitOpsReleaseCustomization,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub annotations: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct TargetSelector {
    #[serde(default, rename = "matchLabels", skip_serializing_if = "BTreeMap::is_empty")]
    pub match_labels: BTreeMap<String, String>,
}

/// A local source when no repository is given, otherwise an immutable remote source.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ApplicationGroupSource {
    #[serde(rename = "repositoryRef", skip_serializing_if = "Option::is_none")]
    pub repository_ref: Option<LocalReference>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repository: Option<InlineGitRepository>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    pub path: String,
    #[serde(default = "default_source_include")]
    pub include: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
    #[serde(default = "default_true")]
    pub recursive: bool,
    #[serde(rename = "rendererConfig", default)]
    pub renderer_config: RendererConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RendererConfig {
    #[serde(default)]
    pub mode: RendererConfigMode,
    #[serde(rename = "projectPath", skip_serializing_if = "Option::is_none")]
    pub project_path: Option<String>,
}

impl Default for RendererConfig {
    fn default() -> Self {
        Self {
            mode: RendererConfigMode::Central,
            project_path: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Default)]
pub enum RendererConfigMode {
    #[default]
    Central,
    Remote,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GitOpsSyncPolicy {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub automated: Option<GitOpsAutomatedSyncPolicy>,
    #[serde(default, rename = "syncOptions", skip_serializing_if = "Vec::is_empty")]
    pub sync_options: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GitOpsAutomatedSyncPolicy {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub prune: bool,
    #[serde(default, rename = "selfHeal")]
    pub self_heal: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Default)]
pub enum ApplicationDeletionPolicy {
    #[default]
    Foreground,
    Background,
    Orphan,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ManagedNamespacePolicy {
    #[serde(default = "default_true")]
    pub create: bool,
    #[serde(rename = "prunePolicy", default)]
    pub prune_policy: ManagedResourceDeletionPolicy,
    #[serde(rename = "deletePolicy", default)]
    pub delete_policy: ManagedResourceDeletionPolicy,
}

impl Default for ManagedNamespacePolicy {
    fn default() -> Self {
        Self {
            create: true,
            prune_policy: ManagedResourceDeletionPolicy::Confirm,
            delete_policy: ManagedResourceDeletionPolicy::Confirm,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Default)]
pub enum ManagedResourceDeletionPolicy {
    Automatic,
    #[default]
    Confirm,
    Retain,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct GitOpsReleaseCustomization {
    #[serde(default, rename = "allowedPaths", skip_serializing_if = "Vec::is_empty")]
    pub allowed_paths: Vec<String>,
    #[serde(default, rename = "deniedPaths", skip_serializing_if = "Vec::is_empty")]
    pub denied_paths: Vec<String>,
}

/// The supported static kind from a GitOps control-resource envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum GitOpsResourceKind {
    #[value(name = "GitRepository", alias = "git-repository", alias = "repository")]
    GitRepository,
    #[value(name = "Cluster", alias = "cluster")]
    Cluster,
    #[value(name = "GitOpsTarget", alias = "gitops-target", alias = "target")]
    GitOpsTarget,
    #[value(name = "AppProjectDefinition", alias = "app-project-definition", alias = "project")]
    AppProjectDefinition,
    #[value(name = "ApplicationGroup", alias = "application-group", alias = "group")]
    ApplicationGroup,
}

impl GitOpsResourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::GitRepository => KIND_GIT_REPOSITORY,
            Self::Cluster => KIND_CLUSTER,
            Self::GitOpsTarget => KIND_GITOPS_TARGET,
            Self::AppProjectDefinition => KIND_APP_PROJECT_DEFINITION,
            Self::ApplicationGroup => KIND_APPLICATION_GROUP,
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            KIND_GIT_REPOSITORY => Some(Self::GitRepository),
            KIND_CLUSTER => Some(Self::Cluster),
            KIND_GITOPS_TARGET => Some(Self::GitOpsTarget),
            KIND_APP_PROJECT_DEFINITION => Some(Self::AppProjectDefinition),
            KIND_APPLICATION_GROUP => Some(Self::ApplicationGroup),
            _ => None,
        }
    }

    pub const fn schema_filename(self) -> &'static str {
        match self {
            Self::GitRepository => "git-repository.schema.json",
            Self::Cluster => "cluster.schema.json",
            Self::GitOpsTarget => "gitops-target.schema.json",
            Self::AppProjectDefinition => "app-project-definition.schema.json",
            Self::ApplicationGroup => "application-group.schema.json",
        }
    }

    pub const fn all() -> [Self; 5] {
        [
            Self::GitRepository,
            Self::Cluster,
            Self::GitOpsTarget,
            Self::AppProjectDefinition,
            Self::ApplicationGroup,
        ]
    }
}

/// Generate the JSON Schema for one GitOps resource kind.
///
/// Schemars cannot infer that the Kubernetes resource envelope is a constant
/// from a Rust `String`, so the two discriminator properties are tightened
/// after deriving the remainder of the schema.
pub fn generate_gitops_resource_schema(kind: GitOpsResourceKind) -> serde_json::Value {
    let schema = match kind {
        GitOpsResourceKind::GitRepository => serde_json::to_value(schema_for!(GitRepository)),
        GitOpsResourceKind::Cluster => serde_json::to_value(schema_for!(Cluster)),
        GitOpsResourceKind::GitOpsTarget => serde_json::to_value(schema_for!(GitOpsTarget)),
        GitOpsResourceKind::AppProjectDefinition => serde_json::to_value(schema_for!(AppProjectDefinition)),
        GitOpsResourceKind::ApplicationGroup => serde_json::to_value(schema_for!(ApplicationGroup)),
    }
    .expect("schema serialization should never fail");
    tighten_resource_envelope(schema, kind)
}

fn tighten_resource_envelope(mut schema: serde_json::Value, kind: GitOpsResourceKind) -> serde_json::Value {
    let properties = schema
        .get_mut("properties")
        .and_then(serde_json::Value::as_object_mut)
        .expect("GitOps resource schema should have object properties");
    properties.insert(
        "apiVersion".to_owned(),
        serde_json::json!({
            "const": API_VERSION_GITOPS,
            "type": "string"
        }),
    );
    properties.insert(
        "kind".to_owned(),
        serde_json::json!({
            "const": kind.as_str(),
            "type": "string"
        }),
    );
    schema
}

/// Generate a portable aggregate schema with relative references to all kinds.
pub fn generate_gitops_aggregate_schema() -> serde_json::Value {
    serde_json::json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "Nyl GitOps resource",
        "description": "A local Nyl rendered-GitOps compiler resource.",
        "oneOf": GitOpsResourceKind::all()
            .iter()
            .map(|kind| serde_json::json!({"$ref": kind.schema_filename()}))
            .collect::<Vec<_>>()
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitOpsResourceIdentity {
    pub kind: GitOpsResourceKind,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum GitOpsResource {
    GitRepository(GitRepository),
    Cluster(Cluster),
    GitOpsTarget(GitOpsTarget),
    AppProjectDefinition(AppProjectDefinition),
    ApplicationGroup(Box<ApplicationGroup>),
}

/// True when the manifest advertises the GitOps API and one of its known kinds.
pub fn is_gitops_resource(value: &serde_json::Value) -> bool {
    gitops_resource_kind(value).is_some()
}

pub fn gitops_resource_kind(value: &serde_json::Value) -> Option<GitOpsResourceKind> {
    if value.get("apiVersion").and_then(serde_json::Value::as_str) != Some(API_VERSION_GITOPS) {
        return None;
    }
    GitOpsResourceKind::parse(value.get("kind")?.as_str()?)
}

/// Parse only the static envelope. Unrelated manifests return `None`.
pub fn parse_gitops_resource_identity(value: &serde_json::Value) -> Result<Option<GitOpsResourceIdentity>> {
    if value.get("apiVersion").and_then(serde_json::Value::as_str) != Some(API_VERSION_GITOPS) {
        return Ok(None);
    }
    let kind_text = value
        .get("kind")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| NylError::config("GitOps resource kind must be a static string"))?;
    let kind = GitOpsResourceKind::parse(kind_text)
        .ok_or_else(|| NylError::config(format!("Unsupported {API_VERSION_GITOPS} kind {kind_text:?}")))?;
    let name = value
        .get("metadata")
        .and_then(|metadata| metadata.get("name"))
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| NylError::config(format!("{kind_text} metadata.name must be a static string")))?;
    validate_static_required("metadata.name", name)?;
    Ok(Some(GitOpsResourceIdentity {
        kind,
        name: name.to_owned(),
    }))
}

/// Strictly parse and validate a complete, already-rendered GitOps resource.
pub fn parse_gitops_resource(value: &serde_json::Value) -> Result<Option<GitOpsResource>> {
    let Some(identity) = parse_gitops_resource_identity(value)? else {
        return Ok(None);
    };
    let resource = match identity.kind {
        GitOpsResourceKind::GitRepository => GitOpsResource::GitRepository(parse_as(value, KIND_GIT_REPOSITORY)?),
        GitOpsResourceKind::Cluster => GitOpsResource::Cluster(parse_as(value, KIND_CLUSTER)?),
        GitOpsResourceKind::GitOpsTarget => GitOpsResource::GitOpsTarget(parse_as(value, KIND_GITOPS_TARGET)?),
        GitOpsResourceKind::AppProjectDefinition => {
            GitOpsResource::AppProjectDefinition(parse_as(value, KIND_APP_PROJECT_DEFINITION)?)
        }
        GitOpsResourceKind::ApplicationGroup => {
            GitOpsResource::ApplicationGroup(Box::new(parse_as(value, KIND_APPLICATION_GROUP)?))
        }
    };
    resource.validate()?;
    Ok(Some(resource))
}

fn parse_as<T: serde::de::DeserializeOwned>(value: &serde_json::Value, kind: &str) -> Result<T> {
    serde_json::from_value(value.clone()).map_err(|error| NylError::config(format!("Invalid {kind}: {error}")))
}

impl GitOpsResource {
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::GitRepository(resource) => resource.validate(),
            Self::Cluster(resource) => resource.validate(),
            Self::GitOpsTarget(resource) => resource.validate(),
            Self::AppProjectDefinition(resource) => resource.validate(),
            Self::ApplicationGroup(resource) => resource.validate(),
        }
    }
}

impl GitRepository {
    pub fn validate(&self) -> Result<()> {
        validate_envelope(
            self.api_version.as_str(),
            self.kind.as_str(),
            KIND_GIT_REPOSITORY,
            &self.metadata,
        )?;
        validate_repository_coordinates(&self.spec.repo_url, self.spec.publish_url.as_deref())
    }
}

impl Cluster {
    pub fn validate(&self) -> Result<()> {
        validate_envelope(
            self.api_version.as_str(),
            self.kind.as_str(),
            KIND_CLUSTER,
            &self.metadata,
        )?;
        self.spec.destination.validate()?;
        self.spec.kubernetes.validate()?;
        if let Some(live) = &self.spec.live {
            validate_static_required("spec.live.context", &live.context)?;
        }
        Ok(())
    }
}

impl ClusterDestination {
    pub fn validate(&self) -> Result<()> {
        match (&self.server, &self.name) {
            (Some(server), None) => validate_static_required("spec.destination.server", server),
            (None, Some(name)) => validate_static_required("spec.destination.name", name),
            (Some(_), Some(_)) => Err(NylError::config(
                "spec.destination.server and spec.destination.name are mutually exclusive",
            )),
            (None, None) => Err(NylError::config(
                "Exactly one of spec.destination.server or spec.destination.name is required",
            )),
        }
    }
}

impl ClusterKubernetesCapabilities {
    pub fn validate(&self) -> Result<()> {
        if let Some(kube_version) = &self.kube_version {
            validate_static_required("spec.kubernetes.kubeVersion", kube_version)?;
        }
        validate_unique_static_names("spec.kubernetes.apiVersions", &self.api_versions)
    }
}

impl GitOpsTarget {
    pub fn validate(&self) -> Result<()> {
        validate_envelope(
            self.api_version.as_str(),
            self.kind.as_str(),
            KIND_GITOPS_TARGET,
            &self.metadata,
        )?;
        validate_static_required("spec.clusterRef.name", &self.spec.cluster_ref.name)?;
        self.spec.publication.validate()?;
        validate_unique_static_names("spec.projects", &self.spec.projects)
    }
}

impl GitPublication {
    pub fn validate(&self) -> Result<()> {
        validate_repository_choice(
            self.repository_ref.as_ref(),
            self.repository.as_ref(),
            "spec.publication",
        )?;
        validate_static_required("spec.publication.revision", &self.revision)?;
        validate_relative_path("spec.publication.pathPrefix", &self.path_prefix, true, false)
    }
}

impl AppProjectDefinition {
    pub fn validate(&self) -> Result<()> {
        validate_envelope(
            self.api_version.as_str(),
            self.kind.as_str(),
            KIND_APP_PROJECT_DEFINITION,
            &self.metadata,
        )?;
        let object = self
            .spec
            .manifest
            .as_object()
            .ok_or_else(|| NylError::config("spec.manifest must be an object"))?;
        if object.get("apiVersion").and_then(serde_json::Value::as_str) != Some("argoproj.io/v1alpha1")
            || object.get("kind").and_then(serde_json::Value::as_str) != Some("AppProject")
        {
            return Err(NylError::config(
                "spec.manifest must be an argoproj.io/v1alpha1 AppProject",
            ));
        }
        let name = object
            .get("metadata")
            .and_then(serde_json::Value::as_object)
            .and_then(|metadata| metadata.get("name"))
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| NylError::config("spec.manifest.metadata.name must be a string"))?;
        validate_dns_subdomain("spec.manifest.metadata.name", name)?;
        if !object.get("spec").is_some_and(serde_json::Value::is_object) {
            return Err(NylError::config("spec.manifest.spec must be an object"));
        }
        Ok(())
    }
}

impl ApplicationGroup {
    pub fn validate(&self) -> Result<()> {
        validate_envelope(
            self.api_version.as_str(),
            self.kind.as_str(),
            KIND_APPLICATION_GROUP,
            &self.metadata,
        )?;
        validate_static_required("spec.projectRef", &self.spec.project_ref)?;
        validate_required("spec.applicationNamespace", &self.spec.application_namespace)?;
        if let Some(namespace) = &self.spec.destination_namespace {
            validate_required("spec.destinationNamespace", namespace)?;
        }
        if let Some(path) = &self.spec.output_path {
            validate_relative_path("spec.outputPath", path, false, false)?;
        }
        if let Some(source) = &self.spec.source {
            source.validate()?;
        }
        for pattern in self
            .spec
            .release_customization
            .allowed_paths
            .iter()
            .chain(&self.spec.release_customization.denied_paths)
        {
            crate::resources::validate_path_glob_pattern(pattern)?;
        }
        Ok(())
    }
}

impl ApplicationGroupSource {
    pub fn is_remote(&self) -> bool {
        self.repository_ref.is_some() || self.repository.is_some()
    }

    pub fn validate(&self) -> Result<()> {
        validate_relative_path("spec.source.path", &self.path, false, false)?;
        let remote = self.is_remote();
        match (&self.repository_ref, &self.repository) {
            (Some(reference), None) => validate_static_required("spec.source.repositoryRef.name", &reference.name)?,
            (None, Some(repository)) => repository.validate("spec.source.repository")?,
            (Some(_), Some(_)) => {
                return Err(NylError::config(
                    "spec.source.repositoryRef and spec.source.repository are mutually exclusive",
                ))
            }
            (None, None) => {}
        }
        if remote {
            let revision = self
                .revision
                .as_deref()
                .ok_or_else(|| NylError::config("spec.source.revision is required for a remote source"))?;
            validate_static_required("spec.source.revision", revision)?;
            let commit = self
                .commit
                .as_deref()
                .ok_or_else(|| NylError::config("spec.source.commit is required for a remote source"))?;
            validate_immutable_git_commit("spec.source.commit", commit)?;
        } else if self.revision.is_some() || self.commit.is_some() {
            return Err(NylError::config(
                "spec.source.revision and spec.source.commit require a remote repository",
            ));
        }
        for pattern in self.include.iter().chain(&self.exclude) {
            glob::Pattern::new(pattern).map_err(|error| {
                NylError::config(format!(
                    "Invalid ApplicationGroup source include/exclude pattern {pattern:?}: {error}"
                ))
            })?;
        }
        match self.renderer_config.mode {
            RendererConfigMode::Central if self.renderer_config.project_path.is_some() => Err(NylError::config(
                "spec.source.rendererConfig.projectPath is only valid in Remote mode",
            )),
            RendererConfigMode::Remote if !remote => Err(NylError::config(
                "spec.source.rendererConfig.mode Remote requires a remote repository",
            )),
            RendererConfigMode::Remote => {
                let project_path = self.renderer_config.project_path.as_deref().unwrap_or(".");
                validate_relative_path("spec.source.rendererConfig.projectPath", project_path, false, true)
            }
            RendererConfigMode::Central => Ok(()),
        }
    }
}

impl InlineGitRepository {
    fn validate(&self, field: &str) -> Result<()> {
        validate_repository_coordinates(&self.repo_url, self.publish_url.as_deref())
            .map_err(|error| NylError::config(format!("{field}: {error}")))
    }
}

fn validate_envelope(
    api_version: &str,
    actual_kind: &str,
    expected_kind: &str,
    metadata: &GitOpsResourceMetadata,
) -> Result<()> {
    if api_version != API_VERSION_GITOPS {
        return Err(NylError::config(format!(
            "{expected_kind} apiVersion must be {API_VERSION_GITOPS:?}"
        )));
    }
    if actual_kind != expected_kind {
        return Err(NylError::config(format!(
            "Expected kind {expected_kind:?}, got {actual_kind:?}"
        )));
    }
    validate_dns_subdomain("metadata.name", &metadata.name)
}

fn validate_dns_subdomain(field: &str, value: &str) -> Result<()> {
    validate_static_required(field, value)?;
    let valid = value.len() <= 253
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-'))
        && value.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric)
        && value.as_bytes().last().is_some_and(u8::is_ascii_alphanumeric);
    if valid {
        Ok(())
    } else {
        Err(NylError::config(format!("{field} must be a Kubernetes DNS subdomain")))
    }
}

fn validate_repository_choice(
    repository_ref: Option<&LocalReference>,
    repository: Option<&InlineGitRepository>,
    field: &str,
) -> Result<()> {
    match (repository_ref, repository) {
        (Some(reference), None) => validate_static_required(&format!("{field}.repositoryRef.name"), &reference.name),
        (None, Some(repository)) => repository.validate(&format!("{field}.repository")),
        (Some(_), Some(_)) => Err(NylError::config(format!(
            "{field}.repositoryRef and {field}.repository are mutually exclusive"
        ))),
        (None, None) => Err(NylError::config(format!(
            "Exactly one of {field}.repositoryRef or {field}.repository is required"
        ))),
    }
}

fn validate_repository_coordinates(repo_url: &str, publish_url: Option<&str>) -> Result<()> {
    validate_static_required("spec.repoURL", repo_url)?;
    reject_http_userinfo("spec.repoURL", repo_url)?;
    if let Some(publish_url) = publish_url {
        validate_static_required("spec.publishURL", publish_url)?;
        reject_http_userinfo("spec.publishURL", publish_url)?;
    }
    Ok(())
}

fn reject_http_userinfo(field: &str, value: &str) -> Result<()> {
    let lower = value.to_ascii_lowercase();
    let Some(authority_and_path) = lower.strip_prefix("https://").or_else(|| lower.strip_prefix("http://")) else {
        return Ok(());
    };
    let authority = authority_and_path
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(authority_and_path);
    if authority.contains('@') {
        return Err(NylError::config(format!(
            "{field} must not contain HTTP user information; configure Git credentials outside GitOps resources"
        )));
    }
    Ok(())
}

fn validate_required(field: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        return Err(NylError::config(format!("{field} must not be empty")));
    }
    Ok(())
}

/// Require a literal, non-empty value in the pre-template resource envelope or coordinates.
pub fn validate_static_required(field: &str, value: &str) -> Result<()> {
    validate_required(field, value)?;
    if value.contains("{{") || value.contains("{%") || value.contains("{#") {
        return Err(NylError::config(format!("{field} must be a static value")));
    }
    Ok(())
}

/// Validate a slash-separated, project-relative path without traversal.
pub fn validate_relative_path(field: &str, value: &str, allow_empty: bool, allow_dot: bool) -> Result<()> {
    if value.is_empty() && allow_empty {
        return Ok(());
    }
    validate_static_required(field, value)?;
    if value == "." && allow_dot {
        return Ok(());
    }
    if value.contains('\\')
        || value
            .split('/')
            .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    {
        return Err(NylError::config(format!(
            "{field} must be a normalized relative path without traversal"
        )));
    }
    if Path::new(value)
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(NylError::config(format!(
            "{field} must be a normalized relative path without traversal"
        )));
    }
    Ok(())
}

/// Convert a platform-native relative path to the slash-separated form stored
/// in rendered GitOps metadata.
pub fn relative_path_to_posix(field: &str, value: &Path) -> Result<String> {
    let mut segments = Vec::new();
    for component in value.components() {
        let Component::Normal(segment) = component else {
            return Err(NylError::config(format!(
                "{field} must be a normalized relative path without traversal"
            )));
        };
        segments.push(
            segment
                .to_str()
                .ok_or_else(|| NylError::config(format!("{field} must be valid UTF-8")))?,
        );
    }
    let value = segments.join("/");
    validate_relative_path(field, &value, false, false)?;
    Ok(value)
}

/// Accept only full hexadecimal Git object IDs, never a mutable ref abbreviation.
pub fn validate_immutable_git_commit(field: &str, value: &str) -> Result<()> {
    validate_static_required(field, value)?;
    if !matches!(value.len(), 40 | 64) || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(NylError::config(format!(
            "{field} must be a full 40- or 64-character hexadecimal Git object ID"
        )));
    }
    Ok(())
}

fn validate_unique_static_names(field: &str, values: &[String]) -> Result<()> {
    let mut seen = BTreeSet::new();
    for value in values {
        validate_static_required(field, value)?;
        if !seen.insert(value) {
            return Err(NylError::config(format!("{field} contains duplicate value {value:?}")));
        }
    }
    Ok(())
}

fn default_true() -> bool {
    true
}

fn default_source_include() -> Vec<String> {
    vec!["*.yaml".to_owned(), "*.yml".to_owned()]
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use serde_json::json;

    use super::*;

    fn target() -> serde_json::Value {
        json!({
            "apiVersion": API_VERSION_GITOPS,
            "kind": KIND_GITOPS_TARGET,
            "metadata": {"name": "production", "labels": {"environment": "production"}},
            "spec": {
                "clusterRef": {"name": "kasoku"},
                "values": {"replicas": 3},
                "publication": {
                    "repositoryRef": {"name": "deploy"},
                    "revision": "deploy/production"
                },
                "projects": ["platform"]
            }
        })
    }

    fn cluster() -> serde_json::Value {
        json!({
            "apiVersion": API_VERSION_GITOPS,
            "kind": KIND_CLUSTER,
            "metadata": {"name": "kasoku", "labels": {"region": "fsn1"}},
            "spec": {
                "destination": {"server": "https://kubernetes.default.svc"},
                "kubernetes": {
                    "kubeVersion": "1.31.4",
                    "apiVersions": ["apps/v1", "v1"]
                },
                "values": {"region": "fsn1", "nested": {"unrestricted": true}},
                "live": {"context": "kasoku"}
            }
        })
    }

    fn application_group() -> serde_json::Value {
        json!({
            "apiVersion": API_VERSION_GITOPS,
            "kind": KIND_APPLICATION_GROUP,
            "metadata": {"name": "cloud"},
            "spec": {
                "targetSelector": {"matchLabels": {"environment": "production"}},
                "projectRef": "applications",
                "applicationNamespace": "argocd-applications",
                "source": {"path": "applications/cloud"},
                "destinationNamespace": "cloud",
                "outputPath": "cloud",
                "namespace": {"create": true, "prunePolicy": "Confirm", "deletePolicy": "Confirm"}
            }
        })
    }

    #[test]
    fn parses_and_validates_target() {
        let parsed = parse_gitops_resource(&target()).unwrap().unwrap();
        let GitOpsResource::GitOpsTarget(parsed) = parsed else {
            panic!("expected target");
        };
        assert_eq!(parsed.metadata.name, "production");
        assert_eq!(parsed.spec.cluster_ref.name, "kasoku");
        assert_eq!(parsed.spec.values["replicas"], 3);
        assert_eq!(parsed.spec.publication.path_prefix, "");
    }

    #[test]
    fn parses_and_validates_cluster() {
        let parsed = parse_gitops_resource(&cluster()).unwrap().unwrap();
        let GitOpsResource::Cluster(parsed) = parsed else {
            panic!("expected cluster");
        };
        assert_eq!(parsed.metadata.name, "kasoku");
        assert_eq!(
            parsed.spec.destination.server.as_deref(),
            Some("https://kubernetes.default.svc")
        );
        assert_eq!(parsed.spec.kubernetes.kube_version.as_deref(), Some("1.31.4"));
        assert_eq!(parsed.spec.kubernetes.api_versions, ["apps/v1", "v1"]);
        assert_eq!(parsed.spec.values["nested"]["unrestricted"], true);
        assert_eq!(parsed.spec.live.unwrap().context, "kasoku");
    }

    #[test]
    fn parses_cluster_capability_defaults() {
        let mut value = cluster();
        value["spec"]["kubernetes"] = json!({});
        let GitOpsResource::Cluster(parsed) = parse_gitops_resource(&value).unwrap().unwrap() else {
            panic!("expected cluster");
        };
        assert!(parsed.spec.kubernetes.kube_version.is_none());
        assert!(parsed.spec.kubernetes.api_versions.is_empty());
    }

    #[test]
    fn parses_application_group_defaults() {
        let parsed = parse_gitops_resource(&application_group()).unwrap().unwrap();
        let GitOpsResource::ApplicationGroup(parsed) = parsed else {
            panic!("expected application group");
        };
        assert!(parsed.spec.enabled);
        assert_eq!(
            parsed.spec.application_deletion_policy,
            ApplicationDeletionPolicy::Foreground
        );
        assert_eq!(
            parsed.spec.namespace.prune_policy,
            ManagedResourceDeletionPolicy::Confirm
        );
        assert_eq!(parsed.spec.source.unwrap().include, vec!["*.yaml", "*.yml"]);
    }

    #[test]
    fn strict_deserialization_rejects_unknown_fields() {
        let mut value = target();
        value["spec"]["unknown"] = json!(true);
        let error = parse_gitops_resource(&value).unwrap_err().to_string();
        assert!(error.contains("unknown field"));
    }

    #[test]
    fn identifies_only_supported_gitops_resources() {
        assert!(is_gitops_resource(&target()));
        assert_eq!(
            parse_gitops_resource_identity(&target()).unwrap().unwrap(),
            GitOpsResourceIdentity {
                kind: GitOpsResourceKind::GitOpsTarget,
                name: "production".to_owned()
            }
        );
        assert!(!is_gitops_resource(&json!({"apiVersion": "v1", "kind": "ConfigMap"})));
        assert!(
            parse_gitops_resource_identity(&json!({"apiVersion": "v1", "kind": "ConfigMap"}))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn rejects_unknown_kind_in_gitops_api() {
        let error = parse_gitops_resource_identity(&json!({
            "apiVersion": API_VERSION_GITOPS,
            "kind": "Mystery",
            "metadata": {"name": "x"}
        }))
        .unwrap_err()
        .to_string();
        assert!(error.contains("Unsupported"));
    }

    #[test]
    fn rejects_templated_static_identity() {
        let mut value = target();
        value["metadata"]["name"] = json!("{{ values.name }}");
        assert!(parse_gitops_resource_identity(&value).is_err());
    }

    #[test]
    fn validates_repository_choice() {
        let mut value = target();
        value["spec"]["publication"]["repository"] = json!({"repoURL": "ssh://example/deploy.git"});
        let error = parse_gitops_resource(&value).unwrap_err().to_string();
        assert!(error.contains("mutually exclusive"));

        value["spec"]["publication"]
            .as_object_mut()
            .unwrap()
            .remove("repositoryRef");
        assert!(parse_gitops_resource(&value).is_ok());
    }

    #[test]
    fn rejects_credentials_in_http_repository_coordinates() {
        let mut value = target();
        value["spec"]["publication"]
            .as_object_mut()
            .unwrap()
            .remove("repositoryRef");
        value["spec"]["publication"]["repository"] = json!({"repoURL": "https://token@example.invalid/deploy.git"});
        let error = parse_gitops_resource(&value).unwrap_err().to_string();
        assert!(error.contains("must not contain HTTP user information"));
    }

    #[test]
    fn rejects_paths_with_traversal_or_non_normal_forms() {
        for path in [
            "../outside",
            "applications/../outside",
            "/absolute",
            "applications//cloud",
            "applications/./cloud",
        ] {
            let mut value = application_group();
            value["spec"]["source"]["path"] = json!(path);
            assert!(parse_gitops_resource(&value).is_err(), "accepted {path}");
        }
    }

    #[test]
    fn serializes_platform_paths_with_posix_separators() {
        let path = PathBuf::from("applications").join("cloud").join("deployment.yaml");
        assert_eq!(
            relative_path_to_posix("rendered path", &path).unwrap(),
            "applications/cloud/deployment.yaml"
        );
    }

    #[test]
    fn validates_immutable_remote_source() {
        let mut value = application_group();
        value["spec"]["source"] = json!({
            "repositoryRef": {"name": "workloads"},
            "revision": "refs/heads/main",
            "commit": "0123456789abcdef0123456789abcdef01234567",
            "path": "applications/cloud",
            "rendererConfig": {"mode": "Remote", "projectPath": "."}
        });
        assert!(parse_gitops_resource(&value).is_ok());

        value["spec"]["source"]["commit"] = json!("main");
        assert!(parse_gitops_resource(&value).is_err());
        value["spec"]["source"].as_object_mut().unwrap().remove("commit");
        assert!(parse_gitops_resource(&value).is_err());
    }

    #[test]
    fn local_source_rejects_remote_only_fields() {
        let mut value = application_group();
        value["spec"]["source"]["revision"] = json!("main");
        assert!(parse_gitops_resource(&value).is_err());
        value["spec"]["source"] = json!({
            "path": "applications/cloud",
            "rendererConfig": {"mode": "Remote"}
        });
        assert!(parse_gitops_resource(&value).is_err());
    }

    #[test]
    fn rejects_invalid_application_group_source_globs() {
        let mut value = application_group();
        value["spec"]["source"]["include"] = json!(["["]);
        let error = parse_gitops_resource(&value).unwrap_err().to_string();
        assert!(error.contains("include/exclude pattern"));
    }

    #[test]
    fn validates_cluster_destination_exclusivity() {
        let mut value = cluster();
        value["spec"]["destination"]["name"] = json!("in-cluster");
        assert!(parse_gitops_resource(&value).is_err());
        value["spec"]["destination"].as_object_mut().unwrap().remove("server");
        assert!(parse_gitops_resource(&value).is_ok());
    }

    #[test]
    fn rejects_cluster_unknown_fields_and_duplicate_capabilities() {
        let mut value = cluster();
        value["spec"]["destination"]["namespace"] = json!("default");
        assert!(parse_gitops_resource(&value)
            .unwrap_err()
            .to_string()
            .contains("unknown field"));

        value = cluster();
        value["spec"]["kubernetes"]["apiVersions"] = json!(["v1", "v1"]);
        assert!(parse_gitops_resource(&value)
            .unwrap_err()
            .to_string()
            .contains("duplicate value"));
    }

    #[test]
    fn validates_app_project_manifest_shape() {
        let valid = json!({
            "apiVersion": API_VERSION_GITOPS,
            "kind": KIND_APP_PROJECT_DEFINITION,
            "metadata": {"name": "platform"},
            "spec": {
                "management": "Rendered",
                "manifest": {
                    "apiVersion": "argoproj.io/v1alpha1",
                    "kind": "AppProject",
                    "metadata": {"name": "platform"},
                    "spec": {}
                }
            }
        });
        assert!(parse_gitops_resource(&valid).is_ok());
        let mut invalid = valid;
        invalid["spec"]["manifest"]["kind"] = json!("Application");
        assert!(parse_gitops_resource(&invalid).is_err());
    }

    #[test]
    fn validates_repository_and_schema_generation() {
        let repository = json!({
            "apiVersion": API_VERSION_GITOPS,
            "kind": KIND_GIT_REPOSITORY,
            "metadata": {"name": "deploy"},
            "spec": {"repoURL": "ssh://git@example/deploy.git"}
        });
        assert!(parse_gitops_resource(&repository).is_ok());
        let schema = schemars::schema_for!(GitOpsTarget);
        assert!(serde_json::to_value(schema).unwrap().is_object());
    }

    #[test]
    fn generated_schemas_have_constant_resource_envelopes() {
        for kind in GitOpsResourceKind::all() {
            let schema = generate_gitops_resource_schema(kind);
            assert_eq!(schema["properties"]["apiVersion"]["const"], API_VERSION_GITOPS);
            assert_eq!(schema["properties"]["kind"]["const"], kind.as_str());
            assert_eq!(
                serde_json::to_string_pretty(&schema).unwrap(),
                serde_json::to_string_pretty(&generate_gitops_resource_schema(kind)).unwrap()
            );
        }
    }

    #[test]
    fn aggregate_schema_has_relative_refs() {
        let schema = generate_gitops_aggregate_schema();
        let references = schema["oneOf"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["$ref"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            references,
            vec![
                "git-repository.schema.json",
                "cluster.schema.json",
                "gitops-target.schema.json",
                "app-project-definition.schema.json",
                "application-group.schema.json"
            ]
        );
    }

    #[test]
    fn generated_gitops_schemas_match_published_artifacts() {
        let schema_directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("book")
            .join("public")
            .join("reference")
            .join("schemas");
        for kind in GitOpsResourceKind::all() {
            let published = fs::read_to_string(schema_directory.join(kind.schema_filename())).unwrap();
            let published: serde_json::Value = serde_json::from_str(&published).unwrap();
            assert_eq!(
                published,
                generate_gitops_resource_schema(kind),
                "Published {} is out of date; run `nyl generate schema all --output-dir nyl/book/public/reference/schemas`",
                kind.schema_filename()
            );
        }
        let published = fs::read_to_string(schema_directory.join("gitops-resource.schema.json")).unwrap();
        let published: serde_json::Value = serde_json::from_str(&published).unwrap();
        assert_eq!(published, generate_gitops_aggregate_schema());
    }
}
