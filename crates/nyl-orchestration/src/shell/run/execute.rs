//! Executing one wave: a thread per unit, each in its concurrency slot's
//! worktree, and the mapping of each driver outcome to what the run records.
//!
//! Contract: [Execution](../../../../../design/orchestration-core.md#execution),
//! [Recovery](../../../../../design/orchestration-core.md#recovery), and
//! [Runs and leases](../../../../../design/orchestration-core.md#runs-and-leases)
//! (checkpoints and cancellation).
//!
//! - Every outcome becomes the unit's observed document through
//!   [`transitions::apply_to`]: a success replaces the receipt, with a copy
//!   of the executed desired document, and clears any condition; a failure
//!   or uncertain outcome keeps the last receipt and its copy.
//! - A unit with an `uncertain` condition whose execution recovery policy is
//!   `inspect` is inspected first: applied records the receipt (outputs
//!   filtered like a success), not applied executes it, and unknown or
//!   unsupported leaves the condition for `nyl recover`.
//! - An execution that clears an `uncertain` condition reports `recovered`.
//! - Once the run is cancelled, a unit whose driver was not called yet is not
//!   started, and a unit whose driver returns anything but a success is
//!   `uncertain`: its tools were signalled while it ran.
//! - A `--local` run's approved unit records the `local` approval in its receipt.

#[allow(clippy::wildcard_imports)] // The run module's private helpers and imports.
use super::*;
use crate::rules::transitions::{self, Applied, Event};
use nyl_core::orchestration::driver::{Inspection, RecoveryPolicy};
use nyl_core::orchestration::state::{ApprovalRecord, FailureCategory};

/// One finished execution.
pub(super) struct Execution {
    pub(super) name: UnitName,
    pub(super) result: UnitResult,
    pub(super) message: Option<String>,
    pub(super) change_digest: Option<nyl_core::orchestration::ids::ChangeDigest>,
    pub(super) approval: Option<ApprovalRecord>,
    pub(super) warnings: Vec<Finding>,
    pub(super) errors: Vec<Finding>,
    /// What the run ref records; `None` when nothing is recorded, such as
    /// for a driver error before any effect.
    pub(super) checkpoint: Option<Checkpoint>,
    /// The new artifact set of a success.
    pub(super) artifacts: Option<BTreeMap<String, ArtifactDocument>>,
    /// A fault killed the run after the effect.
    pub(super) killed: bool,
}

/// Everything one execution thread needs, prepared before it starts.
struct Job<'j> {
    unit: &'j DesiredUnit,
    commit: &'j CommitId,
    observed: Option<&'j ObservedUnit>,
    previous_artifacts: BTreeMap<String, ArtifactDocument>,
    /// This execution's own directory, for its temp dir.
    directory: PathBuf,
    /// The concurrency slot whose worktree the execution uses: its index in
    /// the batch, which never exceeds the concurrency limit.
    slot: usize,
    /// The approval a `--local` run records for a unit `--approve` named.
    approval: Option<ApprovalRecord>,
}

#[allow(clippy::too_many_arguments)] // One wave's inputs: the run, its resolution, its evidence, and the batch.
pub(super) fn execute_batch(
    session: &Session<'_, '_>,
    prepared: &Prepared,
    resolved: &Resolved,
    observed: &BTreeMap<UnitName, ObservedUnit>,
    artifacts: &BTreeMap<UnitName, BTreeMap<String, ArtifactDocument>>,
    batch: &[UnitName],
    run: &RunId,
    snapshot: &EnvironmentSnapshot,
    approvals: &BTreeMap<UnitName, ApprovalRecord>,
) -> Vec<Execution> {
    let jobs: Vec<Job<'_>> = batch
        .iter()
        .enumerate()
        .map(|(slot, name)| Job {
            unit: &resolved.units[name],
            commit: &resolved.commits[name],
            observed: observed.get(name),
            previous_artifacts: artifacts.get(name).cloned().unwrap_or_default(),
            directory: session.execution_directory(name),
            slot,
            approval: approvals.get(name).cloned(),
        })
        .collect();
    let shared = Shared {
        context: session.context,
        slots: &session.slots,
        environment: session.environment(),
        project_dir: &prepared.rendered.project_dir,
        cache_dir: session.context.cache_dir(),
        run,
        read_desired: snapshot.tips.desired.as_ref(),
        cancellation: &session.request.cancellation,
        lease_deadline: session.lease_deadline.get(),
    };
    std::thread::scope(|scope| {
        let handles: Vec<_> = jobs
            .into_iter()
            .map(|job| {
                let shared = &shared;
                scope.spawn(move || execute(shared, job))
            })
            .collect();
        handles.into_iter().map(join).collect()
    })
}

/// What every execution of a wave shares.
struct Shared<'s> {
    context: &'s RunContext<'s>,
    slots: &'s Slots,
    environment: &'s EnvironmentName,
    project_dir: &'s str,
    cache_dir: PathBuf,
    run: &'s RunId,
    read_desired: Option<&'s CommitId>,
    cancellation: &'s super::Cancellation,
    lease_deadline: Option<Timestamp>,
}

/// The condition reason of a unit that was running when the run was cancelled.
const CANCELLED_WHILE_RUNNING: &str = "the run was cancelled while it executed the unit; its tools received the signal";

/// What an inspection of an uncertain unit decided.
enum Inspected {
    /// The effects are applied: record this receipt.
    Applied {
        outputs: Value,
        artifacts: Vec<ArtifactDocument>,
    },
    /// Nothing was applied: execute.
    NotApplied,
    /// Unknown or unsupported: the condition stays for `nyl recover`.
    Unknown(String),
}

/// Execute one unit in a worktree of its effective commit and map its outcome.
#[allow(clippy::too_many_lines)] // One execution in order: worktree, inspection, driver call, fault point, and the outcome's record.
fn execute(shared: &Shared<'_>, job: Job<'_>) -> Execution {
    let context = shared.context;
    let unit = job.unit;
    let name = unit.unit.name.clone();
    let mut execution = Execution {
        name: name.clone(),
        result: UnitResult::Current,
        message: None,
        change_digest: None,
        approval: job.approval.clone(),
        warnings: Vec::new(),
        errors: Vec::new(),
        checkpoint: None,
        artifacts: None,
        killed: false,
    };
    let before_effects = |execution: &mut Execution, message: String| {
        execution.errors.push(Finding::new(
            Code::DriverError,
            format!("unit {name}: {message}"),
            "fix the cause, such as a missing tool or an unreadable worktree, then run nyl reconcile again",
        ));
        execution.result = UnitResult::Blocked {
            reason: format!("driver error: {message}"),
        };
        execution.message = Some(message);
    };
    let Some(driver) = context.drivers.get(&unit.unit.api_version, &unit.unit.kind) else {
        before_effects(&mut execution, format!("no driver serves kind {}", unit.unit.kind));
        return execution;
    };
    let temp_dir = job.directory.join("tmp");
    let prepared = std::fs::create_dir_all(&temp_dir)
        .map_err(|error| format!("cannot create {}: {error}", job.directory.display()))
        .and_then(|()| {
            shared.slots.checkout_unit(
                context.sources,
                job.slot,
                job.commit,
                driver,
                unit.resolved_spec.as_ref().unwrap_or(&unit.spec),
                shared.project_dir,
            )
        });
    let (worktree, source_roots) = match prepared {
        Ok(roots) => roots,
        Err(message) => {
            before_effects(&mut execution, message);
            let _ = std::fs::remove_dir_all(&job.directory);
            return execution;
        }
    };
    // A pause point: the scenario harness holds a run here while the unit runs.
    let _ = context.base.faults.at(FaultPoint::Executing, Some(&name));
    if shared.cancellation.is_cancelled() {
        // A cancelled run starts no unit.
        execution.result = UnitResult::Blocked {
            reason: "the run was cancelled before it started the unit".to_owned(),
        };
        let _ = std::fs::remove_dir_all(&job.directory);
        return execution;
    }
    let current = same_incarnation(unit, job.observed);
    let uncertain = matches!(
        current.and_then(|observed| observed.condition.as_ref()),
        Some(Condition::Uncertain { .. })
    );
    let (env, secrets) = match execution_credentials(context, driver, &unit.spec) {
        Ok(env) => env,
        Err(message) => {
            if uncertain {
                // Missing credentials cannot settle an earlier uncertain effect.
                before_effects(&mut execution, message);
            } else {
                execution.result = UnitResult::Failed {
                    category: FailureCategory::Credentials,
                    retryable: true,
                };
                execution.message = Some(message.clone());
                let event = Event::Failed {
                    since: context.base.clock.now(),
                    run: shared.run.clone(),
                    category: FailureCategory::Credentials,
                    retryable: true,
                    message,
                };
                if let Ok(Applied::Changed(observed)) = transitions::apply_to(unit, current, event) {
                    execution.checkpoint = Some(Checkpoint {
                        desired: unit.clone(),
                        observed: *observed,
                        artifacts: job.previous_artifacts,
                    });
                }
            }
            let _ = std::fs::remove_dir_all(&job.directory);
            return execution;
        }
    };
    let started = context.base.clock.now();
    let deadline = shared.lease_deadline.map_or_else(
        || unit_timeout(context.drivers, unit).after(started),
        |lease| {
            unit_timeout(context.drivers, unit)
                .after(started)
                .min(crate::shell::lease::bound(&context.base.settings, lease))
        },
    );
    let budget = crate::shell::tools::DeadlineRunner {
        tools: context.tools,
        clock: context.base.clock,
        deadline,
    };
    let call = |operation: DriverOperation, now: Timestamp| ExecutionContext {
        secrets: secrets.clone(),
        interactive: false,
        artifacts: job.previous_artifacts.clone(),
        source_roots: source_roots.clone(),
        unit: UnitAddress::new(shared.environment.clone(), name.clone()),
        operation,
        project_dir: project_path(&worktree, shared.project_dir),
        worktree: worktree.clone(),
        temp_dir: temp_dir.clone(),
        env: env.clone(),
        approved_digest: job.approval.as_ref().and_then(|approval| approval.digest.clone()),
        deadline,
        now,
        observed: job.observed,
        tools: &budget,
        commit_identity: context.commit_identity,
        cache_dir: shared.cache_dir.clone(),
        teardown_wait: None,
    };

    let spec = unit.resolved_spec.as_ref().unwrap_or(&unit.spec);
    let inspected = if uncertain && driver.recovery(spec).execution == RecoveryPolicy::Inspect {
        match driver.inspect(&call(DriverOperation::Inspect, started), unit) {
            Ok(Supported::Supported(Inspection::Applied { outputs, artifacts })) => {
                Inspected::Applied { outputs, artifacts }
            }
            Ok(Supported::Supported(Inspection::NotApplied)) => Inspected::NotApplied,
            Ok(Supported::Supported(Inspection::Unknown { reason })) => Inspected::Unknown(reason),
            Ok(Supported::Unsupported) => Inspected::Unknown(format!("kind {} cannot inspect", unit.unit.kind)),
            Err(error) => Inspected::Unknown(error.to_string()),
        }
    } else {
        Inspected::NotApplied
    };
    let outcome = match inspected {
        Inspected::Unknown(reason) => {
            let _ = std::fs::remove_dir_all(&job.directory);
            let reason = format!("inspect could not tell whether its effects are applied: {reason}");
            execution.result = UnitResult::Uncertain { reason: reason.clone() };
            execution.message = Some(reason);
            return execution;
        }
        Inspected::Applied { outputs, artifacts } => Ok(Supported::Supported(Outcome::Succeeded {
            versions: BTreeMap::default(),
            outputs,
            artifacts,
            attestations: Vec::new(),
        })),
        Inspected::NotApplied => driver.reconcile(&call(DriverOperation::Reconcile, started), unit),
    };
    execution.killed = context.base.faults.at(FaultPoint::AfterEffect, Some(&name)) == FaultAction::Kill;
    let finished = context.base.clock.now();
    let _ = std::fs::remove_dir_all(&job.directory);
    // The run was cancelled while the unit ran: its tools got the signal, so
    // anything but a success may have left effects behind.
    let outcome = match outcome {
        Ok(Supported::Supported(Outcome::Succeeded { .. }) | Supported::Unsupported) => outcome,
        _ if shared.cancellation.is_cancelled() => Ok(Supported::Supported(Outcome::Uncertain {
            reason: CANCELLED_WHILE_RUNNING.to_owned(),
        })),
        other => other,
    };
    let outcome = match outcome {
        Ok(Supported::Supported(outcome)) => outcome,
        Ok(Supported::Unsupported) => {
            before_effects(&mut execution, format!("kind {} cannot reconcile", unit.unit.kind));
            return execution;
        }
        Err(error) if !error.effects_possible() => {
            before_effects(&mut execution, error.to_string());
            return execution;
        }
        Err(error) => Outcome::Uncertain {
            reason: error.to_string(),
        },
    };
    let event = match outcome {
        Outcome::Succeeded {
            outputs,
            artifacts,
            mut versions,
            ..
        } => {
            let key = unit
                .execution_key
                .clone()
                .expect("an executed unit has an execution key");
            let (outputs, warnings) = recorded_outputs(unit, outputs);
            execution.warnings = warnings;
            let (set, digests) = recorded_artifacts(shared.environment, unit, &key, artifacts);
            execution.result = if uncertain {
                UnitResult::Recovered {
                    execution_key: key.clone(),
                }
            } else {
                UnitResult::Executed {
                    execution_key: key.clone(),
                }
            };
            execution.artifacts = Some(set);
            Event::Succeeded {
                receipt: Receipt {
                    execution_key: key,
                    behavior_version: driver.behavior_version(),
                    run: shared.run.clone(),
                    read_desired: shared.read_desired.cloned(),
                    started_at: started,
                    finished_at: finished,
                    versions: {
                        versions.insert("nyl".to_owned(), context.base.version.clone());
                        versions
                    },
                    approval: job.approval.clone(),
                    provenance: unit.provenance.clone(),
                    outputs,
                    artifacts: digests,
                },
            }
        }
        Outcome::AwaitingApproval { digest } => {
            execution.change_digest = Some(digest);
            execution.result = UnitResult::AwaitingApproval;
            return execution;
        }
        Outcome::RetryRequired {
            category,
            message,
            context,
        } => {
            execution.result = UnitResult::Failed {
                category,
                retryable: false,
            };
            execution.message = Some(message.clone());
            Event::RetryRequired {
                since: finished,
                run: shared.run.clone(),
                category,
                message,
                context,
            }
        }
        Outcome::Failed {
            category,
            retryable,
            message,
        } => {
            execution.result = UnitResult::Failed { category, retryable };
            execution.message = Some(message.clone());
            Event::Failed {
                since: finished,
                run: shared.run.clone(),
                category,
                retryable,
                message,
            }
        }
        Outcome::Uncertain { reason } | Outcome::Waiting { reason, .. } => {
            execution.result = UnitResult::Uncertain { reason: reason.clone() };
            execution.message = Some(reason.clone());
            Event::Uncertain {
                since: finished,
                run: shared.run.clone(),
                reason,
            }
        }
    };
    // Executions always change the observed document; the table refuses
    // only `recover` events and keeps late imports as evidence.
    // A new receipt carries a copy of the document it was produced for (Q16).
    let Ok(Applied::Changed(observed)) = transitions::apply_to(unit, current, event) else {
        return execution;
    };
    execution.checkpoint = Some(Checkpoint {
        desired: unit.clone(),
        observed: *observed,
        artifacts: execution.artifacts.clone().unwrap_or(job.previous_artifacts),
    });
    execution
}

/// The outputs a receipt records: the declared, non-sensitive ones a
/// driver reported, with a `NYL-OUTPUT-UNDECLARED` warning for each
/// undeclared one.
fn recorded_outputs(unit: &DesiredUnit, outputs: Value) -> (Value, Vec<Finding>) {
    let name = &unit.unit.name;
    let declared = declared_outputs(unit.resolved_spec.as_ref().unwrap_or(&unit.spec));
    let mut recorded = Map::new();
    let mut warnings = Vec::new();
    if let Value::Object(outputs) = outputs {
        for (output, value) in outputs {
            match declared.get(&output) {
                Some(declaration) if !declaration.sensitive => {
                    recorded.insert(output, value);
                }
                Some(_) => {}
                None => warnings.push(Finding::new(
                    Code::OutputUndeclared,
                    format!("unit {name} reported output {output:?}, which it does not declare; it was not recorded"),
                    format!("declare {output} under the unit's spec.outputs to record it, or stop writing it"),
                )),
            }
        }
    }
    (Value::Object(recorded), warnings)
}

/// A success's artifact documents with `metadata.unit` and `executionKey`
/// filled in, and the digests its receipt lists.
fn recorded_artifacts(
    environment: &EnvironmentName,
    unit: &DesiredUnit,
    key: &ExecutionKey,
    artifacts: Vec<ArtifactDocument>,
) -> (BTreeMap<String, ArtifactDocument>, Vec<ArtifactDigest>) {
    let uid = unit.unit.uid.clone().expect("an executed unit has a uid");
    let mut set = BTreeMap::new();
    let mut digests = Vec::new();
    for mut document in artifacts {
        document.metadata.unit = ArtifactUnit {
            environment: environment.clone(),
            name: unit.unit.name.clone(),
            uid: uid.clone(),
        };
        document.execution_key = key.clone();
        digests.push(ArtifactDigest {
            name: document.metadata.name.clone(),
            kind: document.kind.clone(),
            digest: document.digest(),
        });
        set.insert(document.metadata.name.clone(), document);
    }
    (set, digests)
}
