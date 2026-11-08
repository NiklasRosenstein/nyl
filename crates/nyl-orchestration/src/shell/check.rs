//! `nyl detect-drift -e <env>`, `nyl attest -e <env>`, and `nyl observe -e
//! <env>`: checks of current receipts by their drivers.
//!
//! Contract: [Drift](../../../../design/orchestration-core.md#drift),
//! [Attestations](../../../../design/orchestration-core.md#attestations),
//! [Observed values](../../../../design/orchestration-core.md#observed-values),
//! [Commands](../../../../design/orchestration-core.md#commands),
//! [Runs and leases](../../../../design/orchestration-core.md#runs-and-leases)
//! (a check takes the lease, its deadline set by the timeouts of the units
//! it checks), [Drivers](../../../../design/orchestration-core.md#drivers),
//! and [Inspection](../../../../design/orchestration-core.md#inspection).
//!
//! Reads one snapshot, picks the units the check covers
//! ([`crate::rules::checks::targets`]), checks each out at its effective
//! commit in a concurrency slot ([`super::run::slots`]), calls the driver's
//! `detect_drift`, `attest`, or `observe`, and writes one observed commit
//! (operation `detect-drift`, `attest`, or `observe`) with the drift records,
//! the attestations, or the observed values; it never writes a receipt and
//! writes nothing when nothing changed. The report is a `DetectDriftReport`,
//! an `AttestReport`, or an `ObserveReport`, rendered like a reconcile's.
//! `observe` also names, with `NYL-OBSERVATION-CHANGED`, the consumers whose
//! stored resolution of an observed value no longer equals the recorded one;
//! they are work for the next reconcile. They include consumers in other
//! environments that read the value through a cross-environment reference:
//! their snapshots are read without a lease and never written.
//!
//! The lease it holds lists no units: a check has no effects, so a run
//! taking over from a dead check has nothing to mark uncertain. It writes
//! no run ref for the same reason. Its deadline covers the units it
//! checks: before each batch, the check moves the lease's deadline to the
//! batch's longest unit timeout from now (plus the grace period), as a
//! reconcile does for each wave, and no unit's deadline passes the lease's
//! less its grace period. A check that passes its unit's deadline fails
//! with a `timeout` error and records nothing for the unit.
//!
//! Once its cancellation is set (SIGINT or SIGTERM), a check starts no
//! further batch, its running tools receive the signal through the process
//! runner, and it releases the lease without writing an observed commit
//! (`NYL-RUN-CANCELLED`, exit 4).
//!
//! A driver sees the project directory reconcile gave it for the same
//! commit: the project located at the unit's source revision S through the
//! [`SourceRenderer`](nyl_core::orchestration::ports::SourceRenderer)
//! (entry location, then `[project] previous_paths`), not the entry
//! checkout's directory, so a project moved since the unit executed is
//! still checked where it reconciled.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use nyl_core::orchestration::codes::{Code, Finding};
use nyl_core::orchestration::driver::{
    AttestationCheck, DriftDetection, DriverSet, ExecutionContext, ObservedValues, Supported,
};
use nyl_core::orchestration::ids::{CommitId, EnvironmentName, UnitAddress, UnitName};
use nyl_core::orchestration::ports::{EnvironmentSnapshot, TransitionChanges};
use nyl_core::orchestration::state::{Attestation, DriftRecord, ObservationRecord, StateLocation};
use nyl_core::orchestration::time::{Duration, Timestamp};

use super::lease::{self, HeldLease};
use super::run::slots::Slots;
use super::run::{
    execution_credentials, fresh_directory, join, longest_timeout, off_runtime, project_path, unit_timeout, RunContext,
    RunRequest,
};
use super::{store_error, Settings};
use crate::rules::checks::{
    attest_check, attestations, drift_check, drift_record, observe_check, settle_attestations, settle_drift, targets,
    warnings, Check, Target,
};
use crate::rules::drift::stored_drift_repair;
use crate::rules::observed_values::{self, SettleInputs};
use crate::rules::selection::execution_recovery;
use crate::rules::sources::effective_commit;
use crate::rules::unit_state::receipt_state;
use crate::run_report::{headline, CheckResult, EnvironmentRunReport, RunReport, UnitCheck, UnitReport};
use crate::transition::{Trailers, TransitionSummary};

/// `nyl detect-drift -e <env>`; `request.units` limits the units checked.
pub fn detect_drift(context: &RunContext<'_>, request: &RunRequest) -> RunReport {
    check(context, request, Check::DetectDrift)
}

/// `nyl attest -e <env>`; `request.units` limits the units attested.
pub fn attest(context: &RunContext<'_>, request: &RunRequest) -> RunReport {
    check(context, request, Check::Attest)
}

/// `nyl observe -e <env>`; `request.units` limits the units observed.
pub fn observe(context: &RunContext<'_>, request: &RunRequest) -> RunReport {
    check(context, request, Check::Observe)
}

fn check(context: &RunContext<'_>, request: &RunRequest, check: Check) -> RunReport {
    // Drivers may block on runtimes of their own: run outside any caller's.
    let environment = off_runtime(|| check_environment(context, request, check));
    let mut report = RunReport::new(check.report_kind());
    report.environments.push(environment);
    report
}

/// The documents one unit's check records.
#[derive(Debug, Clone, PartialEq)]
enum Records {
    Drift(DriftRecord),
    Attestations(BTreeMap<String, Attestation>),
    /// The unit's observed values after the read, and the observations whose
    /// recorded value changed.
    Observations {
        record: Option<ObservationRecord>,
        changed: BTreeSet<String>,
    },
}

/// What checking one unit found.
struct Checked {
    name: UnitName,
    /// The documents to store, `None` when the driver reported nothing to record.
    records: Option<Records>,
    result: UnitCheck,
    message: Option<String>,
    error: Option<Finding>,
}

fn check_environment(context: &RunContext<'_>, request: &RunRequest, check: Check) -> EnvironmentRunReport {
    let base = context.base;
    let environment = &request.environment;
    let location = &request.location;
    let mut report = EnvironmentRunReport::new(environment.clone());
    let finish = |mut report: EnvironmentRunReport| {
        report.finish(check.report_kind());
        report
    };

    let snapshot = match read(context, environment, location, request) {
        Ok(snapshot) => snapshot,
        Err(finding) => {
            report.errors.push(finding);
            return finish(report);
        }
    };
    let planned = targets(check, &snapshot, request.units.as_ref(), context.drivers);
    let checked: Vec<&UnitName> = planned
        .iter()
        .filter(|(_, target)| **target == Target::Check)
        .map(|(name, _)| name)
        .collect();
    if checked.is_empty() {
        // Nothing to check writes nothing, so it needs no lease.
        report_units(check, &mut report, &snapshot, &planned, Vec::new(), context.drivers);
        return finish(report);
    }

    let run = base.ids.run_id();
    let longest = longest_timeout(context.drivers, checked.iter().map(|name| &snapshot.desired[*name]));
    let mut held = match lease::acquire(
        base,
        location,
        environment,
        &run,
        check.operation(),
        crate::rules::lease::BrokenLease::Refuse,
        longest,
        request.wait,
        &request.cancellation,
    ) {
        Ok(held) => held,
        Err(finding) => {
            report.errors.push(finding);
            return finish(report);
        }
    };
    // A run exists only once it holds the lease: a refused one reports none.
    report.run = Some(run.clone());

    if let Err(finding) = holding(context, request, check, &mut held, &mut report) {
        report.errors.push(finding);
    }
    if let Err(error) = lease::release(base, &held, true) {
        report.warnings.push(store_error(environment, &error));
    }
    finish(report)
}

/// Everything a check does while it holds the lease: read the snapshot
/// again (another run may have changed state since), check, and write the
/// changed documents in one fenced commit.
fn holding(
    context: &RunContext<'_>,
    request: &RunRequest,
    check: Check,
    held: &mut HeldLease,
    report: &mut EnvironmentRunReport,
) -> Result<(), Finding> {
    let base = context.base;
    let environment = &request.environment;
    let location = &request.location;
    super::local::check_remote_tips(base.store, location, environment)?;
    let snapshot = read(context, environment, location, request)?;
    let planned = targets(check, &snapshot, request.units.as_ref(), context.drivers);
    let results = check_units(context, request, check, held, &snapshot, &planned)?;

    let mut changes = TransitionChanges::default();
    for result in &results {
        match &result.records {
            None => {}
            Some(Records::Drift(record)) => {
                if snapshot.drift.get(&result.name) != Some(record) {
                    changes.drift.insert(result.name.clone(), Some(record.clone()));
                }
            }
            Some(Records::Attestations(set)) => {
                let set = (!set.is_empty()).then(|| set.clone());
                if snapshot.attestations.get(&result.name) != set.as_ref() {
                    changes.attestations.insert(result.name.clone(), set);
                }
            }
            Some(Records::Observations { record, changed }) => {
                if snapshot.observations.get(&result.name) != record.as_ref() {
                    changes.observations.insert(result.name.clone(), record.clone());
                }
                if !changed.is_empty() {
                    report
                        .observations_changed
                        .insert(result.name.clone(), changed.iter().cloned().collect());
                }
            }
        }
    }
    if check == Check::Observe {
        report.warnings.extend(changed_consumer_warnings(
            context,
            request,
            &snapshot,
            &changes.observations,
        ));
    }
    report_units(check, report, &snapshot, &planned, results, context.drivers);
    if changes.is_empty() {
        return Ok(());
    }
    let trailers = Trailers {
        local: request.local.is_some(),
        ..super::trailers(base, held, &snapshot.tips)
    };
    let commits = super::commit_transition(
        base,
        location,
        held,
        &snapshot.tips,
        summary(check, report),
        changes,
        &trailers,
    )
    .map_err(|error| store_error(environment, &error))?;
    report.commits.extend(commits);
    Ok(())
}

/// `NYL-OBSERVATION-CHANGED` for each consumer whose stored resolution of an
/// observed value differs from what the snapshot, with `written` applied,
/// records: its inputs changed, so it is ready for the next reconcile.
///
/// The consumers are this environment's, and those of every other
/// environment the request names that read this environment's observed
/// values through a cross-environment reference. The other environments'
/// snapshots are read like a reconcile reads its producers' (no lease, no
/// write); one that cannot be read is a warning, never a failed check.
fn changed_consumer_warnings(
    context: &RunContext<'_>,
    request: &RunRequest,
    snapshot: &EnvironmentSnapshot,
    written: &BTreeMap<UnitName, Option<ObservationRecord>>,
) -> Vec<Finding> {
    let environment = &request.environment;
    let now = context.base.clock.now();
    let mut records = snapshot.observations.clone();
    for (unit, record) in written {
        match record {
            Some(record) => records.insert(unit.clone(), record.clone()),
            None => records.remove(unit),
        };
    }
    let changed = observed_values::changed_consumers(&snapshot.desired, &snapshot.observed, &records, now);
    let consumers: BTreeSet<&UnitName> = changed.iter().map(|changed| &changed.consumer).collect();
    let mut warnings: Vec<Finding> = consumers
        .into_iter()
        .flat_map(|consumer| {
            let reasons: Vec<_> = changed.iter().filter(|changed| &changed.consumer == consumer).collect();
            observed_values::changed_warnings(environment, consumer, &reasons)
        })
        .collect();

    let mut others = BTreeMap::new();
    for (name, location) in &request.environments {
        if name == environment {
            continue;
        }
        match context.base.store.snapshot(location) {
            Ok(other) => {
                others.insert(name.clone(), other);
            }
            Err(error) => warnings.push(store_error(name, &error)),
        }
    }
    let view = observed_values::ProducerView {
        desired: &snapshot.desired,
        observed: &snapshot.observed,
        records: &records,
    };
    warnings.extend(
        observed_values::changed_consumers_reading(environment, view, &others, now)
            .iter()
            .map(observed_values::external_changed_warning),
    );
    warnings
}

/// The snapshot, refused like a reconcile's when state is missing or
/// belongs elsewhere, and with every `--unit` name known.
fn read(
    context: &RunContext<'_>,
    environment: &EnvironmentName,
    location: &StateLocation,
    request: &RunRequest,
) -> Result<EnvironmentSnapshot, Finding> {
    let snapshot = context
        .base
        .store
        .snapshot(location)
        .map_err(|error| store_error(environment, &error))?;
    super::local::check_record(context.base.store, snapshot.record.as_ref(), environment, location)?;
    if let Some(unknown) = request
        .units
        .iter()
        .flatten()
        .find(|unit| !snapshot.desired.contains_key(*unit))
    {
        return Err(Finding::new(
            Code::UnitUnknown,
            format!("--unit {unknown} names no unit in the state of {environment}"),
            format!("list the environment's units with `nyl get units -e {environment}`"),
        ));
    }
    Ok(snapshot)
}

/// Check every `Check` target, in batches of the concurrency limit, each
/// unit of a batch on its own thread and slot. Before each batch the
/// lease's deadline moves to cover the batch's longest unit timeout; a lease
/// lost meanwhile ends the check with `NYL-LEASE-LOST`.
fn check_units(
    context: &RunContext<'_>,
    request: &RunRequest,
    check: Check,
    held: &mut HeldLease,
    snapshot: &EnvironmentSnapshot,
    planned: &BTreeMap<UnitName, Target>,
) -> Result<Vec<Checked>, Finding> {
    let directory = fresh_directory(context.workspace, check.command());
    let slots = Slots::new(&directory);
    let names: Vec<&UnitName> = planned
        .iter()
        .filter(|(_, target)| **target == Target::Check)
        .map(|(name, _)| name)
        .collect();
    let projects = project_dirs(context, request, snapshot, &names, &slots);
    let mut results = Vec::new();
    let mut outcome = Ok(());
    for batch in names.chunks(request.concurrency.max(1)) {
        if let Some(signal) = request.cancellation.signal() {
            outcome = Err(cancelled(check, &request.environment, signal));
            break;
        }
        let longest = longest_timeout(context.drivers, batch.iter().map(|name| &snapshot.desired[*name]));
        // Move the lease's deadline to cover the batch about to start.
        let deadline = lease::deadline(context.base, context.base.clock.now(), longest);
        if let Err(error) = lease::update(context.base, held, held.lease.units.clone(), deadline) {
            outcome = Err(store_error(&request.environment, &error));
            break;
        }
        let lease_deadline = held.lease.deadline;
        let batch_results: Vec<Checked> = std::thread::scope(|scope| {
            let handles: Vec<_> = batch
                .iter()
                .enumerate()
                .map(|(slot, name)| {
                    let slots = &slots;
                    let directory = &directory;
                    let project_dir = &projects[*name];
                    scope.spawn(move || {
                        let job = UnitJob {
                            name,
                            slot,
                            project_dir,
                            lease_deadline,
                        };
                        check_unit(context, request, check, snapshot, &job, slots, directory)
                    })
                })
                .collect();
            handles.into_iter().map(join).collect()
        });
        results.extend(batch_results);
    }
    if let (Ok(()), Some(signal)) = (&outcome, request.cancellation.signal()) {
        outcome = Err(cancelled(check, &request.environment, signal));
    }
    let _ = std::fs::remove_dir_all(&directory);
    outcome.map(|()| results)
}

/// `NYL-RUN-CANCELLED` for a check cancelled by `signal`
/// ([Runs and leases](../../../../design/orchestration-core.md#runs-and-leases),
/// item 7): it starts no further batch, its running tools received the
/// signal, and it releases the lease without writing an observed commit.
/// A check has no effects, so nothing is left to import.
fn cancelled(check: Check, environment: &EnvironmentName, signal: i32) -> Finding {
    let command = check.command();
    Finding::new(
        Code::RunCancelled,
        format!("{command} of {environment} was cancelled by signal {signal}; it recorded nothing"),
        format!("run `nyl {command} -e {environment}` again"),
    )
}

/// A unit's deadline: its timeout from `now`, but never past the lease's
/// deadline less the grace period, so no check runs once another runner
/// may take the lease over.
fn unit_deadline(settings: &Settings, now: Timestamp, timeout: Duration, lease: Timestamp) -> Timestamp {
    timeout.after(now).min(lease::bound(settings, lease))
}

/// The worktree-rooted project directory at each target's source revision
/// S, as reconcile located it: the source renderer finds the project at the
/// entry location or a `[project] previous_paths` entry. A revision whose
/// project cannot be located maps to the reason.
fn project_dirs(
    context: &RunContext<'_>,
    request: &RunRequest,
    snapshot: &EnvironmentSnapshot,
    names: &[&UnitName],
    slots: &Slots,
) -> BTreeMap<UnitName, Result<String, String>> {
    let mut located: BTreeMap<CommitId, Result<String, String>> = BTreeMap::new();
    let mut projects = BTreeMap::new();
    for name in names {
        // Only rendered documents have receipts to check; a held marker has none.
        let Some(revision) = &snapshot.desired[*name].source_revision else {
            projects.insert((*name).clone(), Err(format!("{name} has never been rendered")));
            continue;
        };
        let project = located
            .entry(revision.clone())
            .or_insert_with(|| {
                let worktree = slots.checkout(context.sources, 0, revision)?;
                context
                    .renderer
                    .render(&worktree, &request.environment, revision)
                    .map(|rendered| rendered.project_dir)
                    .map_err(|error| format!("cannot locate the project at {revision}: {error}"))
            })
            .clone();
        projects.insert((*name).clone(), project);
    }
    projects
}

/// One unit of a batch: its slot, its project directory, and the lease
/// deadline its own deadline stays within.
struct UnitJob<'a> {
    name: &'a UnitName,
    slot: usize,
    project_dir: &'a Result<String, String>,
    lease_deadline: Timestamp,
}

/// A unit whose check could not run or did not finish: an error finding
/// (`NYL-DRIVER-ERROR`) and nothing to record.
fn driver_failure(name: &UnitName, message: String, fix: &str) -> Checked {
    Checked {
        name: name.clone(),
        records: None,
        result: CheckResult::Unknown {
            reason: format!("driver error: {message}"),
        }
        .into(),
        error: Some(Finding::new(Code::DriverError, format!("unit {name}: {message}"), fix)),
        message: Some(message),
    }
}

#[allow(clippy::too_many_lines)] // One unit's check in order: locate, check out, call the driver, record.
fn check_unit(
    context: &RunContext<'_>,
    request: &RunRequest,
    check: Check,
    snapshot: &EnvironmentSnapshot,
    job: &UnitJob<'_>,
    slots: &Slots,
    directory: &Path,
) -> Checked {
    let name = job.name;
    let environment = &request.environment;
    let unit = &snapshot.desired[name];
    let observed = snapshot.observed.get(name);
    let receipt = observed
        .and_then(|observed| observed.receipt.as_ref())
        .expect("a checked unit has a current receipt");
    let command = check.command();
    let failed = |message: String| {
        driver_failure(
            name,
            message,
            &format!("fix the cause, such as a missing tool or an unreadable worktree, then run nyl {command} again"),
        )
    };
    let project_dir = match job.project_dir {
        Ok(project_dir) => project_dir,
        Err(message) => return failed(message.clone()),
    };
    let Some(driver) = context.drivers.get(&unit.unit.api_version, &unit.unit.kind) else {
        return failed(format!("no driver serves kind {}", unit.unit.kind));
    };
    let spec = unit.resolved_spec.as_ref().unwrap_or(&unit.spec);
    let Some(revision) = &unit.source_revision else {
        return failed(format!("{name} has never been rendered"));
    };
    let commit = match effective_commit(spec, &driver.source_fields(spec), revision) {
        Ok(commit) => commit,
        Err(message) => return failed(message),
    };
    let temp_dir = directory.join(format!("tmp-{name}"));
    let (worktree, source_roots) = match std::fs::create_dir_all(&temp_dir)
        .map_err(|error| format!("cannot create {}: {error}", temp_dir.display()))
        .and_then(|()| {
            slots.checkout_unit(
                context.sources,
                job.slot,
                &commit,
                Some(revision),
                driver,
                spec,
                project_dir,
            )
        }) {
        Ok(roots) => roots,
        Err(message) => return failed(message),
    };
    let now = context.base.clock.now();
    let timeout = unit_timeout(context.drivers, unit);
    let deadline = unit_deadline(&context.base.settings, now, timeout, job.lease_deadline);
    let (env, secrets) = match execution_credentials(context, driver, &unit.spec) {
        Ok(env) => env,
        Err(message) => {
            let _ = std::fs::remove_dir_all(&temp_dir);
            return failed(message);
        }
    };
    let budget = crate::shell::tools::DeadlineRunner {
        tools: context.tools,
        clock: context.base.clock,
        deadline,
    };
    let ctx = ExecutionContext {
        instances: context.instances,
        clock: context.base.clock,
        secrets,
        interactive: false,
        artifacts: snapshot.artifacts.get(name).cloned().unwrap_or_default(),
        source_roots,
        unit: UnitAddress::new(environment.clone(), name.clone()),
        operation: check.driver_operation(),
        project_dir: project_path(&worktree, project_dir),
        worktree,
        temp_dir: temp_dir.clone(),
        env,
        approved_digest: None,
        deadline,
        now,
        observed,
        tools: &budget,
        commit_identity: context.commit_identity,
        cache_dir: context.cache_dir(),
        teardown_wait: None,
        incarnations: &nyl_core::orchestration::driver::UnknownIncarnations,
        dirty_source: false,
    };
    let outcome = match check {
        Check::DetectDrift => driver
            .detect_drift(&ctx, unit, receipt)
            .map(|supported| match supported {
                Supported::Supported(detection) => Supported::Supported(Found::Drift(detection)),
                Supported::Unsupported => Supported::Unsupported,
            }),
        Check::Attest => driver.attest(&ctx, unit, receipt).map(|supported| match supported {
            Supported::Supported(check) => Supported::Supported(Found::Attestations(check)),
            Supported::Unsupported => Supported::Unsupported,
        }),
        Check::Observe => driver.observe(&ctx, unit, receipt).map(|supported| match supported {
            Supported::Supported(readings) => Supported::Supported(Found::Observations(readings)),
            Supported::Unsupported => Supported::Unsupported,
        }),
    };
    drop(ctx);
    let _ = std::fs::remove_dir_all(&temp_dir);
    // A check that passed its deadline failed with a `timeout`, whatever
    // the driver made of it (the Command kind reports a terminated step as
    // unknown drift): its result is not evidence of the unit's state.
    if context.base.clock.now() >= deadline {
        let limit = if deadline < timeout.after(now) {
            "the lease's deadline".to_owned()
        } else {
            format!("its timeout of {timeout}")
        };
        return driver_failure(
            name,
            format!("timeout: {command} passed {limit}"),
            &format!("raise the unit's `timeout` or speed up its {command} step, then run nyl {command} again"),
        );
    }
    match outcome {
        Ok(Supported::Supported(Found::Drift(detection))) => {
            let record = settle_drift(drift_record(unit, receipt, &detection, now), snapshot.drift.get(name));
            let result = drift_check(&record.result);
            let message = match &result.result {
                CheckResult::Drifted { summary } => Some(summary.clone()),
                CheckResult::Unknown { reason } => Some(reason.clone()),
                _ => None,
            };
            Checked {
                name: name.clone(),
                records: Some(Records::Drift(record)),
                result,
                message,
                error: None,
            }
        }
        Ok(Supported::Supported(Found::Observations(readings))) => {
            let declared = match observed_values::declarations(spec) {
                Ok(declared) => declared,
                Err(message) => return failed(message),
            };
            let settled = observed_values::settle(&SettleInputs {
                unit,
                receipt,
                declared: &declared,
                stored: snapshot.observations.get(name),
                readings: &readings,
                now,
                refresh: context.base.settings.observation_refresh,
            });
            Checked {
                name: name.clone(),
                result: observe_check(&settled, &declared),
                records: Some(Records::Observations {
                    record: settled.record.clone(),
                    changed: settled.changed.clone(),
                }),
                message: None,
                error: None,
            }
        }
        Ok(Supported::Supported(Found::Attestations(AttestationCheck::Unknown { reason }))) => Checked {
            name: name.clone(),
            records: None,
            result: CheckResult::Unknown { reason: reason.clone() }.into(),
            message: Some(reason),
            error: None,
        },
        Ok(Supported::Supported(Found::Attestations(AttestationCheck::Attested { outcomes }))) => {
            let set = settle_attestations(
                attestations(environment, unit, receipt, &outcomes, "attest", now),
                snapshot.attestations.get(name),
                receipt,
                now,
                context.base.settings.attestation_refresh,
            );
            Checked {
                name: name.clone(),
                result: attest_check(&set),
                records: Some(Records::Attestations(set)),
                message: None,
                error: None,
            }
        }
        Ok(Supported::Unsupported) => Checked {
            name: name.clone(),
            records: None,
            result: CheckResult::Unsupported.into(),
            message: Some(match check {
                Check::DetectDrift => format!("kind {} cannot detect drift", unit.unit.kind),
                Check::Attest => format!("kind {} cannot attest", unit.unit.kind),
                Check::Observe => format!("kind {} cannot read observed values", unit.unit.kind),
            }),
            error: None,
        },
        Err(error) => failed(error.to_string()),
    }
}

/// What a driver's check returned.
enum Found {
    Drift(DriftDetection),
    Attestations(AttestationCheck),
    Observations(ObservedValues),
}

/// One unit report per planned unit, with its check result and warnings.
fn report_units(
    check: Check,
    report: &mut EnvironmentRunReport,
    snapshot: &EnvironmentSnapshot,
    planned: &BTreeMap<UnitName, Target>,
    results: Vec<Checked>,
    drivers: &DriverSet,
) {
    let drift_repair = stored_drift_repair(snapshot.environment.as_ref());
    let mut results: BTreeMap<UnitName, Checked> = results
        .into_iter()
        .map(|result| (result.name.clone(), result))
        .collect();
    for (name, target) in planned {
        let desired = &snapshot.desired[name];
        let observed = snapshot.observed.get(name);
        let mut entry = UnitReport::new(desired.unit.kind.clone());
        entry.receipt = Some(receipt_state(Some(desired), observed));
        entry.last_run = observed
            .and_then(|observed| observed.receipt.as_ref())
            .map(|receipt| receipt.run.clone());
        let result = match results.remove(name) {
            Some(result) => {
                entry.message = result.message;
                report.errors.extend(result.error);
                result.result
            }
            None if *target == Target::Unsupported => CheckResult::Unsupported.into(),
            None => CheckResult::NoReceipt.into(),
        };
        let policy = execution_recovery(drivers, desired);
        entry.warnings = warnings(check, &report.name, name, &result, policy, drift_repair);
        entry.check = Some(result);
        report.units.insert(name.clone(), entry);
    }
}

/// The transition summary of a check: each unit's result as the report
/// holds it, such as `{"check":"drifted","summary":"…"}` or
/// `{"check":"attested","attestations":{…}}`, and a headline such as
/// `2 clean, 1 drifted`.
fn summary(check: Check, report: &EnvironmentRunReport) -> TransitionSummary {
    let results = || {
        report
            .units
            .iter()
            .filter_map(|(name, unit)| Some((name, unit.check.as_ref()?)))
    };
    let headline = headline(results().map(|(_, result)| result.result.name()), check.headline());
    let mut summary = super::state::summary(check.operation(), &report.name, &headline);
    if !report.observations_changed.is_empty() {
        summary.details.insert(
            "observationsChanged".to_owned(),
            serde_json::json!(report.observations_changed),
        );
    }
    summary.units = results()
        .map(|(name, result)| {
            (
                name.clone(),
                serde_json::to_value(result).expect("check results serialize"),
            )
        })
        .collect();
    summary
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unit is checked within its own timeout, and never past the lease's
    /// deadline less the grace period, after which another runner may
    /// take the lease over.
    #[test]
    fn test_unit_deadline_stays_within_the_lease() {
        let settings = Settings::default();
        let now = chrono::DateTime::parse_from_rfc3339("2026-09-24T10:00:00Z")
            .unwrap()
            .to_utc();
        let covering = now + chrono::Duration::minutes(70);
        assert_eq!(
            unit_deadline(&settings, now, Duration::from_mins(60), covering),
            now + chrono::Duration::minutes(60)
        );
        let short = now + chrono::Duration::minutes(40);
        assert_eq!(
            unit_deadline(&settings, now, Duration::from_mins(60), short),
            now + chrono::Duration::minutes(30)
        );
    }
}
