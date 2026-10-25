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
//! no run ref for the same reason.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use nyl_core::orchestration::codes::{Code, Finding};
use nyl_core::orchestration::driver::{DriverOperation, ExecutionContext, Supported};
use nyl_core::orchestration::ids::{EnvironmentName, UnitAddress, UnitName};
use nyl_core::orchestration::ports::{EnvironmentSnapshot, StateError, Transition, TransitionChanges};
use nyl_core::orchestration::state::{Operation, StateLocation};

use super::lease::{self, HeldLease, LeaseError};
use super::run::slots::Slots;
use super::run::{admitted_env, project_path, unit_timeout, RunContext, RunRequest};
use super::state::store_error;
use crate::rules::sources::effective_commit;
use crate::rules::state_location::check_record;
use crate::rules::unit_state::receipt_state;
use crate::rules::verification::{records, targets, warnings, Records, Target};
use crate::run_report::{EnvironmentRunReport, ReportKind, RunReport, UnitReport, UnitVerification, VerifyResult};
use crate::transition::{self, Trailers, TransitionSummary};

/// The verdicts a verify commit's headline counts, in order.
const HEADLINE: [&str; 5] = ["clean", "drifted", "unknown", "unsupported", "no-receipt"];

/// `nyl verify -e <env>`; `request.units` limits the units verified.
pub fn verify(context: &RunContext<'_>, request: &RunRequest) -> RunReport {
    // Drivers may block on runtimes of their own: run outside any caller's.
    let environment = std::thread::scope(
        |scope| match scope.spawn(|| verify_environment(context, request)).join() {
            Ok(report) => report,
            Err(panic) => std::panic::resume_unwind(panic),
        },
    );
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
    let longest = verified
        .iter()
        .map(|name| unit_timeout(&snapshot.desired[*name]))
        .max()
        .unwrap_or_default();
    let mut held = match lease::acquire(
        base,
        location,
        environment,
        &run,
        Operation::Verify,
        longest,
        request.wait,
    ) {
        Ok(held) => held,
        Err(LeaseError::Held(finding)) => {
            report.errors.push(finding);
            return finish(report);
        }
        Err(LeaseError::Store(error)) => {
            report.errors.push(store_error(environment, &error));
            return finish(report);
        }
    };

    if let Err(finding) = holding(context, request, &mut held, &mut report) {
        report.errors.push(finding);
    }
    if let Err(error) = lease::release(base, &held) {
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
    super::local::check_remote_tips(base.store, location)?;
    let snapshot = read(context, environment, location, request)?;
    let planned = targets(&snapshot, request.units.as_ref());
    let results = verify_units(context, request, &snapshot, &planned);

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
    let summary = summary(report, held.broken.as_ref());
    let trailers = Trailers {
        run: held.lease.run.clone(),
        source_commit: None,
        read_desired: snapshot.tips.desired.clone(),
        read_observed: snapshot.tips.observed.clone(),
        runner: base.runner.clone(),
        evaluated_at: held.lease.taken_at,
        version: base.version.clone(),
        requested_by: None,
        reason: None,
        local: request.local.is_some(),
    };
    let transition = Transition {
        message: transition::message(&summary, &trailers),
        changes,
    };
    match base
        .store
        .commit(location, environment, &snapshot.tips, &transition, &held.version)
    {
        Ok(committed) => {
            held.version = committed.lease_version;
            if let Some(commit) = committed.desired {
                report.commits.insert("desired", commit);
            }
            if let Some(commit) = committed.observed {
                report.commits.insert("observed", commit);
            }
            Ok(())
        }
        Err(StateError::LeaseLost) => Err(lease::lost(environment)),
        Err(error) => Err(store_error(environment, &error)),
    }
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
    check_record(snapshot.record.as_ref(), environment, location)?;
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
/// unit of a batch on its own thread and slot.
fn verify_units(
    context: &RunContext<'_>,
    request: &RunRequest,
    snapshot: &EnvironmentSnapshot,
    planned: &BTreeMap<UnitName, Target>,
) -> Vec<Verified> {
    let directory = session_directory(context.workspace);
    let slots = Slots::new(&directory);
    let names: Vec<&UnitName> = planned
        .iter()
        .filter(|(_, target)| **target == Target::Verify)
        .map(|(name, _)| name)
        .collect();
    let mut results = Vec::new();
    for batch in names.chunks(request.concurrency.max(1)) {
        let batch_results: Vec<Verified> = std::thread::scope(|scope| {
            let handles: Vec<_> = batch
                .iter()
                .enumerate()
                .map(|(slot, name)| {
                    let slots = &slots;
                    let directory = &directory;
                    scope.spawn(move || verify_unit(context, request, snapshot, name, slots, slot, directory))
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| match handle.join() {
                    Ok(result) => result,
                    Err(panic) => std::panic::resume_unwind(panic),
                })
                .collect()
        });
        results.extend(batch_results);
    }
    let _ = std::fs::remove_dir_all(&directory);
    results
}

/// A directory of this verify's own beneath the workspace.
fn session_directory(workspace: &Path) -> PathBuf {
    let mut index = 0usize;
    loop {
        let candidate = workspace.join(format!("verify-{index}"));
        if !candidate.exists() {
            return candidate;
        }
        index += 1;
    }
}

fn verify_unit(
    context: &RunContext<'_>,
    request: &RunRequest,
    snapshot: &EnvironmentSnapshot,
    name: &UnitName,
    slots: &Slots,
    slot: usize,
    directory: &Path,
) -> Verified {
    let environment = &request.environment;
    let unit = &snapshot.desired[name];
    let observed = snapshot.observed.get(name);
    let receipt = observed
        .and_then(|observed| observed.receipt.as_ref())
        .expect("a verified unit has a current receipt");
    let failed = |message: String| Verified {
        name: name.clone(),
        records: None,
        verification: UnitVerification {
            result: VerifyResult::Unknown {
                reason: format!("driver error: {message}"),
            },
            attestations: BTreeMap::new(),
        },
        message: Some(message.clone()),
        error: Some(Finding::new(
            Code::DriverError,
            format!("unit {name}: {message}"),
            "fix the cause, such as a missing tool or an unreadable worktree, then run nyl verify again",
        )),
    };
    let Some(driver) = context.drivers.get(&unit.unit.api_version, &unit.unit.kind) else {
        return failed(format!("no driver serves kind {}", unit.unit.kind));
    };
    let spec = unit.resolved_spec.as_ref().unwrap_or(&unit.spec);
    let commit = match effective_commit(spec, &driver.source_fields(spec), &unit.source_revision) {
        Ok(commit) => commit,
        Err(message) => return failed(message),
    };
    let temp_dir = directory.join(format!("tmp-{name}"));
    let worktree = match std::fs::create_dir_all(&temp_dir)
        .map_err(|error| format!("cannot create {}: {error}", temp_dir.display()))
        .and_then(|()| slots.checkout(context.sources, slot, &commit))
    {
        Ok(worktree) => worktree,
        Err(message) => return failed(message),
    };
    let now = context.base.clock.now();
    let ctx = ExecutionContext {
        unit: UnitAddress::new(environment.clone(), name.clone()),
        operation: DriverOperation::Verify,
        project_dir: project_path(&worktree, &request.entry_project_dir),
        worktree,
        temp_dir: temp_dir.clone(),
        env: admitted_env(context.process_env, &unit.spec),
        approved_digest: None,
        deadline: unit_timeout(unit).after(now),
        now,
        observed,
        tools: context.tools,
        commit_identity: context.commit_identity,
        cache_dir: context.workspace.join("cache"),
    };
    let outcome = driver.verify(&ctx, unit, receipt);
    drop(ctx);
    let _ = std::fs::remove_dir_all(&temp_dir);
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
            verification: UnitVerification {
                result: VerifyResult::Unsupported,
                attestations: BTreeMap::new(),
            },
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
            None => UnitVerification {
                result: VerifyResult::NoReceipt,
                attestations: BTreeMap::new(),
            },
        };
        entry.warnings = warnings(&report.name, name, &verification);
        entry.verification = Some(verification);
        report.units.insert(name.clone(), entry);
    }
}

/// The transition summary of a verify: each unit's verification as the
/// report holds it, such as `{"verification":"drifted","summary":"…"}`
/// with its attestation results, and a headline such as `2 clean, 1 drifted`.
fn summary(
    report: &EnvironmentRunReport,
    broken: Option<&nyl_core::orchestration::state::Request>,
) -> TransitionSummary {
    let verdicts: BTreeMap<&UnitName, &'static str> = report
        .units
        .iter()
        .filter_map(|(name, unit)| {
            unit.verification
                .as_ref()
                .map(|verification| (name, verification.result.name()))
        })
        .collect();
    let headline = HEADLINE
        .iter()
        .filter_map(|counted| {
            let count = verdicts.values().filter(|verdict| *verdict == counted).count();
            (count > 0).then(|| format!("{count} {counted}"))
        })
        .collect::<Vec<_>>()
        .join(", ");
    TransitionSummary {
        operation: Operation::Verify,
        environment: report.name.clone(),
        headline,
        units: report
            .units
            .iter()
            .filter_map(|(name, unit)| {
                let verification = unit.verification.as_ref()?;
                Some((
                    name.clone(),
                    serde_json::to_value(verification).expect("verifications serialize"),
                ))
            })
            .collect(),
        recovered: Vec::new(),
        imported: Vec::new(),
        lease_broken: broken.cloned(),
        details: BTreeMap::new(),
    }
}
