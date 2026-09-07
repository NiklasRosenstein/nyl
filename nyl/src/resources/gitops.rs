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

use crate::constants::{API_VERSION_GITOPS, API_VERSION_K8S_GITOPS};
use crate::{NylError, Result};

pub const KIND_GIT_REPOSITORY: &str = "GitRepository";
pub const KIND_CLUSTER: &str = "Cluster";
pub const KIND_ARGOCD_INSTANCE: &str = "ArgoCDInstance";
pub const KIND_DEPLOYMENT_TARGET: &str = "DeploymentTarget";
pub const KIND_APP_PROJECT_DEFINITION: &str = "AppProjectDefinition";
pub const KIND_APPLICATION_GROUP: &str = "ApplicationGroup";

/// Metadata shared by local GitOps compiler resources.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GitOpsResourceMetadata {
    /// Project-local identity. Must be a Kubernetes DNS subdomain and remain static during discovery.
    pub name: String,
    /// Literal labels used for organization and target selection.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
}

/// Names reusable, credential-free Git read and publication coordinates.
///
/// This is shared compiler configuration referenced by Kubernetes GitOps resources; it is not emitted as a workload manifest.
///
/// ## When needed
///
/// Required when a publication, remote ApplicationGroup source, or AppProjectDefinition refers to a named GitRepository.
///
/// ## If omitted
///
/// Publications and remote sources can use inline `repository` coordinates instead. Local ApplicationGroup sources need no repository. Named references must resolve to a declared GitRepository; Nyl does not infer one from the current Git remote.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
#[schemars(example = super::schema::resource_example(super::schema::ResourceKind::GitRepository))]
pub struct GitRepository {
    /// API group and version defining this resource.
    #[serde(rename = "apiVersion")]
    pub api_version: String,
    /// Resource kind within the API group.
    pub kind: String,
    /// Resource identity and metadata.
    pub metadata: GitOpsResourceMetadata,
    /// Configuration for this resource.
    pub spec: GitRepositorySpec,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GitRepositorySpec {
    /// Credential-free Git read URL used by renderers and generated Applications.
    #[serde(rename = "repoURL")]
    pub repo_url: String,
    /// Optional Git write URL. Publication uses `repoURL` when omitted.
    #[serde(rename = "publishURL", skip_serializing_if = "Option::is_none")]
    pub publish_url: Option<String>,
}

/// Describes a concrete Kubernetes destination and its deterministic rendering capabilities.
///
/// Cluster values supply reusable facts; target values take precedence. This is compiler configuration and is not emitted as a workload manifest.
///
/// ## When needed
///
/// Required for every workload destination and every explicitly configured Argo CD control-plane destination in rendered GitOps.
///
/// ## If omitted
///
/// Target rendering fails if the referenced Cluster is missing. Omitting `DeploymentTarget.spec.clusterRef` selects a Cluster with the target name; it does not create that Cluster or infer its capabilities from kubeconfig.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
#[schemars(example = super::schema::resource_example(super::schema::ResourceKind::Cluster))]
pub struct Cluster {
    /// API group and version defining this resource.
    #[serde(rename = "apiVersion")]
    pub api_version: String,
    /// Resource kind within the API group.
    pub kind: String,
    /// Resource identity and metadata.
    pub metadata: GitOpsResourceMetadata,
    /// Configuration for this resource.
    pub spec: ClusterSpec,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ClusterSpec {
    /// Exactly one concrete Argo CD destination, identified by server URL or registered cluster name.
    pub destination: ClusterDestination,
    /// Committed Kubernetes capabilities for deterministic offline rendering. Target rendering requires a version and at least one API version.
    pub kubernetes: ClusterKubernetesCapabilities,
    /// Cluster facts merged recursively with target values; target values win.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub values: BTreeMap<String, serde_json::Value>,
    /// Local connection settings. Omitted from templates and render hashes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub live: Option<ClusterLiveConfiguration>,
}

/// Argo CD's identity for a concrete cluster.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
#[schemars(transform = super::schema::cluster_destination_constraints)]
pub struct ClusterDestination {
    /// Kubernetes API server URL. Exactly one of `server` and `name` must be non-null.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    /// Argo CD registered cluster name. Exactly one of `server` and `name` must be non-null.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// Kubernetes discovery information used for deterministic offline rendering.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ClusterKubernetesCapabilities {
    /// Kubernetes version exposed to Helm. May be omitted for scaffolding; required for target rendering.
    #[serde(rename = "kubeVersion", skip_serializing_if = "Option::is_none")]
    pub kube_version: Option<String>,
    /// API versions exposed to Helm. An empty list is valid for scaffolding; target rendering requires at least one entry.
    #[serde(default, rename = "apiVersions", skip_serializing_if = "Vec::is_empty")]
    pub api_versions: Vec<String>,
}

/// Local-only connection settings for live operations against a cluster.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ClusterLiveConfiguration {
    /// Local kubeconfig context. Live commands prefer an explicit `--context`, then this value, then the current kubeconfig context.
    pub context: String,
}

/// Defines an Argo CD control plane and the catalog defaults for its deployment targets.
///
/// This is compiler configuration. Targets sharing an instance must use unambiguous generated Application and AppProject names.
///
/// ## When needed
///
/// Declare one to configure a shared or remote Argo CD control plane, a different namespace, or shared catalog defaults.
///
/// ## If omitted
///
/// When no ArgoCDInstance resources are declared, each target uses an implicit instance in its workload Cluster, in namespace `argocd`, with the catalog defaults. Leave `spec.argocdRef` unset in this case. Once any explicit instance exists, every DeploymentTarget must set `spec.argocdRef.name` to a declared instance.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
#[schemars(example = super::schema::resource_example(super::schema::ResourceKind::ArgoCDInstance))]
pub struct ArgoCDInstance {
    /// API group and version defining this resource.
    #[serde(rename = "apiVersion")]
    pub api_version: String,
    /// Resource kind within the API group.
    pub kind: String,
    /// Resource identity and metadata.
    pub metadata: GitOpsResourceMetadata,
    /// Configuration for this resource.
    pub spec: ArgoCDInstanceSpec,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ArgoCDInstanceSpec {
    /// Project-local Cluster containing the Argo CD control plane.
    #[serde(rename = "clusterRef")]
    pub cluster_ref: LocalReference,
    /// Namespace containing Argo CD Applications and AppProjects.
    #[serde(default = "default_argocd_namespace")]
    pub namespace: String,
    /// Default policy for the catalog Applications of targets using this instance.
    #[serde(rename = "catalogApplicationDefaults", default)]
    pub catalog_application_defaults: CatalogApplicationDefaults,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CatalogApplicationDefaults {
    /// Argo CD project assigned to generated catalog Applications.
    #[serde(default = "default_argocd_project")]
    pub project: String,
    /// Catalog synchronization policy. Defaults to manual sync with apply-only-out-of-sync and server-side apply.
    #[serde(rename = "syncPolicy", default = "default_catalog_sync_policy")]
    pub sync_policy: GitOpsSyncPolicy,
    /// Argo CD Application finalizer policy: foreground cascade, background cascade, or orphan workloads.
    #[serde(rename = "applicationDeletionPolicy", default)]
    pub application_deletion_policy: ApplicationDeletionPolicy,
    /// Policy for pruning the catalog Application itself. Confirmation is required by default.
    #[serde(rename = "selfPrunePolicy", default)]
    pub self_prune_policy: ManagedResourceDeletionPolicy,
    /// Labels added to generated Applications.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    /// Annotations added to generated Applications.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub annotations: BTreeMap<String, String>,
}

impl Default for CatalogApplicationDefaults {
    fn default() -> Self {
        Self {
            project: default_argocd_project(),
            sync_policy: default_catalog_sync_policy(),
            application_deletion_policy: ApplicationDeletionPolicy::Foreground,
            self_prune_policy: ManagedResourceDeletionPolicy::Confirm,
            labels: BTreeMap::new(),
            annotations: BTreeMap::new(),
        }
    }
}

/// Binds a Kubernetes Cluster to render values, ApplicationGroup selection, and Git publication coordinates.
///
/// A target owns one independently renderable publication slice. It is compiler configuration and does not represent a general infrastructure environment.
///
/// ## When needed
///
/// Required for target-aware rendered GitOps operations. It supplies workload Cluster selection and publication coordinates.
///
/// ## If omitted
///
/// Operations requiring a target fail when none are declared. If exactly one target exists, Nyl selects it without `--target`; with multiple targets, select one explicitly. Target selection never creates a DeploymentTarget.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
#[schemars(example = super::schema::resource_example(super::schema::ResourceKind::DeploymentTarget))]
pub struct DeploymentTarget {
    /// API group and version defining this resource.
    #[serde(rename = "apiVersion")]
    pub api_version: String,
    /// Resource kind within the API group.
    pub kind: String,
    /// Resource identity and metadata.
    pub metadata: GitOpsResourceMetadata,
    /// Configuration for this resource.
    pub spec: DeploymentTargetSpec,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct DeploymentTargetSpec {
    /// Project-local workload Cluster. Defaults to the target name when omitted.
    #[serde(rename = "clusterRef", skip_serializing_if = "Option::is_none")]
    pub cluster_ref: Option<LocalReference>,
    /// Project-local ArgoCDInstance. Required when any explicit instance exists; otherwise Nyl uses a target-local instance in the workload Cluster and `argocd` namespace.
    #[serde(rename = "argocdRef", skip_serializing_if = "Option::is_none")]
    pub argocd_ref: Option<LocalReference>,
    /// Matches literal ApplicationGroup metadata labels before rendering their specs. An empty selector matches all groups.
    #[serde(rename = "applicationGroupSelector", default)]
    pub application_group_selector: LabelSelector,
    /// Target values recursively overlaid on Cluster values and exposed as template inputs.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub values: BTreeMap<String, serde_json::Value>,
    /// Git repository, revision, and path prefix owned by this target. Targets on the same repository revision must have disjoint prefixes.
    pub publication: GitPublication,
    /// Target-specific catalog Application settings. Unspecified policy inherits from the ArgoCDInstance.
    #[serde(rename = "catalogApplication", default)]
    pub catalog_application: CatalogApplicationOverrides,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CatalogApplicationOverrides {
    /// Whether to generate a catalog Application recursively sourcing `_nyl/catalog`.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Catalog Application name. Defaults to the target name; names must be unique within the Argo CD instance.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Argo CD project override; inherits the instance catalog default when omitted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// Catalog synchronization override; inherits the instance default when omitted.
    #[serde(rename = "syncPolicy", skip_serializing_if = "Option::is_none")]
    pub sync_policy: Option<GitOpsSyncPolicy>,
    /// Application deletion override; inherits the instance default when omitted.
    #[serde(rename = "applicationDeletionPolicy", skip_serializing_if = "Option::is_none")]
    pub application_deletion_policy: Option<ApplicationDeletionPolicy>,
    /// Self-pruning override; inherits the instance default when omitted.
    #[serde(rename = "selfPrunePolicy", skip_serializing_if = "Option::is_none")]
    pub self_prune_policy: Option<ManagedResourceDeletionPolicy>,
    /// Labels added to generated Applications.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    /// Annotations added to generated Applications.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub annotations: BTreeMap<String, String>,
}

impl Default for CatalogApplicationOverrides {
    fn default() -> Self {
        Self {
            enabled: true,
            name: None,
            project: None,
            sync_policy: None,
            application_deletion_policy: None,
            self_prune_policy: None,
            labels: BTreeMap::new(),
            annotations: BTreeMap::new(),
        }
    }
}

/// Git coordinates used as a rendered output and Argo CD source.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
#[schemars(transform = super::schema::publication_constraints)]
pub struct GitPublication {
    /// Project-local GitRepository identity in `gitops.nyl/v1`. Exactly one of `repositoryRef` and `repository` is required.
    #[serde(rename = "repositoryRef", skip_serializing_if = "Option::is_none")]
    pub repository_ref: Option<LocalReference>,
    /// Inline Git coordinates. Exactly one of `repositoryRef` and `repository` is required.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repository: Option<InlineGitRepository>,
    /// Publication branch or revision for this target.
    pub revision: String,
    /// Normalized repository-relative output prefix. Defaults to the target name.
    #[serde(rename = "pathPrefix", skip_serializing_if = "Option::is_none")]
    pub path_prefix: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LocalReference {
    /// Project-local name of the referenced resource; its kind and API group are determined by the containing field.
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InlineGitRepository {
    /// Credential-free Git read URL used by renderers and generated Applications.
    #[serde(rename = "repoURL")]
    pub repo_url: String,
    /// Optional Git write URL. Publication uses `repoURL` when omitted.
    #[serde(rename = "publishURL", skip_serializing_if = "Option::is_none")]
    pub publish_url: Option<String>,
}

/// Defines an Argo CD AppProject manifest or an externally managed project contract.
///
/// Rendered projects are emitted into the catalog. External projects supply an admission contract without transferring ownership to Nyl.
///
/// ## When needed
///
/// Required when an ApplicationGroup uses `spec.projectRef`, including when the Argo CD AppProject is externally managed.
///
/// ## If omitted
///
/// An ApplicationGroup can use `spec.projectTemplate` to generate its AppProject instead. Exactly one of `projectRef` and `projectTemplate` must be set; Nyl does not assume an existing Argo CD project supplies the required policy contract.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
#[schemars(example = super::schema::resource_example(super::schema::ResourceKind::AppProjectDefinition))]
pub struct AppProjectDefinition {
    /// API group and version defining this resource.
    #[serde(rename = "apiVersion")]
    pub api_version: String,
    /// Resource kind within the API group.
    pub kind: String,
    /// Resource identity and metadata.
    pub metadata: GitOpsResourceMetadata,
    /// Configuration for this resource.
    pub spec: AppProjectDefinitionSpec,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AppProjectDefinitionSpec {
    /// Whether Nyl emits the AppProject or validates against an externally managed project contract.
    pub management: AppProjectManagement,
    /// GitRepository names admitted as source repositories. Valid only for a Rendered project; duplicate names are rejected.
    #[serde(default, rename = "sourceRepositoryRefs", skip_serializing_if = "Vec::is_empty")]
    pub source_repository_refs: Vec<LocalReference>,
    /// Argo CD `argoproj.io/v1alpha1` AppProject manifest. Its metadata name supplies the Argo CD project identity.
    pub manifest: serde_json::Value,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub enum AppProjectManagement {
    /// Emit this AppProject into the target catalog.
    Rendered,
    /// Use the project contract for validation without emitting or owning the AppProject.
    External,
}

/// Selects Kubernetes Releases and defines their Argo CD Application, project, and namespace policies.
///
/// Literal metadata labels participate in target selection; the spec is rendered afterward. This is compiler configuration, not a workload manifest.
///
/// ## When needed
///
/// Required to select Release entry files and generate their workload Applications in rendered GitOps.
///
/// ## If omitted
///
/// Nyl does not infer groups from directories or Release files. Without a selected, enabled ApplicationGroup, a target emits no workload Applications. Direct file rendering does not require an ApplicationGroup.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
#[schemars(example = super::schema::resource_example(super::schema::ResourceKind::ApplicationGroup))]
pub struct ApplicationGroup {
    /// API group and version defining this resource.
    #[serde(rename = "apiVersion")]
    pub api_version: String,
    /// Resource kind within the API group.
    pub kind: String,
    /// Resource identity and metadata.
    pub metadata: GitOpsResourceMetadata,
    /// Configuration for this resource.
    pub spec: ApplicationGroupSpec,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
#[schemars(transform = super::schema::application_group_constraints)]
pub struct ApplicationGroupSpec {
    /// Whether to render this selected group. Evaluated after target selection and may be templated.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Release source selection. Omission derives `applications/<group-name>` for central groups or the containing directory for `_application-group.yaml`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<ApplicationGroupSource>,
    /// Project-local AppProjectDefinition identity. Exactly one of `projectRef` and `projectTemplate` is required.
    #[serde(rename = "projectRef", skip_serializing_if = "Option::is_none")]
    pub project_ref: Option<String>,
    /// Constrained generated AppProject. Exactly one of `projectRef` and `projectTemplate` is required.
    #[serde(rename = "projectTemplate", skip_serializing_if = "Option::is_none")]
    pub project_template: Option<AppProjectTemplate>,
    /// Namespace containing generated Argo CD Applications.
    #[serde(rename = "applicationNamespace")]
    pub application_namespace: String,
    /// Workload namespace override. Defaults to each Release namespace.
    #[serde(rename = "destinationNamespace", skip_serializing_if = "Option::is_none")]
    pub destination_namespace: Option<String>,
    /// Relative output directory beneath the target prefix. Defaults to the group name.
    #[serde(rename = "outputPath", skip_serializing_if = "Option::is_none")]
    pub output_path: Option<String>,
    /// Generated workload Application name template, with `release` context. Defaults to the Release name; use target-qualified templates when targets share an Argo CD namespace.
    #[serde(rename = "applicationNameTemplate", skip_serializing_if = "Option::is_none")]
    pub application_name_template: Option<String>,
    /// Workload Application sync policy. Generated Applications include apply-only-out-of-sync and server-side apply unless explicitly overridden.
    #[serde(rename = "syncPolicy", skip_serializing_if = "Option::is_none")]
    pub sync_policy: Option<GitOpsSyncPolicy>,
    /// Argo CD Application finalizer policy: foreground cascade, background cascade, or orphan workloads.
    #[serde(rename = "applicationDeletionPolicy", default)]
    pub application_deletion_policy: ApplicationDeletionPolicy,
    /// Namespace creation and deletion policy for workload-owned namespaces.
    #[serde(default)]
    pub namespace: ManagedNamespacePolicy,
    /// Explicit ownership by namespace name. Every consuming group must agree. Kubernetes bootstrap namespaces are externally owned unless explicitly delegated here.
    #[serde(rename = "sharedNamespaces", default, skip_serializing_if = "BTreeMap::is_empty")]
    pub shared_namespaces: BTreeMap<String, SharedNamespacePolicy>,
    /// Limits on per-release Application overrides. Releases cannot expand the generated project or namespace policy.
    #[serde(rename = "releaseCustomization", default)]
    pub release_customization: GitOpsReleaseCustomization,
    /// Labels added to generated Applications.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    /// Annotations added to generated Applications.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub annotations: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct LabelSelector {
    /// Exact label matches, combined with AND. An empty map selects every ApplicationGroup.
    #[serde(default, rename = "matchLabels", skip_serializing_if = "BTreeMap::is_empty")]
    pub match_labels: BTreeMap<String, String>,
}

/// A constrained AppProject generated for one ApplicationGroup and target.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AppProjectTemplate {
    /// Generated AppProject name; defaults to the ApplicationGroup name. Must be unambiguous across targets sharing an Argo CD namespace.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Permitted destination namespace patterns. Must cover all effective Release namespaces and additional namespaces; required when no fixed destination namespace is configured.
    #[serde(default, rename = "destinationNamespaces", skip_serializing_if = "Vec::is_empty")]
    pub destination_namespaces: Vec<String>,
    /// Explicit cluster-scoped resource permissions. Namespace permissions are added for approved namespaces when creation is enabled.
    #[serde(default, rename = "clusterResourceWhitelist", skip_serializing_if = "Vec::is_empty")]
    pub cluster_resource_whitelist: Vec<AppProjectResourcePattern>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct AppProjectResourcePattern {
    /// Kubernetes API group pattern, such as `apiextensions.k8s.io` or `*`.
    pub group: String,
    /// Kubernetes resource kind pattern.
    pub kind: String,
    /// Optional resource name pattern.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// A local source when no repository is given, otherwise an immutable remote source.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
#[schemars(transform = super::schema::source_constraints)]
pub struct ApplicationGroupSource {
    /// GitRepository name for a remote source. Mutually exclusive with inline `repository`; requires `revision` and `commit`.
    #[serde(rename = "repositoryRef", skip_serializing_if = "Option::is_none")]
    pub repository_ref: Option<LocalReference>,
    /// Inline remote Git coordinates. Mutually exclusive with `repositoryRef`; requires `revision` and `commit`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repository: Option<InlineGitRepository>,
    /// Human-readable remote Git revision refreshed by `nyl update source-locks`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    /// Full immutable remote Git commit lock used for rendering.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// Normalized project-relative source directory; relative to the checkout for a remote source.
    pub path: String,
    /// Relative candidate file globs. Only entries with a literal Release are rendered; attach other files using Release `spec.include`.
    #[serde(default = "default_source_include")]
    pub include: Vec<String>,
    /// Relative file globs removed after inclusion.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
    /// Whether source discovery descends into subdirectories.
    #[serde(default = "default_true")]
    pub recursive: bool,
    /// Whether remote rendering uses central configuration or the remote project. Remote sessions cannot access secrets or the process environment.
    #[serde(rename = "rendererConfig", default)]
    pub renderer_config: RendererConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RendererConfig {
    /// Central uses platform configuration; Remote loads the remote project and requires a remote source.
    #[serde(default)]
    pub mode: RendererConfigMode,
    /// Remote project root, relative to the checkout. Defaults to `.`; valid only in Remote mode and must remain inside the checkout.
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
    /// Use the central platform project configuration.
    Central,
    /// Load project configuration from the remote source checkout with restricted inputs.
    Remote,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GitOpsSyncPolicy {
    /// Presence enables automated synchronization unless `enabled` is false.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub automated: Option<GitOpsAutomatedSyncPolicy>,
    /// Argo CD sync options. Generated Applications add `ApplyOutOfSyncOnly=true` and `ServerSideApply=true` unless an option with the same key is supplied.
    #[serde(default, rename = "syncOptions", skip_serializing_if = "Vec::is_empty")]
    pub sync_options: Vec<String>,
}

impl GitOpsSyncPolicy {
    pub(crate) fn add_generated_application_defaults(&mut self) {
        for default in ["ApplyOutOfSyncOnly=true", "ServerSideApply=true"] {
            let default_key = sync_option_key(default);
            if !self
                .sync_options
                .iter()
                .any(|option| sync_option_key(option) == default_key)
            {
                self.sync_options.push(default.to_owned());
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GitOpsAutomatedSyncPolicy {
    /// Whether automated sync is enabled. Omission enables it when the automated block is present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Allow Argo CD to prune resources removed from desired manifests.
    #[serde(default)]
    pub prune: bool,
    /// Allow Argo CD to repair live drift automatically.
    #[serde(default, rename = "selfHeal")]
    pub self_heal: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Default)]
pub enum ApplicationDeletionPolicy {
    #[default]
    /// Cascade deletion in the foreground using the Argo CD resources finalizer.
    Foreground,
    /// Cascade deletion in the background using the Argo CD background finalizer.
    Background,
    /// Omit the finalizer and leave workload resources in place.
    Orphan,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ManagedNamespacePolicy {
    /// Synthesize missing destination and additional Namespaces within the owning workload Application.
    #[serde(default = "default_true")]
    pub create: bool,
    /// Namespace pruning policy: Automatic, Confirm, or Retain.
    #[serde(rename = "prunePolicy", default)]
    pub prune_policy: ManagedResourceDeletionPolicy,
    /// Namespace deletion policy when the owning Application is deleted.
    #[serde(rename = "deletePolicy", default)]
    pub delete_policy: ManagedResourceDeletionPolicy,
}

/// Explicit authorization and ownership for a namespace consumed by more than
/// one generated workload Application.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SharedNamespacePolicy {
    /// The sole owner of a shared Namespace. Release and Dedicated owners identify the responsible ApplicationGroup.
    pub owner: SharedNamespaceOwner,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(tag = "kind", deny_unknown_fields)]
pub enum SharedNamespaceOwner {
    /// The selected workload Release owns the Namespace; other workloads must not render its Namespace object.
    Release {
        #[serde(rename = "applicationGroup")]
        /// ApplicationGroup owning the Namespace Application or selected workload Release.
        application_group: String,
        /// Release name within the owning ApplicationGroup.
        release: String,
    },
    /// Generate a dedicated Namespace Application under the selected group; workloads must not render this Namespace.
    Dedicated {
        #[serde(rename = "applicationGroup")]
        /// ApplicationGroup owning the Namespace Application or selected workload Release.
        application_group: String,
    },
    /// The Namespace is externally owned. Nyl neither synthesizes nor accepts an authored Namespace object.
    External,
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
    /// Allow pruning or deletion without additional restrictions.
    Automatic,
    #[default]
    /// Require confirmation through the Argo CD Prune or Delete sync option.
    Confirm,
    /// Set the Argo CD Prune or Delete sync option to false.
    Retain,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct GitOpsReleaseCustomization {
    /// Dotted field globs admitted in Release Application overrides. `*` matches one segment; `**` matches multiple segments. Core ownership fields remain protected.
    #[serde(default, rename = "allowedPaths", skip_serializing_if = "Vec::is_empty")]
    pub allowed_paths: Vec<String>,
    /// Dotted field globs forbidden in Release Application overrides. Deny takes precedence over allow.
    #[serde(default, rename = "deniedPaths", skip_serializing_if = "Vec::is_empty")]
    pub denied_paths: Vec<String>,
    /// Exact allowed values for Release `spec.syncPolicy.+syncOptions` overrides. Same-key values replace the generated option.
    #[serde(default, rename = "allowedSyncOptions", skip_serializing_if = "Vec::is_empty")]
    pub allowed_sync_options: Vec<String>,
}

/// The supported static kind from a GitOps control-resource envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum GitOpsResourceKind {
    #[value(name = "GitRepository", alias = "git-repository", alias = "repository")]
    GitRepository,
    #[value(name = "Cluster", alias = "cluster")]
    Cluster,
    #[value(name = "ArgoCDInstance", alias = "argocd-instance", alias = "argocd")]
    ArgoCDInstance,
    #[value(name = "DeploymentTarget", alias = "deployment-target", alias = "target")]
    DeploymentTarget,
    #[value(name = "AppProjectDefinition", alias = "app-project-definition", alias = "project")]
    AppProjectDefinition,
    #[value(name = "ApplicationGroup", alias = "application-group", alias = "group")]
    ApplicationGroup,
}

impl GitOpsResourceKind {
    /// API version of this control resource.
    pub const fn api_version(self) -> &'static str {
        match self {
            Self::GitRepository => API_VERSION_GITOPS,
            _ => API_VERSION_K8S_GITOPS,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::GitRepository => KIND_GIT_REPOSITORY,
            Self::Cluster => KIND_CLUSTER,
            Self::ArgoCDInstance => KIND_ARGOCD_INSTANCE,
            Self::DeploymentTarget => KIND_DEPLOYMENT_TARGET,
            Self::AppProjectDefinition => KIND_APP_PROJECT_DEFINITION,
            Self::ApplicationGroup => KIND_APPLICATION_GROUP,
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            KIND_GIT_REPOSITORY => Some(Self::GitRepository),
            KIND_CLUSTER => Some(Self::Cluster),
            KIND_ARGOCD_INSTANCE => Some(Self::ArgoCDInstance),
            KIND_DEPLOYMENT_TARGET => Some(Self::DeploymentTarget),
            KIND_APP_PROJECT_DEFINITION => Some(Self::AppProjectDefinition),
            KIND_APPLICATION_GROUP => Some(Self::ApplicationGroup),
            _ => None,
        }
    }

    pub const fn schema_filename(self) -> &'static str {
        match self {
            Self::GitRepository => "git-repository.schema.json",
            Self::Cluster => "cluster.schema.json",
            Self::ArgoCDInstance => "argocd-instance.schema.json",
            Self::DeploymentTarget => "deployment-target.schema.json",
            Self::AppProjectDefinition => "app-project-definition.schema.json",
            Self::ApplicationGroup => "application-group.schema.json",
        }
    }

    pub const fn all() -> [Self; 6] {
        [
            Self::GitRepository,
            Self::Cluster,
            Self::ArgoCDInstance,
            Self::DeploymentTarget,
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
        GitOpsResourceKind::ArgoCDInstance => serde_json::to_value(schema_for!(ArgoCDInstance)),
        GitOpsResourceKind::DeploymentTarget => serde_json::to_value(schema_for!(DeploymentTarget)),
        GitOpsResourceKind::AppProjectDefinition => serde_json::to_value(schema_for!(AppProjectDefinition)),
        GitOpsResourceKind::ApplicationGroup => serde_json::to_value(schema_for!(ApplicationGroup)),
    }
    .expect("schema serialization should never fail");
    tighten_resource_envelope(schema, kind)
}

fn tighten_resource_envelope(mut schema: serde_json::Value, kind: GitOpsResourceKind) -> serde_json::Value {
    super::schema::set_envelope(&mut schema, kind.api_version(), Some(kind.as_str()));
    schema
}

/// Generate a portable aggregate schema with relative references to all kinds.
pub fn generate_gitops_aggregate_schema() -> serde_json::Value {
    let mut references = GitOpsResourceKind::all()
        .iter()
        .map(|kind| serde_json::json!({"$ref": kind.schema_filename()}))
        .collect::<Vec<_>>();
    references.push(serde_json::json!({"$ref": super::RELEASE_SCHEMA_FILENAME}));
    serde_json::json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "Nyl GitOps resource",
        "description": "A local Nyl rendered-GitOps compiler resource.",
        "oneOf": references
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitOpsResourceIdentity {
    /// Resource kind within the API group.
    pub kind: GitOpsResourceKind,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum GitOpsResource {
    GitRepository(GitRepository),
    Cluster(Cluster),
    ArgoCDInstance(ArgoCDInstance),
    DeploymentTarget(DeploymentTarget),
    AppProjectDefinition(AppProjectDefinition),
    ApplicationGroup(Box<ApplicationGroup>),
}

/// True when the manifest advertises the GitOps API and one of its known kinds.
pub fn is_gitops_resource(value: &serde_json::Value) -> bool {
    gitops_resource_kind(value).is_some()
}

pub fn gitops_resource_kind(value: &serde_json::Value) -> Option<GitOpsResourceKind> {
    let kind = GitOpsResourceKind::parse(value.get("kind")?.as_str()?)?;
    (value.get("apiVersion")?.as_str()? == kind.api_version()).then_some(kind)
}

/// Parse only the static envelope. Unrelated manifests return `None`.
pub fn parse_gitops_resource_identity(value: &serde_json::Value) -> Result<Option<GitOpsResourceIdentity>> {
    super::schema::validate_resource_api(value)?;
    let api_version = value.get("apiVersion").and_then(serde_json::Value::as_str);
    if !matches!(api_version, Some(API_VERSION_GITOPS | API_VERSION_K8S_GITOPS)) {
        return Ok(None);
    }
    let kind_text = value
        .get("kind")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| NylError::config("GitOps resource kind must be a static string"))?;
    // Release is discovered by the workload bundle loader.
    if kind_text == super::KIND_RELEASE {
        return Ok(None);
    }
    let kind = GitOpsResourceKind::parse(kind_text).ok_or_else(|| {
        NylError::config(format!(
            "Unsupported {} kind {kind_text:?}",
            api_version.unwrap_or_default()
        ))
    })?;
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
        GitOpsResourceKind::ArgoCDInstance => GitOpsResource::ArgoCDInstance(parse_as(value, KIND_ARGOCD_INSTANCE)?),
        GitOpsResourceKind::DeploymentTarget => {
            let mut target: DeploymentTarget = parse_as(value, KIND_DEPLOYMENT_TARGET)?;
            target.apply_defaults();
            GitOpsResource::DeploymentTarget(target)
        }
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
            Self::ArgoCDInstance(resource) => resource.validate(),
            Self::DeploymentTarget(resource) => resource.validate(),
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

impl ArgoCDInstance {
    pub fn validate(&self) -> Result<()> {
        validate_envelope(
            self.api_version.as_str(),
            self.kind.as_str(),
            KIND_ARGOCD_INSTANCE,
            &self.metadata,
        )?;
        validate_static_required("spec.clusterRef.name", &self.spec.cluster_ref.name)?;
        super::release::validate_namespace_name("spec.namespace", &self.spec.namespace)?;
        validate_static_required(
            "spec.catalogApplicationDefaults.project",
            &self.spec.catalog_application_defaults.project,
        )?;
        Ok(())
    }
}

impl DeploymentTarget {
    /// Materialize target-relative defaults so downstream rendering and
    /// templates observe the effective configuration.
    pub fn apply_defaults(&mut self) {
        self.spec.cluster_ref.get_or_insert_with(|| LocalReference {
            name: self.metadata.name.clone(),
        });
        self.spec
            .publication
            .path_prefix
            .get_or_insert_with(|| self.metadata.name.clone());
    }

    pub fn cluster_name(&self) -> &str {
        self.spec
            .cluster_ref
            .as_ref()
            .map_or(self.metadata.name.as_str(), |reference| reference.name.as_str())
    }

    pub fn publication_path_prefix(&self) -> &str {
        self.spec
            .publication
            .path_prefix
            .as_deref()
            .unwrap_or(&self.metadata.name)
    }

    pub fn validate(&self) -> Result<()> {
        validate_envelope(
            self.api_version.as_str(),
            self.kind.as_str(),
            KIND_DEPLOYMENT_TARGET,
            &self.metadata,
        )?;
        validate_static_required("spec.clusterRef.name", self.cluster_name())?;
        if let Some(reference) = &self.spec.argocd_ref {
            validate_static_required("spec.argocdRef.name", &reference.name)?;
        }
        self.spec.publication.validate()?;
        validate_relative_path(
            "spec.publication.pathPrefix",
            self.publication_path_prefix(),
            true,
            false,
        )?;
        if let Some(name) = &self.spec.catalog_application.name {
            validate_dns_subdomain("spec.catalogApplication.name", name)?;
        }
        if let Some(project) = &self.spec.catalog_application.project {
            validate_static_required("spec.catalogApplication.project", project)?;
        }
        Ok(())
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
        if let Some(path_prefix) = &self.path_prefix {
            validate_relative_path("spec.publication.pathPrefix", path_prefix, true, false)?;
        }
        Ok(())
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
        let spec = object.get("spec").and_then(serde_json::Value::as_object);
        if spec.is_none() {
            return Err(NylError::config("spec.manifest.spec must be an object"));
        }
        if let Some(source_repos) = spec.and_then(|spec| spec.get("sourceRepos")) {
            let valid = source_repos
                .as_array()
                .is_some_and(|values| values.iter().all(serde_json::Value::is_string));
            if !valid {
                return Err(NylError::config(
                    "spec.manifest.spec.sourceRepos must be an array of strings",
                ));
            }
        }
        if self.spec.management == AppProjectManagement::External && !self.spec.source_repository_refs.is_empty() {
            return Err(NylError::config(
                "spec.sourceRepositoryRefs is only valid when spec.management is Rendered",
            ));
        }
        let mut repository_refs = BTreeSet::new();
        for reference in &self.spec.source_repository_refs {
            validate_static_required("spec.sourceRepositoryRefs[].name", &reference.name)?;
            if !repository_refs.insert(&reference.name) {
                return Err(NylError::config(format!(
                    "spec.sourceRepositoryRefs contains duplicate GitRepository reference {:?}",
                    reference.name
                )));
            }
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
        match (&self.spec.project_ref, &self.spec.project_template) {
            (Some(reference), None) => validate_static_required("spec.projectRef", reference)?,
            (None, Some(template)) => template.validate()?,
            (Some(_), Some(_)) => {
                return Err(NylError::config(
                    "spec.projectRef and spec.projectTemplate are mutually exclusive",
                ))
            }
            (None, None) => {
                return Err(NylError::config(
                    "Exactly one of spec.projectRef or spec.projectTemplate is required",
                ))
            }
        }
        validate_required("spec.applicationNamespace", &self.spec.application_namespace)?;
        if let Some(namespace) = &self.spec.destination_namespace {
            super::release::validate_namespace_name("spec.destinationNamespace", namespace)?;
        }
        for (namespace, policy) in &self.spec.shared_namespaces {
            super::release::validate_namespace_name("spec.sharedNamespaces key", namespace)?;
            match &policy.owner {
                SharedNamespaceOwner::Release {
                    application_group,
                    release,
                } => {
                    validate_dns_subdomain("spec.sharedNamespaces.*.owner.applicationGroup", application_group)?;
                    validate_dns_subdomain("spec.sharedNamespaces.*.owner.release", release)?;
                }
                SharedNamespaceOwner::Dedicated { application_group } => {
                    validate_dns_subdomain("spec.sharedNamespaces.*.owner.applicationGroup", application_group)?;
                }
                SharedNamespaceOwner::External => {}
            }
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
        validate_unique_static_names(
            "spec.releaseCustomization.allowedSyncOptions",
            &self.spec.release_customization.allowed_sync_options,
        )?;
        Ok(())
    }
}

impl AppProjectTemplate {
    fn validate(&self) -> Result<()> {
        if let Some(name) = &self.name {
            validate_dns_subdomain("spec.projectTemplate.name", name)?;
        }
        validate_unique_static_names(
            "spec.projectTemplate.destinationNamespaces",
            &self.destination_namespaces,
        )?;
        for namespace in &self.destination_namespaces {
            validate_namespace_pattern("spec.projectTemplate.destinationNamespaces", namespace)?;
        }
        for pattern in &self.cluster_resource_whitelist {
            validate_static_required("spec.projectTemplate.clusterResourceWhitelist[].kind", &pattern.kind)?;
            if let Some(name) = &pattern.name {
                validate_static_required("spec.projectTemplate.clusterResourceWhitelist[].name", name)?;
            }
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
    let expected_api = GitOpsResourceKind::parse(expected_kind)
        .expect("known control kind")
        .api_version();
    if api_version != expected_api {
        return Err(NylError::config(format!(
            "{expected_kind} apiVersion must be {expected_api:?}"
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

fn validate_namespace_pattern(field: &str, value: &str) -> Result<()> {
    validate_static_required(field, value)?;
    if value.contains('/') || value.len() > 253 {
        return Err(NylError::config(format!(
            "{field} must be an Argo CD namespace glob without slashes"
        )));
    }
    glob::Pattern::new(value)
        .map(|_| ())
        .map_err(|error| NylError::config(format!("Invalid {field} glob {value:?}: {error}")))
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

pub(crate) fn validate_repository_coordinates(repo_url: &str, publish_url: Option<&str>) -> Result<()> {
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

fn default_argocd_namespace() -> String {
    "argocd".to_owned()
}

fn default_argocd_project() -> String {
    "default".to_owned()
}

fn default_catalog_sync_policy() -> GitOpsSyncPolicy {
    let mut policy = GitOpsSyncPolicy {
        automated: None,
        sync_options: Vec::new(),
    };
    policy.add_generated_application_defaults();
    policy
}

fn sync_option_key(option: &str) -> &str {
    option.split_once('=').map_or(option, |(key, _)| key)
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
            "apiVersion": API_VERSION_K8S_GITOPS,
            "kind": KIND_DEPLOYMENT_TARGET,
            "metadata": {"name": "production", "labels": {"environment": "production"}},
            "spec": {
                "clusterRef": {"name": "kasoku"},
                "values": {"replicas": 3},
                "publication": {
                    "repositoryRef": {"name": "deploy"},
                    "revision": "deploy/production"
                }
            }
        })
    }

    fn cluster() -> serde_json::Value {
        json!({
            "apiVersion": API_VERSION_K8S_GITOPS,
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

    fn argocd_instance() -> serde_json::Value {
        json!({
            "apiVersion": API_VERSION_K8S_GITOPS,
            "kind": KIND_ARGOCD_INSTANCE,
            "metadata": {"name": "central"},
            "spec": {"clusterRef": {"name": "kasoku"}}
        })
    }

    fn application_group() -> serde_json::Value {
        json!({
            "apiVersion": API_VERSION_K8S_GITOPS,
            "kind": KIND_APPLICATION_GROUP,
            "metadata": {"name": "cloud"},
            "spec": {
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
        let GitOpsResource::DeploymentTarget(parsed) = parsed else {
            panic!("expected target");
        };
        assert_eq!(parsed.metadata.name, "production");
        assert_eq!(parsed.cluster_name(), "kasoku");
        assert_eq!(parsed.spec.values["replicas"], 3);
        assert_eq!(parsed.publication_path_prefix(), "production");
    }

    #[test]
    fn deployment_target_defaults_same_named_cluster_and_preserves_explicit_root_prefix() {
        let mut value = target();
        value["spec"].as_object_mut().unwrap().remove("clusterRef");
        value["spec"]["publication"]["pathPrefix"] = json!("");
        let GitOpsResource::DeploymentTarget(parsed) = parse_gitops_resource(&value).unwrap().unwrap() else {
            panic!("expected target");
        };
        assert_eq!(parsed.cluster_name(), "production");
        assert_eq!(parsed.publication_path_prefix(), "");
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
    fn parses_argocd_instance_security_defaults() {
        let GitOpsResource::ArgoCDInstance(parsed) = parse_gitops_resource(&argocd_instance()).unwrap().unwrap() else {
            panic!("expected ArgoCD instance");
        };
        assert_eq!(parsed.spec.namespace, "argocd");
        let defaults = parsed.spec.catalog_application_defaults;
        assert_eq!(defaults.project, "default");
        assert_eq!(
            defaults.application_deletion_policy,
            ApplicationDeletionPolicy::Foreground
        );
        assert_eq!(defaults.self_prune_policy, ManagedResourceDeletionPolicy::Confirm);
        assert!(defaults.sync_policy.automated.is_none());
    }

    #[test]
    fn automated_sync_accepts_argocd_enabled_semantics() {
        let mut value = argocd_instance();
        value["spec"]["catalogApplicationDefaults"] = json!({
            "syncPolicy": {
                "automated": {"prune": true, "selfHeal": true}
            }
        });
        let GitOpsResource::ArgoCDInstance(parsed) = parse_gitops_resource(&value).unwrap().unwrap() else {
            panic!("expected ArgoCD instance");
        };
        let automated = parsed.spec.catalog_application_defaults.sync_policy.automated.unwrap();
        assert_eq!(automated.enabled, None);
        assert!(serde_json::to_value(&automated).unwrap().get("enabled").is_none());
        assert!(automated.prune);
        assert!(automated.self_heal);

        value["spec"]["catalogApplicationDefaults"]["syncPolicy"]["automated"]["enabled"] = json!(false);
        let GitOpsResource::ArgoCDInstance(parsed) = parse_gitops_resource(&value).unwrap().unwrap() else {
            panic!("expected ArgoCD instance");
        };
        let automated = parsed.spec.catalog_application_defaults.sync_policy.automated.unwrap();
        assert_eq!(automated.enabled, Some(false));
        assert_eq!(serde_json::to_value(automated).unwrap()["enabled"], false);
    }

    #[test]
    fn application_group_requires_exactly_one_project_source() {
        let mut value = application_group();
        value["spec"]["projectTemplate"] = json!({"destinationNamespaces": ["cloud"]});
        assert!(parse_gitops_resource(&value)
            .unwrap_err()
            .to_string()
            .contains("mutually exclusive"));
        value["spec"].as_object_mut().unwrap().remove("projectRef");
        assert!(parse_gitops_resource(&value).is_ok());
        value["spec"].as_object_mut().unwrap().remove("projectTemplate");
        assert!(parse_gitops_resource(&value)
            .unwrap_err()
            .to_string()
            .contains("Exactly one"));
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
                kind: GitOpsResourceKind::DeploymentTarget,
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
            "apiVersion": API_VERSION_K8S_GITOPS,
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
    fn validates_shared_namespace_owners() {
        let mut value = application_group();
        value["spec"]["sharedNamespaces"] = json!({
            "monitoring": {
                "owner": {
                    "kind": "Release",
                    "applicationGroup": "cloud",
                    "release": "prometheus"
                }
            },
            "kube-system": {"owner": {"kind": "External"}}
        });
        assert!(parse_gitops_resource(&value).is_ok());

        value["spec"]["sharedNamespaces"] = json!({
            "not.valid": {"owner": {"kind": "External"}}
        });
        assert!(parse_gitops_resource(&value).is_err());

        value["spec"]["sharedNamespaces"] = json!({
            "monitoring": {
                "owner": {
                    "kind": "Release",
                    "applicationGroup": "cloud"
                }
            }
        });
        assert!(parse_gitops_resource(&value).is_err());
    }

    #[test]
    fn validates_allowed_release_sync_options() {
        let mut value = application_group();
        value["spec"]["releaseCustomization"] = json!({
            "allowedSyncOptions": ["RespectIgnoreDifferences=false"]
        });
        assert!(parse_gitops_resource(&value).is_ok());

        value["spec"]["releaseCustomization"]["allowedSyncOptions"] =
            json!(["RespectIgnoreDifferences=false", "RespectIgnoreDifferences=false"]);
        let error = parse_gitops_resource(&value).unwrap_err().to_string();
        assert!(error.contains("contains duplicate value"));
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
            "apiVersion": API_VERSION_K8S_GITOPS,
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
        let mut invalid = valid.clone();
        invalid["spec"]["manifest"]["kind"] = json!("Application");
        assert!(parse_gitops_resource(&invalid).is_err());

        let mut with_references = valid.clone();
        with_references["spec"]["sourceRepositoryRefs"] = json!([{"name": "deploy"}]);
        assert!(parse_gitops_resource(&with_references).is_ok());
        with_references["spec"]["sourceRepositoryRefs"] = json!([{"name": "deploy"}, {"name": "deploy"}]);
        assert!(parse_gitops_resource(&with_references)
            .unwrap_err()
            .to_string()
            .contains("duplicate GitRepository reference"));

        let mut external = valid;
        external["spec"]["management"] = json!("External");
        external["spec"]["sourceRepositoryRefs"] = json!([{"name": "deploy"}]);
        assert!(parse_gitops_resource(&external)
            .unwrap_err()
            .to_string()
            .contains("only valid when spec.management is Rendered"));
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
        let schema = schemars::schema_for!(DeploymentTarget);
        assert!(serde_json::to_value(schema).unwrap().is_object());
    }

    #[test]
    fn generated_schemas_have_constant_resource_envelopes() {
        for kind in GitOpsResourceKind::all() {
            let schema = generate_gitops_resource_schema(kind);
            assert_eq!(schema["properties"]["apiVersion"]["const"], kind.api_version());
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
                "argocd-instance.schema.json",
                "deployment-target.schema.json",
                "app-project-definition.schema.json",
                "application-group.schema.json",
                "release.schema.json"
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
        for (path, expected) in crate::resources::schema::schema_artifacts() {
            let published = fs::read_to_string(schema_directory.join(&path)).unwrap();
            let published: serde_json::Value = serde_json::from_str(&published).unwrap();
            assert_eq!(
                published, expected,
                "Regenerate published schema {path} with nyl schema all"
            );
        }
    }
}
