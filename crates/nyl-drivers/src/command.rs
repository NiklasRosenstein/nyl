//! The `Command` unit kind: runs argv steps, without a shell, in a pinned
//! worktree of the source commit.
//!
//! Contract: [Command unit](../../../design/orchestration-core.md#command-unit)
//! and [Drivers](../../../design/orchestration-core.md#drivers). Every
//! process runs through the context's [`ToolRunner`](nyl_core::orchestration::driver::ToolRunner)
//! with an environment built here from nothing: `PATH` from the runner,
//! `HOME` as a fresh directory, the `NYL_*` variables, and the admitted
//! passthrough and secrets. The spec type is
//! [`CommandSpec`](nyl_core::resources::units::CommandSpec).

use std::ffi::OsString;

use nyl_core::orchestration::driver::{
    Capabilities, Driver, DriverResult, ExecStatus, ExecutionContext, Inspection, KeyInputs, Outcome, OwnershipScope,
    PlanReport, RecoveryPolicies, ResolutionContext, Supported, Verification,
};
use nyl_core::orchestration::ids::EnvironmentName;
use nyl_core::orchestration::state::{ArtifactDocument, DesiredUnit, Receipt};
use nyl_core::resources::units::KIND_COMMAND;
use serde_json::Value;

/// Largest outputs file a step may write: 1 MiB.
pub const MAX_OUTPUTS_BYTES: u64 = 1024 * 1024;

/// The built-in `Command` kind.
#[derive(Debug, Clone, Copy, Default)]
pub struct CommandDriver;

impl Driver for CommandDriver {
    fn api_version(&self) -> &'static str {
        nyl_core::constants::API_VERSION_UNITS
    }

    fn kind(&self) -> &'static str {
        KIND_COMMAND
    }

    fn behavior_version(&self) -> u32 {
        1
    }

    fn capabilities(&self, _spec: &Value) -> Capabilities {
        todo!("command-driver")
    }

    fn recovery(&self, _spec: &Value) -> RecoveryPolicies {
        todo!("command-driver")
    }

    fn spec_schema(&self) -> schemars::Schema {
        todo!("command-driver")
    }

    fn validate_spec(&self, _spec: &Value) -> Result<(), String> {
        todo!("command-driver")
    }

    fn attestations(&self, _spec: &Value) -> Vec<String> {
        Vec::new()
    }

    fn ownership_scopes(&self, _environment: &EnvironmentName, _spec: &Value) -> Vec<OwnershipScope> {
        todo!("command-driver")
    }

    fn execution_key_inputs(&self, _ctx: &ResolutionContext<'_>, _spec: &Value) -> DriverResult<KeyInputs> {
        todo!("command-driver")
    }

    fn plan(&self, _ctx: &ExecutionContext<'_>, _unit: &DesiredUnit) -> DriverResult<Supported<PlanReport>> {
        todo!("command-driver")
    }

    fn reconcile(&self, _ctx: &ExecutionContext<'_>, _unit: &DesiredUnit) -> DriverResult<Supported<Outcome>> {
        todo!("command-driver")
    }

    fn verify(
        &self,
        _ctx: &ExecutionContext<'_>,
        _unit: &DesiredUnit,
        _receipt: &Receipt,
    ) -> DriverResult<Supported<Verification>> {
        todo!("command-driver")
    }

    fn inspect(&self, _ctx: &ExecutionContext<'_>, _unit: &DesiredUnit) -> DriverResult<Supported<Inspection>> {
        Ok(Supported::Unsupported)
    }

    /// Teardown is part 5; a Command supports it only with a `teardown` step.
    fn teardown(&self, _ctx: &ExecutionContext<'_>, _unit: &DesiredUnit) -> DriverResult<Supported<Outcome>> {
        todo!("command-driver")
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
