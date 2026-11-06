//! The `KubernetesPublication` driver.
//!
//! Contract: [Kubernetes publication unit](../../../../design/orchestration-core.md#kubernetes-publication-unit).
//!
//! - **Execution key.** The render's portable input record, through
//!   [`resolution_inputs`], plus the resolved spec (`mode`, the Release
//!   inputs under `releases`).
//! - **Teardown settings.** Resolution renders the target anyway, so the
//!   driver records the effective settings that decide whether a teardown
//!   can complete (`teardownSettings`: catalog pruning, Application deletion
//!   policy, Namespace delete policies, catalog ownership) in the resolved
//!   spec through [`KeyInputs::recorded`]. `status`, `plan`, and a teardown
//!   then read the same settings from the stored document without rendering.
//! - **Ownership.** The unit claims its DeploymentTarget by name. Two units
//!   that publish one target overlap
//!   (`NYL-UNIT-OWNERSHIP-OVERLAP`); the destination (repository, revision,
//!   and prefix) belongs to the target, and the ownership index refuses a
//!   tree owned by another target at publication.
//! - **Drift.** Drift is the destination branch no longer holding the tree
//!   the unit renders: a dry publication that would commit something, or one
//!   that finds an owned file modified outside Nyl. A comparison that cannot
//!   be made, such as an unreachable remote, is `unknown`.
//!
//! `mode: observe` is rejected by validation until the driver can read
//! Argo CD, so nothing promises health that is never checked.

use std::ffi::OsString;

use nyl_core::orchestration::driver::{
    Capabilities, DriftDetection, Driver, DriverError, DriverResult, ExecStatus, ExecutionContext, Inspection,
    KeyInputs, Outcome, OwnershipScope, PlanReport, RecoveryPolicies, RecoveryPolicy, ResolutionContext, Supported,
    TeardownSettings,
};
use nyl_core::orchestration::ids::EnvironmentName;
use nyl_core::orchestration::ports::CommitIdentity;
use nyl_core::orchestration::state::{ArtifactDocument, DesiredUnit, Drift, Receipt};
use nyl_render::gitops::publication::{prepare_target_publication, PublicationAuthor};
use serde_json::{json, Value};

use super::{plan, publish, render, resolution_inputs, teardown, unit_target, KIND};
use crate::command::resolved_spec;

/// The pointer, in the resolved spec, of the recorded teardown settings.
const TEARDOWN_SETTINGS: &str = "/teardownSettings";

/// The built-in publication kind, rendering against the caller's resource catalogue.
#[derive(Debug, Clone)]
pub struct KubernetesPublicationDriver {
    resource_catalogue: nyl_render::config::ResourceCatalogue,
}

impl KubernetesPublicationDriver {
    /// A publication driver that discovers and renders against `resource_catalogue`.
    pub fn new(resource_catalogue: nyl_render::config::ResourceCatalogue) -> Self {
        Self { resource_catalogue }
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

    fn capabilities(&self, _spec: &Value) -> Capabilities {
        Capabilities {
            requires_committed_source: true,
            plan: true,
            reconcile: true,
            detect_drift: true,
            teardown: true,
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
        schemars::json_schema!({
            "type": "object",
            "required": ["target"],
            "properties": {
                "target": {
                    "type": "string",
                    "description": "The static DeploymentTarget this unit renders and publishes."
                },
                "mode": {
                    "type": "string",
                    "enum": ["publish"],
                    "default": "publish",
                    "description": "How the unit relates to the published tree. Only publish is available; observe needs Argo CD access."
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
                "releases": {
                    "type": "object",
                    "description": "Filled from the target's releaseInputs; not written by hand.",
                    "additionalProperties": {"type": "object"}
                }
            }
        })
    }

    fn validate_spec(&self, spec: &Value) -> Result<(), String> {
        super::spec::validate(spec)
    }

    fn attestations(&self, _spec: &Value) -> Vec<String> {
        Vec::new()
    }

    fn ownership_scopes(&self, _environment: &EnvironmentName, spec: &Value) -> Vec<OwnershipScope> {
        spec.get("target")
            .and_then(Value::as_str)
            .map(|target| OwnershipScope {
                namespace: "kubernetes-target".to_owned(),
                key: vec![target.to_owned()],
                path: Vec::new(),
                description: format!("DeploymentTarget {target}"),
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
        Ok(inputs)
    }

    fn plan(&self, ctx: &ExecutionContext<'_>, unit: &DesiredUnit) -> DriverResult<Supported<PlanReport>> {
        Ok(Supported::Supported(plan(ctx, unit)))
    }

    fn reconcile(&self, ctx: &ExecutionContext<'_>, unit: &DesiredUnit) -> DriverResult<Supported<Outcome>> {
        publish(&self.resource_catalogue, ctx, unit, json!({})).map(Supported::Supported)
    }

    fn detect_drift(
        &self,
        ctx: &ExecutionContext<'_>,
        unit: &DesiredUnit,
        _receipt: &Receipt,
    ) -> DriverResult<Supported<DriftDetection>> {
        let spec = resolved_spec(unit)?;
        let rendered = render(
            &self.resource_catalogue,
            &ctx.worktree,
            &ctx.project_dir,
            &ctx.cache_dir,
            spec,
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
        let drift =
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
                Err(error) => Drift::Unknown {
                    reason: format!("cannot compare the publication branch with the rendered tree: {error}"),
                },
            };
        Ok(Supported::Supported(drift.into()))
    }

    fn inspect(&self, _ctx: &ExecutionContext<'_>, _unit: &DesiredUnit) -> DriverResult<Supported<Inspection>> {
        Ok(Supported::Unsupported)
    }

    fn teardown(&self, ctx: &ExecutionContext<'_>, unit: &DesiredUnit) -> DriverResult<Supported<Outcome>> {
        teardown(&self.resource_catalogue, ctx, unit).map(Supported::Supported)
    }

    /// The `teardownSettings` resolution recorded in the stored document;
    /// unsupported (no requirements) for a document resolved without them.
    fn teardown_settings(&self, unit: &DesiredUnit) -> DriverResult<Supported<TeardownSettings>> {
        let spec = unit.resolved_spec.as_ref().unwrap_or(&unit.spec);
        match spec.pointer(TEARDOWN_SETTINGS) {
            None => Ok(Supported::Unsupported),
            Some(settings) => serde_json::from_value(settings.clone())
                .map(Supported::Supported)
                .map_err(|error| DriverError::before_effects(format!("spec.teardownSettings: {error}"))),
        }
    }

    fn build(
        &self,
        _ctx: &ExecutionContext<'_>,
        _unit: &DesiredUnit,
        _options: &[OsString],
    ) -> DriverResult<Supported<Vec<ArtifactDocument>>> {
        Ok(Supported::Unsupported)
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
