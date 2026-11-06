//! What an observer of a published target needs: where its reconciler is,
//! which deployments the target expects it to run, and the publication
//! history that matches a running revision to a publication (the Git side).
//!
//! Contract: [Kubernetes publications](../../../../design/promotion.md#kubernetes-publications)
//! (observer, running revision, matching a publication, recorded commit).
//! Nothing here reads a reconciler: reading it and judging health belong to
//! the publication driver, which asks this module only for declared facts
//! ([`ReconcilerFacts`]) and Git answers ([`PublicationHistory`]).
//!
//! - [`ReconcilerFacts`] is what a driver records at resolution under
//!   `/reconciler`: which reconciler runs the target and where
//!   ([`ReconcilerLocation`], tagged by `kind`), the publication it reads,
//!   and what the target expects of it ([`Expected`]): the deployments it
//!   should run ([`ExpectedDeployment`]) and the anchors that show a reading
//!   comes from the reconciler that runs this target ([`Anchor`]). So that
//!   `attest` and an `observe` teardown wait need no render, they read the
//!   stored document. Only the location is specific to a reconciler kind,
//!   and each deployment and anchor carries an opaque `locator` that only
//!   that kind's reader interprets.
//! - [`ArgoCdLocation`], [`GeneratedApplication`], and
//!   [`generated_applications`] are the Argo CD renderer: they derive the
//!   expected deployments from the Applications the target generates. A
//!   second reconciler kind adds its own location variant and renderer here,
//!   and a reader in the publication driver.
//! - [`PublicationHistory`] answers the matching questions over the
//!   publication branch. [`running_publication`] applies the rule: the
//!   publication commits that are the running revision or its ancestors and
//!   whose deployment directory tree and recorded inputs equal those at the
//!   running revision form a run, and the recorded commit is the oldest of
//!   the unbroken run ending at it. [`GitPublicationHistory`] implements the
//!   trait over a fetched copy of the branch.

use std::collections::BTreeMap;
use std::path::Path;

use git2::{Oid, Repository};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::publication::{PublicationDestination, RenderedTarget};
use super::reconcile::{parse_index, DEFAULT_INDEX_PATH};
use crate::resources::{ArgoCDAccess, ArgoCDAccessMethod};
use crate::{NylError, Result};

/// Where a target's Argo CD instance is and how Nyl reads it, from the
/// declaration (the instance, its control-plane Cluster, and the target).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArgoCdLocation {
    /// The ArgoCDInstance's name, or `implicit:<target>` without explicit instances.
    pub instance: String,
    /// The namespace holding the instance's Applications and AppProjects.
    pub namespace: String,
    /// The control-plane Cluster.
    pub cluster: String,
    /// The Cluster's declared local kubeconfig context (`spec.live.context`), if any.
    pub context: Option<String>,
    /// The declared access, the Kubernetes API of the Cluster by default.
    pub access: ArgoCDAccess,
}

/// Where a direct target's Releases are applied and how Nyl reads them: the
/// workload Cluster, its declared local context, and the server its
/// destination declares.
///
/// Contract: [Direct reconciler](../../../../design/orchestration-core.md#direct-reconciler).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DirectLocation {
    /// The workload Cluster's name.
    pub cluster: String,
    /// The Cluster's declared local kubeconfig context (`spec.live.context`), if any.
    pub context: Option<String>,
    /// The Cluster's declared destination server when a context can be
    /// verified against it (a concrete URL, not the in-cluster address and not
    /// a destination by name). A connection whose server equals it proves the
    /// cluster itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
}

/// How a target is delivered, as the render knows it: the declaration the
/// compile resolved, which enters no rendered byte.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delivery {
    /// Argo CD syncs the published tree.
    ArgoCd(ArgoCdLocation),
    /// Nyl applies the Releases itself.
    Direct(DirectLocation),
}

impl Delivery {
    /// The reconciler's recorded kind: `argocd` or `direct`.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::ArgoCd(_) => "argocd",
            Self::Direct(_) => "direct",
        }
    }
}

/// The reconciler that runs a target and where Nyl reads it. The tag is the
/// recorded `kind`; a new reconciler kind is a new variant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum ReconcilerLocation {
    #[serde(rename = "argocd")]
    ArgoCd(ArgoCdLocation),
    #[serde(rename = "direct")]
    Direct(DirectLocation),
}

impl ReconcilerLocation {
    /// The recorded fields of this kind's location (JSON Pointers relative to
    /// the facts) that say only how the reconciler is read, never what the
    /// unit produces: renaming a kube context or moving an API server must
    /// not re-execute the unit. A new kind names its own.
    pub fn unkeyed(&self) -> &'static [&'static str] {
        match self {
            Self::ArgoCd(_) => &["/access", "/context"],
            Self::Direct(_) => &["/context"],
        }
    }

    /// The reconciler's kind and name, as an attestation's details name them:
    /// `{kind, instance, ...}` with whatever identifies how it is read.
    pub fn summary(&self) -> Value {
        match self {
            Self::ArgoCd(location) => serde_json::json!({
                "kind": "argocd",
                "instance": location.instance,
                "namespace": location.namespace,
                "access": match location.access.method {
                    ArgoCDAccessMethod::Kubernetes => "kubernetes",
                    ArgoCDAccessMethod::Api => "api",
                },
            }),
            Self::Direct(location) => serde_json::json!({
                "kind": "direct",
                "cluster": location.cluster,
            }),
        }
    }
}

/// Where a target's rendered resources live: its workload Cluster, with that
/// Cluster's declared local context (`spec.live.context`). It may differ from
/// the control-plane Cluster of the reconciler.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkloadCluster {
    /// The Cluster's name.
    pub cluster: String,
    /// The Cluster's declared local kubeconfig context, if any.
    pub context: Option<String>,
}

/// The part a deployment plays in a target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DeploymentRole {
    /// The deployment sourcing `_nyl/catalog`, which creates the others.
    Catalog,
    /// Any other deployment: a Release's or a dedicated Namespace deployment.
    Workload,
}

/// Where a deployment takes its manifests from: the repository, the
/// directory, and whatever else changes how the reconciler renders the
/// directory. The revision is left out: it is what an observer reads, not a
/// setting to compare.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Source {
    pub repository: String,
    /// The directory, from the publication repository's root.
    pub path: String,
    /// The remaining settings, without nulls and empty containers, so that
    /// what a reconciler defaults or omits is no difference; null when none.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub options: Value,
}

impl Source {
    /// The names of what differs between two sources, empty when they agree:
    /// `repository`, `path`, and each option that differs.
    pub fn differences(&self, other: &Self) -> Vec<String> {
        let mut differing = Vec::new();
        if self.repository != other.repository {
            differing.push("repository".to_owned());
        }
        if self.path != other.path {
            differing.push("path".to_owned());
        }
        if self.options != other.options {
            match (&self.options, &other.options) {
                (Value::Object(a), Value::Object(b)) => differing.extend(
                    a.keys()
                        .chain(b.keys())
                        .collect::<std::collections::BTreeSet<_>>()
                        .into_iter()
                        .filter(|key| a.get(*key) != b.get(*key))
                        .cloned(),
                ),
                _ => differing.push("options".to_owned()),
            }
        }
        differing
    }
}

/// A deployment the target expects its reconciler to run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExpectedDeployment {
    /// What readings and findings call it, unique within the target.
    pub id: String,
    /// What messages call it for a person, qualified where the id is not
    /// (Argo CD: `<namespace>/<name>`); the id when empty.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub display: String,
    pub role: DeploymentRole,
    /// The directory it deploys, from the publication repository's root.
    pub path: String,
    /// `<applicationGroup>/<release>` for a Release's deployment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release: Option<String>,
    /// The source the target generated for it.
    pub source: Source,
    /// How the reconciler names it, opaque to everything but that kind's reader.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub locator: Value,
}

impl ExpectedDeployment {
    /// The name messages use for it.
    pub fn label(&self) -> &str {
        if self.display.is_empty() {
            &self.id
        } else {
            &self.display
        }
    }
}

/// A thing only the reconciler that runs this target has, such as a catalog
/// deployment or a project the target depends on: seeing it shows a reading
/// comes from that reconciler, so that a deployment's absence from it means
/// something.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Anchor {
    pub id: String,
    /// How the reconciler names it, opaque to everything but that kind's reader.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub locator: Value,
}

/// Where the target publishes, as a reader of the branch needs it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ObservedPublication {
    /// The URL the branch is read from (the publication URL; never carries credentials).
    pub url: String,
    pub branch: String,
    pub path_prefix: String,
}

/// What a target expects of its reconciler: the publication it reads, the
/// deployments it should run, and the anchors of a trustworthy reading.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Expected {
    pub publication: ObservedPublication,
    /// Every deployment of the target, the catalog first.
    #[serde(rename = "expected")]
    pub deployments: Vec<ExpectedDeployment>,
    /// Empty when the target has no workload deployment.
    #[serde(default)]
    pub anchors: Vec<Anchor>,
}

impl Expected {
    /// The deployments that are not the catalog: what a teardown's first
    /// phase removes.
    pub fn workload(&self) -> impl Iterator<Item = &ExpectedDeployment> {
        self.deployments
            .iter()
            .filter(|deployment| deployment.role == DeploymentRole::Workload)
    }

    /// The same expectation limited to the deployments `keep` accepts, with
    /// every anchor.
    #[must_use]
    pub fn restricted_to(&self, keep: impl Fn(&ExpectedDeployment) -> bool) -> Self {
        Self {
            publication: self.publication.clone(),
            deployments: self
                .deployments
                .iter()
                .filter(|deployment| keep(deployment))
                .cloned()
                .collect(),
            anchors: self.anchors.clone(),
        }
    }
}

/// Everything an observer needs from the declaration, recorded by the
/// publication driver at resolution under `/reconciler` (a persisted format
/// whose camelCase keys a test pins).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReconcilerFacts {
    #[serde(flatten)]
    pub location: ReconcilerLocation,
    #[serde(flatten)]
    pub expected: Expected,
}

impl ReconcilerFacts {
    /// The fields of these facts, as JSON Pointers below `prefix` (where they
    /// are recorded), that stay out of the unit's key: the location's own
    /// ([`ReconcilerLocation::unkeyed`]) and the URL the publication branch is
    /// read from. Everything else the render derives keys the unit.
    pub fn unkeyed(&self, prefix: &str) -> Vec<String> {
        self.location
            .unkeyed()
            .iter()
            .chain(&["/publication/url"])
            .map(|field| format!("{prefix}{field}"))
            .collect()
    }

    /// The reconciler facts of a rendered target: the reconciler of its
    /// target decides the kind.
    pub fn of(rendered: &RenderedTarget) -> Result<Self> {
        let PublicationDestination {
            push_url,
            branch,
            path_prefix,
            ..
        } = &rendered.destination;
        let publication = ObservedPublication {
            url: push_url.clone(),
            branch: branch.clone(),
            path_prefix: path_prefix.clone(),
        };
        match &rendered.delivery {
            Delivery::ArgoCd(location) => {
                let applications = generated_applications(rendered)?;
                Ok(Self {
                    location: ReconcilerLocation::ArgoCd(location.clone()),
                    expected: Expected {
                        publication,
                        deployments: applications
                            .iter()
                            .map(|application| application.expected(&location.namespace))
                            .collect(),
                        anchors: argocd_anchors(&applications, &location.namespace),
                    },
                })
            }
            Delivery::Direct(location) => {
                let deployments = direct_deployments(rendered)?
                    .iter()
                    .map(|release| release.expected(&rendered.destination))
                    .collect::<Vec<_>>();
                let anchors = if deployments.is_empty() {
                    Vec::new()
                } else {
                    vec![Anchor {
                        id: DIRECT_ANCHOR.to_owned(),
                        locator: Value::Null,
                    }]
                };
                Ok(Self {
                    location: ReconcilerLocation::Direct(location.clone()),
                    expected: Expected {
                        publication,
                        deployments,
                        anchors,
                    },
                })
            }
        }
    }
}

/// The id of the one anchor of a direct target: the release state of its
/// Releases in the cluster.
pub const DIRECT_ANCHOR: &str = "release-state";

/// How the direct reader names an expected deployment: the Release's name and
/// namespace, which key its release state, and the namespace its resources
/// without one belong to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DirectRelease {
    pub name: String,
    pub namespace: String,
    pub destination_namespace: String,
}

/// One Release of a rendered direct target, as the compilation record of its
/// ownership index holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedDirectRelease {
    /// `<group>/<release>`.
    pub key: String,
    pub group: String,
    pub release: DirectRelease,
    /// The Release's directory below the publication prefix.
    pub directory: String,
}

impl PublishedDirectRelease {
    /// The Release's state identity in the cluster: its namespace and name.
    pub fn identity(&self) -> (&str, &str) {
        (&self.release.namespace, &self.release.name)
    }

    /// The directory from the repository root.
    pub fn path(&self, destination: &PublicationDestination) -> String {
        join_repository_path(&destination.path_prefix, &self.directory)
    }

    /// This Release as an expected deployment.
    pub fn expected(&self, destination: &PublicationDestination) -> ExpectedDeployment {
        let path = self.path(destination);
        ExpectedDeployment {
            id: self.key.clone(),
            display: format!("{}/{}", self.release.namespace, self.release.name),
            role: DeploymentRole::Workload,
            path: path.clone(),
            release: Some(self.key.clone()),
            source: Source {
                repository: destination.repository.clone(),
                path,
                options: Value::Null,
            },
            locator: serde_json::to_value(&self.release).expect("a locator serializes"),
        }
    }
}

fn join_repository_path(prefix: &str, directory: &str) -> String {
    let prefix = prefix.trim_matches('/');
    if prefix.is_empty() {
        directory.to_owned()
    } else {
        format!("{prefix}/{directory}")
    }
}

/// The Releases an ownership index's compilation record lists, by
/// `<group>/<release>`; empty without a record. The record is what a
/// publication wrote, so this reads the Releases of a direct tree as well as
/// the Releases a previous publication held.
pub fn direct_releases_of(compilation: Option<&Value>) -> Vec<PublishedDirectRelease> {
    compilation
        .and_then(|compilation| compilation.get("releases"))
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter_map(|(key, release)| {
            let text = |pointer: &str| release.pointer(pointer).and_then(Value::as_str).map(str::to_owned);
            Some(PublishedDirectRelease {
                key: key.clone(),
                group: text("/workload/group/metadata/name")?,
                release: DirectRelease {
                    name: text("/workload/release/metadata/name")?,
                    namespace: text("/workload/release/metadata/namespace")?,
                    destination_namespace: text("/workload/destination_namespace")?,
                },
                directory: text("/workload/release_directory")?.replace('\\', "/"),
            })
        })
        .collect()
}

/// The Releases of a rendered direct target, ordered by key.
pub fn direct_deployments(rendered: &RenderedTarget) -> Result<Vec<PublishedDirectRelease>> {
    let Some(bytes) = rendered.files.get(Path::new(DEFAULT_INDEX_PATH)) else {
        return Ok(Vec::new());
    };
    let index = parse_index(bytes, DEFAULT_INDEX_PATH)?;
    let mut releases = direct_releases_of(index.compilation.as_ref());
    releases.sort_by(|a, b| a.key.cmp(&b.key));
    Ok(releases)
}

/// How Argo CD names an expected deployment: the Application's namespace,
/// name, and project (which its policy names it by; empty in a document
/// recorded before it was).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArgoCdApplication {
    pub namespace: String,
    pub name: String,
    #[serde(default)]
    pub project: String,
}

/// How Argo CD names an anchor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ArgoCdAnchor {
    /// The catalog Application, in the reading.
    Application { namespace: String, name: String },
    /// An AppProject of the instance's namespace, visible.
    Project { name: String },
}

/// The Argo CD part of what a target generates, as an Application: the
/// expected deployment in the form Argo CD names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedApplication {
    pub role: DeploymentRole,
    pub name: String,
    /// The namespace the Application object lives in.
    pub namespace: String,
    /// The directory it sources, from the publication repository's root.
    pub path: String,
    /// The Argo CD project it belongs to (`spec.project`), which Argo CD's
    /// policy names it by.
    pub project: String,
    /// `<applicationGroup>/<release>` for a Release's Application.
    pub release: Option<String>,
    /// The generated `spec.source`.
    pub source: Value,
}

impl GeneratedApplication {
    /// The id a deployment of an instance with `instance_namespace` has: the
    /// Application's name, qualified by its namespace outside the instance's.
    pub fn id(&self, instance_namespace: &str) -> String {
        if self.namespace == instance_namespace {
            self.name.clone()
        } else {
            format!("{}/{}", self.namespace, self.name)
        }
    }

    /// This Application as an expected deployment.
    pub fn expected(&self, instance_namespace: &str) -> ExpectedDeployment {
        ExpectedDeployment {
            id: self.id(instance_namespace),
            display: format!("{}/{}", self.namespace, self.name),
            role: self.role,
            path: self.path.clone(),
            release: self.release.clone(),
            source: argocd_source(&self.source),
            locator: serde_json::to_value(ArgoCdApplication {
                namespace: self.namespace.clone(),
                name: self.name.clone(),
                project: self.project.clone(),
            })
            .expect("a locator serializes"),
        }
    }
}

/// An Argo CD `spec.source` as a [`Source`]: `repoURL` and `path`, and the
/// rest (how Argo CD renders the directory: `directory.recurse`, `exclude`,
/// `include`, or a tool block) as options, without `targetRevision`, nulls,
/// and empty containers.
pub fn argocd_source(source: &Value) -> Source {
    fn prune(value: &Value) -> Option<Value> {
        match value {
            Value::Null => None,
            Value::Object(map) => {
                let map = map
                    .iter()
                    .filter_map(|(key, value)| prune(value).map(|value| (key.clone(), value)))
                    .collect::<serde_json::Map<_, _>>();
                (!map.is_empty()).then_some(Value::Object(map))
            }
            Value::Array(items) => {
                let items = items.iter().filter_map(prune).collect::<Vec<_>>();
                (!items.is_empty()).then_some(Value::Array(items))
            }
            other => Some(other.clone()),
        }
    }
    let text = |key: &str| source.get(key).and_then(Value::as_str).unwrap_or_default().to_owned();
    let mut options = source.clone();
    if let Some(object) = options.as_object_mut() {
        for key in ["targetRevision", "repoURL", "path"] {
            object.remove(key);
        }
    }
    Source {
        repository: text("repoURL"),
        path: text("path"),
        options: prune(&options).unwrap_or(Value::Null),
    }
}

/// The anchors of an Argo CD target, in the order they are checked:
///
/// - the catalog Application, when the target generates one: a reading that
///   shows it comes from the Argo CD that runs this target;
/// - without one (`catalogApplication.shared`, or `enabled: false`), the
///   AppProjects the target's Applications belong to: phase 1 keeps the
///   generated ones, and an externally owned one is a project the target
///   depends on. The `default` project exists in every Argo CD and anchors
///   nothing;
/// - none for a target without a workload Application, which has nothing to
///   conclude.
fn argocd_anchors(applications: &[GeneratedApplication], instance_namespace: &str) -> Vec<Anchor> {
    if !applications
        .iter()
        .any(|application| application.role == DeploymentRole::Workload)
    {
        return Vec::new();
    }
    if let Some(catalog) = applications
        .iter()
        .find(|application| application.role == DeploymentRole::Catalog)
    {
        return vec![Anchor {
            id: catalog.id(instance_namespace),
            locator: serde_json::to_value(ArgoCdAnchor::Application {
                namespace: catalog.namespace.clone(),
                name: catalog.name.clone(),
            })
            .expect("a locator serializes"),
        }];
    }
    applications
        .iter()
        .map(|application| application.project.as_str())
        .filter(|project| !project.is_empty() && *project != "default")
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .map(|project| Anchor {
            id: format!("project/{project}"),
            locator: serde_json::to_value(ArgoCdAnchor::Project {
                name: project.to_owned(),
            })
            .expect("a locator serializes"),
        })
        .collect()
}

/// The Applications a rendered target generates, read from its catalog
/// files as Argo CD will see them: the one sourcing `_nyl/catalog` is the
/// catalog, every other one is a workload Application. The catalog sorts
/// first, the rest by name.
pub fn generated_applications(rendered: &RenderedTarget) -> Result<Vec<GeneratedApplication>> {
    let prefix = rendered.destination.path_prefix.trim_matches('/');
    let catalog_source = if prefix.is_empty() {
        super::CATALOG_DIRECTORY.to_owned()
    } else {
        format!("{prefix}/{}", super::CATALOG_DIRECTORY)
    };
    let releases = release_applications(rendered)?;
    let mut applications = Vec::new();
    for (path, bytes) in &rendered.files {
        if !path.starts_with(super::CATALOG_DIRECTORY) {
            continue;
        }
        let text = std::str::from_utf8(bytes)
            .map_err(|_| NylError::config(format!("generated catalog file {} is not UTF-8", path.display())))?;
        for manifest in serde_saphyr::from_multiple::<Value>(text).map_err(NylError::Yaml)? {
            if manifest.get("kind").and_then(Value::as_str) != Some("Application") {
                continue;
            }
            let text = |pointer: &str| manifest.pointer(pointer).and_then(Value::as_str).map(str::to_owned);
            let (Some(name), Some(namespace), Some(source)) = (
                text("/metadata/name"),
                text("/metadata/namespace"),
                text("/spec/source/path"),
            ) else {
                return Err(NylError::config(format!(
                    "generated Application in {} lacks a name, namespace, or source path",
                    path.display()
                )));
            };
            let mut generated_source = manifest.pointer("/spec/source").cloned().unwrap_or(Value::Null);
            if let Some(object) = generated_source.as_object_mut() {
                object.remove("targetRevision");
            }
            let project = text("/spec/project").unwrap_or_default();
            applications.push(GeneratedApplication {
                role: if source == catalog_source {
                    DeploymentRole::Catalog
                } else {
                    DeploymentRole::Workload
                },
                release: releases.get(&name).cloned(),
                name,
                namespace,
                path: source,
                project,
                source: generated_source,
            });
        }
    }
    applications.sort_by(|a, b| (a.role, &a.name, &a.namespace).cmp(&(b.role, &b.name, &b.namespace)));
    Ok(applications)
}

/// A Kubernetes resource as the Kubernetes API names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResourceRef {
    pub api_version: String,
    pub kind: String,
    /// The namespace of a namespaced resource; absent for a cluster-scoped one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    pub name: String,
}

impl std::fmt::Display for ResourceRef {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.namespace {
            Some(namespace) => write!(
                formatter,
                "{} {namespace}/{} ({})",
                self.kind, self.name, self.api_version
            ),
            None => write!(formatter, "{} {} ({})", self.kind, self.name, self.api_version),
        }
    }
}

/// The workload Application of `rendered` whose directory renders `resource`,
/// `None` when the target renders no such resource: the check that keeps a
/// driver from reading anything the declaration does not own.
///
/// A manifest without `metadata.namespace` belongs to the namespace its
/// Application deploys to (`spec.destination.namespace`), as Argo CD applies
/// it. Contract: [Kubernetes publication unit](../../../../design/orchestration-core.md#kubernetes-publication-unit),
/// observed values.
pub fn owning_application(rendered: &RenderedTarget, resource: &ResourceRef) -> Result<Option<GeneratedApplication>> {
    let applications = generated_applications(rendered)?;
    let destinations = destination_namespaces(rendered)?;
    owning_application_in(
        &rendered.files,
        &rendered.destination.path_prefix,
        &applications,
        &destinations,
        resource,
    )
}

/// The namespace each generated Application deploys to, by its namespace and name.
fn destination_namespaces(rendered: &RenderedTarget) -> Result<BTreeMap<(String, String), String>> {
    let mut destinations = BTreeMap::new();
    for (path, bytes) in &rendered.files {
        if !path.starts_with(super::CATALOG_DIRECTORY) {
            continue;
        }
        let text = std::str::from_utf8(bytes)
            .map_err(|_| NylError::config(format!("generated catalog file {} is not UTF-8", path.display())))?;
        for manifest in serde_saphyr::from_multiple::<Value>(text).map_err(NylError::Yaml)? {
            if manifest.get("kind").and_then(Value::as_str) != Some("Application") {
                continue;
            }
            let text = |pointer: &str| manifest.pointer(pointer).and_then(Value::as_str).map(str::to_owned);
            if let (Some(name), Some(namespace), Some(destination)) = (
                text("/metadata/name"),
                text("/metadata/namespace"),
                text("/spec/destination/namespace"),
            ) {
                destinations.insert((namespace, name), destination);
            }
        }
    }
    Ok(destinations)
}

fn owning_application_in(
    files: &BTreeMap<std::path::PathBuf, Vec<u8>>,
    prefix: &str,
    applications: &[GeneratedApplication],
    destinations: &BTreeMap<(String, String), String>,
    resource: &ResourceRef,
) -> Result<Option<GeneratedApplication>> {
    let prefix = prefix.trim_matches('/');
    for application in applications
        .iter()
        .filter(|application| application.role == DeploymentRole::Workload)
    {
        let directory = application.path.trim_matches('/');
        let directory = if prefix.is_empty() {
            directory
        } else {
            directory.strip_prefix(prefix).unwrap_or(directory).trim_matches('/')
        };
        let destination = destinations.get(&(application.namespace.clone(), application.name.clone()));
        for (path, bytes) in files {
            let manifests = path.starts_with(directory)
                && matches!(
                    path.extension().and_then(|extension| extension.to_str()),
                    Some("yaml" | "yml" | "json")
                );
            if !manifests {
                continue;
            }
            let text = std::str::from_utf8(bytes)
                .map_err(|_| NylError::config(format!("rendered file {} is not UTF-8", path.display())))?;
            for manifest in serde_saphyr::from_multiple::<Value>(text).map_err(NylError::Yaml)? {
                let text = |pointer: &str| manifest.pointer(pointer).and_then(Value::as_str);
                if text("/apiVersion") != Some(resource.api_version.as_str())
                    || text("/kind") != Some(resource.kind.as_str())
                    || text("/metadata/name") != Some(resource.name.as_str())
                {
                    continue;
                }
                let namespace = match (text("/metadata/namespace"), &resource.namespace) {
                    (Some(namespace), _) => Some(namespace),
                    // Applied into the namespace the Application deploys to.
                    (None, Some(_)) => destination.map(String::as_str),
                    (None, None) => None,
                };
                if namespace == resource.namespace.as_deref() {
                    return Ok(Some(application.clone()));
                }
            }
        }
    }
    Ok(None)
}

/// The Release of a rendered direct target whose directory renders
/// `resource`, `None` when the target renders no such resource: the check that
/// keeps a driver from reading anything the declaration does not own. A
/// manifest without `metadata.namespace` belongs to the Release's destination
/// namespace, where `nyl apply` puts it.
pub fn owning_direct_release(
    rendered: &RenderedTarget,
    resource: &ResourceRef,
) -> Result<Option<PublishedDirectRelease>> {
    for release in direct_deployments(rendered)? {
        for (path, bytes) in &rendered.files {
            let manifests = path.starts_with(&release.directory)
                && matches!(
                    path.extension().and_then(|extension| extension.to_str()),
                    Some("yaml" | "yml" | "json")
                );
            if !manifests {
                continue;
            }
            let text = std::str::from_utf8(bytes)
                .map_err(|_| NylError::config(format!("rendered file {} is not UTF-8", path.display())))?;
            for manifest in serde_saphyr::from_multiple::<Value>(text).map_err(NylError::Yaml)? {
                let text = |pointer: &str| manifest.pointer(pointer).and_then(Value::as_str);
                if text("/apiVersion") != Some(resource.api_version.as_str())
                    || text("/kind") != Some(resource.kind.as_str())
                    || text("/metadata/name") != Some(resource.name.as_str())
                {
                    continue;
                }
                let namespace = match (text("/metadata/namespace"), &resource.namespace) {
                    (Some(namespace), _) => Some(namespace),
                    (None, Some(_)) => Some(release.release.destination_namespace.as_str()),
                    (None, None) => None,
                };
                if namespace == resource.namespace.as_deref() {
                    return Ok(Some(release));
                }
            }
        }
    }
    Ok(None)
}

/// `<group>/<release>` by generated Application name, from the ownership
/// index's compilation record; empty for an index without one.
fn release_applications(rendered: &RenderedTarget) -> Result<BTreeMap<String, String>> {
    let Some(bytes) = rendered.files.get(Path::new(DEFAULT_INDEX_PATH)) else {
        return Ok(BTreeMap::new());
    };
    let index = parse_index(bytes, DEFAULT_INDEX_PATH)?;
    Ok(release_applications_of(index.compilation.as_ref()))
}

fn release_applications_of(compilation: Option<&Value>) -> BTreeMap<String, String> {
    compilation
        .and_then(|compilation| compilation.get("releases"))
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter_map(|(key, release)| {
            let name = release.pointer("/workload/application_name")?.as_str()?;
            Some((name.to_owned(), key.clone()))
        })
        .collect()
}

/// A revision of the publication branch, as [`PublicationHistory`] names
/// commits: the full hexadecimal object id.
pub type Revision = String;

/// What the matching rules ask of the publication branch. Every answer is
/// about the branch as it was fetched; an error is an unreadable repository,
/// never a missing commit.
pub trait PublicationHistory {
    /// Whether `revision` names a commit of the fetched history.
    fn contains(&self, revision: &str) -> Result<bool>;

    /// The identity of the directory `path` (from the repository root) at
    /// `revision`, `None` when it does not exist there. Equal identities mean
    /// equal contents, everything Argo CD would read.
    fn directory(&self, revision: &str, path: &str) -> Result<Option<String>>;

    /// The recorded inputs of `release` in the ownership index at
    /// `revision`: its `compilation` record's `inputs` for the Release, an
    /// empty map when the index records none, and `None` when the commit
    /// has no index.
    fn recorded_inputs(&self, revision: &str, release: &str) -> Result<Option<BTreeMap<String, String>>>;

    /// The publication commits (those that change the ownership index) that
    /// are `revision` or its ancestors along first parents, newest first, at
    /// most `limit`; the flag says whether older ones were left out.
    fn publication_commits(&self, revision: &str, limit: usize) -> Result<(Vec<Revision>, bool)>;
}

/// How many publication commits a match walks back at most.
pub const MATCH_WALK_LIMIT: usize = 1_000;

/// An Application's directory and Release, the identity the matching rules
/// compare.
#[derive(Debug, Clone, Copy)]
pub struct DeploymentTree<'a> {
    pub path: &'a str,
    pub release: Option<&'a str>,
}

/// Whether two revisions hold the same Application: the same directory tree
/// and, for a Release's Application, the same recorded inputs.
pub fn equivalent(history: &dyn PublicationHistory, application: DeploymentTree<'_>, a: &str, b: &str) -> Result<bool> {
    if history.directory(a, application.path)? != history.directory(b, application.path)? {
        return Ok(false);
    }
    match application.release {
        Some(release) => Ok(history.recorded_inputs(a, release)? == history.recorded_inputs(b, release)?),
        None => Ok(true),
    }
}

/// The publication an Application runs when Argo CD reports `revision`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunningPublication {
    /// The commit that introduced what runs: the oldest publication commit of
    /// the unbroken run of equivalent ones ending at the running revision.
    pub recorded: Revision,
    /// The newest publication commit in the run, which is at or before the
    /// running revision.
    pub newest: Revision,
    /// The run reaches the oldest commit the walk examined, so it may start earlier.
    pub truncated: bool,
}

/// What matching a running revision found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunningMatch {
    /// The revision is not on the fetched branch.
    NotOnBranch,
    /// No publication commit holds the Application as the revision does:
    /// something other than a publication, such as another tool's commit
    /// inside the Application's directory, is what runs.
    NoPublication,
    Matched(RunningPublication),
}

/// Match the revision an Application runs to the target's publication
/// commits. A revision that is another target's commit, or a write-back
/// outside the Application's directory, matches the newest publication at or
/// before it, whose directory tree and recorded inputs it shares; one that
/// changed the directory without publishing matches none, even when it
/// restored the tree of an older publication.
pub fn running_publication(
    history: &dyn PublicationHistory,
    application: DeploymentTree<'_>,
    running: &str,
    limit: usize,
) -> Result<RunningMatch> {
    if !history.contains(running)? {
        return Ok(RunningMatch::NotOnBranch);
    }
    let (commits, more) = history.publication_commits(running, limit)?;
    let mut equal = Vec::new();
    for commit in &commits {
        equal.push(equivalent(history, application, commit, running)?);
    }
    // The newest publication at or before the running revision is what Nyl
    // says the revision holds, so it must be the one that matches: a commit
    // that restores an older tree by hand, or edits the directory, matches
    // none, however well an older publication would fit.
    if !equal.first().copied().unwrap_or(false) {
        return Ok(RunningMatch::NoPublication);
    }
    let newest = 0;
    // The run: consecutive equivalent commits from the newest match back.
    let mut oldest = newest;
    while oldest + 1 < commits.len() && equal[oldest + 1] {
        oldest += 1;
    }
    Ok(RunningMatch::Matched(RunningPublication {
        recorded: commits[oldest].clone(),
        newest: commits[newest].clone(),
        truncated: more && oldest + 1 == commits.len(),
    }))
}

/// [`PublicationHistory`] over a fetched copy of the publication branch
/// beneath one path prefix.
pub struct GitPublicationHistory {
    repository: Repository,
    index_path: String,
    /// Keeps the scratch repository alive.
    _scratch: Option<tempfile::TempDir>,
}

impl GitPublicationHistory {
    /// Fetch `branch` of `url` into a scratch repository, authenticating as
    /// the project's `[git]` settings say (variables read only through
    /// `variable`), and read it beneath `path_prefix`. A branch the remote
    /// does not have reads as an empty history.
    pub fn fetch(
        publication: &ObservedPublication,
        project_dir: &Path,
        variable: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Self> {
        let auth = super::publication::project_auth(project_dir, variable)?;
        let scratch = tempfile::TempDir::new()?;
        let repository = git(Repository::init_bare(scratch.path()))?;
        crate::git::transport::fetch_branch(
            &repository,
            &publication.url,
            &publication.branch,
            "refs/nyl/observed",
            &auth,
        )
        .map_err(|error| {
            NylError::config(format!(
                "cannot read publication branch {} of {}: {error}",
                publication.branch,
                nyl_git::sanitize_url(&publication.url)
            ))
        })?;
        Ok(Self::over(repository, &publication.path_prefix, Some(scratch)))
    }

    /// The Releases that every ownership index along the first-parent history of
    /// the fetched branch holds, from its head back to the publication that was
    /// current at `receipt` (the commit of the last complete execution's
    /// `PublishedTree`), at most `limit` publications. Without a receipt, only the
    /// newest publication counts. A history that holds more publications than
    /// `limit` before reaching the receipt's is an error, never a guess. Direct
    /// targets use it to find the Releases that left the tree.
    pub fn direct_releases_since(&self, receipt: Option<&str>, limit: usize) -> Result<Vec<PublishedDirectRelease>> {
        let Some(head) = self.head()? else {
            return Ok(Vec::new());
        };
        let (publications, truncated) = self.publication_commits(&head, limit)?;
        let stop = match receipt {
            Some(receipt) => self.publication_commits(receipt, 1)?.0.into_iter().next(),
            None => publications.first().cloned(),
        };
        let mut releases = BTreeMap::new();
        let mut reached = stop.is_none();
        for commit in &publications {
            let Some(commit) = self.commit(commit)? else {
                continue;
            };
            if let Some(blob) = self.index_blob(&commit)? {
                let blob = git(self.repository.find_blob(blob))?;
                let index = parse_index(blob.content(), DEFAULT_INDEX_PATH)?;
                for release in direct_releases_of(index.compilation.as_ref()) {
                    releases
                        .entry((release.release.namespace.clone(), release.release.name.clone()))
                        .or_insert(release);
                }
            }
            if Some(commit.id().to_string()) == stop {
                reached = true;
                break;
            }
        }
        if !reached && truncated {
            return Err(NylError::config(format!(
                "the publication branch holds more than {limit} publications since the last receipt's, so the Releases \
                 that left the tree cannot be told; publish fewer commits between executions, or release the leftovers by hand"
            )));
        }
        Ok(releases.into_values().collect())
    }

    /// The commit the fetched branch points at, `None` for an empty history.
    pub fn head(&self) -> Result<Option<String>> {
        match self.repository.find_reference("refs/nyl/observed") {
            Ok(reference) => Ok(reference.target().map(|oid| oid.to_string())),
            Err(error) if error.code() == git2::ErrorCode::NotFound => Ok(None),
            Err(error) => Err(crate::git::GitError::from(error).into()),
        }
    }

    /// Read `repository`, which already holds the history, beneath `path_prefix`.
    pub fn over(repository: Repository, path_prefix: &str, scratch: Option<tempfile::TempDir>) -> Self {
        let prefix = path_prefix.trim_matches('/');
        Self {
            repository,
            index_path: if prefix.is_empty() {
                DEFAULT_INDEX_PATH.to_owned()
            } else {
                format!("{prefix}/{DEFAULT_INDEX_PATH}")
            },
            _scratch: scratch,
        }
    }

    fn commit(&self, revision: &str) -> Result<Option<git2::Commit<'_>>> {
        let Ok(oid) = Oid::from_str(revision) else {
            return Ok(None);
        };
        match self.repository.find_commit(oid) {
            Ok(commit) => Ok(Some(commit)),
            Err(error) if error.code() == git2::ErrorCode::NotFound => Ok(None),
            Err(error) => Err(crate::git::GitError::from(error).into()),
        }
    }

    fn index_blob(&self, commit: &git2::Commit<'_>) -> Result<Option<Oid>> {
        match git(commit.tree())?.get_path(Path::new(&self.index_path)) {
            Ok(entry) => Ok(Some(entry.id())),
            Err(error) if error.code() == git2::ErrorCode::NotFound => Ok(None),
            Err(error) => Err(crate::git::GitError::from(error).into()),
        }
    }
}

impl PublicationHistory for GitPublicationHistory {
    fn contains(&self, revision: &str) -> Result<bool> {
        Ok(self.commit(revision)?.is_some())
    }

    fn directory(&self, revision: &str, path: &str) -> Result<Option<String>> {
        let Some(commit) = self.commit(revision)? else {
            return Ok(None);
        };
        match git(commit.tree())?.get_path(Path::new(path.trim_matches('/'))) {
            Ok(entry) if entry.kind() == Some(git2::ObjectType::Tree) => Ok(Some(entry.id().to_string())),
            // A file where the directory should be is no Application directory.
            Ok(_) => Ok(None),
            Err(error) if error.code() == git2::ErrorCode::NotFound => Ok(None),
            Err(error) => Err(crate::git::GitError::from(error).into()),
        }
    }

    fn recorded_inputs(&self, revision: &str, release: &str) -> Result<Option<BTreeMap<String, String>>> {
        let Some(commit) = self.commit(revision)? else {
            return Ok(None);
        };
        let Some(blob) = self.index_blob(&commit)? else {
            return Ok(None);
        };
        let blob = git(self.repository.find_blob(blob))?;
        let index = parse_index(blob.content(), DEFAULT_INDEX_PATH)?;
        let inputs = index
            .compilation
            .as_ref()
            .and_then(|compilation| compilation.pointer(&format!("/releases/{}/inputs", escape_pointer(release))))
            .and_then(Value::as_object)
            .map(|inputs| {
                inputs
                    .iter()
                    .filter_map(|(key, value)| Some((key.clone(), value.as_str()?.to_owned())))
                    .collect()
            })
            .unwrap_or_default();
        Ok(Some(inputs))
    }

    fn publication_commits(&self, revision: &str, limit: usize) -> Result<(Vec<Revision>, bool)> {
        let mut commits = Vec::new();
        let mut current = self.commit(revision)?;
        while let Some(commit) = current {
            let parent = commit.parents().next();
            let index = self.index_blob(&commit)?;
            let parent_index = match &parent {
                Some(parent) => self.index_blob(parent)?,
                None => None,
            };
            if index.is_some() && index != parent_index {
                if commits.len() == limit {
                    return Ok((commits, true));
                }
                commits.push(commit.id().to_string());
            }
            current = parent;
        }
        Ok((commits, false))
    }
}

fn git<T>(result: std::result::Result<T, git2::Error>) -> Result<T> {
    result.map_err(|error| crate::git::GitError::from(error).into())
}

/// A JSON Pointer reference token: `~` and `/` escaped.
fn escape_pointer(token: &str) -> String {
    token.replace('~', "~0").replace('/', "~1")
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::BTreeSet;

    use super::*;

    /// A history scripted by commit: each holds directory identities, the
    /// release inputs, and whether it publishes; parents are given oldest
    /// first as a chain.
    #[derive(Default)]
    struct Script {
        chain: Vec<&'static str>,
        directories: BTreeMap<(&'static str, &'static str), &'static str>,
        inputs: BTreeMap<(&'static str, &'static str), Vec<(&'static str, &'static str)>>,
        publishes: BTreeSet<&'static str>,
        asked: RefCell<usize>,
    }

    impl PublicationHistory for Script {
        fn contains(&self, revision: &str) -> Result<bool> {
            Ok(self.chain.contains(&revision))
        }
        fn directory(&self, revision: &str, path: &str) -> Result<Option<String>> {
            *self.asked.borrow_mut() += 1;
            Ok(self
                .directories
                .iter()
                .find(|((commit, directory), _)| *commit == revision && *directory == path)
                .map(|(_, tree)| (*tree).to_owned()))
        }
        fn recorded_inputs(&self, revision: &str, release: &str) -> Result<Option<BTreeMap<String, String>>> {
            Ok(self
                .inputs
                .iter()
                .find(|((commit, name), _)| *commit == revision && *name == release)
                .map(|(_, inputs)| {
                    inputs
                        .iter()
                        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                        .collect()
                }))
        }
        fn publication_commits(&self, revision: &str, limit: usize) -> Result<(Vec<Revision>, bool)> {
            let Some(position) = self.chain.iter().position(|commit| *commit == revision) else {
                return Ok((Vec::new(), false));
            };
            let all = self.chain[..=position]
                .iter()
                .rev()
                .filter(|commit| self.publishes.contains(*commit))
                .map(|commit| (*commit).to_owned())
                .collect::<Vec<_>>();
            let more = all.len() > limit;
            Ok((all.into_iter().take(limit).collect(), more))
        }
    }

    const WEB: DeploymentTree<'static> = DeploymentTree {
        path: "dev/web/web",
        release: Some("web/web"),
    };

    fn script(web_trees: &[(&'static str, &'static str)]) -> Script {
        let mut script = Script::default();
        for (commit, tree) in web_trees {
            script.chain.push(commit);
            script.directories.insert((commit, "dev/web/web"), tree);
            script.publishes.insert(commit);
            script
                .inputs
                .insert((commit, "web/web"), vec![("@input/web/web/image", "sha256:1")]);
        }
        script
    }

    /// Catches a commit that did not change the Application ending the run
    /// early: a tree that stays while other Applications publish is one run,
    /// whose recorded commit is the first of them, so a promotion names the
    /// commit that introduced what runs.
    #[test]
    fn test_recorded_commit_is_the_oldest_of_the_unbroken_run() {
        let history = script(&[
            ("p1", "tree-a"),
            ("p2", "tree-a"),
            ("p3", "tree-b"),
            ("p4", "tree-b"),
            ("p5", "tree-b"),
        ]);
        let matched = running_publication(&history, WEB, "p5", 10).unwrap();
        assert_eq!(
            matched,
            RunningMatch::Matched(RunningPublication {
                recorded: "p3".to_owned(),
                newest: "p5".to_owned(),
                truncated: false
            })
        );
    }

    /// Catches a tree that went A, B, A recording the first A: the change
    /// that was reverted starts a new run, so the second A is recorded.
    #[test]
    fn test_a_reverted_tree_starts_a_new_run() {
        let history = script(&[("p1", "tree-a"), ("p2", "tree-b"), ("p3", "tree-a")]);
        let RunningMatch::Matched(matched) = running_publication(&history, WEB, "p3", 10).unwrap() else {
            panic!("matches");
        };
        assert_eq!(matched.recorded, "p3");
    }

    /// Catches an Application synced to a commit of another target (or a
    /// write-back) not matching the publication whose directory it equals:
    /// the running revision is not a publication commit itself.
    #[test]
    fn test_a_foreign_commit_matches_the_publication_with_the_same_directory() {
        let mut history = script(&[("p1", "tree-a"), ("p2", "tree-b")]);
        // `other` is another target's commit after p2, leaving dev/web/web alone.
        history.chain.push("other");
        history.directories.insert(("other", "dev/web/web"), "tree-b");
        history
            .inputs
            .insert(("other", "web/web"), vec![("@input/web/web/image", "sha256:1")]);
        let RunningMatch::Matched(matched) = running_publication(&history, WEB, "other", 10).unwrap() else {
            panic!("matches");
        };
        assert_eq!((matched.recorded.as_str(), matched.newest.as_str()), ("p2", "p2"));
    }

    /// Catches a hand edit that restores an older publication's tree being
    /// credited to that publication: the newest publication before the
    /// revision holds another tree, so the revision is no publication.
    #[test]
    fn test_a_hand_edited_revert_matches_no_publication() {
        let mut history = script(&[("p1", "tree-a"), ("p2", "tree-b")]);
        history.chain.push("revert");
        history.directories.insert(("revert", "dev/web/web"), "tree-a");
        history
            .inputs
            .insert(("revert", "web/web"), vec![("@input/web/web/image", "sha256:1")]);
        assert_eq!(
            running_publication(&history, WEB, "revert", 10).unwrap(),
            RunningMatch::NoPublication
        );
    }

    /// Catches a commit that edited the Application's directory without
    /// publishing being credited to the publication before it, which would
    /// let a hand-edited tree attest as the published one.
    #[test]
    fn test_a_commit_that_changes_the_directory_without_publishing_matches_none() {
        let mut history = script(&[("p1", "tree-a")]);
        history.chain.push("edit");
        history.directories.insert(("edit", "dev/web/web"), "tree-edited");
        history
            .inputs
            .insert(("edit", "web/web"), vec![("@input/web/web/image", "sha256:1")]);
        assert_eq!(
            running_publication(&history, WEB, "edit", 10).unwrap(),
            RunningMatch::NoPublication
        );
        assert_eq!(
            running_publication(&history, WEB, "unknown", 10).unwrap(),
            RunningMatch::NotOnBranch
        );
    }

    /// Catches equal manifests with different recorded inputs counting as one
    /// publication: values come from the publication that recorded them, so
    /// a changed input starts a new run even when the bytes did not change.
    #[test]
    fn test_changed_recorded_inputs_break_the_run() {
        let mut history = script(&[("p1", "tree-a"), ("p2", "tree-a")]);
        history
            .inputs
            .insert(("p2", "web/web"), vec![("@input/web/web/image", "sha256:2")]);
        let RunningMatch::Matched(matched) = running_publication(&history, WEB, "p2", 10).unwrap() else {
            panic!("matches");
        };
        assert_eq!(matched.recorded, "p2");
    }

    /// Catches a long history being walked without bound, and a run that
    /// reaches the walk's end claiming to start there.
    #[test]
    fn test_a_run_reaching_the_walk_limit_is_reported_truncated() {
        let history = script(&[("p1", "tree-a"), ("p2", "tree-a"), ("p3", "tree-a")]);
        let RunningMatch::Matched(matched) = running_publication(&history, WEB, "p3", 2).unwrap() else {
            panic!("matches");
        };
        assert_eq!((matched.recorded.as_str(), matched.truncated), ("p2", true));
        let RunningMatch::Matched(matched) = running_publication(&history, WEB, "p3", 3).unwrap() else {
            panic!("matches");
        };
        assert_eq!((matched.recorded.as_str(), matched.truncated), ("p1", false));
    }

    fn git(directory: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .current_dir(directory)
            .args([
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn index(inputs: &str) -> String {
        format!(
            r#"{{"version":3,"target":"dev","cluster":"dev","publication":{{"repository":"r","revision":"deploy","pathPrefix":"dev"}},"dirty":false,"inputs":{{}},"files":{{}},"compilation":{{"releases":{{"web/web":{{"inputs":{inputs}}}}}}}}}"#
        )
    }

    /// Catches the Git side disagreeing with the rules: publication commits
    /// are those that change the index (another tool's commit is not one),
    /// directory identity follows content, and recorded inputs come from the
    /// index's compilation record, all through a real repository.
    #[test]
    fn test_git_history_answers_the_matching_questions() {
        let directory = tempfile::TempDir::new().unwrap();
        let root = directory.path();
        git(root, &["init", "--quiet", "--initial-branch=deploy"]);
        let write = |path: &str, text: &str| {
            let file = root.join(path);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, text).unwrap();
        };
        let commit = |message: &str| {
            git(root, &["add", "--all"]);
            git(root, &["commit", "--quiet", "--message", message]);
            git(root, &["rev-parse", "HEAD"])
        };
        write("dev/web/web/deployment.yaml", "a: 1\n");
        write("dev/_nyl/index.json", &index(r#"{"@input/web/web/image":"sha256:1"}"#));
        let p1 = commit("publish 1");
        write("other/file.yaml", "x: 1\n");
        let other = commit("another target");
        write("dev/web/web/deployment.yaml", "a: 2\n");
        write("dev/_nyl/index.json", &index(r#"{"@input/web/web/image":"sha256:2"}"#));
        let p2 = commit("publish 2");
        write("dev/web/web/note.yaml", "edited: true\n");
        let edit = commit("edit inside the application");

        let history = GitPublicationHistory::over(Repository::open(root).unwrap(), "dev", None);
        assert!(history.contains(&p1).unwrap());
        assert!(!history.contains(&"0".repeat(40)).unwrap());
        assert_eq!(
            history.publication_commits(&edit, 10).unwrap(),
            (vec![p2.clone(), p1.clone()], false)
        );
        assert_eq!(
            history.publication_commits(&other, 10).unwrap(),
            (vec![p1.clone()], false)
        );
        assert_eq!(history.publication_commits(&p2, 1).unwrap(), (vec![p2.clone()], true));
        assert_eq!(
            history.directory(&p1, "dev/web/web").unwrap(),
            history.directory(&other, "dev/web/web").unwrap()
        );
        assert_ne!(
            history.directory(&p2, "dev/web/web").unwrap(),
            history.directory(&edit, "dev/web/web").unwrap()
        );
        assert_eq!(history.directory(&p1, "dev/missing").unwrap(), None);
        assert_eq!(
            history.recorded_inputs(&p2, "web/web").unwrap(),
            Some(BTreeMap::from([(
                "@input/web/web/image".to_owned(),
                "sha256:2".to_owned()
            )]))
        );
        assert_eq!(
            history.recorded_inputs(&p2, "web/other").unwrap(),
            Some(BTreeMap::new())
        );

        // The edit is inside the Application's directory and publishes nothing.
        assert_eq!(
            running_publication(
                &history,
                DeploymentTree {
                    path: "dev/web/web",
                    release: Some("web/web")
                },
                &edit,
                10
            )
            .unwrap(),
            RunningMatch::NoPublication
        );
        // Another target's commit leaves the first publication's directory as it was.
        let RunningMatch::Matched(matched) = running_publication(
            &history,
            DeploymentTree {
                path: "dev/web/web",
                release: Some("web/web"),
            },
            &other,
            10,
        )
        .unwrap() else {
            panic!("matches");
        };
        assert_eq!(matched.recorded, p1);
    }

    fn application(name: &str, path: &str, role: DeploymentRole) -> GeneratedApplication {
        GeneratedApplication {
            role,
            name: name.to_owned(),
            namespace: "argocd".to_owned(),
            path: path.to_owned(),
            project: String::new(),
            release: None,
            source: Value::Null,
        }
    }

    fn service(namespace: Option<&str>, name: &str) -> ResourceRef {
        ResourceRef {
            api_version: "v1".to_owned(),
            kind: "Service".to_owned(),
            namespace: namespace.map(str::to_owned),
            name: name.to_owned(),
        }
    }

    /// Catches a driver reading a resource the target does not render (the
    /// cluster would be asked about anything), a resource attributed to the
    /// Application of another directory (the wrong Application's revision
    /// would gate the read), and a manifest without a namespace not
    /// belonging to the namespace its Application deploys to.
    #[test]
    fn test_owning_application_names_the_directory_that_renders_the_resource() {
        let files = BTreeMap::from([
            (
                std::path::PathBuf::from("web/web/service.yaml"),
                b"apiVersion: v1\nkind: Service\nmetadata: {name: web, namespace: web}\n---\napiVersion: v1\nkind: Service\nmetadata: {name: bare}\n"
                    .to_vec(),
            ),
            (
                std::path::PathBuf::from("api/api/service.yaml"),
                b"apiVersion: v1\nkind: Service\nmetadata: {name: web, namespace: api}\n".to_vec(),
            ),
            (
                std::path::PathBuf::from("web/web/notes.txt"),
                b"apiVersion: v1\nkind: Service\nmetadata: {name: ignored, namespace: web}\n".to_vec(),
            ),
        ]);
        let applications = [
            application("catalog", "dev/_nyl/catalog", DeploymentRole::Catalog),
            application("web", "dev/web/web", DeploymentRole::Workload),
            application("api", "dev/api/api", DeploymentRole::Workload),
        ];
        let destinations = BTreeMap::from([
            (("argocd".to_owned(), "web".to_owned()), "web".to_owned()),
            (("argocd".to_owned(), "api".to_owned()), "api".to_owned()),
        ]);
        let owner = |resource: &ResourceRef| {
            owning_application_in(&files, "dev", &applications, &destinations, resource)
                .unwrap()
                .map(|application| application.name)
        };
        assert_eq!(owner(&service(Some("web"), "web")).as_deref(), Some("web"));
        assert_eq!(owner(&service(Some("api"), "web")).as_deref(), Some("api"));
        // No namespace in the manifest: the Application's destination namespace.
        assert_eq!(owner(&service(Some("web"), "bare")).as_deref(), Some("web"));
        assert_eq!(owner(&service(Some("api"), "bare")), None);
        // Not rendered: another name, kind, namespace, or a file that is no manifest.
        assert_eq!(owner(&service(Some("web"), "missing")), None);
        assert_eq!(owner(&service(Some("other"), "web")), None);
        assert_eq!(owner(&service(Some("web"), "ignored")), None);
        let mut deployment = service(Some("web"), "web");
        deployment.kind = "Deployment".to_owned();
        assert_eq!(owner(&deployment), None);
        // A resource declared without a namespace is one the manifest leaves
        // without, such as a cluster-scoped kind; a namespaced one never matches it.
        assert_eq!(owner(&service(None, "bare")).as_deref(), Some("web"));
        assert_eq!(owner(&service(None, "web")), None);
    }

    #[test]
    fn test_release_applications_map_names_to_release_keys() {
        let compilation = serde_json::json!({"releases": {
            "web/web": {"workload": {"application_name": "web-web"}},
            "web/api": {"workload": {"application_name": "web-api"}},
        }});
        let map = release_applications_of(Some(&compilation));
        assert_eq!(map["web-web"], "web/web");
        assert_eq!(map["web-api"], "web/api");
        assert!(release_applications_of(None).is_empty());
    }

    fn argocd_facts() -> ReconcilerFacts {
        ReconcilerFacts {
            location: ReconcilerLocation::ArgoCd(ArgoCdLocation {
                instance: "prod".to_owned(),
                namespace: "argocd".to_owned(),
                cluster: "control".to_owned(),
                context: Some("control-admin".to_owned()),
                access: ArgoCDAccess::kubernetes(),
            }),
            expected: Expected {
                publication: ObservedPublication {
                    url: "https://example.invalid/deploy.git".to_owned(),
                    branch: "deploy".to_owned(),
                    path_prefix: "dev".to_owned(),
                },
                deployments: vec![GeneratedApplication {
                    role: DeploymentRole::Workload,
                    name: "web".to_owned(),
                    namespace: "argocd".to_owned(),
                    path: "dev/web/web".to_owned(),
                    project: "web".to_owned(),
                    release: Some("web/web".to_owned()),
                    source: serde_json::json!({
                        "repoURL": "https://example.invalid/deploy.git", "path": "dev/web/web",
                        "targetRevision": "deploy", "directory": {"recurse": true, "include": []}
                    }),
                }
                .expected("argocd")],
                anchors: vec![Anchor {
                    id: "project/web".to_owned(),
                    locator: serde_json::json!({"project": {"name": "web"}}),
                }],
            },
        }
    }

    /// Pins the recorded format: it is persisted in desired documents, and
    /// only the location (the `kind` tag and what follows it) is specific to
    /// a reconciler kind.
    #[test]
    fn test_reconciler_facts_serialize_with_camel_case_keys() {
        let facts = argocd_facts();
        let value = serde_json::to_value(&facts).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "kind": "argocd",
                "instance": "prod", "namespace": "argocd", "cluster": "control", "context": "control-admin",
                "access": {"method": "kubernetes"},
                "publication": {"url": "https://example.invalid/deploy.git", "branch": "deploy", "pathPrefix": "dev"},
                "expected": [{
                    "id": "web", "display": "argocd/web", "role": "workload", "path": "dev/web/web", "release": "web/web",
                    "source": {"repository": "https://example.invalid/deploy.git", "path": "dev/web/web",
                               "options": {"directory": {"recurse": true}}},
                    "locator": {"namespace": "argocd", "name": "web", "project": "web"}
                }],
                "anchors": [{"id": "project/web", "locator": {"project": {"name": "web"}}}],
            })
        );
        assert_eq!(serde_json::from_value::<ReconcilerFacts>(value).unwrap(), facts);
    }

    fn generated(role: DeploymentRole, name: &str, namespace: &str, project: &str) -> GeneratedApplication {
        GeneratedApplication {
            role,
            name: name.to_owned(),
            namespace: namespace.to_owned(),
            path: format!("dev/{name}"),
            project: project.to_owned(),
            release: None,
            source: Value::Null,
        }
    }

    /// Catches the wrong thing anchoring a reading: the catalog Application
    /// alone when there is one, else the target's own projects but never
    /// `default` (every Argo CD has it), and nothing for a target without a
    /// workload deployment, which has nothing to conclude.
    #[test]
    fn test_anchors_are_the_catalog_else_the_projects_the_target_depends_on() {
        let ids = |applications: &[GeneratedApplication]| {
            argocd_anchors(applications, "argocd")
                .into_iter()
                .map(|anchor| anchor.id)
                .collect::<Vec<_>>()
        };
        let catalog = generated(DeploymentRole::Catalog, "dev-catalog", "argocd", "web");
        let web = generated(DeploymentRole::Workload, "web", "argocd", "web");
        let api = generated(DeploymentRole::Workload, "api", "team-a", "api");
        let plain = generated(DeploymentRole::Workload, "plain", "argocd", "default");
        assert_eq!(ids(&[catalog.clone(), web.clone(), api.clone()]), ["dev-catalog"]);
        assert_eq!(
            ids(&[web.clone(), api.clone(), plain.clone()]),
            ["project/api", "project/web"]
        );
        assert!(ids(&[plain]).is_empty());
        assert!(ids(&[catalog]).is_empty());
        // A deployment outside the instance's namespace is named with its own.
        assert_eq!(api.expected("argocd").id, "team-a/api");
        assert_eq!(web.expected("argocd").id, "web");
    }

    /// Catches what a reconciler defaults or omits counting as a difference
    /// between the source Nyl generated and the one a deployment has, and a
    /// real difference going unnamed.
    #[test]
    fn test_sources_differ_by_what_changes_how_the_directory_is_read() {
        let generated = argocd_source(&serde_json::json!({
            "repoURL": "r", "path": "p", "targetRevision": "deploy",
            "directory": {"recurse": true, "include": []},
        }));
        let live = argocd_source(&serde_json::json!({
            "repoURL": "r", "path": "p", "targetRevision": "other", "directory": {"recurse": true}, "plugin": null,
        }));
        assert!(generated.differences(&live).is_empty());
        let edited = argocd_source(&serde_json::json!({
            "repoURL": "r2", "path": "p", "directory": {"recurse": true, "exclude": "*.yaml"},
        }));
        assert_eq!(generated.differences(&edited), ["repository", "directory"]);
    }
}
