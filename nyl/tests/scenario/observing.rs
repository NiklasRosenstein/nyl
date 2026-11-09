//! A test driver that gives the Command kind the `observed_values`
//! capability, with reads a test scripts per unit, so the generic engine
//! rules for observed values are tested without any kind-specific driver.
//! Several orchestration tests register it next to the built-in kinds.

use std::collections::{BTreeMap, VecDeque};
use std::ffi::OsString;
use std::sync::{Arc, Mutex};

use nyl_core::orchestration::driver::{
    Capabilities, DriftDetection, Driver, DriverResult, ExecStatus, ExecutionContext, Inspection, KeyInputs,
    LocalPathField, ObservedValue, ObservedValues, Outcome, OwnershipScope, PlanReport, RecoveryPolicies,
    ResolutionContext, Supported,
};
use nyl_core::orchestration::ids::EnvironmentName;
use nyl_core::orchestration::ports::Clock;
use nyl_core::orchestration::state::{ArtifactDocument, DesiredUnit, Receipt};
use nyl_core::orchestration::time::Timestamp;
use nyl_units::command::CommandDriver;
use serde_json::Value;

/// What a unit's next read finds, as a test scripts it.
#[derive(Clone)]
pub enum Read {
    Values(Vec<(&'static str, ObservedValue)>),
    Error,
}

pub fn value(name: &'static str, value: Value) -> (&'static str, ObservedValue) {
    (name, ObservedValue::Value { value })
}

pub fn pending(name: &'static str, reason: &str) -> (&'static str, ObservedValue) {
    (
        name,
        ObservedValue::Pending {
            reason: reason.to_owned(),
        },
    )
}

pub fn failed(name: &'static str, reason: &str) -> (&'static str, ObservedValue) {
    (
        name,
        ObservedValue::Failed {
            reason: reason.to_owned(),
        },
    )
}

/// What every `Observing` driver shares: the scripted reads per unit and
/// the time of each read and each execution.
#[derive(Default)]
pub struct Probe {
    /// The clock a read advances by `read_takes`, when a test gives one.
    pub clock: Option<Arc<nyl_orchestration::testing::FakeClock>>,
    pub read_takes: Mutex<std::time::Duration>,
    pub reads: Mutex<BTreeMap<String, VecDeque<Read>>>,
    pub observed_at: Mutex<Vec<(String, Timestamp)>>,
    pub executed_at: Mutex<Vec<(String, Timestamp)>>,
}

impl Probe {
    pub fn script(&self, unit: &str, reads: impl IntoIterator<Item = Read>) {
        self.reads
            .lock()
            .unwrap()
            .entry(unit.to_owned())
            .or_default()
            .extend(reads);
    }

    pub fn observations(&self, unit: &str) -> Vec<Timestamp> {
        self.observed_at
            .lock()
            .unwrap()
            .iter()
            .filter(|(name, _)| name == unit)
            .map(|(_, at)| *at)
            .collect()
    }

    pub fn executions(&self, unit: &str) -> Vec<Timestamp> {
        self.executed_at
            .lock()
            .unwrap()
            .iter()
            .filter(|(name, _)| name == unit)
            .map(|(_, at)| *at)
            .collect()
    }
}

/// A test driver that gives the Command kind the `observed_values`
/// capability: its reads are the scripted ones, as the generic engine rules
/// are tested without any kind-specific driver.
pub struct Observing {
    pub inner: CommandDriver,
    pub probe: Arc<Probe>,
}

impl Driver for Observing {
    fn api_version(&self) -> &'static str {
        self.inner.api_version()
    }
    fn kind(&self) -> &'static str {
        self.inner.kind()
    }
    fn behavior_version(&self) -> u32 {
        self.inner.behavior_version()
    }
    fn capabilities(&self, spec: &Value) -> Capabilities {
        Capabilities {
            observed_values: true,
            ..self.inner.capabilities(spec)
        }
    }
    fn recovery(&self, spec: &Value) -> RecoveryPolicies {
        self.inner.recovery(spec)
    }
    fn spec_schema(&self) -> schemars::Schema {
        self.inner.spec_schema()
    }
    fn validate_spec(&self, spec: &Value) -> Result<(), String> {
        self.inner.validate_spec(spec)
    }
    fn attestations(&self, spec: &Value) -> Vec<String> {
        self.inner.attestations(spec)
    }
    fn ownership_scopes(&self, environment: &EnvironmentName, spec: &Value) -> Vec<OwnershipScope> {
        self.inner.ownership_scopes(environment, spec)
    }
    fn source_fields(&self, spec: &Value) -> Vec<String> {
        self.inner.source_fields(spec)
    }
    fn local_path_fields(&self, spec: &Value) -> Vec<LocalPathField> {
        self.inner.local_path_fields(spec)
    }
    fn execution_key_inputs(&self, ctx: &ResolutionContext<'_>, spec: &Value) -> DriverResult<KeyInputs> {
        self.inner.execution_key_inputs(ctx, spec)
    }
    fn plan(&self, ctx: &ExecutionContext<'_>, unit: &DesiredUnit) -> DriverResult<Supported<PlanReport>> {
        self.inner.plan(ctx, unit)
    }
    fn reconcile(&self, ctx: &ExecutionContext<'_>, unit: &DesiredUnit) -> DriverResult<Supported<Outcome>> {
        self.probe
            .executed_at
            .lock()
            .unwrap()
            .push((unit.unit.name.to_string(), ctx.now));
        self.inner.reconcile(ctx, unit)
    }
    fn detect_drift(
        &self,
        ctx: &ExecutionContext<'_>,
        unit: &DesiredUnit,
        receipt: &Receipt,
    ) -> DriverResult<Supported<DriftDetection>> {
        self.inner.detect_drift(ctx, unit, receipt)
    }
    fn observe(
        &self,
        ctx: &ExecutionContext<'_>,
        unit: &DesiredUnit,
        _receipt: &Receipt,
    ) -> DriverResult<Supported<ObservedValues>> {
        let name = unit.unit.name.to_string();
        self.probe.observed_at.lock().unwrap().push((name.clone(), ctx.now));
        if let Some(clock) = &self.probe.clock {
            let takes = *self.probe.read_takes.lock().unwrap();
            clock.set(clock.now() + chrono::Duration::from_std(takes).unwrap());
        }
        let read = self
            .probe
            .reads
            .lock()
            .unwrap()
            .get_mut(&name)
            .and_then(VecDeque::pop_front);
        match read {
            Some(Read::Values(values)) => Ok(Supported::Supported(ObservedValues {
                values: values
                    .into_iter()
                    .map(|(name, value)| (name.to_owned(), value))
                    .collect(),
            })),
            Some(Read::Error) => Err(nyl_core::orchestration::driver::DriverError::before_effects(
                "the system cannot be reached",
            )),
            None => Ok(Supported::Supported(ObservedValues::default())),
        }
    }
    fn inspect(&self, ctx: &ExecutionContext<'_>, unit: &DesiredUnit) -> DriverResult<Supported<Inspection>> {
        self.inner.inspect(ctx, unit)
    }
    fn teardown(&self, ctx: &ExecutionContext<'_>, unit: &DesiredUnit) -> DriverResult<Supported<Outcome>> {
        self.inner.teardown(ctx, unit)
    }
    fn build(
        &self,
        ctx: &ExecutionContext<'_>,
        unit: &DesiredUnit,
        options: &[OsString],
    ) -> DriverResult<Supported<Vec<ArtifactDocument>>> {
        self.inner.build(ctx, unit, options)
    }
    fn exec(
        &self,
        ctx: &ExecutionContext<'_>,
        unit: &DesiredUnit,
        args: &[OsString],
    ) -> DriverResult<Supported<ExecStatus>> {
        self.inner.exec(ctx, unit, args)
    }
}
