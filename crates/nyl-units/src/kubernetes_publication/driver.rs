//! The `KubernetesPublication` driver.
//!
//! Contract: [Kubernetes publication unit](../../../../design/orchestration-core.md#kubernetes-publication-unit).
//!
//! - **Execution key.** The render's portable input record, through
//!   [`resolution_inputs`], plus the resolved spec (`observe`, the Release
//!   inputs under `releases`).
//! - **Teardown settings.** Resolution renders the target anyway, so the
//!   driver records the effective settings that decide whether a teardown
//!   can complete (`teardownSettings`: catalog pruning, Application deletion
//!   policy, Namespace delete policies, catalog ownership) in the resolved
//!   spec through [`KeyInputs::recorded`]. `status`, `plan`, and a teardown
//!   then read the same settings from the stored document without rendering.
//! - **Ownership.** The unit claims its DeploymentTarget by name, inline
//!   targets included. Two units that publish one target overlap
//!   (`NYL-UNIT-OWNERSHIP-OVERLAP`); the destination (repository, revision,
//!   and prefix) belongs to the target, and the ownership index refuses a
//!   tree owned by another target at publication. The index also records
//!   the owning unit incarnation, and publication and teardown refuse a
//!   prefix another existing incarnation owns (owner fencing, see
//!   [`super::run`]).
//! - **Teardown readiness.** The driver evaluates the recorded settings
//!   itself ([`super::readiness`]); the shell only displays the findings.
//! - **Build.** `nyl build` renders the tree, compares it with the published
//!   one (`--diff`), or publishes it (`--push`) when no environment owns the
//!   target ([`super::build`]).
//! - **Drift.** Drift is the destination branch no longer holding the tree
//!   the unit renders: a dry publication that would commit something, or one
//!   that finds an owned file modified outside Nyl. For a direct target it is
//!   also the cluster no longer holding what the unit last applied
//!   ([`super::direct::drift`]). A comparison that cannot be made, such as an
//!   unreachable remote or cluster, is `unknown`.
//!
//! - **Observed values.** With `observe: {}` the unit reads a field of a
//!   resource its target renders from the workload Cluster, once the
//!   Application that owns it runs the receipt's publication
//!   ([`super::observed_values`]). Resolution checks the resource is the
//!   target's (`NYL-KUBERNETES-OBSERVATION-UNOWNED`) and records its owning
//!   Application and the workload Cluster.
//! - **Observation.** A unit that declares `observe: {}` records the
//!   reconciler of its target at resolution (`reconciler`: its kind and
//!   instance, how it is read, and the deployments and anchors the target
//!   expects of it), declares the attestations `accepted` and `healthy`, and
//!   reads the reconciler for `nyl attest` and the `observe` teardown wait
//!   ([`super::observe`]); which reconciler it reads follows the target, not
//!   the flag.

use std::ffi::OsString;
use std::sync::Arc;

use super::run::{read_shared_catalog, SHARED_CATALOG_READING};
use nyl_core::orchestration::driver::{
    AttestationCheck, Capabilities, DriftDetection, Driver, DriverError, DriverResult, ExecStatus, ExecutionContext,
    Inspection, KeyInputs, ObservedValues, Outcome, OwnershipScope, PlanReport, RecoveryPolicies, RecoveryPolicy,
    RemovalContext, RemovalObservation, ResolutionContext, Supported, TeardownReadiness, WaitStrategy,
};
use nyl_core::orchestration::ids::EnvironmentName;
use nyl_core::orchestration::ports::CommitIdentity;
use nyl_core::orchestration::state::{ArtifactDocument, DesiredUnit, Drift, Receipt};
use nyl_render::gitops::publication::{prepare_target_publication, PublicationAuthor, TeardownSettings};
use serde_json::{json, Value};

use super::direct::{self, ClusterAccess};
use super::observe::{self, RECONCILER};
use super::observed_values::{self, ClusterReader, LiveReader};
use super::readiness::{evaluate, evaluate_direct};
use super::run::publish_via;
use super::spec::{OBSERVATION_SOURCES, WORKLOAD};
use super::{build, plan, render, resolution_inputs, teardown, teardown_wait, unit_owner, unit_target, KIND};
use crate::command::resolved_spec;

use super::reconciler::evaluate::{ACCEPTED, HEALTHY};

/// The pointer, in the resolved spec, of the recorded teardown settings.
const TEARDOWN_SETTINGS: &str = "/teardownSettings";

/// The built-in publication kind, rendering against the caller's resource catalogue.
#[derive(Debug, Clone)]
pub struct KubernetesPublicationDriver {
    resource_catalogue: nyl_render::config::ResourceCatalogue,
    /// How observed values are read from the live system.
    live: Arc<dyn LiveReader>,
    /// How a direct target's cluster is reached.
    cluster: ClusterAccess,
}

impl KubernetesPublicationDriver {
    /// A publication driver that discovers and renders against
    /// `resource_catalogue` and reads observed values from Argo CD and the
    /// workload Cluster.
    pub fn new(resource_catalogue: nyl_render::config::ResourceCatalogue) -> Self {
        Self {
            resource_catalogue,
            live: Arc::new(ClusterReader),
            cluster: ClusterAccess::Live,
        }
    }

    /// The same driver reaching a direct target's cluster through `access`,
    /// such as a fixed mock client for tests that run no cluster.
    #[must_use]
    pub fn with_cluster_access(mut self, access: ClusterAccess) -> Self {
        self.cluster = access;
        self
    }

    /// The same driver reading observed values through `live`, such as a
    /// scripted reader for tier-1 scenarios; the declaration, its validation,
    /// and what resolution records are the driver's own either way.
    #[must_use]
    pub fn with_live_reader(mut self, live: Arc<dyn LiveReader>) -> Self {
        self.live = live;
        self
    }
}

impl KubernetesPublicationDriver {
    /// Whether the destination branch still holds the tree the unit renders:
    /// a dry publication that would commit something, or one that finds an
    /// owned file modified outside Nyl, is drift.
    fn tree_drift(&self, ctx: &ExecutionContext<'_>, unit: &DesiredUnit, spec: &Value) -> DriverResult<Drift> {
        let rendered = render(
            &self.resource_catalogue,
            &ctx.worktree,
            &ctx.project_dir,
            &ctx.cache_dir,
            ctx.instances,
            spec,
            Some(unit_owner(unit)?),
        )?;
        // Nothing is committed to the destination: the commit this prepares
        // in a temporary checkout is only compared, so its author is a
        // placeholder that never reaches the repository.
        let author = PublicationAuthor {
            identity: CommitIdentity {
                name: "nyl drift check".to_owned(),
                email: "nyl@localhost".to_owned(),
            },
            time: ctx.now,
        };
        let variable = |name: &str| ctx.env.get(name).cloned();
        Ok(
            match prepare_target_publication(&rendered, &ctx.project_dir, "nyl: drift check", &author, &variable) {
                Ok(prepared) if prepared.is_unchanged() => Drift::Clean,
                Ok(_) => Drift::Drifted {
                    summary: format!(
                        "{}@{} no longer holds the tree the unit publishes",
                        rendered.destination.repository, rendered.destination.branch
                    ),
                },
                // An owned file edited outside Nyl is drift: reconciling it fails as a publication does.
                Err(error) if error.is_owned_file_modified() => Drift::Drifted {
                    summary: error.to_string(),
                },
                // Another incarnation's prefix is not this unit's to compare; reconciling decides whether it takes it over.
                Err(error) if error.is_owner_fenced() => Drift::Unknown {
                    reason: error.to_string(),
                },
                Err(error) => Drift::Unknown {
                    reason: format!("cannot compare the publication branch with the rendered tree: {error}"),
                },
            },
        )
    }
}

impl Driver for KubernetesPublicationDriver {
    fn api_version(&self) -> &'static str {
        nyl_core::constants::API_VERSION_UNITS
    }

    fn kind(&self) -> &'static str {
        KIND
    }

    fn behavior_version(&self) -> u32 {
        1
    }

    fn default_timeout(&self) -> nyl_core::orchestration::time::Duration {
        nyl_core::orchestration::time::Duration::from_mins(10)
    }

    fn capabilities(&self, spec: &Value) -> Capabilities {
        Capabilities {
            requires_committed_source: true,
            plan: true,
            reconcile: true,
            detect_drift: true,
            teardown: true,
            build: true,
            attest: observe::observes(spec),
            observed_values: observe::observes(spec),
            ..Capabilities::default()
        }
    }

    fn recovery(&self, _spec: &Value) -> RecoveryPolicies {
        // Publishing the same tree again is a no-op, and a stale branch is
        // refused by compare-and-swap, so re-executing always converges.
        RecoveryPolicies {
            execution: RecoveryPolicy::Converge,
            teardown: RecoveryPolicy::Converge,
        }
    }

    fn spec_schema(&self) -> schemars::Schema {
        let schema = schemars::json_schema!({
            "type": "object",
            "required": ["target"],
            "properties": {
                "target": {
                    "description": "The DeploymentTarget this unit renders and publishes: the name of a static one, or one declared inline and owned by the unit.",
                    "oneOf": [
                        {"type": "string"},
                        {
                            "type": "object",
                            "required": ["inline"],
                            "properties": {
                                "inline": {
                                    "type": "object",
                                    "description": "A DeploymentTarget spec, rendered with the unit per environment."
                                },
                                "name": {
                                    "type": "string",
                                    "description": "Overrides the inline target's name, `<environment>-<unit>` by default; unique like every target name."
                                }
                            },
                            "additionalProperties": false
                        }
                    ]
                },
                "observe": {
                    "type": "object",
                    "description": "Opts the unit into observing the reconciler of its target (Argo CD, or Nyl's direct reconciler): `nyl attest` reads it and attests `accepted` and `healthy`, the `observe` teardownWait strategy becomes available and its default. Written `observe: {}`. Which reconciler it reads follows the target. `waitHealthy` makes a reconcile wait for health.",
                    "properties": {
                        "waitHealthy": {
                            "type": "object",
                            "description": "After the publish (and, for a direct target, the apply), reconcile polls the reconciler with backoff until `healthy` passes, then records the `accepted` and `healthy` attestations `nyl attest` would. On the timeout the unit fails, retryable, with NYL-KUBERNETES-HEALTH-TIMEOUT naming the worst deployment and its last health; a health that cannot recover, such as a rollout past its progress deadline, fails at once with NYL-KUBERNETES-HEALTH-DEGRADED. Without it, reconcile does not wait.",
                            "required": ["timeout"],
                            "properties": {
                                "timeout": {"type": "string", "description": "How long to wait, such as 5m; never longer than the unit's own timeout."}
                            },
                            "additionalProperties": false
                        }
                    },
                    "additionalProperties": false
                },
                "teardownWait": {
                    "type": "object",
                    "description": "When phase 1 of a teardown counts as complete.",
                    "required": ["strategy"],
                    "properties": {
                        "strategy": {"type": "string", "enum": ["observe", "delay", "manual"]},
                        "duration": {"type": "string", "description": "How long after phase 1, for strategy delay, such as 10m."}
                    },
                    "additionalProperties": false
                },
                "observations": {
                    "type": "object",
                    "description": "Observed values, which need `observe: {}`: a field of a resource the target renders, read from the workload Cluster once the Application that owns the resource runs the current publication.",
                    "additionalProperties": {
                        "type": "object",
                        "required": ["type", "from"],
                        "properties": {
                            "type": {"type": "string", "enum": ["string", "integer", "number", "boolean", "object", "array"]},
                            "maxAge": {"type": "string", "description": "How old the last successful read may get before the value stops resolving, such as 1h."},
                            "from": {
                                "type": "object",
                                "description": "What to read.",
                                "required": ["resource", "pointer"],
                                "properties": {
                                    "resource": {
                                        "type": "object",
                                        "description": "The resource as the Kubernetes API names it; a Secret is never read.",
                                        "required": ["apiVersion", "kind", "name"],
                                        "properties": {
                                            "apiVersion": {"type": "string"},
                                            "kind": {"type": "string"},
                                            "namespace": {"type": "string", "description": "For a namespaced kind."},
                                            "name": {"type": "string"}
                                        },
                                        "additionalProperties": false
                                    },
                                    "pointer": {"type": "string", "description": "A JSON Pointer into the live resource, such as /status/loadBalancer/ingress/0/ip."}
                                },
                                "additionalProperties": false
                            }
                        }
                    }
                },
                "releases": {
                    "type": "object",
                    "description": "Filled from the target's releaseInputs; not written by hand.",
                    "additionalProperties": {"type": "object"}
                }
            }
        });
        let (inline, definitions) = inline_target_schema();
        let mut root = Value::from(schema);
        root["properties"]["target"]["oneOf"][1]["properties"]["inline"] = inline;
        if let Some(definitions) = definitions {
            root["$defs"] = definitions;
        }
        schemars::Schema::try_from(root).expect("an object schema")
    }

    fn validate_spec(&self, spec: &Value) -> Result<(), String> {
        super::spec::validate(spec)
    }

    /// `accepted` and `healthy` for a unit that observes; one that does not
    /// is satisfied by its receipt and declares none.
    fn attestations(&self, spec: &Value) -> Vec<String> {
        if observe::observes(spec) {
            vec![ACCEPTED.to_owned(), HEALTHY.to_owned()]
        } else {
            Vec::new()
        }
    }

    fn ownership_scopes(&self, _environment: &EnvironmentName, spec: &Value) -> Vec<OwnershipScope> {
        unit_target(spec)
            .ok()
            .map(|target| OwnershipScope {
                description: format!("DeploymentTarget {target}"),
                namespace: "kubernetes-target".to_owned(),
                key: vec![target],
                path: Vec::new(),
            })
            .into_iter()
            .collect()
    }

    fn execution_key_inputs(&self, ctx: &ResolutionContext<'_>, spec: &Value) -> DriverResult<KeyInputs> {
        let (rendered, mut inputs) = resolution_inputs(&self.resource_catalogue, ctx, spec)?;
        let settings = nyl_render::gitops::publication::teardown_settings(&rendered).map_err(|error| {
            DriverError::before_effects(format!(
                "cannot read the teardown settings of DeploymentTarget {:?}: {error}",
                unit_target(spec).unwrap_or_default()
            ))
        })?;
        let settings = serde_json::to_value(settings)
            .map_err(|error| DriverError::before_effects(format!("cannot record the teardown settings: {error}")))?;
        inputs.recorded.insert(TEARDOWN_SETTINGS.to_owned(), settings);
        // What the branch holds of a shared catalog follows the writer, not the
        // target: recorded for readiness, never keyed.
        if let Some(reading) = read_shared_catalog(&ctx.env, &ctx.project_dir, &rendered)? {
            inputs.recorded.insert(SHARED_CATALOG_READING.to_owned(), reading);
            inputs.excluded_fields.push(SHARED_CATALOG_READING.to_owned());
        }
        // A direct unit always records where it is applied and read: its
        // teardown follows the recorded reconciler, never the declaration as it
        // reads later, whether or not the unit observes.
        let direct = matches!(rendered.delivery, nyl_render::gitops::observation::Delivery::Direct(_));
        if let nyl_render::gitops::observation::Delivery::Direct(location) = &rendered.delivery {
            if location.context.is_none() {
                return Err(DriverError::before_effects(format!(
                    "NYL-KUBERNETES-CLUSTER-CONTEXT-MISSING: Cluster {} declares no spec.live.context, which a direct target \
                     is applied and read through (Nyl never uses the current context); declare it on the Cluster",
                    location.cluster
                )));
            }
        }
        if observe::observes(spec) || direct {
            let (facts, unkeyed) = observe::recorded(&rendered)?;
            inputs.recorded.insert(RECONCILER.to_owned(), facts);
            // How the reconciler is read (the declared access, the
            // control-plane context, the URL the branch is read from) changes
            // no result: only what the target expects of it keys the unit.
            inputs.excluded_fields.extend(unkeyed);
        }
        // What an observation reads must be rendered by the target: the
        // owning Application and the workload Cluster are recorded, and the
        // Cluster's local context never keys the unit.
        if let Some((sources, workload)) =
            observed_values::recorded(spec, &rendered, &unit_target(spec).unwrap_or_default())?
        {
            inputs.recorded.insert(OBSERVATION_SOURCES.to_owned(), sources);
            inputs.recorded.insert(WORKLOAD.to_owned(), workload);
            inputs
                .excluded_fields
                .extend(observed_values::UNKEYED.iter().map(|field| (*field).to_owned()));
        }
        Ok(inputs)
    }

    fn plan(&self, ctx: &ExecutionContext<'_>, unit: &DesiredUnit) -> DriverResult<Supported<PlanReport>> {
        Ok(Supported::Supported(plan(ctx, unit)))
    }

    fn reconcile(&self, ctx: &ExecutionContext<'_>, unit: &DesiredUnit) -> DriverResult<Supported<Outcome>> {
        publish_via(&self.resource_catalogue, ctx, unit, json!({}), &self.cluster).map(Supported::Supported)
    }

    fn detect_drift(
        &self,
        ctx: &ExecutionContext<'_>,
        unit: &DesiredUnit,
        _receipt: &Receipt,
    ) -> DriverResult<Supported<DriftDetection>> {
        let spec = resolved_spec(unit)?;
        let tree = self.tree_drift(ctx, unit, spec)?;
        // A direct target has no reconciler keeping the cluster in line with
        // the tree between runs, so the cluster is compared with what the
        // unit applied; an Argo CD target is kept in line by Argo CD.
        let cluster = direct::drift::detect(ctx, unit, &unit_target(spec).unwrap_or_default(), &self.cluster);
        let drift = direct::drift::combine([("publication branch", Some(tree)), ("cluster", cluster)]);
        Ok(Supported::Supported(drift.into()))
    }

    /// The observed values the spec declares: a field of a rendered resource,
    /// read from the workload Cluster once its Application runs the receipt's
    /// publication.
    fn observe(
        &self,
        ctx: &ExecutionContext<'_>,
        unit: &DesiredUnit,
        _receipt: &Receipt,
    ) -> DriverResult<Supported<ObservedValues>> {
        Ok(Supported::Supported(observed_values::observe(
            self.live.as_ref(),
            ctx,
            unit,
        )))
    }

    /// `accepted` and `healthy` of the receipt's publication, read from the reconciler.
    fn attest(
        &self,
        ctx: &ExecutionContext<'_>,
        unit: &DesiredUnit,
        _receipt: &Receipt,
    ) -> DriverResult<Supported<AttestationCheck>> {
        observe::attest(ctx, unit, &self.cluster).map(Supported::Supported)
    }

    fn teardown_wait(&self, spec: &Value) -> WaitStrategy {
        teardown_wait(spec)
    }

    /// A `manual` `teardownWait` stops an unattended removal after phase 1.
    fn unattended_teardown(&self, unit: &DesiredUnit) -> Vec<nyl_core::orchestration::codes::Finding> {
        super::unattended_teardown(unit)
    }

    /// The workload deployments the reconciler still reports, for the
    /// `observe` teardown wait.
    fn observe_removal(&self, ctx: &RemovalContext, unit: &DesiredUnit) -> DriverResult<Supported<RemovalObservation>> {
        Ok(Supported::Supported(observe::observe_removal(ctx, unit, &self.cluster)))
    }

    fn inspect(&self, _ctx: &ExecutionContext<'_>, _unit: &DesiredUnit) -> DriverResult<Supported<Inspection>> {
        Ok(Supported::Unsupported)
    }

    fn teardown(&self, ctx: &ExecutionContext<'_>, unit: &DesiredUnit) -> DriverResult<Supported<Outcome>> {
        teardown(&self.resource_catalogue, ctx, unit, &self.cluster).map(Supported::Supported)
    }

    /// The readiness of a teardown, evaluated from the `teardownSettings`
    /// resolution recorded in the stored document; unsupported (no
    /// requirements) for a document resolved without them.
    fn teardown_readiness(&self, unit: &DesiredUnit) -> DriverResult<Supported<TeardownReadiness>> {
        let spec = unit.resolved_spec.as_ref().unwrap_or(&unit.spec);
        // A shared catalog's rows follow the manifest the branch holds, recorded unkeyed.
        if let Some(reading) = spec.pointer(SHARED_CATALOG_READING) {
            return super::readiness::evaluate_reading(&unit.unit.environment, &unit.unit.name, reading)
                .map(Supported::Supported)
                .map_err(DriverError::before_effects);
        }
        match spec.pointer(TEARDOWN_SETTINGS) {
            None => Ok(Supported::Unsupported),
            Some(settings) => {
                let settings: TeardownSettings = serde_json::from_value(settings.clone())
                    .map_err(|error| DriverError::before_effects(format!("spec.teardownSettings: {error}")))?;
                let direct = spec.pointer("/reconciler/kind").and_then(Value::as_str) == Some("direct");
                let rows = if direct { evaluate_direct } else { evaluate };
                Ok(Supported::Supported(rows(
                    &unit.unit.environment,
                    &unit.unit.name,
                    &settings,
                )))
            }
        }
    }

    fn build(
        &self,
        ctx: &ExecutionContext<'_>,
        unit: &DesiredUnit,
        options: &[OsString],
    ) -> DriverResult<Supported<Vec<ArtifactDocument>>> {
        build(&self.resource_catalogue, ctx, unit, options).map(Supported::Supported)
    }

    fn exec(
        &self,
        _ctx: &ExecutionContext<'_>,
        _unit: &DesiredUnit,
        _args: &[OsString],
    ) -> DriverResult<Supported<ExecStatus>> {
        Ok(Supported::Unsupported)
    }
}

/// The schema of an inline target, a DeploymentTarget spec, so editors complete
/// it, and the definitions it references, which belong at the document root
/// where its `#/$defs/...` references resolve.
fn inline_target_schema() -> (Value, Option<Value>) {
    let mut inline = serde_json::to_value(schemars::schema_for!(nyl_render::resources::DeploymentTargetSpec))
        .expect("a schema serializes");
    let object = inline.as_object_mut().expect("a spec schema is an object");
    let definitions = object.remove("$defs");
    object.remove("$schema");
    object.insert(
        "description".to_owned(),
        json!("A DeploymentTarget spec, rendered with the unit per environment."),
    );
    (inline, definitions)
}
