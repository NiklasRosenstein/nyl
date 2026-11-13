//! Purging the live resources of an Argo CD Application from its recorded
//! inventory, without generating manifests.
//!
//! An Application whose source path was removed from Git cannot generate
//! manifests, and Argo CD neither prunes nor cascade-deletes without them. Its
//! `status.resources` still lists every resource it tracks. `nyl argocd
//! resources` and `nyl argocd purge` read that list and delete those resources
//! on the destination cluster, keeping what the Application's own deletion
//! keeps. See the [command reference](https://niklasrosenstein.github.io/nyl/reference/cli/argocd/).
//!
//! Rules, each with tests below:
//! - A resource is deleted only when its live object carries this
//!   Application's Argo CD tracking ([`tracking_matches`]).
//! - The live object's `Delete` sync option keeps it ([`crate::kubernetes::prune::delete_decision`]).
//! - Deletion runs in reverse apply order ([`crate::kubernetes::delete_in_prune_order`]).
//! - The Application itself is deleted only after a clean purge
//!   ([`ApplicationPurgeReport::application_deletable`]).

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::kubernetes::prune::{delete_decision, PruneDecision};
use crate::kubernetes::{delete_in_prune_order, DeleteOptions, GroupVersionKind, KubeClient, PruneEvent, ResourceKey};
use crate::resources::ClusterDestination;
use crate::{NylError, Result};

/// The Argo CD annotation that records which Application tracks an object.
pub const TRACKING_ID_ANNOTATION: &str = "argocd.argoproj.io/tracking-id";
/// The label Argo CD's label tracking method sets to the Application name.
pub const INSTANCE_LABEL: &str = "app.kubernetes.io/instance";
/// The API server URL Argo CD uses for the cluster it runs in.
pub const IN_CLUSTER_SERVER: &str = "https://kubernetes.default.svc";
/// The name Argo CD gives the cluster it runs in.
pub const IN_CLUSTER_NAME: &str = "in-cluster";

/// The kind of Argo CD Applications.
pub fn application_gvk() -> GroupVersionKind {
    GroupVersionKind {
        group: "argoproj.io".to_owned(),
        version: "v1alpha1".to_owned(),
        kind: "Application".to_owned(),
    }
}

/// One entry of an Application's `status.resources`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackedResource {
    #[serde(default)]
    pub group: String,
    #[serde(default)]
    pub version: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    pub name: String,
    /// Argo CD's sync status of the resource, as recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Whether the resource is a sync hook.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hook: bool,
}

impl TrackedResource {
    /// The resource's key; an empty namespace means a cluster-scoped resource.
    pub fn key(&self) -> ResourceKey {
        ResourceKey {
            gvk: GroupVersionKind {
                group: self.group.clone(),
                version: self.version.clone(),
                kind: self.kind.clone(),
            },
            namespace: self.namespace.clone().filter(|namespace| !namespace.is_empty()),
            name: self.name.clone(),
        }
    }
}

/// An Application's destination, as its `spec.destination` names it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApplicationDestination {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
}

impl ApplicationDestination {
    /// Whether the destination is the cluster Argo CD runs in.
    pub fn is_in_cluster(&self) -> bool {
        self.server
            .as_deref()
            .is_some_and(|server| normalize_server_url(server) == IN_CLUSTER_SERVER)
            || self.name.as_deref() == Some(IN_CLUSTER_NAME)
    }

    fn describe(&self) -> String {
        describe(self.server.as_deref(), self.name.as_deref())
    }
}

fn describe(server: Option<&str>, name: Option<&str>) -> String {
    match (server, name) {
        (Some(server), _) => format!("server {server}"),
        (None, Some(name)) => format!("cluster {name:?}"),
        (None, None) => "no cluster".to_owned(),
    }
}

/// What an Application records about the resources it tracks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplicationInventory {
    pub name: String,
    pub namespace: String,
    pub destination: ApplicationDestination,
    pub resources: Vec<TrackedResource>,
    pub finalizers: Vec<String>,
}

/// Read an Application object's identity, destination, and `status.resources`.
pub fn parse_application(application: &Value) -> Result<ApplicationInventory> {
    let metadata = application
        .get("metadata")
        .ok_or_else(|| NylError::config("The Application has no metadata"))?;
    let text = |value: Option<&Value>| value.and_then(Value::as_str).unwrap_or_default().to_owned();
    let name = text(metadata.get("name"));
    let namespace = text(metadata.get("namespace"));
    let destination = application
        .pointer("/spec/destination")
        .cloned()
        .map(serde_json::from_value)
        .transpose()?
        .unwrap_or_default();
    let resources = application
        .pointer("/status/resources")
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|error| NylError::config(format!("Application {name:?} has malformed status.resources: {error}")))?
        .unwrap_or_default();
    let finalizers = metadata
        .get("finalizers")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_str).map(str::to_owned).collect())
        .unwrap_or_default();
    Ok(ApplicationInventory {
        name,
        namespace,
        destination,
        resources,
        finalizers,
    })
}

/// Normalize an API server URL for comparison: lowercase scheme and host, no
/// trailing slash.
pub fn normalize_server_url(value: &str) -> String {
    let value = value.trim().trim_end_matches('/');
    let Some((scheme, remainder)) = value.split_once("://") else {
        return value.to_owned();
    };
    let boundary = remainder.find(['/', '?', '#']).unwrap_or(remainder.len());
    let (authority, suffix) = remainder.split_at(boundary);
    format!(
        "{}://{}{suffix}",
        scheme.to_ascii_lowercase(),
        authority.to_ascii_lowercase()
    )
}

/// Check that the Application deploys to a target's declared Cluster.
pub fn check_declared_destination(inventory: &ApplicationInventory, cluster: &ClusterDestination) -> Result<()> {
    let destination = &inventory.destination;
    let matches = match (&cluster.server, &cluster.name) {
        (Some(server), _) => destination
            .server
            .as_deref()
            .is_some_and(|actual| normalize_server_url(actual) == normalize_server_url(server)),
        (None, Some(name)) => destination.name.as_deref() == Some(name.as_str()),
        (None, None) => false,
    };
    if matches {
        Ok(())
    } else {
        Err(destination_mismatch(
            inventory,
            &describe(cluster.server.as_deref(), cluster.name.as_deref()),
        ))
    }
}

/// Check that the cluster a kube context reaches is the Application's
/// destination, when the destination names a server to compare with.
pub fn check_live_destination(inventory: &ApplicationInventory, context_server: &str) -> Result<()> {
    match inventory.destination.server.as_deref() {
        Some(server)
            if normalize_server_url(server) != IN_CLUSTER_SERVER
                && normalize_server_url(server) != normalize_server_url(context_server) =>
        {
            Err(destination_mismatch(inventory, &format!("server {context_server}")))
        }
        _ => Ok(()),
    }
}

fn destination_mismatch(inventory: &ApplicationInventory, selected: &str) -> NylError {
    NylError::config(format!(
        "NYL-ARGOCD-DESTINATION-MISMATCH: Application {:?} deploys to {}, but the selected cluster is {selected}; \
         select the cluster the Application deploys to",
        inventory.name,
        inventory.destination.describe()
    ))
}

/// Whether a live object carries the tracking of Application `name` in
/// `namespace`: its tracking-id annotation names the Application, as
/// `<name>:…` or, for Applications outside the control-plane namespace,
/// `<namespace>_<name>:…`, or its instance label is the Application's name.
pub fn tracking_matches(
    name: &str,
    namespace: &str,
    annotations: Option<&BTreeMap<String, String>>,
    labels: Option<&BTreeMap<String, String>>,
) -> bool {
    let qualified = format!("{namespace}_{name}");
    let names_application = |value: &str| value == name || value == qualified;
    if let Some(tracking) = annotations.and_then(|annotations| annotations.get(TRACKING_ID_ANNOTATION)) {
        return tracking
            .split_once(':')
            .is_some_and(|(application, _)| names_application(application));
    }
    labels
        .and_then(|labels| labels.get(INSTANCE_LABEL))
        .is_some_and(|instance| names_application(instance))
}

/// Options of a purge.
#[derive(Debug, Clone, Copy, Default)]
pub struct PurgeOptions {
    /// `--confirm-delete`: also delete resources whose sync option is `Delete=confirm`.
    pub confirm_delete: bool,
    /// `--ignore-tracking`: delete resources whatever tracking they carry.
    pub ignore_tracking: bool,
    /// Native deletion options.
    pub delete: DeleteOptions,
    /// Maximum number of concurrent deletions.
    pub concurrency: usize,
}

/// What happened, or would happen, to one tracked resource.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case", tag = "outcome")]
pub enum PurgeOutcome {
    /// A dry run would delete it.
    WouldDelete,
    /// It was deleted.
    Deleted,
    /// It no longer exists, or its kind is no longer served.
    AlreadyGone,
    /// `Delete=false` keeps it, as an Application deletion would.
    Retained,
    /// `Delete=confirm` keeps it until `--confirm-delete`.
    AwaitingConfirmation,
    /// Its live object does not carry this Application's tracking.
    NotTracked,
    /// Reading or deleting it failed.
    Failed { error: String },
}

impl PurgeOutcome {
    /// The stable code of an outcome that needs attention.
    pub fn code(&self) -> Option<&'static str> {
        match self {
            Self::AwaitingConfirmation => Some("NYL-ARGOCD-DELETE-CONFIRM"),
            Self::NotTracked => Some("NYL-ARGOCD-NOT-TRACKED"),
            Self::Failed { .. } => Some("NYL-ARGOCD-DELETE-FAILED"),
            _ => None,
        }
    }

    /// The fix for an outcome that needs attention.
    pub fn fix(&self) -> Option<&'static str> {
        match self {
            Self::AwaitingConfirmation => Some("Pass --confirm-delete to delete resources marked Delete=confirm"),
            Self::NotTracked => {
                Some("Check that no other owner manages the resource, then pass --ignore-tracking to delete it")
            }
            Self::Failed { .. } => Some("Fix the error and run the purge again; deleted resources are not retried"),
            _ => None,
        }
    }

    /// A short label for tables.
    pub fn label(&self) -> &'static str {
        match self {
            Self::WouldDelete => "would-delete",
            Self::Deleted => "deleted",
            Self::AlreadyGone => "already-gone",
            Self::Retained => "retained",
            Self::AwaitingConfirmation => "awaiting-confirmation",
            Self::NotTracked => "not-tracked",
            Self::Failed { .. } => "failed",
        }
    }
}

/// One tracked resource and its outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PurgeEntry {
    #[serde(flatten)]
    pub resource: TrackedResource,
    #[serde(flatten)]
    pub outcome: PurgeOutcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<&'static str>,
}

impl PurgeEntry {
    fn new(resource: TrackedResource, outcome: PurgeOutcome) -> Self {
        Self {
            code: outcome.code(),
            fix: outcome.fix(),
            resource,
            outcome,
        }
    }

    fn set_outcome(&mut self, outcome: PurgeOutcome) {
        self.code = outcome.code();
        self.fix = outcome.fix();
        self.outcome = outcome;
    }
}

/// What a purge did, or would do, with the Application object itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApplicationAction {
    /// `--delete-application` was not passed.
    NotRequested,
    /// A dry run would delete it.
    WouldDelete,
    /// Its finalizers were removed and it was deleted.
    Deleted,
    /// Resources still need attention, so it was kept.
    Kept,
}

/// How a purge ended, for the exit code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PurgeExitCategory {
    /// Everything was deleted or deliberately kept, or a dry run.
    Complete,
    /// Resources await `--confirm-delete` or are not tracked by the Application.
    NeedsAttention,
    /// A deletion failed.
    Failed,
}

impl PurgeExitCategory {
    /// The process exit code.
    pub fn exit_code(self) -> i32 {
        match self {
            Self::Complete => 0,
            Self::NeedsAttention => 2,
            Self::Failed => 3,
        }
    }
}

/// A finding about the purge as a whole.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PurgeFinding {
    pub code: &'static str,
    pub message: String,
    pub fix: String,
}

/// The single typed report every output of `nyl argocd resources` and
/// `nyl argocd purge` renders from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplicationPurgeReport {
    pub application: String,
    pub namespace: String,
    pub destination: ApplicationDestination,
    pub dry_run: bool,
    pub resources: Vec<PurgeEntry>,
    pub application_action: ApplicationAction,
    pub findings: Vec<PurgeFinding>,
}

impl ApplicationPurgeReport {
    /// Whether nothing still needs attention, so the Application can go:
    /// every resource is deleted, gone, or deliberately retained.
    pub fn application_deletable(&self) -> bool {
        self.resources.iter().all(|entry| {
            matches!(
                entry.outcome,
                PurgeOutcome::WouldDelete | PurgeOutcome::Deleted | PurgeOutcome::AlreadyGone | PurgeOutcome::Retained
            )
        })
    }

    /// How the purge ended.
    pub fn exit_category(&self) -> PurgeExitCategory {
        let outcomes = || self.resources.iter().map(|entry| &entry.outcome);
        if outcomes().any(|outcome| matches!(outcome, PurgeOutcome::Failed { .. })) {
            PurgeExitCategory::Failed
        } else if outcomes()
            .any(|outcome| matches!(outcome, PurgeOutcome::AwaitingConfirmation | PurgeOutcome::NotTracked))
        {
            PurgeExitCategory::NeedsAttention
        } else {
            PurgeExitCategory::Complete
        }
    }

    /// Record what happened to the Application object.
    pub fn set_application_action(&mut self, action: ApplicationAction) {
        self.application_action = action;
        if action == ApplicationAction::Kept {
            self.findings.push(PurgeFinding {
                code: "NYL-ARGOCD-APPLICATION-KEPT",
                message: format!(
                    "Application {:?} was kept because resources still need attention",
                    self.application
                ),
                fix: "Resolve the resources the report flags, then run the purge again with --delete-application"
                    .to_owned(),
            });
        }
    }
}

/// Classify every tracked resource by its live object, deleting nothing: the
/// report of a dry run, and the plan a purge executes.
pub async fn plan_purge(
    client: &dyn KubeClient,
    inventory: &ApplicationInventory,
    options: &PurgeOptions,
) -> ApplicationPurgeReport {
    let mut resources = Vec::with_capacity(inventory.resources.len());
    for resource in &inventory.resources {
        let key = resource.key();
        let outcome = match client.get_resource(&key.gvk, key.namespace.as_deref(), &key.name).await {
            Ok(None) => PurgeOutcome::AlreadyGone,
            Ok(Some(live)) => classify_live(inventory, &live.metadata, options),
            Err(error) if error.is_api_resource_not_found_error() => match client.is_kind_removed(&key.gvk).await {
                Ok(true) => PurgeOutcome::AlreadyGone,
                _ => PurgeOutcome::Failed {
                    error: error.to_string(),
                },
            },
            Err(error) => PurgeOutcome::Failed {
                error: error.to_string(),
            },
        };
        resources.push(PurgeEntry::new(resource.clone(), outcome));
    }
    let mut findings = Vec::new();
    if inventory.resources.is_empty() {
        findings.push(PurgeFinding {
            code: "NYL-ARGOCD-NO-RESOURCES",
            message: format!("Application {:?} records no resources in its status", inventory.name),
            fix: "Nothing to purge. If resources remain, find them by the Application's tracking label or annotation"
                .to_owned(),
        });
    }
    ApplicationPurgeReport {
        application: inventory.name.clone(),
        namespace: inventory.namespace.clone(),
        destination: inventory.destination.clone(),
        dry_run: true,
        resources,
        application_action: ApplicationAction::NotRequested,
        findings,
    }
}

fn classify_live(
    inventory: &ApplicationInventory,
    metadata: &kube::api::ObjectMeta,
    options: &PurgeOptions,
) -> PurgeOutcome {
    if !options.ignore_tracking
        && !tracking_matches(
            &inventory.name,
            &inventory.namespace,
            metadata.annotations.as_ref(),
            metadata.labels.as_ref(),
        )
    {
        return PurgeOutcome::NotTracked;
    }
    match delete_decision(metadata.annotations.as_ref(), options.confirm_delete) {
        PruneDecision::Prune => PurgeOutcome::WouldDelete,
        PruneDecision::Retain => PurgeOutcome::Retained,
        PruneDecision::AwaitConfirmation => PurgeOutcome::AwaitingConfirmation,
    }
}

/// Delete the resources a plan would delete, in reverse apply order, and
/// record each result. `on_event` follows the deletion's progress.
pub async fn execute_purge(
    client: &dyn KubeClient,
    mut report: ApplicationPurgeReport,
    options: &PurgeOptions,
    on_event: &mut (dyn FnMut(PruneEvent<'_>) + Send),
) -> ApplicationPurgeReport {
    report.dry_run = false;
    let keys: Vec<ResourceKey> = report
        .resources
        .iter()
        .filter(|entry| entry.outcome == PurgeOutcome::WouldDelete)
        .map(|entry| entry.resource.key())
        .collect();
    let mut errors: HashMap<ResourceKey, String> = HashMap::new();
    delete_in_prune_order(
        client,
        keys.iter().collect(),
        &options.delete,
        options.concurrency,
        &mut |event| {
            if let PruneEvent::Deleted {
                key,
                result: Err(error),
            } = &event
            {
                errors.insert((*key).clone(), error.to_string());
            }
            on_event(event);
        },
    )
    .await;
    for entry in &mut report.resources {
        if entry.outcome != PurgeOutcome::WouldDelete {
            continue;
        }
        let outcome = match errors.remove(&entry.resource.key()) {
            Some(error) => PurgeOutcome::Failed { error },
            None => PurgeOutcome::Deleted,
        };
        entry.set_outcome(outcome);
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kubernetes::{DeletePropagation, MockKubeClient};
    use kube::api::DynamicObject;
    use serde_json::json;

    fn application(resources: Value) -> Value {
        json!({
            "apiVersion": "argoproj.io/v1alpha1",
            "kind": "Application",
            "metadata": {
                "name": "web",
                "namespace": "argocd",
                "finalizers": ["resources-finalizer.argocd.argoproj.io"]
            },
            "spec": {"destination": {"server": "https://kubernetes.default.svc", "namespace": "web"}},
            "status": {"resources": resources}
        })
    }

    fn live(key: &ResourceKey, annotations: Value, labels: Value) -> DynamicObject {
        serde_json::from_value(json!({
            "apiVersion": key.gvk.to_api_version(),
            "kind": key.gvk.kind,
            "metadata": {
                "name": key.name,
                "namespace": key.namespace,
                "annotations": annotations,
                "labels": labels,
            }
        }))
        .unwrap()
    }

    fn tracked(key: &ResourceKey, extra_options: Option<&str>) -> DynamicObject {
        let mut annotations = json!({
            TRACKING_ID_ANNOTATION: format!("web:{}/{}:{}/{}", key.gvk.group, key.gvk.kind, key.namespace.clone().unwrap_or_default(), key.name)
        });
        if let Some(options) = extra_options {
            annotations["argocd.argoproj.io/sync-options"] = json!(options);
        }
        live(key, annotations, json!({}))
    }

    fn inventory(resources: Value) -> ApplicationInventory {
        parse_application(&application(resources)).unwrap()
    }

    fn options() -> PurgeOptions {
        PurgeOptions {
            concurrency: 4,
            ..PurgeOptions::default()
        }
    }

    #[test]
    fn test_parse_application_reads_status_resources_and_destination() {
        let inventory = inventory(json!([
            {"group": "apps", "version": "v1", "kind": "Deployment", "namespace": "web", "name": "api", "status": "Synced"},
            {"version": "v1", "kind": "Namespace", "name": "web"},
            {"group": "rbac.authorization.k8s.io", "version": "v1", "kind": "ClusterRole", "namespace": "", "name": "reader"}
        ]));
        assert_eq!(inventory.name, "web");
        assert_eq!(inventory.namespace, "argocd");
        assert!(inventory.destination.is_in_cluster());
        assert_eq!(inventory.finalizers, vec!["resources-finalizer.argocd.argoproj.io"]);
        let keys: Vec<String> = inventory
            .resources
            .iter()
            .map(|resource| resource.key().to_string())
            .collect();
        assert_eq!(keys.len(), 3);
        assert_eq!(inventory.resources[1].key().namespace, None);
        assert_eq!(inventory.resources[2].key().namespace, None);
        assert_eq!(inventory.resources[0].key().gvk.to_api_version(), "apps/v1");
    }

    #[test]
    fn test_parse_application_without_status_has_no_resources() {
        let mut value = application(json!([]));
        value.as_object_mut().unwrap().remove("status");
        assert!(parse_application(&value).unwrap().resources.is_empty());
    }

    #[test]
    fn test_check_declared_destination_compares_server_or_name() {
        let inventory = inventory(json!([]));
        let same = ClusterDestination {
            server: Some("https://KUBERNETES.default.svc/".to_owned()),
            name: None,
        };
        assert!(check_declared_destination(&inventory, &same).is_ok());
        let other = ClusterDestination {
            server: Some("https://prod.example:6443".to_owned()),
            name: None,
        };
        let error = check_declared_destination(&inventory, &other).unwrap_err().to_string();
        assert!(error.contains("NYL-ARGOCD-DESTINATION-MISMATCH"), "{error}");
        let by_name = ClusterDestination {
            server: None,
            name: Some("prod".to_owned()),
        };
        assert!(check_declared_destination(&inventory, &by_name).is_err());
    }

    #[test]
    fn test_check_live_destination_compares_named_servers_only() {
        let mut inventory = inventory(json!([]));
        assert!(check_live_destination(&inventory, "https://127.0.0.1:6443").is_ok());
        inventory.destination.server = Some("https://prod.example:6443".to_owned());
        assert!(check_live_destination(&inventory, "https://prod.example:6443/").is_ok());
        assert!(check_live_destination(&inventory, "https://dev.example:6443").is_err());
        inventory.destination = ApplicationDestination {
            name: Some("prod".to_owned()),
            ..ApplicationDestination::default()
        };
        assert!(check_live_destination(&inventory, "https://dev.example:6443").is_ok());
    }

    #[test]
    fn test_tracking_matches_annotation_label_and_namespaced_application() {
        let map = |key: &str, value: &str| BTreeMap::from([(key.to_owned(), value.to_owned())]);
        let annotation = map(TRACKING_ID_ANNOTATION, "web:apps/Deployment:web/api");
        assert!(tracking_matches("web", "argocd", Some(&annotation), None));
        let namespaced = map(TRACKING_ID_ANNOTATION, "team_web:apps/Deployment:web/api");
        assert!(tracking_matches("web", "team", Some(&namespaced), None));
        assert!(!tracking_matches("web", "argocd", Some(&namespaced), None));
        let label = map(INSTANCE_LABEL, "web");
        assert!(tracking_matches("web", "argocd", None, Some(&label)));
        let other = map(TRACKING_ID_ANNOTATION, "webapp:apps/Deployment:web/api");
        // The annotation decides when present, whatever the label says.
        assert!(!tracking_matches("web", "argocd", Some(&other), Some(&label)));
        assert!(!tracking_matches("web", "argocd", None, None));
    }

    #[tokio::test]
    async fn test_plan_purge_classifies_by_tracking_and_delete_option() {
        let inventory = inventory(json!([
            {"group": "apps", "version": "v1", "kind": "Deployment", "namespace": "web", "name": "api"},
            {"version": "v1", "kind": "ConfigMap", "namespace": "web", "name": "kept"},
            {"version": "v1", "kind": "Namespace", "name": "web"},
            {"version": "v1", "kind": "Secret", "namespace": "web", "name": "foreign"},
            {"version": "v1", "kind": "Service", "namespace": "web", "name": "gone"}
        ]));
        let client = MockKubeClient::new();
        let keys: Vec<ResourceKey> = inventory.resources.iter().map(TrackedResource::key).collect();
        client.add_resource(keys[0].clone(), tracked(&keys[0], None));
        client.add_resource(keys[1].clone(), tracked(&keys[1], Some("Delete=false")));
        client.add_resource(keys[2].clone(), tracked(&keys[2], Some("Prune=confirm,Delete=confirm")));
        client.add_resource(
            keys[3].clone(),
            live(&keys[3], json!({}), json!({INSTANCE_LABEL: "other"})),
        );

        let report = plan_purge(&client, &inventory, &options()).await;
        let outcomes: Vec<&str> = report.resources.iter().map(|entry| entry.outcome.label()).collect();
        assert_eq!(
            outcomes,
            [
                "would-delete",
                "retained",
                "awaiting-confirmation",
                "not-tracked",
                "already-gone"
            ]
        );
        assert_eq!(report.exit_category(), PurgeExitCategory::NeedsAttention);
        assert!(!report.application_deletable());
        assert_eq!(report.resources[3].code, Some("NYL-ARGOCD-NOT-TRACKED"));

        let confirmed = PurgeOptions {
            confirm_delete: true,
            ignore_tracking: true,
            ..options()
        };
        let report = plan_purge(&client, &inventory, &confirmed).await;
        let outcomes: Vec<&str> = report.resources.iter().map(|entry| entry.outcome.label()).collect();
        assert_eq!(
            outcomes,
            [
                "would-delete",
                "retained",
                "would-delete",
                "would-delete",
                "already-gone"
            ]
        );
        assert!(report.application_deletable());
    }

    #[tokio::test]
    async fn test_execute_purge_deletes_planned_resources_with_options() {
        let inventory = inventory(json!([
            {"group": "apps", "version": "v1", "kind": "Deployment", "namespace": "web", "name": "api"},
            {"version": "v1", "kind": "ConfigMap", "namespace": "web", "name": "kept"}
        ]));
        let client = MockKubeClient::new();
        let keys: Vec<ResourceKey> = inventory.resources.iter().map(TrackedResource::key).collect();
        client.add_resource(keys[0].clone(), tracked(&keys[0], None));
        client.add_resource(keys[1].clone(), tracked(&keys[1], Some("Delete=false")));
        let options = PurgeOptions {
            delete: DeleteOptions {
                propagation: Some(DeletePropagation::Foreground),
                grace_period_seconds: Some(30),
            },
            ..options()
        };

        let plan = plan_purge(&client, &inventory, &options).await;
        let report = execute_purge(&client, plan, &options, &mut |_| {}).await;

        assert!(!report.dry_run);
        assert_eq!(report.resources[0].outcome, PurgeOutcome::Deleted);
        assert_eq!(report.resources[1].outcome, PurgeOutcome::Retained);
        assert_eq!(report.exit_category(), PurgeExitCategory::Complete);
        assert!(report.application_deletable());
        assert_eq!(client.deletion_options(), vec![options.delete]);
        let remaining: Vec<ResourceKey> = client.get_all_resources().into_keys().collect();
        assert_eq!(remaining, vec![keys[1].clone()]);
    }

    #[tokio::test]
    async fn test_plan_purge_reports_empty_inventory() {
        let report = plan_purge(&MockKubeClient::new(), &inventory(json!([])), &options()).await;
        assert_eq!(report.findings[0].code, "NYL-ARGOCD-NO-RESOURCES");
        assert_eq!(report.exit_category(), PurgeExitCategory::Complete);
    }

    #[test]
    fn test_application_kept_adds_finding() {
        let mut report = ApplicationPurgeReport {
            application: "web".to_owned(),
            namespace: "argocd".to_owned(),
            destination: ApplicationDestination::default(),
            dry_run: false,
            resources: vec![PurgeEntry::new(
                TrackedResource {
                    group: String::new(),
                    version: "v1".to_owned(),
                    kind: "ConfigMap".to_owned(),
                    namespace: Some("web".to_owned()),
                    name: "x".to_owned(),
                    status: None,
                    hook: false,
                },
                PurgeOutcome::Failed {
                    error: "boom".to_owned(),
                },
            )],
            application_action: ApplicationAction::NotRequested,
            findings: Vec::new(),
        };
        assert_eq!(report.exit_category().exit_code(), 3);
        report.set_application_action(ApplicationAction::Kept);
        assert_eq!(report.findings[0].code, "NYL-ARGOCD-APPLICATION-KEPT");
    }
}
