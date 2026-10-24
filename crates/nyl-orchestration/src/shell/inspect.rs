//! Reading state without the lease: `status -e` and `get -e`.
//!
//! Contract: [Inspection](../../../../design/orchestration-core.md#inspection).
//! Both read one snapshot of the state refs (and, for status, the lease and
//! run refs) and never write. `get output` and `get artifact` resolve
//! exactly like `fromUnit`, through `rules::references::resolve_reference`.

use nyl_core::bindings::UnitInputSource;
use nyl_core::orchestration::codes::Finding;
use nyl_core::orchestration::ids::{EnvironmentName, UnitName};
use nyl_core::orchestration::ports::{EnvironmentSnapshot, StateStore, StateTips};
use nyl_core::orchestration::state::{DesiredUnit, ObservedUnit, StateLocation};
use serde_json::Value;

use super::Context;
use crate::run_report::RunReport;

/// `nyl status -e <env>`.
pub fn status(context: &Context<'_>, environment: &EnvironmentName, location: &StateLocation) -> RunReport {
    let _ = (context, environment, location);
    todo!("cli-integration")
}

/// One snapshot, at the refs' tips or at `--state-revision`.
pub fn snapshot(
    store: &dyn StateStore,
    location: &StateLocation,
    at: Option<&StateTips>,
) -> Result<EnvironmentSnapshot, Finding> {
    let _ = (store, location, at);
    todo!("cli-integration")
}

/// The persisted documents of one unit, exactly as stored.
#[derive(Debug, Clone, PartialEq)]
pub struct UnitDocuments {
    pub desired: Option<DesiredUnit>,
    pub observed: Option<ObservedUnit>,
}

/// `nyl get unit <u> -e <env>`; unknown units are
/// [`Code::UnitUnknown`](nyl_core::orchestration::codes::Code::UnitUnknown).
pub fn unit_documents(snapshot: &EnvironmentSnapshot, unit: &UnitName) -> Result<UnitDocuments, Finding> {
    let _ = (snapshot, unit);
    todo!("cli-integration")
}

/// A value form's answer.
#[derive(Debug, Clone, PartialEq)]
pub enum GetValue {
    Value(Value),
    /// `--allow-stale`: the latest value, with a
    /// [`Code::ReferenceStale`](nyl_core::orchestration::codes::Code::ReferenceStale) warning.
    Stale {
        value: Value,
        warning: Finding,
    },
}

/// `nyl get output u/o` and `nyl get artifact u/n`: resolve `source` like a
/// `fromUnit` reference against `snapshot`. Unresolvable is
/// [`Code::ReferenceUnresolved`](nyl_core::orchestration::codes::Code::ReferenceUnresolved),
/// unless `allow_stale` finds an older value.
pub fn get_value(
    environment: &EnvironmentName,
    snapshot: &EnvironmentSnapshot,
    source: &UnitInputSource,
    allow_stale: bool,
) -> Result<GetValue, Finding> {
    let _ = (environment, snapshot, source, allow_stale);
    todo!("cli-integration")
}
