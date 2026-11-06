//! The publication driver's observed values: a field of a resource its
//! target renders, read from the workload Cluster once the Application that
//! owns the resource runs the unit's current publication.
//!
//! Contract: [Kubernetes publication unit](../../../../design/orchestration-core.md#kubernetes-publication-unit),
//! observed values, and [Observed values](../../../../design/orchestration-core.md#observed-values).
//!
//! - **Declared.** `spec.observations.<name>.from` is `{resource:
//!   {apiVersion, kind, namespace?, name}, pointer}` ([`super::spec`]).
//! - **Recorded at resolution.** The driver renders the target anyway, so it
//!   checks that the target renders each resource, with the code
//!   `NYL-KUBERNETES-OBSERVATION-UNOWNED` otherwise ([`recorded`]), and
//!   records the Application that owns it (`observationSources`) and the
//!   workload Cluster with its local context (`workload`) in the resolved
//!   spec. `observe` then reads the stored document and renders nothing, and
//!   the cluster read is the target's Cluster: nothing outside the
//!   declaration is read.
//! - **Reading.** [`ClusterReader`] reads Argo CD as `nyl attest` does
//!   (through the instance's declared access) and judges only the Applications
//!   that own an observed resource, so an Application that did not change in
//!   a publication resolves at once, and a slow, unrelated one holds nothing
//!   back. Until an owning Application runs the receipt's publication its
//!   observations are `pending` (`application-not-running-publication`); a
//!   pointer that does not resolve yet is `pending` (`field-not-set`), since
//!   controllers fill status in after Argo CD applies the manifest; a
//!   resource that is missing after the Application runs the publication,
//!   and anything that cannot be read, is `failed`.
//! - **Credentials.** The workload Cluster's resources are read through the
//!   kubeconfig the unit admits as `KUBECONFIG`, at that Cluster's declared
//!   `spec.live.context`, as for the Argo CD control-plane Cluster (they may
//!   be different contexts of one file, or of two files). A credential that
//!   is not admitted is an error naming the variable and the `env` entry
//!   that admits it. Nothing falls back to the process environment, the
//!   current context, `~/.kube/config`, or in-cluster configuration.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use nyl_core::orchestration::driver::{DriverError, DriverResult, ObservedValue};
use nyl_core::orchestration::state::DesiredUnit;
use nyl_core::orchestration::time::Timestamp;
use nyl_render::gitops::observation::{
    generated_applications, owning_application, ArgoCdObservation, GitPublicationHistory, ResourceRef, WorkloadCluster,
};
use nyl_render::gitops::publication::RenderedTarget;
use serde_json::{json, Value};

use super::argocd::evaluate::{evaluate, State};
use super::argocd::{reader, ReadError, KUBECONFIG_ENV};
use super::observe::{budget, ensure_anchored, ensure_gone_is_visible, read_all};
use super::spec::{observation_sources, OBSERVATION_SOURCES, WORKLOAD};
use super::workload::{KubeResourceReader, ResourceReader};
use nyl_render::resources::ArgoCDAccessMethod;

/// The stable code of an observation of a resource the target does not render.
pub const UNOWNED: &str = "NYL-KUBERNETES-OBSERVATION-UNOWNED";

/// Why an observation is pending: no owning Application runs the publication yet.
pub const NOT_RUNNING: &str = "application-not-running-publication";

/// Why an observation is pending: the pointer has no value in the live resource yet.
pub const FIELD_NOT_SET: &str = "field-not-set";

/// The recorded facts that do not key the unit: how the workload Cluster is
/// read (its local context) says nothing about what the unit produces.
pub(super) const UNKEYED: [&str; 1] = ["/workload/context"];

/// One observation as `observe` reads it: what to read, and the Application
/// whose publication gates the read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedSource {
    pub resource: ResourceRef,
    /// A JSON Pointer into the live resource.
    pub pointer: String,
    /// The owning Application, by namespace and name.
    pub application: (String, String),
}

/// What one reading of a unit's observed values needs.
pub struct LiveRequest<'a> {
    pub unit: &'a DesiredUnit,
    /// The unit's admitted environment: its credentials, and nothing else.
    pub env: &'a BTreeMap<String, String>,
    pub project_dir: &'a Path,
    pub now: Timestamp,
    pub deadline: Timestamp,
    /// The receipt's publication commit.
    pub commit: &'a str,
    pub argocd: &'a ArgoCdObservation,
    pub workload: &'a WorkloadCluster,
    /// Every declared observation, by name.
    pub sources: &'a BTreeMap<String, ObservedSource>,
}

/// How the driver reads the live system. The production reader asks Argo CD
/// and the workload Cluster; tests give the driver a scripted one.
pub trait LiveReader: Send + Sync + std::fmt::Debug {
    /// Each declared observation's reading; a name left out is `failed`.
    fn read(&self, request: &LiveRequest<'_>) -> BTreeMap<String, ObservedValue>;
}

/// Check that the target renders every resource the spec observes and
/// return what the resolved spec records: the owning Application of each
/// observation (`/observationSources`) and the workload Cluster
/// (`/workload`). `None` when the spec observes nothing.
pub(super) fn recorded(spec: &Value, rendered: &RenderedTarget, target: &str) -> DriverResult<Option<(Value, Value)>> {
    let declared = observation_sources(spec).map_err(DriverError::before_effects)?;
    if declared.is_empty() {
        return Ok(None);
    }
    let applications = generated_applications(rendered).map_err(|error| {
        DriverError::before_effects(format!("cannot read the Applications of target {target}: {error}"))
    })?;
    let mut sources = serde_json::Map::new();
    for (name, source) in &declared {
        let owner = owning_application(rendered, &source.resource).map_err(|error| {
            DriverError::before_effects(format!("cannot read what target {target} renders: {error}"))
        })?;
        let Some(owner) = owner else {
            let rendered_here = applications
                .iter()
                .filter(|application| application.role == nyl_render::gitops::observation::ApplicationRole::Workload)
                .map(|application| application.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(DriverError::before_effects(format!(
                "{UNOWNED}: observation {name} reads {}, which target {target} does not render (its Applications: {}); \
                 observe a resource one of its Releases renders, so that the cluster read stays the target's",
                source.resource,
                if rendered_here.is_empty() { "none".to_owned() } else { rendered_here }
            )));
        };
        sources.insert(
            name.clone(),
            json!({"application": {"namespace": owner.namespace, "name": owner.name}}),
        );
    }
    let workload = serde_json::to_value(&rendered.workload)
        .map_err(|error| DriverError::before_effects(format!("cannot record the workload Cluster: {error}")))?;
    Ok(Some((Value::Object(sources), workload)))
}

/// The sources a stored unit observes, joined with their recorded owners.
pub(super) fn stored_sources(spec: &Value) -> Result<BTreeMap<String, ObservedSource>, String> {
    let declared = observation_sources(spec)?;
    let owners = spec.pointer(OBSERVATION_SOURCES);
    declared
        .into_iter()
        .map(|(name, source)| {
            let application = owners
                .and_then(|owners| owners.get(&name))
                .and_then(|owner| {
                    Some((
                        owner.pointer("/application/namespace")?.as_str()?.to_owned(),
                        owner.pointer("/application/name")?.as_str()?.to_owned(),
                    ))
                })
                .ok_or_else(|| {
                    format!(
                        "the unit records no owning Application for observation {name}: it was resolved before it observed values; reconcile it so it records them"
                    )
                })?;
            Ok((
                name,
                ObservedSource {
                    resource: source.resource,
                    pointer: source.pointer,
                    application,
                },
            ))
        })
        .collect()
}

/// The workload Cluster a stored unit records.
pub(super) fn stored_workload(spec: &Value) -> Result<WorkloadCluster, String> {
    let workload = spec.pointer(WORKLOAD).ok_or_else(|| {
        "the unit records no workload Cluster: it was resolved before it observed values; reconcile it so it records it"
            .to_owned()
    })?;
    serde_json::from_value(workload.clone()).map_err(|error| format!("spec.workload: {error}"))
}

/// Every declared observation `failed` with `reason`.
fn fail_all(names: impl Iterator<Item = impl AsRef<str>>, reason: &str) -> BTreeMap<String, ObservedValue> {
    names
        .map(|name| {
            (
                name.as_ref().to_owned(),
                ObservedValue::Failed {
                    reason: reason.to_owned(),
                },
            )
        })
        .collect()
}

/// The unit's observed values as `reader` reads them for its current
/// receipt. Everything that stops the reading before it starts, such as a
/// document resolved before it recorded what to read, is every observation
/// `failed` with the reason; nothing here is a Nyl-side error.
pub(super) fn observe(
    reader: &dyn LiveReader,
    ctx: &nyl_core::orchestration::driver::ExecutionContext<'_>,
    unit: &DesiredUnit,
) -> nyl_core::orchestration::driver::ObservedValues {
    let spec = unit.resolved_spec.as_ref().unwrap_or(&unit.spec);
    let values = (|| {
        let sources = stored_sources(spec)?;
        let workload = stored_workload(spec)?;
        let argocd = super::observe::observation_of(unit)?;
        let commit = ctx
            .artifacts
            .get("tree")
            .and_then(|tree| tree.spec.get("commit"))
            .and_then(Value::as_str)
            .ok_or_else(|| {
                "the receipt records no PublishedTree artifact, so there is no publication whose Application to wait for"
                    .to_owned()
            })?;
        Ok::<_, String>(reader.read(&LiveRequest {
            unit,
            env: &ctx.env,
            project_dir: &ctx.project_dir,
            now: ctx.now,
            deadline: ctx.deadline,
            commit,
            argocd: &argocd,
            workload: &workload,
            sources: &sources,
        }))
    })();
    let values = values.unwrap_or_else(|reason| {
        let names = observation_sources(spec)
            .map(|declared| declared.into_keys().collect::<Vec<_>>())
            .unwrap_or_default();
        fail_all(names.iter(), &reason)
    });
    nyl_core::orchestration::driver::ObservedValues { values }
}

/// The production reader: Argo CD through the instance's declared access,
/// then the workload Cluster through its declared context.
#[derive(Debug, Clone, Copy, Default)]
pub struct ClusterReader;

impl LiveReader for ClusterReader {
    fn read(&self, request: &LiveRequest<'_>) -> BTreeMap<String, ObservedValue> {
        let names = || request.sources.keys();
        let owners: BTreeSet<&(String, String)> = request.sources.values().map(|source| &source.application).collect();
        let argo = match reader(request.argocd, request.env, budget(request.now, request.deadline)) {
            Ok(reader) => reader,
            Err(error) => return fail_all(names(), &error.message),
        };
        // The whole reading, anchored as `nyl attest` anchors it, so a
        // credential or context for another Argo CD is never mistaken for a
        // target whose Applications are not running yet.
        let live =
            match read_all(argo.as_ref(), request.argocd).and_then(|live| {
                if request.argocd.location.access.method == ArgoCDAccessMethod::Kubernetes {
                    ensure_anchored(argo.as_ref(), request.argocd, &live)?;
                }
                ensure_gone_is_visible(
                    argo.as_ref(),
                    request.argocd,
                    &live,
                    request.argocd.applications.iter().filter(|application| {
                        owners.contains(&(application.namespace.clone(), application.name.clone()))
                    }),
                )?;
                Ok(live)
            }) {
                Ok(live) => live,
                Err(error) => return fail_all(names(), &error.message),
            };
        let variable = |name: &str| request.env.get(name).cloned();
        let history = match GitPublicationHistory::fetch(&request.argocd.publication, request.project_dir, &variable) {
            Ok(history) => history,
            Err(error) => {
                return fail_all(
                    names(),
                    &format!("cannot read the publication branch to match what Argo CD runs: {error}"),
                )
            }
        };
        // Only the Applications that own an observed resource decide.
        let mut owning = request.argocd.clone();
        owning
            .applications
            .retain(|application| owners.contains(&(application.namespace.clone(), application.name.clone())));
        let evaluation = match evaluate(&owning, request.commit, &live, &history) {
            Ok(evaluation) => evaluation,
            Err(error) => {
                return fail_all(
                    names(),
                    &format!("cannot match what Argo CD runs to the publication branch: {error}"),
                )
            }
        };
        let state_of = |owner: &(String, String)| {
            evaluation
                .findings
                .iter()
                .find(|finding| (&finding.application.namespace, &finding.application.name) == (&owner.0, &owner.1))
        };
        let mut cluster: Option<Result<KubeResourceReader, ReadError>> = None;
        let mut values = BTreeMap::new();
        for (name, source) in request.sources {
            let running = state_of(&source.application);
            if let Some(finding) = running.filter(|finding| finding.state != State::Accepted) {
                values.insert(
                    name.clone(),
                    ObservedValue::Pending {
                        reason: format!(
                            "{NOT_RUNNING}: Application {}/{} does not run the publication {} yet ({}){}",
                            source.application.0,
                            source.application.1,
                            request.commit,
                            finding.state.name(),
                            finding
                                .reason
                                .as_ref()
                                .map(|reason| format!(": {reason}"))
                                .unwrap_or_default()
                        ),
                    },
                );
                continue;
            }
            if running.is_none() {
                values.insert(
                    name.clone(),
                    ObservedValue::Failed {
                        reason: format!(
                            "the evaluation of Application {}/{} is missing",
                            source.application.0, source.application.1
                        ),
                    },
                );
                continue;
            }
            let reading = cluster.get_or_insert_with(|| {
                KubeResourceReader::new(
                    &request.workload.cluster,
                    request.workload.context.as_deref(),
                    request.env.get(KUBECONFIG_ENV).map(String::as_str),
                    budget(request.now, request.deadline),
                )
            });
            values.insert(name.clone(), read_field(reading, source));
        }
        values
    }
}

/// One field of one live resource.
fn read_field(reader: &Result<KubeResourceReader, ReadError>, source: &ObservedSource) -> ObservedValue {
    let reader = match reader {
        Ok(reader) => reader,
        Err(error) => {
            return ObservedValue::Failed {
                reason: error.message.clone(),
            }
        }
    };
    match reader.get(&source.resource) {
        Err(error) => ObservedValue::Failed { reason: error.message },
        Ok(None) => ObservedValue::Failed {
            reason: format!(
                "{} does not exist in the workload Cluster, although Application {}/{} runs the publication that renders it: check that the Cluster's spec.live.context is the workload Cluster",
                source.resource, source.application.0, source.application.1
            ),
        },
        Ok(Some(object)) => match nyl_core::json_pointer::resolve(&object, &source.pointer) {
            Some(value) => ObservedValue::Value { value: value.clone() },
            None => ObservedValue::Pending {
                reason: format!("{FIELD_NOT_SET}: {} of {} is not set yet", source.pointer, source.resource),
            },
        },
    }
}
