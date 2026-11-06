//! Observed values, end to end through the orchestration shell with Git
//! state: a driver reads a value of the system a unit manages, the shell
//! records it for the unit's current receipt, and consumers resolve it.
//!
//! Contract: [Observed values](../../design/orchestration-core.md#observed-values).

mod scenario;

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
use scenario::shell::{self, project, ENVIRONMENT, NYL_TOML};
use serde_json::{json, Value};

const OBSERVED: &str = "refs/heads/nyl/dev/observed";

fn unit<'a>(report: &'a Value, name: &str) -> &'a Value {
    &report["environments"][0]["units"][name]
}

/// What a unit's next read finds, as a test scripts it.
#[derive(Clone)]
enum Read {
    Values(Vec<(&'static str, ObservedValue)>),
    Error,
}

fn value(name: &'static str, value: Value) -> (&'static str, ObservedValue) {
    (name, ObservedValue::Value { value })
}

fn pending(name: &'static str, reason: &str) -> (&'static str, ObservedValue) {
    (
        name,
        ObservedValue::Pending {
            reason: reason.to_owned(),
        },
    )
}

fn failed(name: &'static str, reason: &str) -> (&'static str, ObservedValue) {
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
struct Probe {
    reads: Mutex<BTreeMap<String, VecDeque<Read>>>,
    observed_at: Mutex<Vec<(String, Timestamp)>>,
    executed_at: Mutex<Vec<(String, Timestamp)>>,
}

impl Probe {
    fn script(&self, unit: &str, reads: impl IntoIterator<Item = Read>) {
        self.reads
            .lock()
            .unwrap()
            .entry(unit.to_owned())
            .or_default()
            .extend(reads);
    }

    fn observations(&self, unit: &str) -> Vec<Timestamp> {
        self.observed_at
            .lock()
            .unwrap()
            .iter()
            .filter(|(name, _)| name == unit)
            .map(|(_, at)| *at)
            .collect()
    }

    fn executions(&self, unit: &str) -> Vec<Timestamp> {
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
struct Observing {
    inner: CommandDriver,
    probe: Arc<Probe>,
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

/// `lb` observes `ip` (a string) and declares `port` (an integer); `dns`
/// consumes `ip`; `later` only waits for `lb` to have a receipt; `solo`
/// depends on nothing. `extra_lb` and `extra_dns` add spec lines.
struct Harness {
    shell: shell::Harness,
    probe: Arc<Probe>,
    _project: tempfile::TempDir,
}

impl Harness {
    fn new(extra_lb: &str, extra_dns: &str) -> Self {
        let units = format!(
            "apiVersion: units.gitops.nyl/v1
kind: Command
metadata:
  name: lb
  labels: {{dev: 'true'}}
spec:
  command: [sh, -c, 'true']
  observations:
    ip: {{type: string, from: {{service: web}}}}
    port: {{type: integer, from: {{service: web}}}}
{extra_lb}---
apiVersion: units.gitops.nyl/v1
kind: Command
metadata:
  name: dns
  labels: {{dev: 'true'}}
spec:
  command: [sh, -c, 'true']
  values:
    address: {{fromUnit: {{unit: lb, observation: ip}}}}
{extra_dns}---
apiVersion: units.gitops.nyl/v1
kind: Command
metadata:
  name: later
  labels: {{dev: 'true'}}
spec:
  command: [sh, -c, 'true']
  dependsOn: [lb]
---
apiVersion: units.gitops.nyl/v1
kind: Command
metadata:
  name: solo
  labels: {{dev: 'true'}}
spec:
  command: [sh, -c, 'true']
"
        );
        let project = project(&[
            NYL_TOML,
            ("nyl/environment.yaml", ENVIRONMENT),
            ("nyl/units.yaml", &units),
        ]);
        let mut shell = shell::Harness::new(project.path(), true);
        let probe = Arc::new(Probe::default());
        shell.drivers.register(Arc::new(Observing {
            inner: CommandDriver,
            probe: probe.clone(),
        }));
        assert_eq!(shell.invoke(&["state", "init", "-e", "dev"]).exit, 0);
        Self {
            shell,
            probe,
            _project: project,
        }
    }

    fn reconcile(&self) -> nyl::orchestration::run::CommandOutcome {
        self.shell.invoke(&["reconcile", "-e", "dev", "-o", "json"])
    }

    fn get(&self) -> nyl::orchestration::run::CommandOutcome {
        self.shell.invoke(&["get", "observation", "lb/ip", "-e", "dev"])
    }

    fn lb_record(&self) -> String {
        self.shell.show(OBSERVED, "observed/units/lb/observations.yaml")
    }
}

/// The `observed-value` walkthrough, which `scenario_harness_test` runs once
/// the harness reaches M5, runs here already with the publication fake: its
/// steps are the contract of the rules below, in the scenario format.
#[test]
fn test_the_observed_value_walkthrough_passes_with_the_fake_publication() {
    let (loaded, problems) = scenario::load_all();
    assert!(problems.is_empty(), "invalid scenarios:\n{}", problems.join("\n"));
    let walkthrough = loaded
        .iter()
        .find(|loaded| loaded.name == "walkthroughs/observed-value")
        .expect("the walkthrough exists");
    if let Err(failure) = scenario::runner::run(walkthrough) {
        panic!("{failure}");
    }
}

/// The same walkthrough against the built-in `KubernetesPublication` driver:
/// its declaration is validated, its resolution renders the target and
/// checks the observed Service is one it renders, and only the live system
/// it reads is the script.
#[test]
fn test_the_observed_value_walkthrough_passes_with_the_real_publication_driver() {
    let (loaded, problems) = scenario::load_all();
    assert!(problems.is_empty(), "invalid scenarios:\n{}", problems.join("\n"));
    let walkthrough = loaded
        .iter()
        .find(|loaded| loaded.name == "walkthroughs/observed-value")
        .expect("the walkthrough exists");
    if let Err(failure) = scenario::runner::run_real_publication(walkthrough) {
        panic!("{failure}");
    }
}

fn blocked_by(report: &Value, name: &str) -> Vec<(String, String, String)> {
    unit(report, name)["blockedBy"]
        .as_array()
        .map(|entries| {
            entries
                .iter()
                .map(|entry| {
                    (
                        entry["pointer"].as_str().unwrap().to_owned(),
                        entry["reason"].as_str().unwrap().to_owned(),
                        format!(
                            "{}/{}",
                            entry["unit"].as_str().unwrap(),
                            entry["observation"].as_str().unwrap()
                        ),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

fn results(report: &Value) -> BTreeMap<String, String> {
    report["environments"][0]["units"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(name, unit)| (name.clone(), unit["result"].as_str().unwrap_or("-").to_owned()))
        .collect()
}

/// A kind without the `observed_values` capability rejects `observations`
/// with its own code, before its own validation reads them.
#[test]
fn test_a_kind_that_reads_no_observed_values_rejects_observations() {
    let units = "apiVersion: units.gitops.nyl/v1
kind: Command
metadata:
  name: lb
  labels: {dev: 'true'}
spec:
  command: [sh, -c, 'true']
  observations:
    ip: {type: string, from: {service: web}}
";
    let project = project(&[
        NYL_TOML,
        ("nyl/environment.yaml", ENVIRONMENT),
        ("nyl/units.yaml", units),
    ]);
    let shell = shell::Harness::new(project.path(), true);
    assert_eq!(shell.invoke(&["state", "init", "-e", "dev"]).exit, 0);
    let rejected = shell.invoke(&["reconcile", "-e", "dev"]);
    assert_eq!(rejected.exit, 1, "{}", rejected.stderr);
    assert!(
        rejected.stderr.contains("NYL-OBSERVATIONS-UNSUPPORTED"),
        "{}",
        rejected.stderr
    );
    assert!(rejected.stderr.contains("kind Command"), "{}", rejected.stderr);
}

/// A declaration the engine owns is checked before a unit is stored: an
/// observed value is never sensitive, and its name is not an output's.
#[test]
fn test_observation_declarations_are_validated_before_anything_runs() {
    for (declaration, expected) in [
        ("ip: {type: string, sensitive: true, from: {}}", "declared sensitive"),
        ("ip: {type: string}", "missing field `from`"),
        ("ip: {type: bogus, from: {}}", "unknown variant `bogus`"),
        ("ip: {type: string, maxAge: 0s, from: {}}", "maxAge must be positive"),
    ] {
        let units = format!(
            "apiVersion: units.gitops.nyl/v1
kind: Command
metadata:
  name: lb
  labels: {{dev: 'true'}}
spec:
  command: [sh, -c, 'true']
  observations:
    {declaration}
"
        );
        let project = project(&[
            NYL_TOML,
            ("nyl/environment.yaml", ENVIRONMENT),
            ("nyl/units.yaml", &units),
        ]);
        let mut shell = shell::Harness::new(project.path(), true);
        shell.drivers.register(Arc::new(Observing {
            inner: CommandDriver,
            probe: Arc::new(Probe::default()),
        }));
        assert_eq!(shell.invoke(&["state", "init", "-e", "dev"]).exit, 0);
        let rejected = shell.invoke(&["reconcile", "-e", "dev"]);
        assert_eq!(rejected.exit, 1, "{declaration}: {}", rejected.stderr);
        assert!(
            rejected.stderr.contains("NYL-UNIT-INVALID") && rejected.stderr.contains(expected),
            "{declaration}: {}",
            rejected.stderr
        );
    }
    let units = "apiVersion: units.gitops.nyl/v1
kind: Command
metadata:
  name: lb
  labels: {dev: 'true'}
spec:
  command: [sh, -c, 'true']
  outputs: {ip: {type: string}}
  observations:
    ip: {type: string, from: {}}
";
    let project = project(&[
        NYL_TOML,
        ("nyl/environment.yaml", ENVIRONMENT),
        ("nyl/units.yaml", units),
    ]);
    let mut shell = shell::Harness::new(project.path(), true);
    shell.drivers.register(Arc::new(Observing {
        inner: CommandDriver,
        probe: Arc::new(Probe::default()),
    }));
    assert_eq!(shell.invoke(&["state", "init", "-e", "dev"]).exit, 0);
    let rejected = shell.invoke(&["reconcile", "-e", "dev"]);
    assert!(
        rejected.stderr.contains("also a declared output"),
        "{}",
        rejected.stderr
    );
}

/// A value's whole path: pending after the producer executed (its consumer
/// blocks with the driver's words, exit 2), arriving (the consumer runs in
/// that same run, and the commit lists the change), read again unchanged (no
/// commit), and changing by itself (the consumer runs with the new value
/// while the producer's receipt stays byte for byte). Catches a pending value
/// not blocking, a consumer needing a second run to see a value, a re-read
/// rewriting state, and a changed value re-executing the producer.
#[test]
fn test_a_value_arrives_reaches_its_consumer_and_changes_without_the_producer_executing() {
    let harness = Harness::new("", "");
    let shell = &harness.shell;
    harness.probe.script(
        "lb",
        [Read::Values(vec![
            pending("ip", "no address yet"),
            pending("port", "no port"),
        ])],
    );
    let first = harness.reconcile();
    assert_eq!(first.exit, 2, "{}", first.stderr);
    assert_eq!(
        results(&first.report),
        BTreeMap::from([
            ("dns".to_owned(), "blocked".to_owned()),
            ("later".to_owned(), "executed".to_owned()),
            ("lb".to_owned(), "executed".to_owned()),
            ("solo".to_owned(), "executed".to_owned()),
        ])
    );
    assert_eq!(
        blocked_by(&first.report, "dns"),
        [(
            "/values/address".to_owned(),
            "observation-pending".to_owned(),
            "lb/ip".to_owned()
        )]
    );
    assert!(first.stderr.contains("NYL-OBSERVATION-PENDING"), "{}", first.stderr);
    assert!(first.stderr.contains("no address yet"), "{}", first.stderr);
    assert!(
        harness.lb_record().contains("pending: no address yet"),
        "{}",
        harness.lb_record()
    );
    let unresolved = harness.get();
    assert_eq!(unresolved.exit, 1);
    assert!(
        unresolved.stderr.contains("NYL-REFERENCE-UNRESOLVED"),
        "{}",
        unresolved.stderr
    );
    // A plan and a status show the same blocker, from the recorded reason.
    for command in ["plan", "status"] {
        let shown = shell.invoke(&[command, "-e", "dev", "-o", "json"]);
        assert_eq!(
            unit(&shown.report, "dns")["blockedBy"][0]["reason"],
            "observation-pending",
            "{command}"
        );
        assert!(
            shown.stderr.contains("NYL-OBSERVATION-PENDING"),
            "{command}: {}",
            shown.stderr
        );
    }

    // The value arrives: the consumer runs in the run that reads it.
    harness.probe.script(
        "lb",
        [Read::Values(vec![
            value("ip", json!("203.0.113.10")),
            pending("port", "no port"),
        ])],
    );
    let arrived = harness.reconcile();
    assert_eq!(arrived.exit, 0, "{}", arrived.stderr);
    assert_eq!(unit(&arrived.report, "dns")["result"], "executed");
    assert_eq!(unit(&arrived.report, "lb")["result"], "current");
    assert_eq!(
        arrived.report["environments"][0]["observationsChanged"],
        json!({"lb": ["ip"]})
    );
    let commit = shell.message(OBSERVED);
    assert!(commit.contains("observationsChanged: {\"lb\":[\"ip\"]}"), "{commit}");
    assert_eq!(harness.get().stdout, "203.0.113.10\n");
    let record = harness.lb_record();
    assert!(
        record.contains("value: \"203.0.113.10\"") && record.contains("pending: no port"),
        "{record}"
    );
    let receipt_of_lb = shell.show(OBSERVED, "observed/units/lb.yaml");

    // Reading the same value again writes nothing.
    harness.probe.script(
        "lb",
        [Read::Values(vec![
            value("ip", json!("203.0.113.10")),
            pending("port", "still no port"),
        ])],
    );
    let tip = shell.tip(OBSERVED);
    let again = harness.reconcile();
    assert_eq!(again.exit, 0, "{}", again.stderr);
    assert!(
        results(&again.report).values().all(|result| result == "current"),
        "{:?}",
        results(&again.report)
    );
    assert_eq!(shell.tip(OBSERVED), tip, "an unchanged read writes no commit");
    assert!(again.report["environments"][0].get("observationsChanged").is_none());

    // The address changes by itself: dns runs again, lb does not.
    harness.probe.script(
        "lb",
        [Read::Values(vec![
            value("ip", json!("203.0.113.20")),
            pending("port", "no port"),
        ])],
    );
    let executions = harness.probe.executions("lb").len();
    let changed = harness.reconcile();
    assert_eq!(unit(&changed.report, "dns")["result"], "executed");
    assert_eq!(unit(&changed.report, "lb")["result"], "current");
    assert_eq!(
        harness.probe.executions("lb").len(),
        executions,
        "the producer did not execute"
    );
    assert_eq!(shell.show(OBSERVED, "observed/units/lb.yaml"), receipt_of_lb);
    assert_eq!(harness.get().stdout, "203.0.113.20\n");
    // It is not drift: nothing was repaired.
    assert!(changed.report["environments"][0].get("driftRepaired").is_none());
    assert_eq!(unit(&changed.report, "dns")["executionReason"], json!("inputs-changed"));
}

/// A read that cannot be made keeps what was recorded for the receipt, warns
/// with its own code, and writes nothing, whether the driver reports the
/// failure or errors. Catches a failed read erasing the value (blocking every
/// consumer whenever the system is unreachable) or turning into a commit.
#[test]
fn test_a_failed_read_keeps_the_last_value_and_writes_nothing() {
    let harness = Harness::new("", "");
    harness.probe.script(
        "lb",
        [Read::Values(vec![
            value("ip", json!("203.0.113.10")),
            value("port", json!(443)),
        ])],
    );
    assert_eq!(harness.reconcile().exit, 0);
    let tip = harness.shell.tip(OBSERVED);
    for read in [
        Read::Values(vec![failed("ip", "no credentials"), failed("port", "no credentials")]),
        Read::Error,
    ] {
        harness.probe.script("lb", [read]);
        let kept = harness.reconcile();
        assert_eq!(kept.exit, 0, "{}", kept.stderr);
        assert!(results(&kept.report).values().all(|result| result == "current"));
        assert!(kept.stderr.contains("NYL-OBSERVATION-FAILED"), "{}", kept.stderr);
        assert_eq!(harness.shell.tip(OBSERVED), tip);
        assert_eq!(harness.get().stdout, "203.0.113.10\n");
    }
    // A value of the wrong type is a failed read too, never recorded.
    harness.probe.script(
        "lb",
        [Read::Values(vec![value("ip", json!(7)), value("port", json!(443))])],
    );
    let wrong = harness.reconcile();
    assert!(
        wrong.stderr.contains("NYL-OBSERVATION-FAILED") && wrong.stderr.contains("declared type"),
        "{}",
        wrong.stderr
    );
    assert_eq!(harness.get().stdout, "203.0.113.10\n");
}

/// A run that executed a unit waits for its pending values every poll
/// interval until they have one or `observationWait` passed since the
/// execution, and does not hold back units that do not depend on them.
/// Catches a wait that polls at the wrong rate, never ends, ends at the first
/// poll, or runs units only after it.
#[test]
fn test_observation_wait_polls_until_the_value_arrives_or_the_wait_passes() {
    let harness = Harness::new("  observationWait: 1m\n", "");
    let pending_reads =
        |count: usize| (0..count).map(|_| Read::Values(vec![pending("ip", "no address"), pending("port", "no port")]));
    // Two pending polls, then the value: dns runs in the same run.
    harness.probe.script("lb", pending_reads(2));
    harness.probe.script(
        "lb",
        [Read::Values(vec![
            value("ip", json!("203.0.113.10")),
            value("port", json!(443)),
        ])],
    );
    let start = harness.shell.clock.now();
    let run = harness.reconcile();
    assert_eq!(run.exit, 0, "{}", run.stderr);
    assert_eq!(unit(&run.report, "dns")["result"], "executed");
    let at = harness.probe.observations("lb");
    let seconds = |index: usize| (at[index] - start).num_seconds();
    assert_eq!(at.len(), 3, "{at:?}");
    assert_eq!([seconds(0), seconds(1), seconds(2)], [0, 15, 30]);
    // `later` depends on `lb`'s receipt only, and `solo` on nothing: neither
    // waited for the poll that read the value.
    for name in ["later", "solo"] {
        assert!(
            harness.probe.executions(name).iter().all(|at| *at == start),
            "{name} ran at {:?}",
            harness.probe.executions(name)
        );
    }

    // A value that never arrives ends the wait one minute after the
    // execution: five polls, then dns waits for a later run.
    harness.shell.world.commit_files(
        "main",
        "Change lb",
        &[(
            "nyl/units.yaml",
            &std::fs::read_to_string(harness._project.path().join("nyl/units.yaml"))
                .unwrap()
                .replacen("command: [sh, -c, 'true']", "command: [sh, -c, 'true; true']", 1),
        )],
    );
    let before = harness.probe.observations("lb").len();
    harness.probe.script("lb", pending_reads(10));
    let started = harness.shell.clock.now();
    let unresolved = harness.reconcile();
    assert_eq!(unresolved.exit, 2, "{}", unresolved.stderr);
    assert_eq!(unit(&unresolved.report, "dns")["result"], "blocked");
    let polls = harness.probe.observations("lb");
    let polls = &polls[before..];
    assert_eq!(
        polls.iter().map(|at| (*at - started).num_seconds()).collect::<Vec<_>>(),
        [0, 15, 30, 45, 60]
    );
    assert!(
        unresolved.stderr.contains("NYL-OBSERVATION-PENDING"),
        "{}",
        unresolved.stderr
    );
}

/// With the default wait a run reads once and moves on: units it did not
/// execute are read once and never waited for.
#[test]
fn test_without_a_wait_a_run_reads_once_per_receipt() {
    let harness = Harness::new("", "");
    harness.probe.script(
        "lb",
        [Read::Values(vec![
            pending("ip", "no address"),
            pending("port", "no port"),
        ])],
    );
    let start = harness.shell.clock.now();
    let first = harness.reconcile();
    assert_eq!(first.exit, 2);
    assert_eq!(harness.probe.observations("lb"), [start]);
    let second = harness.reconcile();
    assert_eq!(second.exit, 2);
    assert_eq!(harness.probe.observations("lb").len(), 2, "one read per run");
    assert_eq!(harness.shell.clock.now(), start, "the run never waited");
}

fn observe_json(shell: &shell::Harness, extra: &[&str]) -> nyl::orchestration::run::CommandOutcome {
    let mut args = vec!["observe", "-e", "dev", "-o", "json"];
    args.extend_from_slice(extra);
    shell.invoke(&args)
}

/// `nyl observe` reads and records without executing anything and names the
/// consumers whose inputs changed; `status` and `plan` then show each of them
/// as ready, and the next reconcile executes it. Catches a read that executes
/// units, a changed value invisible until a reconcile, a changed consumer
/// reported as current, and an observe that exits 0 with a pending value.
#[test]
fn test_observe_records_values_and_leaves_consumers_ready_for_the_next_reconcile() {
    let harness = Harness::new("", "");
    let shell = &harness.shell;
    // The address is not there yet when the first run reads it: dns waits.
    harness.probe.script(
        "lb",
        [Read::Values(vec![
            pending("ip", "no address yet"),
            pending("port", "no port"),
        ])],
    );
    assert_eq!(harness.reconcile().exit, 2);
    // `observe` finds it: dns can resolve now, which status and the report
    // say, and the next reconcile executes it.
    harness.probe.script(
        "lb",
        [Read::Values(vec![
            value("ip", json!("203.0.113.10")),
            value("port", json!(443)),
        ])],
    );
    let found = observe_json(shell, &[]);
    assert_eq!(found.exit, 0, "{}", found.stderr);
    assert!(
        found.stderr.contains("NYL-OBSERVATION-CHANGED") && found.stderr.contains("has a value now"),
        "{}",
        found.stderr
    );
    let waiting = shell.invoke(&["status", "-e", "dev", "-o", "json"]);
    assert_eq!(unit(&waiting.report, "dns")["state"], "ready", "{}", waiting.stderr);
    assert!(
        unit(&waiting.report, "dns")["message"]
            .as_str()
            .unwrap()
            .contains("observed value ip of lb arrived"),
        "{}",
        unit(&waiting.report, "dns")["message"]
    );
    assert!(unit(&waiting.report, "dns").get("blockedBy").is_none());
    assert_eq!(
        unit(&shell.invoke(&["plan", "-e", "dev", "-o", "json"]).report, "dns")["state"],
        "ready"
    );
    harness.probe.script(
        "lb",
        [Read::Values(vec![
            value("ip", json!("203.0.113.10")),
            value("port", json!(443)),
        ])],
    );
    let ran = harness.reconcile();
    assert_eq!(ran.exit, 0, "{}", ran.stderr);
    assert_eq!(unit(&ran.report, "dns")["result"], "executed");

    // A pending value is exit 2, and records the pending state.
    harness.probe.script(
        "lb",
        [Read::Values(vec![
            value("ip", json!("203.0.113.10")),
            pending("port", "no port"),
        ])],
    );
    let pending_port = observe_json(shell, &[]);
    assert_eq!(pending_port.exit, 2, "{}", pending_port.stderr);
    assert_eq!(unit(&pending_port.report, "lb")["check"], "observed");
    assert_eq!(
        unit(&pending_port.report, "lb")["observations"]["port"]["observation"],
        "pending"
    );
    assert_eq!(
        unit(&pending_port.report, "lb")["observations"]["ip"]["value"],
        "203.0.113.10"
    );
    assert!(
        pending_port.stderr.contains("NYL-OBSERVATION-PENDING"),
        "{}",
        pending_port.stderr
    );
    assert!(
        shell.message(OBSERVED).starts_with("nyl: observe dev"),
        "{}",
        shell.message(OBSERVED)
    );
    assert!(shell.message(OBSERVED).contains("Nyl-Operation: observe"));

    // A changed value: exit 0, one observed commit, no desired commit, and
    // the consumer named.
    harness.probe.script(
        "lb",
        [Read::Values(vec![
            value("ip", json!("203.0.113.30")),
            value("port", json!(443)),
        ])],
    );
    let desired = shell.tip("refs/heads/nyl/dev/desired");
    let observed = shell.tip(OBSERVED);
    let executions = harness.probe.executions("dns").len();
    let changed = observe_json(shell, &[]);
    assert_eq!(changed.exit, 0, "{}", changed.stderr);
    assert_eq!(
        changed.report["environments"][0]["observationsChanged"],
        json!({"lb": ["ip", "port"]})
    );
    assert!(changed.stderr.contains("NYL-OBSERVATION-CHANGED"), "{}", changed.stderr);
    assert!(changed.stderr.contains("nyl reconcile -e dev"), "{}", changed.stderr);
    assert_eq!(shell.tip("refs/heads/nyl/dev/desired"), desired);
    assert_ne!(shell.tip(OBSERVED), observed);
    assert_eq!(
        harness.probe.executions("dns").len(),
        executions,
        "observe executes nothing"
    );
    assert_eq!(harness.get().stdout, "203.0.113.30\n");

    // Status and plan show the consumer ready, with the reason.
    let status = shell.invoke(&["status", "-e", "dev", "-o", "json"]);
    assert_eq!(status.exit, 2, "{}", status.stderr);
    assert_eq!(unit(&status.report, "dns")["state"], "ready");
    assert_eq!(unit(&status.report, "lb")["state"], "current");
    assert!(
        unit(&status.report, "dns")["message"]
            .as_str()
            .unwrap()
            .contains("observed value ip of lb changed"),
        "{}",
        unit(&status.report, "dns")["message"]
    );
    assert!(status.stderr.contains("NYL-OBSERVATION-CHANGED"), "{}", status.stderr);
    let plan = shell.invoke(&["plan", "-e", "dev", "-o", "json"]);
    assert_eq!(unit(&plan.report, "dns")["state"], "ready", "{}", plan.stderr);
    assert_eq!(unit(&plan.report, "lb")["state"], "current");

    // The next reconcile executes it, and everything is current.
    harness.probe.script(
        "lb",
        [Read::Values(vec![
            value("ip", json!("203.0.113.30")),
            value("port", json!(443)),
        ])],
    );
    let run = harness.reconcile();
    assert_eq!(unit(&run.report, "dns")["result"], "executed");
    assert_eq!(shell.invoke(&["status", "-e", "dev"]).exit, 0);

    // `--unit` names units; a unit whose kind or spec reads nothing is unsupported.
    harness.probe.script(
        "lb",
        [Read::Values(vec![
            value("ip", json!("203.0.113.30")),
            value("port", json!(443)),
        ])],
    );
    let only = observe_json(shell, &["--unit", "lb"]);
    assert_eq!(only.exit, 0, "{}", only.stderr);
    assert_eq!(only.report["environments"][0]["units"].as_object().unwrap().len(), 1);
    let solo = observe_json(shell, &["--unit", "solo"]);
    assert_eq!(solo.exit, 2, "{}", solo.stderr);
    assert_eq!(unit(&solo.report, "solo")["check"], "unsupported");
    assert!(solo.stderr.contains("NYL-OBSERVE-UNSUPPORTED"), "{}", solo.stderr);
}

/// A value older than its `maxAge` stops resolving: consumers see it as
/// pending, and `nyl get observation` refuses it, until a read renews it.
/// Catches an aged value still satisfying a reference, which would let an
/// unreachable system keep a decision alive forever.
#[test]
fn test_a_value_older_than_its_max_age_stops_resolving() {
    let harness = Harness::new("", "");
    // Redeclare `ip` with a maxAge through the project: 10 minutes.
    let units = std::fs::read_to_string(harness._project.path().join("nyl/units.yaml"))
        .unwrap()
        .replacen("ip: {type: string, from", "ip: {type: string, maxAge: 10m, from", 1);
    harness
        .shell
        .world
        .commit_files("main", "Age ip", &[("nyl/units.yaml", &units)]);
    harness.probe.script(
        "lb",
        [Read::Values(vec![
            value("ip", json!("203.0.113.10")),
            value("port", json!(443)),
        ])],
    );
    assert_eq!(harness.reconcile().exit, 0);
    assert_eq!(harness.get().exit, 0);
    harness.shell.advance(11);
    let aged = harness.get();
    assert_eq!(aged.exit, 1);
    assert!(aged.stderr.contains("maxAge of 10m"), "{}", aged.stderr);
    // The system cannot be read: the value stays aged, and dns waits for it.
    harness.probe.script("lb", [Read::Error]);
    let blocked = harness.reconcile();
    assert_eq!(blocked.exit, 2, "{}", blocked.stderr);
    assert_eq!(unit(&blocked.report, "dns")["result"], "blocked");
    assert!(blocked.stderr.contains("NYL-OBSERVATION-PENDING"), "{}", blocked.stderr);
    assert!(blocked.stderr.contains("NYL-OBSERVATION-FAILED"), "{}", blocked.stderr);
    // A successful read renews it.
    harness.probe.script(
        "lb",
        [Read::Values(vec![
            value("ip", json!("203.0.113.10")),
            value("port", json!(443)),
        ])],
    );
    let renewed = harness.reconcile();
    assert_eq!(renewed.exit, 0, "{}", renewed.stderr);
    assert_eq!(harness.get().exit, 0);
}

/// A new receipt of the producer removes the values read for the one it
/// replaced, in the commit that records it, so a value read from what ran
/// before can never satisfy a reference. Catches stale values surviving an
/// execution.
#[test]
fn test_a_new_receipt_starts_with_its_values_pending() {
    let harness = Harness::new("", "");
    harness.probe.script(
        "lb",
        [Read::Values(vec![
            value("ip", json!("203.0.113.10")),
            value("port", json!(443)),
        ])],
    );
    assert_eq!(harness.reconcile().exit, 0);
    assert!(harness.lb_record().contains("203.0.113.10"));
    let units = std::fs::read_to_string(harness._project.path().join("nyl/units.yaml"))
        .unwrap()
        .replacen("command: [sh, -c, 'true']", "command: [sh, -c, 'true; true']", 1);
    harness
        .shell
        .world
        .commit_files("main", "Change lb", &[("nyl/units.yaml", &units)]);
    // The next read finds nothing yet: the old address is not the new receipt's.
    harness.probe.script(
        "lb",
        [Read::Values(vec![
            pending("ip", "balancer not ready"),
            pending("port", "balancer not ready"),
        ])],
    );
    let run = harness.reconcile();
    assert_eq!(unit(&run.report, "lb")["result"], "executed");
    assert_eq!(unit(&run.report, "dns")["result"], "blocked");
    let record = harness.lb_record();
    assert!(
        !record.contains("203.0.113.10") && record.contains("balancer not ready"),
        "{record}"
    );
    assert_eq!(harness.get().exit, 1);
}
