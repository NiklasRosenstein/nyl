//! `nyl verify -e <env>`: driver verification against current receipts.
//!
//! Contract: [Commands](../../../../design/orchestration-core.md#commands),
//! [Runs and leases](../../../../design/orchestration-core.md#runs-and-leases)
//! (`verify` takes the lease, its deadline set by the timeouts of the units
//! it verifies), [Drivers](../../../../design/orchestration-core.md#drivers),
//! and [Inspection](../../../../design/orchestration-core.md#inspection).
//!
//! Reads one snapshot, picks the units with a current receipt
//! ([`crate::rules::verification::targets`]), checks each out at its
//! effective commit in a concurrency slot ([`super::run::slots`]), calls the
//! driver's `verify`, and writes one observed commit (operation `verify`)
//! with the latest observation per type and the driver's attestations; it
//! never writes a receipt and writes nothing when nothing changed. The
//! report is a `VerifyReport`, rendered like a reconcile's.
//!
//! The lease it holds lists no units: verify has no effects, so a run
//! taking over from a dead verify has nothing to mark uncertain. It writes
//! no run ref for the same reason. Its deadline covers the units it
//! verifies: before each batch, verify moves the lease's deadline to the
//! batch's longest unit timeout from now (plus the grace period), as a
//! reconcile does for each wave, and no unit's deadline passes the lease's
//! less its grace period. A verify that passes its unit's deadline fails
//! with a `timeout` error and records nothing for the unit.
//!
//! Once its cancellation is set (SIGINT or SIGTERM), verify starts no
//! further batch, its running tools receive the signal through the process
//! runner, and it releases the lease without writing an observed commit
//! (`NYL-RUN-CANCELLED`, exit 4).
//!
//! A driver sees the project directory reconcile gave it for the same
//! commit: the project located at the unit's source revision S through the
//! [`SourceRenderer`](nyl_core::orchestration::ports::SourceRenderer)
//! (entry location, then `[project] previous_paths`), not the entry
//! checkout's directory, so a project moved since the unit executed still
//! verifies where it reconciled.

use std::collections::BTreeMap;
use std::path::Path;

use nyl_core::orchestration::codes::{Code, Finding};
use nyl_core::orchestration::driver::{DriverOperation, ExecutionContext, Supported};
use nyl_core::orchestration::ids::{CommitId, EnvironmentName, UnitAddress, UnitName};
use nyl_core::orchestration::ports::{EnvironmentSnapshot, TransitionChanges};
use nyl_core::orchestration::state::{Operation, StateLocation};
use nyl_core::orchestration::time::{Duration, Timestamp};

use super::lease::{self, HeldLease};
use super::run::slots::Slots;
use super::run::{
    admitted_env, fresh_directory, join, longest_timeout, off_runtime, project_path, unit_timeout, RunContext,
    RunRequest,
};
use super::{store_error, Settings};
use crate::rules::sources::effective_commit;
use crate::rules::unit_state::receipt_state;
use crate::rules::verification::{records, targets, warnings, Records, Target};
use crate::run_report::{
    headline, EnvironmentRunReport, ReportKind, RunReport, UnitReport, UnitVerification, VerifyResult,
};
use crate::transition::{Trailers, TransitionSummary};

/// The verdicts a verify commit's headline counts, in order.
const HEADLINE: [&str; 5] = ["clean", "drifted", "unknown", "unsupported", "no-receipt"];

/// `nyl verify -e <env>`; `request.units` limits the units verified.
pub fn verify(context: &RunContext<'_>, request: &RunRequest) -> RunReport {
    // Drivers may block on runtimes of their own: run outside any caller's.
    let environment = off_runtime(|| verify_environment(context, request));
    let mut report = RunReport::new(ReportKind::VerifyReport);
    report.environments.push(environment);
    report
}

/// What verifying one unit found.
struct Verified {
    name: UnitName,
    /// The documents to store, `None` when the driver reported nothing to record.
    records: Option<Records>,
    verification: UnitVerification,
    message: Option<String>,
    error: Option<Finding>,
}

fn verify_environment(context: &RunContext<'_>, request: &RunRequest) -> EnvironmentRunReport {
    let base = context.base;
    let environment = &request.environment;
    let location = &request.location;
    let mut report = EnvironmentRunReport::new(environment.clone());
    let finish = |mut report: EnvironmentRunReport| {
        report.finish(ReportKind::VerifyReport);
        report
    };

    let snapshot = match read(context, environment, location, request) {
        Ok(snapshot) => snapshot,
        Err(finding) => {
            report.errors.push(finding);
            return finish(report);
        }
    };
    let planned = targets(&snapshot, request.units.as_ref());
    let verified: Vec<&UnitName> = planned
        .iter()
        .filter(|(_, target)| **target == Target::Verify)
        .map(|(name, _)| name)
        .collect();
    if verified.is_empty() {
        // Nothing to verify writes nothing, so it needs no lease.
        report_units(&mut report, &snapshot, &planned, Vec::new());
        return finish(report);
    }

    let run = base.ids.run_id();
    report.run = Some(run.clone());
    let longest = longest_timeout(verified.iter().map(|name| &snapshot.desired[*name]));
    let mut held = match lease::acquire(
        base,
        location,
        environment,
        &run,
        Operation::Verify,
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

    if let Err(finding) = holding(context, request, &mut held, &mut report) {
        report.errors.push(finding);
    }
    if let Err(error) = lease::release(base, &held, true) {
        report.warnings.push(store_error(environment, &error));
    }
    finish(report)
}

/// Everything verify does while it holds the lease: read the snapshot
/// again (another run may have changed state since), verify, and write the
/// changed documents in one fenced commit.
fn holding(
    context: &RunContext<'_>,
    request: &RunRequest,
    held: &mut HeldLease,
    report: &mut EnvironmentRunReport,
) -> Result<(), Finding> {
    let base = context.base;
    let environment = &request.environment;
    let location = &request.location;
    super::local::check_remote_tips(base.store, location, environment)?;
    let snapshot = read(context, environment, location, request)?;
    let planned = targets(&snapshot, request.units.as_ref());
    let results = verify_units(context, request, held, &snapshot, &planned)?;

    let mut changes = TransitionChanges::default();
    for result in &results {
        let Some(records) = &result.records else {
            continue;
        };
        let observations = (!records.observations.is_empty()).then(|| records.observations.clone());
        if snapshot.observations.get(&result.name) != observations.as_ref() {
            changes.observations.insert(result.name.clone(), observations);
        }
        let attestations = (!records.attestations.is_empty()).then(|| records.attestations.clone());
        if snapshot.attestations.get(&result.name) != attestations.as_ref() {
            changes.attestations.insert(result.name.clone(), attestations);
        }
    }
    report_units(report, &snapshot, &planned, results);
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
        summary(report),
        changes,
        &trailers,
    )
    .map_err(|error| store_error(environment, &error))?;
    report.commits.extend(commits);
    Ok(())
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

/// Verify every `Verify` target, in batches of the concurrency limit, each
/// unit of a batch on its own thread and slot. Before each batch the
/// lease's deadline moves to cover the batch's longest unit timeout; a lease
/// lost meanwhile ends verify with `NYL-LEASE-LOST`.
fn verify_units(
    context: &RunContext<'_>,
    request: &RunRequest,
    held: &mut HeldLease,
    snapshot: &EnvironmentSnapshot,
    planned: &BTreeMap<UnitName, Target>,
) -> Result<Vec<Verified>, Finding> {
    let directory = fresh_directory(context.workspace, "verify");
    let slots = Slots::new(&directory);
    let names: Vec<&UnitName> = planned
        .iter()
        .filter(|(_, target)| **target == Target::Verify)
        .map(|(name, _)| name)
        .collect();
    let projects = project_dirs(context, request, snapshot, &names, &slots);
    let mut results = Vec::new();
    let mut outcome = Ok(());
    for batch in names.chunks(request.concurrency.max(1)) {
        if let Some(signal) = request.cancellation.signal() {
            outcome = Err(cancelled(&request.environment, signal));
            break;
        }
        let longest = longest_timeout(batch.iter().map(|name| &snapshot.desired[*name]));
        // Move the lease's deadline to cover the batch about to start.
        let deadline = lease::deadline(context.base, context.base.clock.now(), longest);
        if let Err(error) = lease::update(context.base, held, held.lease.units.clone(), deadline) {
            outcome = Err(store_error(&request.environment, &error));
            break;
        }
        let lease_deadline = held.lease.deadline;
        let batch_results: Vec<Verified> = std::thread::scope(|scope| {
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
                        verify_unit(context, request, snapshot, &job, slots, directory)
                    })
                })
                .collect();
            handles.into_iter().map(join).collect()
        });
        results.extend(batch_results);
    }
    if let (Ok(()), Some(signal)) = (&outcome, request.cancellation.signal()) {
        outcome = Err(cancelled(&request.environment, signal));
    }
    let _ = std::fs::remove_dir_all(&directory);
    outcome.map(|()| results)
}

/// `NYL-RUN-CANCELLED` for a verify cancelled by `signal`
/// ([Runs and leases](../../../../design/orchestration-core.md#runs-and-leases),
/// item 7): it starts no further batch, its running tools received the
/// signal, and it releases the lease without writing an observed commit.
/// Verify has no effects, so nothing is left to import.
fn cancelled(environment: &EnvironmentName, signal: i32) -> Finding {
    Finding::new(
        Code::RunCancelled,
        format!("verify of {environment} was cancelled by signal {signal}; it recorded no observation"),
        format!("run `nyl verify -e {environment}` again"),
    )
}

/// A unit's deadline: its timeout from `now`, but never past the lease's
/// deadline less the grace period, so no verify runs once another runner
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
        // Only rendered documents have receipts to verify; a held marker has none.
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

/// A unit whose verification could not run or did not finish: an error
/// finding (`NYL-DRIVER-ERROR`) and nothing to record.
fn driver_failure(name: &UnitName, message: String, fix: &str) -> Verified {
    Verified {
        name: name.clone(),
        records: None,
        verification: VerifyResult::Unknown {
            reason: format!("driver error: {message}"),
        }
        .into(),
        error: Some(Finding::new(Code::DriverError, format!("unit {name}: {message}"), fix)),
        message: Some(message),
    }
}

#[allow(clippy::too_many_lines)] // One unit's verification in order: locate, check out, verify, record.
fn verify_unit(
    context: &RunContext<'_>,
    request: &RunRequest,
    snapshot: &EnvironmentSnapshot,
    job: &UnitJob<'_>,
    slots: &Slots,
    directory: &Path,
) -> Verified {
    let name = job.name;
    let environment = &request.environment;
    let unit = &snapshot.desired[name];
    let observed = snapshot.observed.get(name);
    let receipt = observed
        .and_then(|observed| observed.receipt.as_ref())
        .expect("a verified unit has a current receipt");
    let failed = |message: String| {
        driver_failure(
            name,
            message,
            "fix the cause, such as a missing tool or an unreadable worktree, then run nyl verify again",
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
    let worktree = match std::fs::create_dir_all(&temp_dir)
        .map_err(|error| format!("cannot create {}: {error}", temp_dir.display()))
        .and_then(|()| slots.checkout(context.sources, job.slot, &commit))
    {
        Ok(worktree) => worktree,
        Err(message) => return failed(message),
    };
    let now = context.base.clock.now();
    let timeout = unit_timeout(unit);
    let deadline = unit_deadline(&context.base.settings, now, timeout, job.lease_deadline);
    let ctx = ExecutionContext {
        unit: UnitAddress::new(environment.clone(), name.clone()),
        operation: DriverOperation::Verify,
        project_dir: project_path(&worktree, project_dir),
        worktree,
        temp_dir: temp_dir.clone(),
        env: admitted_env(context.process_env, &unit.spec),
        approved_digest: None,
        deadline,
        now,
        observed,
        tools: context.tools,
        commit_identity: context.commit_identity,
        cache_dir: context.cache_dir(),
        teardown_wait: None,
    };
    let outcome = driver.verify(&ctx, unit, receipt);
    drop(ctx);
    let _ = std::fs::remove_dir_all(&temp_dir);
    // A verify that passed its deadline failed with a `timeout`, whatever
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
            format!("timeout: verify passed {limit}"),
            "raise the unit's `timeout` or speed up its verification, then run nyl verify again",
        );
    }
    match outcome {
        Ok(Supported::Supported(verification)) => {
            let records = records(environment, unit, receipt, &verification, now).settle(
                snapshot.observations.get(name),
                snapshot.attestations.get(name),
                receipt,
            );
            let verification = records.verification();
            let message = match &verification.result {
                VerifyResult::Drifted { summary } => Some(summary.clone()),
                VerifyResult::Unknown { reason } => Some(reason.clone()),
                _ => None,
            };
            Verified {
                name: name.clone(),
                records: Some(records),
                verification,
                message,
                error: None,
            }
        }
        Ok(Supported::Unsupported) => Verified {
            name: name.clone(),
            records: None,
            verification: VerifyResult::Unsupported.into(),
            message: Some(format!("kind {} cannot verify", unit.unit.kind)),
            error: None,
        },
        Err(error) => failed(error.to_string()),
    }
}

/// One unit report per planned unit, with its verification and warnings.
fn report_units(
    report: &mut EnvironmentRunReport,
    snapshot: &EnvironmentSnapshot,
    planned: &BTreeMap<UnitName, Target>,
    results: Vec<Verified>,
) {
    let mut results: BTreeMap<UnitName, Verified> = results
        .into_iter()
        .map(|result| (result.name.clone(), result))
        .collect();
    for name in planned.keys() {
        let desired = &snapshot.desired[name];
        let observed = snapshot.observed.get(name);
        let mut entry = UnitReport::new(desired.unit.kind.clone());
        entry.receipt = Some(receipt_state(Some(desired), observed));
        entry.last_run = observed
            .and_then(|observed| observed.receipt.as_ref())
            .map(|receipt| receipt.run.clone());
        let verification = match results.remove(name) {
            Some(result) => {
                entry.message = result.message;
                report.errors.extend(result.error);
                result.verification
            }
            None => VerifyResult::NoReceipt.into(),
        };
        entry.warnings = warnings(&report.name, name, &verification);
        entry.verification = Some(verification);
        report.units.insert(name.clone(), entry);
    }
}

/// The transition summary of a verify: each unit's verification as the
/// report holds it, such as `{"verification":"drifted","summary":"…"}`
/// with its attestation results, and a headline such as `2 clean, 1 drifted`.
fn summary(report: &EnvironmentRunReport) -> TransitionSummary {
    let verifications = || {
        report
            .units
            .iter()
            .filter_map(|(name, unit)| Some((name, unit.verification.as_ref()?)))
    };
    let headline = headline(
        verifications().map(|(_, verification)| verification.result.name()),
        &HEADLINE,
    );
    let mut summary = super::state::summary(Operation::Verify, &report.name, &headline);
    summary.units = verifications()
        .map(|(name, verification)| {
            (
                name.clone(),
                serde_json::to_value(verification).expect("verifications serialize"),
            )
        })
        .collect();
    summary
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unit verifies within its own timeout, and never past the lease's
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
