//! `FakePublication`, the test kind that stands in for `KubernetesPublication`
//! in tier-1 scenarios and tests that script a publication's outcome.
//!
//! The real driver ([`crate::kubernetes_publication`]) is the built-in kind.
//! This fake delegates every publish and teardown to the same operations, so
//! it publishes the same bytes, and adds what a scenario needs from a test
//! kind: scripted outcomes ([`Script`]), `observe: {}` (answered by the
//! fake reconciler observer, [`super::FakeObserver`], and by scripted observed
//! values), and `teardownSettings`
//! that a scenario declares.
//!
//! Contract: [Kubernetes publication unit](../../../../design/orchestration-core.md#kubernetes-publication-unit)
//! and [Artifacts](../../../../design/orchestration-core.md#artifacts);
//! [Fake kinds](../../../../design/reference-scenarios.md#fake-kinds).
//!
//! After resolution the spec is `{target, observe?: {},
//! teardownWait?, teardownSettings?, releases: {"<group>/<release>":
//! {<input>: <value>}}}`. The execution key's inputs are the render's
//! portable input record, so a change to any file the target reads, or to a
//! resolved Release input, re-executes the unit.
//!
//! Unlike the real driver, which records the target's effective teardown
//! settings at resolution, the fake reads the same shape from a
//! `teardownSettings` that a scenario declares (`catalogPrunes`,
//! `applicationDeletionPolicy`, `namespaceDeletePolicies`,
//! `catalogOwnedByParent`) and evaluates it with the real driver's readiness
//! rules. A document without it records no settings, so the fake reports
//! readiness as unsupported (no requirements).

use std::ffi::OsString;

use nyl_core::orchestration::driver::{
    Capabilities, DriftDetection, Driver, DriverError, DriverOperation, DriverResult, ExecStatus, ExecutionContext,
    Inspection, KeyInputs, ObservedValues, Outcome, OwnershipScope, PlanReport, RecoveryPolicies, RecoveryPolicy,
    RemovalContext, RemovalObservation, ResolutionContext, Supported, TeardownReadiness, WaitStrategy,
};
use nyl_core::orchestration::ids::EnvironmentName;
use nyl_core::orchestration::state::{ArtifactDocument, DesiredUnit, Drift, Receipt};
use serde_json::{json, Value};

use super::{Script, ScriptedOutcome};
use nyl_render::gitops::publication::TeardownSettings;
use nyl_render::gitops::units::PublicationTarget;

use crate::kubernetes_publication::{
    build, evaluate_teardown_readiness, plan, publish, resolution_inputs, teardown, teardown_wait, unit_target,
    validate_observe,
};

pub(super) const KIND: &str = "FakePublication";

/// The fake publication kind.
#[derive(Debug, Clone)]
pub struct FakePublication {
    script: Script,
    resource_catalogue: nyl_render::config::ResourceCatalogue,
}

impl FakePublication {
    /// A fake publication rendering against the caller's resource catalogue.
    pub fn new(script: Script, resource_catalogue: nyl_render::config::ResourceCatalogue) -> Self {
        Self {
            script,
            resource_catalogue,
        }
    }

    /// The recorded `teardownSettings`, with absent fields at their
    /// defaults: an absent catalog setting is external (`None`).
    fn recorded_settings(value: &Value) -> Result<TeardownSettings, String> {
        let mut settings = serde_json::to_value(TeardownSettings::default()).map_err(|error| error.to_string())?;
        let (Value::Object(defaults), Value::Object(recorded)) = (&mut settings, value) else {
            return Err("spec.teardownSettings must be a mapping".to_owned());
        };
        if let Some(key) = recorded.keys().find(|key| !defaults.contains_key(*key)) {
            return Err(format!("spec.teardownSettings has unknown field {key:?}"));
        }
        defaults.extend(recorded.clone());
        serde_json::from_value(settings).map_err(|error| format!("spec.teardownSettings: {error}"))
    }
}

impl Driver for FakePublication {
    fn api_version(&self) -> &'static str {
        super::API_VERSION_TEST_UNITS
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
            observed_values: spec.get("observe").is_some(),
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
                "target": {"type": ["string", "object"]},
                "observe": {"type": "object", "additionalProperties": false},
                "teardownWait": {
                    "type": "object",
                    "required": ["strategy"],
                    "properties": {
                        "strategy": {"type": "string", "enum": ["observe", "delay", "manual"]},
                        "duration": {"type": "string"}
                    },
                    "additionalProperties": false
                },
                "teardownSettings": {
                    "type": "object",
                    "properties": {
                        "catalogPrunes": {"type": ["boolean", "null"]},
                        "applicationDeletionPolicy": {"type": ["string", "null"]},
                        "namespaceDeletePolicies": {
                            "type": "object",
                            "additionalProperties": {"type": "string", "enum": ["Automatic", "Retain", "Confirm"]}
                        },
                        "catalogOwnedByParent": {"type": "boolean"}
                    },
                    "additionalProperties": false
                },
                "releases": {
                    "type": "object",
                    "additionalProperties": {"type": "object"}
                }
            }
        })
    }

    fn validate_spec(&self, spec: &Value) -> Result<(), String> {
        if !spec.is_object() {
            return Err("spec must be a mapping".to_owned());
        }
        match PublicationTarget::from_spec(spec) {
            Ok(PublicationTarget::Static(_)) => {}
            Ok(PublicationTarget::Inline(inline)) => inline.validate().map_err(|error| error.to_string())?,
            Err(message) => return Err(message),
        }
        validate_observe(spec.as_object().expect("a mapping"))?;
        if let Some(settings) = spec.get("teardownSettings") {
            Self::recorded_settings(settings)?;
        }
        if let Some(releases) = spec.get("releases") {
            let releases = releases
                .as_object()
                .ok_or_else(|| "spec.releases must be a mapping of <group>/<release> keys".to_owned())?;
            for (key, inputs) in releases {
                if !inputs.is_object() {
                    return Err(format!("spec.releases.{key} must be a mapping of input names"));
                }
            }
        }
        Ok(())
    }

    fn attestations(&self, _spec: &Value) -> Vec<String> {
        Vec::new()
    }

    /// None: the publication destination (repository, revision, and path
    /// prefix) belongs to the DeploymentTarget, which the spec names but
    /// does not contain, and claims are computed from the spec alone. Two
    /// targets sharing one destination are already refused by the rendered
    /// GitOps rules, and the ownership index refuses a tree owned by another
    /// target at publication.
    fn ownership_scopes(&self, _environment: &EnvironmentName, _spec: &Value) -> Vec<OwnershipScope> {
        Vec::new()
    }

    fn execution_key_inputs(&self, ctx: &ResolutionContext<'_>, spec: &Value) -> DriverResult<KeyInputs> {
        unit_target(spec)?;
        resolution_inputs(&self.resource_catalogue, ctx, spec).map(|(_, inputs)| inputs)
    }

    fn plan(&self, ctx: &ExecutionContext<'_>, unit: &DesiredUnit) -> DriverResult<Supported<PlanReport>> {
        Ok(Supported::Supported(plan(ctx, unit)))
    }

    fn reconcile(&self, ctx: &ExecutionContext<'_>, unit: &DesiredUnit) -> DriverResult<Supported<Outcome>> {
        let outputs = match self.script.outcome(&unit.unit.name, DriverOperation::Reconcile) {
            ScriptedOutcome::Return(outcome) => return Ok(Supported::Supported(outcome)),
            ScriptedOutcome::Run { outputs } => outputs,
        };
        publish(
            &self.resource_catalogue,
            ctx,
            unit,
            outputs.unwrap_or_else(|| json!({})),
        )
        .map(Supported::Supported)
    }

    fn detect_drift(
        &self,
        _ctx: &ExecutionContext<'_>,
        _unit: &DesiredUnit,
        _receipt: &Receipt,
    ) -> DriverResult<Supported<DriftDetection>> {
        Ok(Supported::Supported(Drift::Clean.into()))
    }

    fn inspect(&self, _ctx: &ExecutionContext<'_>, _unit: &DesiredUnit) -> DriverResult<Supported<Inspection>> {
        Ok(Supported::Unsupported)
    }

    /// What the scenario scripted the fake's observed values to be
    /// ([`Script::observed`]); pending when nothing is.
    fn observe(
        &self,
        _ctx: &ExecutionContext<'_>,
        unit: &DesiredUnit,
        _receipt: &Receipt,
    ) -> DriverResult<Supported<ObservedValues>> {
        let spec = unit.resolved_spec.as_ref().unwrap_or(&unit.spec);
        let names: Vec<String> = spec
            .get("observations")
            .and_then(Value::as_object)
            .map(|observations| observations.keys().cloned().collect())
            .unwrap_or_default();
        self.script.observed(&unit.unit.name, &names).map(Supported::Supported)
    }

    fn teardown(&self, ctx: &ExecutionContext<'_>, unit: &DesiredUnit) -> DriverResult<Supported<Outcome>> {
        if let ScriptedOutcome::Return(outcome) = self.script.outcome(&unit.unit.name, DriverOperation::Teardown) {
            return Ok(Supported::Supported(outcome));
        }
        teardown(&self.resource_catalogue, ctx, unit).map(Supported::Supported)
    }

    /// The readiness of the `teardownSettings` the stored document records;
    /// unsupported (no requirements) when it records none.
    fn teardown_readiness(&self, unit: &DesiredUnit) -> DriverResult<Supported<TeardownReadiness>> {
        let spec = unit.resolved_spec.as_ref().unwrap_or(&unit.spec);
        match spec.get("teardownSettings") {
            None => Ok(Supported::Unsupported),
            Some(settings) => Self::recorded_settings(settings)
                .map(|settings| {
                    Supported::Supported(evaluate_teardown_readiness(
                        &unit.unit.environment,
                        &unit.unit.name,
                        &settings,
                    ))
                })
                .map_err(DriverError::before_effects),
        }
    }

    fn teardown_wait(&self, spec: &Value) -> WaitStrategy {
        teardown_wait(spec)
    }

    /// What the scenario scripted the fake Argo CD to report for the unit's
    /// target ([`super::FakeObserver`]); nothing remains by default.
    fn observe_removal(
        &self,
        _ctx: &RemovalContext,
        unit: &DesiredUnit,
    ) -> DriverResult<Supported<RemovalObservation>> {
        let spec = unit.resolved_spec.as_ref().unwrap_or(&unit.spec);
        let target = unit_target(spec)?;
        Ok(Supported::Supported(
            self.script.observer().workload_applications(&target),
        ))
    }

    /// A direct build is not scripted: it is the real driver's, so a scenario
    /// that builds a publication sees what the built-in kind does.
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_validate_spec_checks_field_types() {
        let driver = FakePublication::new(Script::new(), crate::fake::test_catalogue());
        assert!(driver
            .validate_spec(&json!({"target": "dev", "observe": {}, "releases": {"web/web": {"image": "x"}}}))
            .is_ok());
        assert!(driver.validate_spec(&json!({"observe": {}})).is_err());
        assert!(driver.validate_spec(&json!({"target": 1})).is_err());
        assert!(driver
            .validate_spec(&json!({"target": "dev", "observe": {"interval": "1m"}}))
            .is_err());
        assert!(driver
            .validate_spec(&json!({"target": "dev", "teardownWait": {"strategy": "observe"}}))
            .is_err());
        assert!(driver.validate_spec(&json!({"target": "dev", "releases": []})).is_err());
        assert!(driver
            .validate_spec(&json!({"target": "dev", "releases": {"web/web": "x"}}))
            .is_err());
    }
}
