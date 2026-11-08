//! Run checkpoints carry the attestations and observed values a run recorded
//! for the receipt they belong to, end to end through the orchestration shell
//! with Git state: a run that dies after a unit executed leaves them on its
//! run ref, and the run or `nyl recover` that imports the receipt imports
//! them with it, so dependents that require them proceed without a re-read.
//!
//! Contract: [Runs and leases](../../design/orchestration-core.md#runs-and-leases)
//! item 3 (checkpoints), [Attestations](../../design/orchestration-core.md#attestations),
//! and [Observed values](../../design/orchestration-core.md#observed-values).
//! The rules that decide what imports (key, run, and incarnation) are tested
//! where they live, in `nyl_orchestration::rules::takeover` and `::recovery`.

mod scenario;

use std::collections::{BTreeMap, VecDeque};
use std::ffi::OsString;
use std::sync::{Arc, Mutex};

use nyl_core::orchestration::codes::Code;
use nyl_core::orchestration::driver::{
    AttestationOutcome, Capabilities, DriftDetection, Driver, DriverError, DriverResult, ExecStatus, ExecutionContext,
    Inspection, KeyInputs, LocalPathField, ObservedValue, ObservedValues, Outcome, OwnershipScope, PlanReport,
    RecoveryPolicies, ResolutionContext, Supported,
};
use nyl_core::orchestration::ids::EnvironmentName;
use nyl_core::orchestration::ports::Checkpoint;
use nyl_core::orchestration::state::{ArtifactDocument, AttestationResult, DesiredUnit, Receipt, RecordedObservation};
use nyl_orchestration::fault::{FaultPoint, NoFaults};
use nyl_orchestration::report::StateOperationReport;
use nyl_orchestration::shell::recover::recover_environment;
use nyl_orchestration::shell::run::{self, Cancellation, RunContext};
use nyl_orchestration::shell::state as state_ops;
use nyl_units::command::CommandDriver;
use nyl_units::process::ProcessRunner;
use scenario::runner::FaultSlot;
use scenario::shell::{codes, dev, names, outcome, project, set, unit, Harness, ENVIRONMENT, NYL_TOML};
use serde_json::{json, Value};

/// The address `lb` reads.
const IP: &str = "203.0.113.10";

/// What `lb`'s driver does, as a test scripts it: each read of an observed
/// value, and how many executions and reads happened.
#[derive(Default)]
struct Probe {
    reads: Mutex<VecDeque<Option<&'static str>>>,
    executions: Mutex<usize>,
    read_count: Mutex<usize>,
}

impl Probe {
    /// The next read finds `address`, or fails for `None`.
    fn script(&self, address: Option<&'static str>) {
        self.reads.lock().unwrap().push_back(address);
    }

    fn executions(&self) -> usize {
        *self.executions.lock().unwrap()
    }

    fn reads(&self) -> usize {
        *self.read_count.lock().unwrap()
    }
}

/// A test driver that gives the Command kind what a generic engine test
/// needs of a producer: it reports the `healthy` attestation with every
/// successful execution, and reads the scripted observed values.
struct Producer {
    inner: CommandDriver,
    probe: Arc<Probe>,
}

impl Driver for Producer {
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
        if unit.unit.name.as_str() == "lb" {
            *self.probe.executions.lock().unwrap() += 1;
        }
        let mut outcome = self.inner.reconcile(ctx, unit)?;
        if let Supported::Supported(Outcome::Succeeded { attestations, .. }) = &mut outcome {
            attestations.push(AttestationOutcome {
                name: "healthy".to_owned(),
                result: AttestationResult::Pass,
                reason: None,
                details: Value::Null,
            });
        }
        Ok(outcome)
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
        _ctx: &ExecutionContext<'_>,
        _unit: &DesiredUnit,
        _receipt: &Receipt,
    ) -> DriverResult<Supported<ObservedValues>> {
        *self.probe.read_count.lock().unwrap() += 1;
        match self.probe.reads.lock().unwrap().pop_front() {
            Some(Some(address)) => Ok(Supported::Supported(ObservedValues {
                values: BTreeMap::from([("ip".to_owned(), ObservedValue::Value { value: json!(address) })]),
            })),
            Some(None) | None => Err(DriverError::before_effects("the system cannot be reached")),
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

/// `lb` attests `healthy` and observes `ip`; `dns` needs both: it depends on
/// `lb` being attested healthy and takes its address from the observed `ip`.
/// `max_age` is the declared `maxAge` of `ip`.
struct World {
    harness: Harness,
    probe: Arc<Probe>,
    _project: tempfile::TempDir,
}

impl World {
    fn new(max_age: Option<&str>) -> Self {
        let max_age = max_age.map(|age| format!(", maxAge: {age}")).unwrap_or_default();
        let units = format!(
            "apiVersion: units.gitops.nyl/v1
kind: Command
metadata:
  name: lb
  labels: {{dev: 'true'}}
spec:
  command: [sh, -c, 'true']
  attestations: [{{name: healthy}}]
  observations:
    ip: {{type: string{max_age}, from: {{service: web}}}}
---
apiVersion: units.gitops.nyl/v1
kind: Command
metadata:
  name: dns
  labels: {{dev: 'true'}}
spec:
  command: [sh, -c, 'true']
  dependsOn: [{{unit: lb, attestations: [healthy]}}]
  values:
    address: {{fromUnit: {{unit: lb, observation: ip}}}}
"
        );
        let project = project(&[
            NYL_TOML,
            ("nyl/environment.yaml", ENVIRONMENT),
            ("nyl/units.yaml", &units),
        ]);
        let mut harness = Harness::new(project.path(), true);
        let probe = Arc::new(Probe::default());
        harness.drivers.register(Arc::new(Producer {
            inner: CommandDriver,
            probe: probe.clone(),
        }));
        harness.init();
        Self {
            harness,
            probe,
            _project: project,
        }
    }

    /// A reconcile killed before the lease update that starts `dns`: `lb`
    /// executed and was read, and its dependent never started.
    fn die_before_dns(&mut self) -> nyl_core::orchestration::ids::RunId {
        self.harness.faults = Box::new(FaultSlot::kill_once(FaultPoint::BeforeLeaseUpdate, Some("dns")));
        let dead = self.harness.reconcile(&[]);
        self.harness.faults = Box::new(NoFaults);
        dead.run.expect("a killed run reports its id")
    }

    /// The lease and run ref of the dead run are over, one minute past the
    /// resolution lease of 30 minutes and its 10 minutes of grace.
    fn past_the_deadline(&self, minutes: u64) {
        self.harness.advance(minutes);
    }

    /// `lb`'s checkpoints on the dead run's ref.
    fn checkpoints(&self) -> Vec<Checkpoint> {
        let runs = self.harness.coordination().runs;
        assert_eq!(runs.len(), 1, "{runs:#?}");
        runs[0]
            .checkpoints
            .iter()
            .filter(|checkpoint| checkpoint.desired.unit.name == unit("lb"))
            .cloned()
            .collect()
    }
}

/// `nyl recover -e dev` through the shell, which imports a broken run and
/// executes nothing.
fn recover(harness: &Harness) -> StateOperationReport {
    let mut job = harness.job(&[], Cancellation::default());
    job.request.mode = run::RunMode::Recover { unit: None };
    job.request.reason = Some("runner gone".to_owned());
    let base = harness.context();
    let tools = ProcessRunner::new();
    let context = RunContext {
        instances: &nyl_core::orchestration::instance::NO_INSTANCES,
        base: &base,
        sources: &job.sources,
        renderer: &job.renderer,
        drivers: &harness.drivers,
        tools: &tools,
        workspace: job.workspace.path(),
        process_env: &harness.process_env,
        secrets: None,
        commit_identity: Some(&harness.identity),
        progress: None,
        prompt: None,
    };
    recover_environment(&context, &job.request)
}

fn recorded_ip(
    record: &nyl_core::orchestration::state::ObservationRecord,
) -> Option<(Value, chrono::DateTime<chrono::Utc>)> {
    match record.values.get("ip")? {
        RecordedObservation::Value { value, observed_at } => Some((value.clone(), *observed_at)),
        RecordedObservation::Pending { .. } => None,
    }
}

/// The run ref of a run that died after `lb` executed carries `lb`'s
/// attestation with its receipt, and the observed value once it was read,
/// each bound to that receipt. Catches evidence that lives only in the
/// run's memory.
#[test]
fn test_a_run_ref_carries_the_attestations_and_observed_values_of_the_receipt_it_checkpointed() {
    let mut world = World::new(None);
    world.probe.script(Some(IP));
    let dead = world.die_before_dns();
    assert_eq!(world.probe.executions(), 1);

    let checkpoints = world.checkpoints();
    assert_eq!(checkpoints.len(), 2, "the execution and the read of its values");
    let receipt = checkpoints[0].observed.receipt.clone().unwrap();
    assert_eq!(receipt.run, dead);
    assert_eq!(checkpoints[0].observations, None, "the values were not read yet");
    for checkpoint in &checkpoints {
        let healthy = &checkpoint.attestations["healthy"];
        assert_eq!(healthy.result, AttestationResult::Pass);
        assert_eq!(healthy.execution_key, receipt.execution_key.as_str());
        assert_eq!(
            healthy.unit.as_ref().unwrap().uid,
            checkpoint.desired.unit.uid.clone().unwrap()
        );
    }
    let record = checkpoints[1].observations.as_ref().expect("the read is checkpointed");
    assert_eq!(record.execution_key, receipt.execution_key);
    assert_eq!(record.run, dead);
    assert_eq!(record.unit.uid, checkpoints[1].desired.unit.uid);
    assert_eq!(recorded_ip(record).unwrap().0, json!(IP));
}

/// The next reconcile imports the receipt with its evidence and runs the
/// dependent on it, though the value cannot be read again: the evidence keeps
/// its own times, and nothing executes `lb` twice. Catches an import that
/// leaves attestations and values pending until `nyl attest` or a successful
/// read, and one that re-stamps what it imports.
#[test]
fn test_a_run_taking_over_imports_the_evidence_with_the_receipt_and_runs_the_dependent() {
    let mut world = World::new(None);
    world.probe.script(Some(IP));
    let dead = world.die_before_dns();
    let checkpointed = world.checkpoints().pop().unwrap();
    world.past_the_deadline(41);

    // Reading again fails: the dependent must not need it.
    world.probe.script(None);
    let taken = world.harness.reconcile(&[]);
    assert_eq!(taken.exit.code(), 0, "{taken:#?}");
    assert_eq!(names(&taken.imported), set(&["lb"]), "{taken:#?}");
    assert_eq!(outcome(&taken, "executed"), set(&["dns"]), "{taken:#?}");
    assert_eq!(world.probe.executions(), 1, "lb is not executed again");
    assert_eq!(
        codes(&taken.units[&unit("lb")].warnings),
        [Code::ObservationFailed],
        "the read was tried and failed: {taken:#?}"
    );

    let snapshot = world.harness.snapshot();
    let attestation = &snapshot.attestations[&unit("lb")]["healthy"];
    assert_eq!(attestation, &checkpointed.attestations["healthy"], "never re-stamped");
    assert_eq!(
        snapshot.observations[&unit("lb")],
        checkpointed.observations.unwrap(),
        "the value keeps the time it was read at"
    );
    assert_eq!(
        snapshot.observed[&unit("lb")].receipt.as_ref().unwrap().run,
        dead,
        "the receipt is the dead run's"
    );
    let dns = &snapshot.desired[&unit("dns")];
    assert_eq!(dns.resolved_spec.as_ref().unwrap()["values"]["address"], json!(IP));
}

/// `nyl recover` imports the receipt with its evidence, and reads nothing:
/// the value resolves and the dependent is ready, and the next reconcile
/// runs it. Catches the recover entry point diverging from the takeover.
#[test]
fn test_recover_imports_the_evidence_with_the_receipt_without_reading() {
    let mut world = World::new(None);
    world.probe.script(Some(IP));
    world.die_before_dns();
    let checkpointed = world.checkpoints().pop().unwrap();
    let reads = world.probe.reads();
    let broken = state_ops::break_lease(
        &world.harness.context(),
        &dev(),
        &world.harness.location,
        Some("runner lost".to_owned()),
    );
    assert_eq!(broken.exit, 0, "{broken:?}");

    let recovered = recover(&world.harness);
    assert_eq!(recovered.exit, 0, "{recovered:#?}");
    assert_eq!(world.probe.reads(), reads, "recover reads nothing");
    assert_eq!(world.probe.executions(), 1, "recover executes nothing");
    let snapshot = world.harness.snapshot();
    assert_eq!(
        snapshot.attestations[&unit("lb")]["healthy"],
        checkpointed.attestations["healthy"]
    );
    assert_eq!(snapshot.observations[&unit("lb")], checkpointed.observations.unwrap());

    // The read in the next reconcile fails, and the dependent still runs.
    world.probe.script(None);
    let next = world.harness.reconcile(&[]);
    assert_eq!(next.exit.code(), 0, "{next:#?}");
    assert_eq!(outcome(&next, "executed"), set(&["dns"]), "{next:#?}");
    assert_eq!(world.probe.executions(), 1);
}

/// An imported value is judged by its own age, never made younger by the
/// import: older than its `maxAge`, it is pending again and its dependent
/// blocks, until a successful read records it anew. Catches an import that
/// re-stamps `observedAt`, which would let a stale value satisfy `maxAge`.
#[test]
fn test_an_imported_value_keeps_its_age_for_max_age() {
    let mut world = World::new(Some("30m"));
    world.probe.script(Some(IP));
    world.die_before_dns();
    let checkpointed = world.checkpoints().pop().unwrap();
    let (_, read_at) = recorded_ip(checkpointed.observations.as_ref().unwrap()).unwrap();
    world.past_the_deadline(41);

    // 41 minutes after the read, the value is older than its maxAge of 30
    // minutes and reading it fails: it is no longer usable.
    world.probe.script(None);
    let stale = world.harness.reconcile(&[]);
    assert_eq!(names(&stale.imported), set(&["lb"]), "{stale:#?}");
    assert_eq!(outcome(&stale, "blocked"), set(&["dns"]), "{stale:#?}");
    let reason = &stale.units[&unit("dns")];
    assert!(
        format!("{reason:?}").contains("older than its maxAge"),
        "the imported value keeps the time it was read at: {reason:?}"
    );
    let snapshot = world.harness.snapshot();
    assert_eq!(
        recorded_ip(&snapshot.observations[&unit("lb")]).unwrap().1,
        read_at,
        "a failed read does not make the value younger"
    );

    // A read that succeeds records the value again, and the dependent runs.
    world.probe.script(Some(IP));
    let read = world.harness.reconcile(&[]);
    assert_eq!(outcome(&read, "executed"), set(&["dns"]), "{read:#?}");
    let snapshot = world.harness.snapshot();
    let (_, recorded) = recorded_ip(&snapshot.observations[&unit("lb")]).unwrap();
    assert!(recorded > read_at, "the new read has its own time");
}

/// A read that finds the imported value unchanged within the refresh
/// interval leaves the stored record as the checkpoint carried it: the
/// existing refresh rule applies to an imported record as to any other.
#[test]
fn test_an_unchanged_read_within_the_refresh_interval_keeps_the_imported_record() {
    let mut world = World::new(None);
    world.probe.script(Some(IP));
    world.die_before_dns();
    let checkpointed = world.checkpoints().pop().unwrap();
    world.past_the_deadline(41);

    world.probe.script(Some(IP));
    let taken = world.harness.reconcile(&[]);
    assert_eq!(names(&taken.imported), set(&["lb"]), "{taken:#?}");
    assert_eq!(outcome(&taken, "executed"), set(&["dns"]), "{taken:#?}");
    assert!(taken.units[&unit("lb")].warnings.is_empty(), "{taken:#?}");
    assert_eq!(world.probe.reads(), 2, "the dead run's read and this run's");
    assert_eq!(
        world.harness.snapshot().observations[&unit("lb")],
        checkpointed.observations.unwrap()
    );
}
