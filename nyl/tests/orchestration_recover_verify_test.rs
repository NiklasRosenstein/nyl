//! `nyl recover` and `nyl verify` in-process, in the scenario test world
//! with the walkthroughs' shared project: Git state in `project.git`, the
//! fake kinds standing in for the OpenTofu, image, and publication units,
//! and a fault injector to crash a run where a test needs a dead run.

mod scenario;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use nyl::orchestration::run::{CommandOutcome, Wiring};
use nyl_core::orchestration::codes::Code;
use nyl_core::orchestration::driver::{
    Capabilities, Driver, DriverOperation, DriverResult, ExecStatus, ExecutionContext, Inspection, KeyInputs,
    LocalPathField, Outcome, OwnershipScope, PlanReport, RecoveryPolicies, ResolutionContext, Supported, Verification,
};
use nyl_core::orchestration::ids::{EnvironmentName, RunId};
use nyl_core::orchestration::ports::{Checkpoint, Clock as _, Transition, TransitionChanges};
use nyl_core::orchestration::state::{
    ArtifactDocument, Condition, DesiredUnit, FailureCategory, Lease, LeaseKind, ObservedUnit, Operation, Receipt, Run,
    RunKind, StateV1,
};
use nyl_core::orchestration::time::{Duration, Timestamp};
use nyl_drivers::fake::{FakeImage, FakeInfra, FakePublication, Scripted};
use nyl_drivers::process::ProcessRunner;
use nyl_orchestration::fault::{FaultAction, FaultInjector, FaultPoint, NoFaults};
use nyl_orchestration::run_report::EnvironmentRunReport;
use nyl_orchestration::shell::run::Cancellation;
use nyl_orchestration::shell::verify;
use nyl_orchestration::shell::Settings;
use nyl_orchestration::testing::FakeClock;
use scenario::runner::{invoke_in, FaultSlot, AUTHOR};
use scenario::shell::{self, dev, shared_project, unit};
use scenario::world::{git, git_output, lease};
use serde_json::Value;

const OBSERVED: &str = "refs/heads/nyl/dev/observed";
const DESIRED: &str = "refs/heads/nyl/dev/desired";

/// One test world: the shared project, a Git state store to stage state
/// with, a clock tests advance, and the fake kinds.
struct Harness {
    shell: shell::Harness,
}

impl std::ops::Deref for Harness {
    type Target = shell::Harness;

    fn deref(&self) -> &shell::Harness {
        &self.shell
    }
}

impl std::ops::DerefMut for Harness {
    fn deref_mut(&mut self) -> &mut shell::Harness {
        &mut self.shell
    }
}

impl Harness {
    fn new() -> Self {
        Self {
            shell: shell::Harness::new(&shared_project(), true),
        }
    }

    /// `nyl <args>` in-process in a fresh clone of `main`, with `faults`.
    fn invoke_with(&self, args: &[&str], faults: &dyn FaultInjector) -> CommandOutcome {
        self.invoke_in(args, "", faults)
    }

    /// `nyl <args>` in-process in `directory` of a fresh clone of `main`.
    fn invoke_in(&self, args: &[&str], directory: &str, faults: &dyn FaultInjector) -> CommandOutcome {
        let tools = ProcessRunner::new();
        let wiring = Wiring {
            clock: self.clock.clone(),
            ids: &self.ids,
            drivers: &self.drivers,
            tools: &tools,
            faults,
            runner: "test-runner".to_owned(),
            author: Some(AUTHOR.parse().unwrap()),
            process_env: self.process_env.clone(),
            cancellation: Cancellation::default(),
            prompt: None,
            observer: None,
        };
        let args = args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
        invoke_in(&self.world, &wiring, &args, "main", directory).unwrap()
    }

    fn invoke(&self, args: &[&str]) -> CommandOutcome {
        self.invoke_with(args, &NoFaults)
    }

    fn init(&self) {
        let outcome = self.invoke(&["state", "init", "-e", "dev"]);
        assert_eq!(outcome.exit, 0, "{}", outcome.stderr);
    }

    fn reconcile(&self, args: &[&str]) -> CommandOutcome {
        let mut argv = vec!["reconcile", "-e", "dev", "-o", "json"];
        argv.extend_from_slice(args);
        self.invoke(&argv)
    }

    fn tip(&self, reference: &str) -> String {
        git_output(&self.world.bare(), &["rev-parse", reference])
    }

    fn message(&self, reference: &str) -> String {
        git_output(&self.world.bare(), &["log", "-1", "--format=%B", reference])
    }

    /// The coordination refs of `dev` beneath `kind`, such as `runs`.
    fn coordination_refs(&self, kind: &str) -> String {
        git_output(
            &self.world.bare(),
            &["for-each-ref", "--format=%(refname)", &format!("refs/nyl/dev/{kind}/")],
        )
    }

    fn show(&self, reference: &str, path: &str) -> String {
        git_output(&self.world.bare(), &["show", &format!("{reference}:{path}")])
    }
}

fn unit_field<'a>(report: &'a Value, name: &str, field: &str) -> &'a Value {
    &report["environments"][0]["units"][name][field]
}

/// `recover --retry` clears a non-retryable failure in one observed commit
/// that records who asked and why, never executes, and the next reconcile
/// executes the unit and its dependent.
#[test]
fn test_recover_retry_writes_one_observed_commit_and_the_next_reconcile_executes_the_unit() {
    let harness = Harness::new();
    harness.init();
    harness.script.push(
        unit("web-image"),
        DriverOperation::Reconcile,
        &Scripted::Failed {
            category: FailureCategory::ToolError,
            retryable: false,
        },
        1,
    );
    let failed = harness.reconcile(&[]);
    assert_eq!(failed.exit, 3, "{}", failed.stderr);
    let desired = harness.tip(DESIRED);
    let observed = harness.tip(OBSERVED);

    let outcome = harness.invoke(&[
        "recover",
        "-e",
        "dev",
        "--unit",
        "web-image",
        "--retry",
        "--reason",
        "the registry rejected nothing",
        "-o",
        "json",
    ]);
    assert_eq!(outcome.exit, 0, "{}", outcome.stderr);
    assert_eq!(outcome.report["outcome"], "done");
    assert_eq!(outcome.report["details"]["condition"]["state"], "failed");
    assert_eq!(harness.tip(DESIRED), desired, "recover writes only observed state");
    assert_eq!(
        git_output(
            &harness.world.bare(),
            &["rev-list", "--count", &format!("{observed}..{OBSERVED}")]
        ),
        "1"
    );
    let message = harness.message(OBSERVED);
    for expected in [
        "nyl: recover dev (web-image: retry)",
        "web-image: {\"action\":\"retry\"}",
        "recovered: [web-image]",
        "Nyl-Operation: recover",
        &format!("Nyl-Requested-By: {AUTHOR}"),
        "Nyl-Reason: the registry rejected nothing",
    ] {
        assert!(message.contains(expected), "{expected:?} missing in\n{message}");
    }
    assert!(harness.coordination_refs("").is_empty(), "the lease is released");

    let again = harness.invoke(&["recover", "-e", "dev", "--unit", "web-image", "--retry", "-o", "json"]);
    assert_eq!(again.exit, 1);
    assert_eq!(again.report["errors"][0]["code"], "NYL-RECOVER-NOTHING");

    let next = harness.reconcile(&[]);
    assert_eq!(next.exit, 0, "{}", next.stderr);
    assert_eq!(unit_field(&next.report, "web-image", "result"), "executed");
    assert_eq!(unit_field(&next.report, "kubernetes", "result"), "executed");
}

/// `recover` takes the lease like every state-writing operation: while a
/// run holds it, recover exits 2 and writes nothing.
#[test]
fn test_recover_exits_2_while_another_run_holds_the_lease() {
    let harness = Harness::new();
    harness.init();
    let killed = harness.invoke_with(
        &["reconcile", "-e", "dev"],
        &FaultSlot::kill_once(FaultPoint::BeforePush, None),
    );
    assert_ne!(killed.exit, 0);
    let observed = harness.tip(OBSERVED);
    let outcome = harness.invoke(&["recover", "-e", "dev", "--unit", "network", "--retry", "-o", "json"]);
    assert_eq!(outcome.exit, 2, "{}", outcome.stderr);
    assert_eq!(outcome.report["errors"][0]["code"], "NYL-LEASE-HELD");
    assert_eq!(harness.tip(OBSERVED), observed);
}

/// A dead run that crashed after an effect, before its checkpoint.
fn crash_after_effect(harness: &Harness) {
    harness.init();
    harness.script.push(
        unit("web-image"),
        DriverOperation::Reconcile,
        &Scripted::Failed {
            category: FailureCategory::ToolError,
            retryable: false,
        },
        1,
    );
    assert_eq!(harness.reconcile(&["--unit", "web-image"]).exit, 3);
    let killed = harness.invoke_with(
        &["reconcile", "-e", "dev", "--unit", "network"],
        &FaultSlot::kill_once(FaultPoint::AfterEffect, Some("network")),
    );
    assert_ne!(killed.exit, 0);
    assert!(
        !harness.coordination_refs("runs").is_empty(),
        "the dead run keeps its run ref"
    );
    // The dead run's lease and run deadline pass.
    harness.clock.advance(Duration::from_mins(180));
}

/// `recover` never consumes a dead run's ref: it is the only record of the
/// unit the run was executing when it died, which the next reconcile's
/// takeover marks uncertain. Recovering an unrelated unit keeps it.
#[test]
fn test_recover_keeps_the_run_ref_of_a_dead_run() {
    let harness = Harness::new();
    crash_after_effect(&harness);
    let runs = harness.coordination_refs("runs");
    let outcome = harness.invoke(&["recover", "-e", "dev", "--unit", "web-image", "--retry"]);
    assert_eq!(outcome.exit, 0, "{}", outcome.stderr);
    assert_eq!(harness.coordination_refs("runs"), runs);
}

/// After `recover` on an unrelated unit, the next reconcile still takes
/// over the dead run: it consumes its run ref and recovers the unit that
/// run was executing instead of treating it as never started.
#[test]
fn test_reconcile_after_recover_takes_over_the_dead_run() {
    let harness = Harness::new();
    crash_after_effect(&harness);
    let outcome = harness.invoke(&["recover", "-e", "dev", "--unit", "web-image", "--retry"]);
    assert_eq!(outcome.exit, 0, "{}", outcome.stderr);
    let next = harness.reconcile(&["--unit", "network"]);
    let result = unit_field(&next.report, "network", "result");
    assert!(
        result == "recovered" || result == "uncertain",
        "{result}: {}",
        next.stdout
    );
    assert!(harness.coordination_refs("runs").is_empty());
}

/// `--accept` adopts the receipt a late runner checkpointed for an
/// uncertain unit, shows the checkpoint as acceptable evidence first, and
/// deletes the evidence ref once no uncertain unit needs it.
#[test]
fn test_accept_adopts_a_late_checkpoint_and_deletes_its_evidence_ref() {
    let harness = Harness::new();
    harness.init();
    assert_eq!(harness.reconcile(&[]).exit, 0);

    // Stage what a takeover leaves: network uncertain, and the late run's
    // checkpoint of it on an evidence ref.
    let (store, location) = (harness.store.as_ref(), harness.location.clone());
    let snapshot = store.snapshot(&location).unwrap();
    let network = unit("network");
    let late = RunId::new("00000000-0000-7000-9000-0000000000ff").unwrap();
    let now = harness.clock.now();
    let lease = Lease {
        api_version: StateV1,
        kind: LeaseKind,
        run: RunId::new("00000000-0000-7000-9000-0000000000fe").unwrap(),
        runner: "staging".to_owned(),
        operation: Operation::Reconcile,
        units: Vec::new(),
        deadline: Duration::from_mins(30).after(now),
        taken_at: now,
        broken: None,
    };
    let version = store.take_lease(&location, &dev(), &lease, None).unwrap();
    let current = snapshot.observed[&network].clone();
    let uncertain = ObservedUnit {
        condition: Some(Condition::Uncertain {
            since: now,
            run: late.clone(),
            reason: "run lost its lease".to_owned(),
        }),
        ..current.clone()
    };
    let committed = store
        .commit(
            &location,
            &dev(),
            &snapshot.tips,
            &Transition {
                message: "nyl: reconcile dev (1 uncertain)\n\nNyl-Operation: reconcile\n".to_owned(),
                changes: TransitionChanges {
                    observed: BTreeMap::from([(network.clone(), Some(uncertain))]),
                    ..TransitionChanges::default()
                },
            },
            &version,
        )
        .unwrap();
    let mut late_receipt = current.receipt.clone().unwrap();
    late_receipt.run = late.clone();
    late_receipt.finished_at = now;
    let record = Run {
        api_version: StateV1,
        kind: RunKind,
        run: late.clone(),
        runner: "late-runner".to_owned(),
        operation: Operation::Reconcile,
        source_commit: None,
        read_desired: None,
        read_observed: None,
        deadline: now,
        units: Vec::new(),
        finished: true,
    };
    let checkpoint = Checkpoint {
        desired: snapshot.desired[&network].clone(),
        observed: ObservedUnit {
            receipt: Some(late_receipt),
            condition: None,
            ..current.clone()
        },
        artifacts: BTreeMap::new(),
    };
    store.write_run(&location, &dev(), &record, Some(&checkpoint)).unwrap();
    store.retire_run(&location, &dev(), &late, None).unwrap();
    store
        .release_lease(&location, &dev(), &committed.lease_version)
        .unwrap();
    assert!(!harness.coordination_refs("evidence").is_empty());

    let unknown = harness.invoke(&[
        "recover", "-e", "dev", "--unit", "network", "--accept", "nope", "--reason", "r", "-o", "json",
    ]);
    assert_eq!(unknown.exit, 1);
    assert_eq!(unknown.report["errors"][0]["code"], "NYL-RECOVER-CHECKPOINT-MISSING");

    let outcome = harness.invoke(&[
        "recover",
        "-e",
        "dev",
        "--unit",
        "network",
        "--accept",
        late.as_str(),
        "--reason",
        "the runner finished after losing its lease",
        "-o",
        "json",
    ]);
    assert_eq!(outcome.exit, 0, "{}", outcome.stderr);
    let evidence = &outcome.report["details"]["evidence"][0];
    assert_eq!(evidence["run"], late.as_str());
    assert_eq!(
        (evidence["result"].as_str(), evidence["acceptable"].as_bool()),
        (Some("receipt"), Some(true))
    );
    let observed = harness.show(OBSERVED, "observed/units/network.yaml");
    assert!(
        observed.contains(late.as_str()) && !observed.contains("uncertain"),
        "{observed}"
    );
    // The accepted receipt carries the desired document it was produced
    // for, which a later hold reverts to (Q16).
    let accepted: ObservedUnit = serde_saphyr::from_str(&observed).unwrap();
    assert_eq!(accepted.desired.as_deref(), Some(&snapshot.desired[&network]));
    assert!(harness
        .message(OBSERVED)
        .contains(&format!("\"action\":\"accept\",\"run\":\"{late}\"")));
    assert!(
        harness.coordination_refs("evidence").is_empty(),
        "the evidence ref is deleted"
    );
}

/// `verify` records a drift observation per verified unit in one observed
/// commit and releases the lease; units without a current receipt are
/// reported (exit 2). An unchanged second verify writes nothing, a verify
/// after every unit has a receipt exits 0, and a change outside Nyl is
/// recorded as drift (exit 3).
#[test]
fn test_verify_records_observations_once_and_reports_drift() {
    let harness = Harness::new();
    harness.init();
    harness.script.push(
        unit("web-image"),
        DriverOperation::Reconcile,
        &Scripted::Failed {
            category: FailureCategory::ToolError,
            retryable: true,
        },
        1,
    );
    assert_eq!(harness.reconcile(&[]).exit, 3);
    let reconciled = harness.tip(OBSERVED);

    let partial = harness.invoke(&["verify", "-e", "dev", "-o", "json"]);
    assert_eq!(partial.exit, 2, "{}", partial.stderr);
    assert_eq!(unit_field(&partial.report, "network", "verification"), "clean");
    assert_eq!(unit_field(&partial.report, "web-image", "verification"), "no-receipt");
    assert_eq!(unit_field(&partial.report, "kubernetes", "verification"), "no-receipt");
    assert!(partial.stderr.contains("NYL-VERIFY-NO-RECEIPT"), "{}", partial.stderr);
    let verified = harness.tip(OBSERVED);
    assert_eq!(
        git_output(
            &harness.world.bare(),
            &["rev-list", "--count", &format!("{reconciled}..{OBSERVED}")]
        ),
        "1"
    );
    let message = harness.message(OBSERVED);
    assert!(
        message.starts_with("nyl: verify dev (2 clean, 2 no-receipt)"),
        "{message}"
    );
    assert!(message.contains("Nyl-Operation: verify"), "{message}");
    assert!(!message.contains("Nyl-Requested-By"), "{message}");
    let drift = harness.show(OBSERVED, "observed/units/network/observations/drift.yaml");
    assert!(
        drift.contains("kind: Observation") && drift.contains("drift: clean"),
        "{drift}"
    );
    assert!(harness.coordination_refs("").is_empty(), "the lease is released");

    harness.clock.advance(Duration::from_mins(60));
    let again = harness.invoke(&["verify", "-e", "dev"]);
    assert_eq!(again.exit, 2, "{}", again.stderr);
    assert_eq!(harness.tip(OBSERVED), verified, "an unchanged verify writes nothing");

    assert_eq!(harness.reconcile(&[]).exit, 0);
    let clean = harness.invoke(&["verify", "-e", "dev", "-o", "json"]);
    assert_eq!(clean.exit, 0, "{}", clean.stderr);
    for name in ["network", "database", "web-image", "kubernetes"] {
        assert_eq!(unit_field(&clean.report, name, "verification"), "clean", "{name}");
    }

    // Someone changes the network's state outside Nyl.
    let backend = harness.world.scenario_tmp().join("tofu/dev-network.tfstate");
    std::fs::write(&backend, r#"{"inputs": {"cidr": "10.1.0.0/16"}, "outputs": {}}"#).unwrap();
    let drifted = harness.invoke(&["verify", "-e", "dev", "--unit", "network", "-o", "json"]);
    assert_eq!(drifted.exit, 3, "{}", drifted.stderr);
    assert_eq!(unit_field(&drifted.report, "network", "verification"), "drifted");
    assert_eq!(drifted.report["environments"][0]["units"].as_object().unwrap().len(), 1);
    assert!(drifted.stderr.contains("NYL-VERIFY-DRIFT"), "{}", drifted.stderr);
    assert!(harness
        .show(OBSERVED, "observed/units/network/observations/drift.yaml")
        .contains("drift: drifted"));
}

/// What one `verify` call of a [`Recording`] driver saw.
#[derive(Debug, Clone)]
struct Seen {
    unit: String,
    now: Timestamp,
    deadline: Timestamp,
    project_dir: PathBuf,
    /// Whether `project_dir` held a `nyl.toml` during the call.
    project_found: bool,
    /// The lease's deadline in the state repository during the call.
    lease_deadline: Option<Timestamp>,
}

/// What every wrapped fake shares: the clock it advances during `verify`,
/// how far per unit, and what each call saw.
struct Recorder {
    clock: Arc<FakeClock>,
    bare: PathBuf,
    advance: Mutex<BTreeMap<String, Duration>>,
    seen: Mutex<Vec<Seen>>,
    /// Cancelled by the first `verify` call, as a signal during it would.
    cancel: Mutex<Option<Cancellation>>,
}

/// A fake kind that records what its `verify` calls see and lets time pass
/// during them, delegating everything to the fake it wraps.
struct Recording {
    inner: Arc<dyn Driver>,
    recorder: Arc<Recorder>,
}

impl Driver for Recording {
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
        self.inner.capabilities(spec)
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
        self.inner.reconcile(ctx, unit)
    }
    fn verify(
        &self,
        ctx: &ExecutionContext<'_>,
        unit: &DesiredUnit,
        receipt: &Receipt,
    ) -> DriverResult<Supported<Verification>> {
        let name = unit.unit.name.to_string();
        self.recorder.seen.lock().unwrap().push(Seen {
            unit: name.clone(),
            now: ctx.now,
            deadline: ctx.deadline,
            project_dir: ctx.project_dir.clone(),
            project_found: ctx.project_dir.join("nyl.toml").is_file(),
            lease_deadline: lease(&self.recorder.bare).map(|lease| lease.deadline),
        });
        let advance = self.recorder.advance.lock().unwrap().get(&name).copied();
        if let Some(advance) = advance {
            self.recorder.clock.advance(advance);
        }
        if let Some(cancellation) = self.recorder.cancel.lock().unwrap().as_ref() {
            cancellation.cancel(nyl_orchestration::shell::run::SIGTERM);
        }
        self.inner.verify(ctx, unit, receipt)
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
        push: bool,
    ) -> DriverResult<Supported<Vec<ArtifactDocument>>> {
        self.inner.build(ctx, unit, push)
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

impl Harness {
    /// A harness whose fake kinds record their `verify` calls.
    fn recording() -> (Self, Arc<Recorder>) {
        let mut harness = Self::new();
        let recorder = Arc::new(Recorder {
            clock: harness.clock.clone(),
            bare: harness.world.bare(),
            advance: Mutex::new(BTreeMap::new()),
            seen: Mutex::new(Vec::new()),
            cancel: Mutex::new(None),
        });
        let fakes: [(&str, Arc<dyn Driver>); 3] = [
            ("OpenTofu", Arc::new(FakeInfra::new(harness.script.clone()))),
            ("OciImage", Arc::new(FakeImage::new(harness.script.clone()))),
            (
                "KubernetesPublication",
                Arc::new(FakePublication::new(harness.script.clone())),
            ),
        ];
        for (kind, inner) in fakes {
            let recording = Arc::new(Recording {
                inner,
                recorder: recorder.clone(),
            });
            harness
                .drivers
                .register_as(nyl_core::constants::API_VERSION_UNITS, kind, recording);
        }
        (harness, recorder)
    }

    /// `verify` through the shell's API on a fresh clone of `main`, with a
    /// concurrency limit the command line does not offer.
    fn verify_with_concurrency(&self, concurrency: usize) -> EnvironmentRunReport {
        self.verify_cancelled_by(concurrency, Cancellation::default())
    }

    /// [`Self::verify_with_concurrency`] with `cancellation`.
    fn verify_cancelled_by(&self, concurrency: usize, cancellation: Cancellation) -> EnvironmentRunReport {
        let mut job = self.job(&[], cancellation);
        job.request.concurrency = concurrency;
        self.run_with(&job, verify::verify).environments.remove(0)
    }
}

/// With more units than the concurrency limit, verify's lease covers every
/// unit's deadline: a runner may take over an expired lease, so a unit
/// still verifying past it would end the whole verify with a lost lease.
#[test]
fn test_verify_lease_covers_every_unit_deadline_across_batches() {
    let (harness, recorder) = Harness::recording();
    harness.init();
    assert_eq!(harness.reconcile(&[]).exit, 0);
    // Each verify takes 9 minutes, less than the shortest unit timeout
    // (10m) but enough that later batches end past a lease sized once.
    for name in ["network", "database", "web-image", "kubernetes"] {
        recorder
            .advance
            .lock()
            .unwrap()
            .insert(name.to_owned(), Duration::from_mins(9));
    }

    let report = harness.verify_with_concurrency(1);
    assert_eq!(report.exit.code(), 0, "{report:#?}");
    let seen = recorder.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 4, "{seen:#?}");
    let grace = Settings::default().lease_grace;
    for call in &seen {
        let lease = call.lease_deadline.expect("verify holds the lease");
        assert!(
            grace.after(call.deadline) <= lease,
            "{}: unit deadline {} plus grace passes the lease deadline {lease}",
            call.unit,
            call.deadline
        );
    }
    // The last unit starts 27 minutes in and keeps its whole timeout.
    let web = seen.iter().find(|call| call.unit == "web-image").unwrap();
    assert_eq!(web.deadline, Duration::from_mins(60).after(web.now));
    assert!(harness.coordination_refs("").is_empty(), "the lease is released");
}

/// A verify cancelled during a batch ("Runs and leases" item 7) starts no
/// further batch, writes no observed commit, releases the lease, and exits
/// 4 with `NYL-RUN-CANCELLED`, instead of verifying every unit after the
/// signal while CI waits to kill the job.
#[test]
fn test_cancelled_verify_starts_no_batch_and_records_nothing() {
    let (harness, recorder) = Harness::recording();
    harness.init();
    assert_eq!(harness.reconcile(&[]).exit, 0);
    let observed = harness.tip(OBSERVED);
    let cancellation = Cancellation::new();
    *recorder.cancel.lock().unwrap() = Some(cancellation.clone());

    let report = harness.verify_cancelled_by(1, cancellation);
    assert_eq!(report.exit.code(), 4, "{report:#?}");
    let codes: Vec<Code> = report.errors.iter().map(|finding| finding.code).collect();
    assert_eq!(codes, [Code::RunCancelled], "{report:#?}");
    assert_eq!(recorder.seen.lock().unwrap().len(), 1, "one batch ran");
    assert_eq!(harness.tip(OBSERVED), observed, "a cancelled verify records nothing");
    assert!(harness.coordination_refs("").is_empty(), "the lease is released");
}

/// A verify that passes its unit's timeout is a failed `timeout`, exit 1,
/// not evidence of the unit's state: nothing is recorded for the unit.
#[test]
fn test_verify_past_its_timeout_fails_and_records_nothing() {
    let (harness, recorder) = Harness::recording();
    harness.init();
    assert_eq!(harness.reconcile(&[]).exit, 0);
    let observed = harness.tip(OBSERVED);
    recorder
        .advance
        .lock()
        .unwrap()
        .insert("network".to_owned(), Duration::from_mins(61));

    let outcome = harness.invoke(&["verify", "-e", "dev", "--unit", "network", "-o", "json"]);
    assert_eq!(outcome.exit, 1, "{}", outcome.stderr);
    let error = &outcome.report["environments"][0]["errors"][0];
    assert_eq!(error["code"], "NYL-DRIVER-ERROR", "{}", outcome.stdout);
    assert!(error["message"].as_str().unwrap().contains("timeout"), "{error}");
    assert_eq!(harness.tip(OBSERVED), observed, "a timed-out verify records nothing");
}

/// A driver verifies in the project directory reconcile gave it for the
/// unit's commit, found through `[project] previous_paths` after the
/// project moved, not the entry checkout's directory applied to an older
/// commit where it does not exist.
#[test]
fn test_verify_locates_a_moved_project_at_the_units_commit() {
    let (harness, recorder) = Harness::recording();
    harness.init();
    assert_eq!(harness.reconcile(&[]).exit, 0);

    let clone = harness.world.fresh_clone("main");
    git(&clone, &["mv", "nyl", "deploy"]);
    std::fs::write(
        clone.join("deploy/nyl.toml"),
        "[project]\nprevious_paths = [\"/nyl\"]\n",
    )
    .unwrap();
    git(&clone, &["add", "-A"]);
    git(&clone, &["commit", "--quiet", "-m", "Move the project to deploy/"]);
    git(&clone, &["push", "--quiet", "origin", "HEAD:refs/heads/main"]);

    let outcome = harness.invoke_in(&["verify", "-e", "dev", "-o", "json"], "deploy", &NoFaults);
    assert_eq!(outcome.exit, 0, "{}\n{}", outcome.stdout, outcome.stderr);
    let seen = recorder.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 4, "{seen:#?}");
    for call in &seen {
        assert!(
            call.project_dir.ends_with("nyl") && call.project_found,
            "{}: {}",
            call.unit,
            call.project_dir.display()
        );
    }
}

// ---------------------------------------------------------------------------
// Importing a broken run ("Recovery", "Importing a broken run"; "Runs and
// leases" items 1 and 8)

impl Harness {
    /// Write `changes` as one transition under a lease of its own, as a run
    /// would, with `message` (trailers included).
    fn stage(&self, changes: TransitionChanges, message: &str) {
        let (store, location) = (self.store.as_ref(), &self.location);
        let now = self.clock.now();
        let holder = Lease {
            api_version: StateV1,
            kind: LeaseKind,
            run: RunId::new("00000000-0000-7000-9000-0000000000fe").unwrap(),
            runner: "staging".to_owned(),
            operation: Operation::Reconcile,
            units: Vec::new(),
            deadline: Duration::from_mins(30).after(now),
            taken_at: now,
            broken: None,
        };
        let version = store.take_lease(location, &dev(), &holder, None).unwrap();
        let tips = store.snapshot(location).unwrap().tips;
        let committed = store
            .commit(
                location,
                &dev(),
                &tips,
                &Transition {
                    message: message.to_owned(),
                    changes,
                },
                &version,
            )
            .unwrap();
        store.release_lease(location, &dev(), &committed.lease_version).unwrap();
    }

    /// Take the lease for a holder that then dies without a run ref, and
    /// break it with `nyl lease break`.
    fn break_dead_holder(&self, operation: Operation) {
        let now = self.clock.now();
        let holder = Lease {
            api_version: StateV1,
            kind: LeaseKind,
            run: RunId::new("00000000-0000-7000-9000-0000000000fd").unwrap(),
            runner: "lost runner".to_owned(),
            operation,
            units: Vec::new(),
            deadline: Duration::from_mins(60).after(now),
            taken_at: now,
            broken: None,
        };
        self.store.take_lease(&self.location, &dev(), &holder, None).unwrap();
        self.break_lease();
    }

    fn break_lease(&self) {
        let broken = self.invoke(&["lease", "break", "-e", "dev", "--reason", "runner lost"]);
        assert_eq!(broken.exit, 0, "{}", broken.stderr);
    }

    /// The first run dies after `database`'s effect, before its checkpoint,
    /// with network and web-image checkpointed (the walkthrough "Effects
    /// without a receipt"), and its lease is broken. Returns the dead run and
    /// the uid it created for `database`.
    fn break_after_database(&self) -> (RunId, nyl_core::orchestration::ids::Uid) {
        self.init();
        let killed = self.invoke_with(
            &["reconcile", "-e", "dev"],
            &FaultSlot::kill_once(FaultPoint::AfterEffect, Some("database")),
        );
        assert_ne!(killed.exit, 0);
        let coordination = self.coordination();
        let [dead] = coordination.runs.as_slice() else {
            panic!("the dead run keeps its run ref: {coordination:#?}");
        };
        let database = dead
            .created
            .iter()
            .find(|created| created.unit.name == unit("database"))
            .and_then(|created| created.unit.uid.clone())
            .expect("the dead run recorded database's uid before starting it");
        let run = dead.run.run.clone();
        self.break_lease();
        (run, database)
    }
}

fn names(value: &Value) -> Vec<&str> {
    value
        .as_array()
        .map(|items| items.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

/// `nyl recover -e dev` after a break and a crash after an effect imports
/// without executing (Q45): it adopts the uid the dead run created, turns
/// matching checkpoints into imported receipts, marks the unit without a
/// checkpoint uncertain, records the break once with the requester and
/// reason, consumes the run ref, writes no run ref of its own, and exits 0
/// although a unit is left uncertain (Q43).
#[test]
fn test_recover_imports_a_broken_run_without_executing() {
    let harness = Harness::new();
    let (dead, database_uid) = harness.break_after_database();

    let outcome = harness.invoke(&["recover", "-e", "dev", "--reason", "runner gone", "-o", "json"]);
    assert_eq!(outcome.exit, 0, "{}", outcome.stderr);
    let details = &outcome.report["details"];
    assert_eq!(outcome.report["outcome"], "done");
    assert_eq!(names(&details["imported"]), ["network", "web-image"], "{details}");
    assert_eq!(names(&details["recovered"]), ["database"], "{details}");
    assert_eq!(details["leaseBroken"]["reason"], "runner lost");

    let snapshot = harness.snapshot();
    assert_eq!(snapshot.desired[&unit("database")].unit.uid, Some(database_uid));
    let database = &snapshot.observed[&unit("database")];
    assert!(
        matches!(&database.condition, Some(Condition::Uncertain { run, .. }) if *run == dead),
        "{database:#?}"
    );
    assert!(database.receipt.is_none(), "nothing executed");
    assert!(
        !snapshot.observed.contains_key(&unit("kubernetes")),
        "kubernetes never ran"
    );
    let message = harness.message(OBSERVED);
    for expected in [
        "nyl: recover dev",
        "\nimported: [network, web-image]",
        "\nrecovered: [database]",
        "\nleaseBroken: {",
        "runner lost",
        "Nyl-Operation: recover",
        &format!("Nyl-Requested-By: {AUTHOR}"),
        "Nyl-Reason: runner gone",
    ] {
        assert!(message.contains(expected), "{expected:?} missing in\n{message}");
    }
    assert_eq!(message.matches("leaseBroken").count(), 1, "{message}");
    assert!(
        harness.coordination_refs("").is_empty(),
        "every coordination ref is gone"
    );

    // The next reconcile runs database again under its recovery policy.
    let next = harness.reconcile(&[]);
    assert_eq!(next.exit, 0, "{}", next.stderr);
}

/// Q44: the recover commit's headline gives the import's counts, then the
/// unit action.
#[test]
#[ignore = "needs lifecycle: run/mod.rs completes a Recover-mode summary with shell::recover::summarize"]
fn test_recover_import_headline_counts_then_the_unit_action() {
    let harness = Harness::new();
    harness.break_after_database();
    let outcome = harness.invoke(&["recover", "-e", "dev", "--unit", "database", "--retry"]);
    assert_eq!(outcome.exit, 0, "{}", outcome.stderr);
    let message = harness.message(OBSERVED);
    assert!(
        message.starts_with("nyl: recover dev (2 imported, 1 recovered; database: retry)"),
        "{message}"
    );
}

/// A recover that dies before its commit lands leaves the broken run's refs
/// for the next import, and never had a run ref of its own (Q45).
#[test]
fn test_a_recover_crashing_mid_import_leaves_the_broken_runs_refs() {
    let harness = Harness::new();
    let (dead, _) = harness.break_after_database();
    let observed = harness.tip(OBSERVED);
    let killed = harness.invoke_with(
        &["recover", "-e", "dev"],
        &FaultSlot::kill_once(FaultPoint::BeforePush, None),
    );
    assert_ne!(killed.exit, 0, "{}", killed.stdout);
    assert_eq!(harness.tip(OBSERVED), observed, "nothing was committed");
    assert_eq!(
        harness.coordination_refs("runs"),
        format!("refs/nyl/dev/runs/{dead}"),
        "only the broken run's ref"
    );
}

/// `recover -e dev --unit database --retry` imports and decides the unit in
/// one commit per ref: the import's uncertain condition is cleared there.
#[test]
fn test_recover_import_with_a_unit_action_writes_one_commit() {
    let harness = Harness::new();
    harness.break_after_database();
    let observed = harness.tip(OBSERVED);
    let outcome = harness.invoke(&[
        "recover", "-e", "dev", "--unit", "database", "--retry", "--reason", "checked", "-o", "json",
    ]);
    assert_eq!(outcome.exit, 0, "{}", outcome.stderr);
    assert_eq!(outcome.report["details"]["database"]["action"], "retry");
    assert_eq!(
        git_output(
            &harness.world.bare(),
            &["rev-list", "--count", &format!("{observed}..{OBSERVED}")]
        ),
        "1"
    );
    let message = harness.message(OBSERVED);
    assert!(
        message.contains("database: {\"action\":\"retry\"}") && message.contains("\nleaseBroken: {"),
        "{message}"
    );
    let database = &harness.snapshot().observed[&unit("database")];
    assert_eq!(database.condition, None, "{database:#?}");
    assert!(harness.coordination_refs("").is_empty());
}

/// Q40: a broken `state init` leaves no `state.yaml`, so nothing can be
/// imported and every other command refuses the broken lease; `recover -e
/// dev` takes it over and releases it, after which `state init` proceeds.
/// With `--unit` it refuses, as for any missing state.
#[test]
fn test_recover_clears_the_break_of_a_broken_state_init() {
    let harness = Harness::new();
    harness.break_dead_holder(Operation::StateInit);
    let refused = harness.invoke(&["state", "init", "-e", "dev", "-o", "json"]);
    assert_eq!(refused.exit, 2, "{}", refused.stderr);
    assert_eq!(refused.report["errors"][0]["code"], "NYL-LEASE-BROKEN");

    let unit_action = harness.invoke(&["recover", "-e", "dev", "--unit", "network", "--retry", "-o", "json"]);
    assert_eq!(unit_action.exit, 1, "{}", unit_action.stderr);
    assert_eq!(unit_action.report["errors"][0]["code"], "NYL-STATE-MISSING");

    let outcome = harness.invoke(&["recover", "-e", "dev", "-o", "json"]);
    assert_eq!(outcome.exit, 0, "{}", outcome.stderr);
    assert_eq!(outcome.report["outcome"], "done");
    assert_eq!(outcome.report["warnings"][0]["code"], "NYL-STATE-MISSING");
    assert!(harness.coordination_refs("").is_empty(), "the lease is released");
    harness.init();
}

/// Q40: on state that moved away, `recover -e dev` records the break with
/// an unchanged commit at the old location and releases the lease.
#[test]
fn test_recover_clears_the_break_on_moved_state() {
    let harness = Harness::new();
    harness.init();
    let mut record = harness.snapshot().record.unwrap();
    let mut to = harness.location.clone();
    "nyl/next/desired".clone_into(&mut to.desired_ref);
    record
        .events
        .push(nyl_core::orchestration::state::StateEvent::StateMoved {
            at: harness.clock.now(),
            by: "alice".to_owned(),
            reason: None,
            from: Box::new(harness.location.clone()),
            to: Box::new(to),
        });
    harness.stage(
        TransitionChanges {
            record: Some(record),
            ..TransitionChanges::default()
        },
        "nyl: state-move dev\n\nunits: {}\n\nNyl-Operation: state-move\nNyl-Environment: dev\n",
    );
    harness.break_dead_holder(Operation::StateMove);

    let outcome = harness.invoke(&["recover", "-e", "dev", "-o", "json"]);
    assert_eq!(outcome.exit, 0, "{}", outcome.stderr);
    assert_eq!(outcome.report["warnings"][0]["code"], "NYL-STATE-MOVED");
    let message = harness.message(OBSERVED);
    assert!(
        message.starts_with("nyl: recover dev") && message.contains("\nleaseBroken: {"),
        "{message}"
    );
    assert!(harness.coordination_refs("").is_empty(), "the lease is released");
}

/// Q40: with a `Nyl-Local` commit at the remote tip, `recover -e dev`
/// releases the broken lease without committing on that tip (Nyl commits
/// only on tips that pass the local-tip check), so `state restore` can run.
#[test]
fn test_recover_clears_the_break_under_a_local_tip_without_committing() {
    let harness = Harness::new();
    harness.init();
    assert_eq!(harness.reconcile(&[]).exit, 0);
    let network = harness.snapshot().observed[&unit("network")].clone();
    harness.stage(
        TransitionChanges {
            observed: BTreeMap::from([(unit("network"), Some(network))]),
            commit_unchanged: true,
            ..TransitionChanges::default()
        },
        "nyl: reconcile dev\n\nunits: {}\n\nNyl-Operation: reconcile\nNyl-Environment: dev\nNyl-Local: true\n",
    );
    harness.break_dead_holder(Operation::Reconcile);
    let observed = harness.tip(OBSERVED);

    let outcome = harness.invoke(&["recover", "-e", "dev", "-o", "json"]);
    assert_eq!(outcome.exit, 0, "{}", outcome.stderr);
    assert_eq!(outcome.report["warnings"][0]["code"], "NYL-STATE-LOCAL-TIP");
    assert_eq!(harness.tip(OBSERVED), observed, "nothing is committed on a local tip");
    assert!(harness.coordination_refs("").is_empty(), "the lease is released");
}

/// Q56: a recover import records the units as they are: a unit that left
/// the ownership set stays active until the next reconcile decides it.
#[test]
fn test_a_recover_import_writes_no_lifecycle_changes() {
    let harness = Harness::new();
    harness.init();
    assert_eq!(harness.reconcile(&[]).exit, 0);
    harness.world.commit_files(
        "main",
        "Change the web page",
        &[("services/web/index.html", "<h1>Changed</h1>\n")],
    );
    let killed = harness.invoke_with(
        &["reconcile", "-e", "dev"],
        &FaultSlot::kill_once(FaultPoint::AfterEffect, Some("web-image")),
    );
    assert_ne!(killed.exit, 0);
    harness.break_lease();
    // kubernetes leaves the ownership set.
    let units = std::fs::read_to_string(shared_project().join("nyl/units.yaml")).unwrap();
    let leaving = units.replace(
        "name: kubernetes\n  labels: {dev: 'true'}",
        "name: kubernetes\n  labels: {dev: 'false'}",
    );
    assert_ne!(leaving, units);
    harness
        .world
        .commit_files("main", "Drop kubernetes from dev", &[("nyl/units.yaml", &leaving)]);
    let before = harness.snapshot().desired[&unit("kubernetes")].clone();

    let outcome = harness.invoke(&["recover", "-e", "dev", "-o", "json"]);
    assert_eq!(outcome.exit, 0, "{}", outcome.stderr);
    assert_eq!(names(&outcome.report["details"]["recovered"]), ["web-image"]);
    assert_eq!(harness.snapshot().desired[&unit("kubernetes")], before);
}

/// Without a broken lease, `recover -e dev` without `--unit` has nothing to
/// do: `NYL-RECOVER-NOTHING`, exit 1.
#[test]
fn test_recover_without_a_break_or_a_unit_refuses() {
    let harness = Harness::new();
    harness.init();
    let outcome = harness.invoke(&["recover", "-e", "dev", "-o", "json"]);
    assert_eq!(outcome.exit, 1, "{}", outcome.stderr);
    assert_eq!(outcome.report["errors"][0]["code"], "NYL-RECOVER-NOTHING");
    assert!(harness.coordination_refs("").is_empty());
}

/// Runs `nyl recover -e dev` the first time an operation waits for the lease.
struct RecoverWhileWaiting<'h> {
    harness: &'h Harness,
    recovered: Mutex<Option<CommandOutcome>>,
}

impl FaultInjector for RecoverWhileWaiting<'_> {
    fn at(&self, point: FaultPoint, _unit: Option<&nyl_core::orchestration::ids::UnitName>) -> FaultAction {
        if point == FaultPoint::WaitingLease {
            let mut recovered = self.recovered.lock().unwrap();
            if recovered.is_none() {
                *recovered = Some(self.harness.invoke(&["recover", "-e", "dev"]));
            }
        }
        FaultAction::Continue
    }
}

/// `verify` never imports, so a broken lease refuses it (exit 2,
/// `NYL-LEASE-BROKEN`, naming `nyl recover -e dev`); with `--wait-lease` it
/// waits like for a held lease and proceeds once a recover imported the run.
#[test]
fn test_verify_refuses_a_broken_lease_and_waits_for_its_import() {
    let harness = Harness::new();
    harness.init();
    assert_eq!(harness.reconcile(&[]).exit, 0);
    harness.break_dead_holder(Operation::Reconcile);
    let observed = harness.tip(OBSERVED);

    let refused = harness.invoke(&["verify", "-e", "dev", "-o", "json"]);
    assert_eq!(refused.exit, 2, "{}", refused.stderr);
    assert_eq!(
        refused.report["environments"][0]["errors"][0]["code"],
        "NYL-LEASE-BROKEN"
    );
    assert!(refused.stderr.contains("nyl recover -e dev"), "{}", refused.stderr);
    assert_eq!(harness.tip(OBSERVED), observed);

    let injector = RecoverWhileWaiting {
        harness: &harness,
        recovered: Mutex::new(None),
    };
    let waited = harness.invoke_with(&["verify", "-e", "dev", "--wait-lease", "10m", "-o", "json"], &injector);
    let recovered = injector.recovered.lock().unwrap().take().expect("verify waited");
    assert_eq!(recovered.exit, 0, "{}", recovered.stderr);
    assert_eq!(waited.exit, 0, "{}", waited.stderr);
    assert!(harness.coordination_refs("").is_empty());
}

/// "Holds on uncertain units" (Q21): `--retry` on a held uncertain unit
/// clears only its condition and the unit stays held; `--accept` on a held
/// unit is refused, naming `nyl resume` first.
#[test]
fn test_recover_on_a_held_uncertain_unit() {
    let harness = Harness::new();
    harness.init();
    assert_eq!(harness.reconcile(&[]).exit, 0);
    let snapshot = harness.snapshot();
    let network = unit("network");
    let mut held = snapshot.desired[&network].clone();
    held.lifecycle = nyl_core::orchestration::state::Lifecycle::Held {
        hold: serde_json::from_value(serde_json::json!({"since": harness.clock.now(), "by": "alice"})).unwrap(),
        deletion: None,
    };
    let uncertain = ObservedUnit {
        condition: Some(Condition::Uncertain {
            since: harness.clock.now(),
            run: RunId::new("00000000-0000-7000-9000-0000000000fb").unwrap(),
            reason: "run lost its lease".to_owned(),
        }),
        ..snapshot.observed[&network].clone()
    };
    let stage = || {
        harness.stage(
            TransitionChanges {
                desired: BTreeMap::from([(network.clone(), Some(held.clone()))]),
                observed: BTreeMap::from([(network.clone(), Some(uncertain.clone()))]),
                ..TransitionChanges::default()
            },
            "nyl: hold dev\n\nunits: {}\n\nNyl-Operation: hold\nNyl-Environment: dev\n",
        );
    };
    stage();

    let retried = harness.invoke(&[
        "recover", "-e", "dev", "--unit", "network", "--retry", "--reason", "checked",
    ]);
    assert_eq!(retried.exit, 0, "{}", retried.stderr);
    let after = harness.snapshot();
    assert_eq!(after.observed[&network].condition, None);
    assert_eq!(after.observed[&network].receipt, uncertain.receipt);
    assert_eq!(after.desired[&network], held, "the unit stays held");

    stage();
    let accepted = harness.invoke(&[
        "recover",
        "-e",
        "dev",
        "--unit",
        "network",
        "--accept",
        "00000000-0000-7000-9000-0000000000fb",
        "--reason",
        "it finished",
        "-o",
        "json",
    ]);
    assert_eq!(accepted.exit, 1, "{}", accepted.stderr);
    assert_eq!(accepted.report["errors"][0]["code"], "NYL-RECOVER-KEY-MISMATCH");
    assert!(
        accepted.report["errors"][0]["fix"]
            .as_str()
            .unwrap()
            .contains("nyl resume -e dev --unit network"),
        "{}",
        accepted.stderr
    );
}
