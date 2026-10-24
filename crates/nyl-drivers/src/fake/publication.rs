//! `FakePublication`, standing in for `KubernetesPublication` in M3: renders
//! the unit's DeploymentTarget with the resolved Release inputs as a pinned
//! input snapshot, publishes the tree with its ownership index to the
//! target's publication branch, and records a `PublishedTree` artifact named
//! `tree`.
//!
//! Contract: [Kubernetes publication unit](../../../../design/orchestration-core.md#kubernetes-publication-unit)
//! and [Artifacts](../../../../design/orchestration-core.md#artifacts);
//! [Fake kinds](../../../../design/reference-scenarios.md#fake-kinds).
//! Rendering and publishing go through `nyl_render::gitops::publication`, the
//! narrow API the real driver will use in M5.

use std::ffi::OsString;

use nyl_core::orchestration::driver::{
    Capabilities, Driver, DriverResult, ExecStatus, ExecutionContext, Inspection, KeyInputs, Outcome, OwnershipScope,
    PlanReport, RecoveryPolicies, ResolutionContext, Supported, Verification,
};
use nyl_core::orchestration::ids::EnvironmentName;
use nyl_core::orchestration::state::{ArtifactDocument, DesiredUnit, Receipt};
use serde_json::Value;

use super::Script;

pub(super) const KIND: &str = "FakePublication";

/// The fake publication kind.
#[derive(Debug, Clone, Default)]
pub struct FakePublication {
    script: Script,
}

impl FakePublication {
    pub fn new(script: Script) -> Self {
        Self { script }
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

    fn capabilities(&self, _spec: &Value) -> Capabilities {
        let _ = &self.script;
        todo!("publication")
    }

    fn recovery(&self, _spec: &Value) -> RecoveryPolicies {
        todo!("publication")
    }

    fn spec_schema(&self) -> schemars::Schema {
        todo!("publication")
    }

    fn validate_spec(&self, _spec: &Value) -> Result<(), String> {
        todo!("publication")
    }

    fn attestations(&self, _spec: &Value) -> Vec<String> {
        Vec::new()
    }

    fn ownership_scopes(&self, _environment: &EnvironmentName, _spec: &Value) -> Vec<OwnershipScope> {
        todo!("publication")
    }

    fn execution_key_inputs(&self, _ctx: &ResolutionContext<'_>, _spec: &Value) -> DriverResult<KeyInputs> {
        todo!("publication")
    }

    fn plan(&self, _ctx: &ExecutionContext<'_>, _unit: &DesiredUnit) -> DriverResult<Supported<PlanReport>> {
        todo!("publication")
    }

    fn reconcile(&self, _ctx: &ExecutionContext<'_>, _unit: &DesiredUnit) -> DriverResult<Supported<Outcome>> {
        todo!("publication")
    }

    fn verify(
        &self,
        _ctx: &ExecutionContext<'_>,
        _unit: &DesiredUnit,
        _receipt: &Receipt,
    ) -> DriverResult<Supported<Verification>> {
        todo!("publication")
    }

    fn inspect(&self, _ctx: &ExecutionContext<'_>, _unit: &DesiredUnit) -> DriverResult<Supported<Inspection>> {
        Ok(Supported::Unsupported)
    }

    fn teardown(&self, _ctx: &ExecutionContext<'_>, _unit: &DesiredUnit) -> DriverResult<Supported<Outcome>> {
        Ok(Supported::Unsupported)
    }

    fn build(
        &self,
        _ctx: &ExecutionContext<'_>,
        _unit: &DesiredUnit,
        _push: bool,
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
