//! Executing one wave: a thread per unit, each in its concurrency slot's
//! worktree, and the mapping of each driver outcome to what the run records.
//!
//! Contract: [Execution](../../../../../design/orchestration-core.md#execution)
//! and [Runs and leases](../../../../../design/orchestration-core.md#runs-and-leases)
//! (checkpoints).

#[allow(clippy::wildcard_imports)] // The run module's private helpers and imports.
use super::*;

/// One finished execution.
pub(super) struct Execution {
    pub(super) name: UnitName,
    pub(super) result: UnitResult,
    pub(super) message: Option<String>,
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
        })
        .collect();
    let shared = Shared {
        context: session.context,
        slots: &session.slots,
        environment: session.environment(),
        project_dir: &prepared.rendered.project_dir,
        cache_dir: session.cache_dir(),
        run,
        read_desired: snapshot.tips.desired.as_ref(),
    };
    std::thread::scope(|scope| {
        let handles: Vec<_> = jobs
            .into_iter()
            .map(|job| {
                let shared = &shared;
                scope.spawn(move || execute(shared, job))
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| match handle.join() {
                Ok(execution) => execution,
                Err(panic) => std::panic::resume_unwind(panic),
            })
            .collect()
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
}

/// Execute one unit in a worktree of its effective commit and map its outcome.
#[allow(clippy::too_many_lines)] // One execution in order: worktree, driver call, fault point, and each outcome's record.
fn execute(shared: &Shared<'_>, job: Job<'_>) -> Execution {
    let context = shared.context;
    let unit = job.unit;
    let name = unit.unit.name.clone();
    let mut execution = Execution {
        name: name.clone(),
        result: UnitResult::Current,
        message: None,
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
        .and_then(|()| shared.slots.checkout(context.sources, job.slot, job.commit));
    let worktree = match prepared {
        Ok(worktree) => worktree,
        Err(message) => {
            before_effects(&mut execution, message);
            let _ = std::fs::remove_dir_all(&job.directory);
            return execution;
        }
    };
    // A pause point: the scenario harness holds a run here while the unit runs.
    let _ = context.base.faults.at(FaultPoint::Executing, Some(&name));
    let started = context.base.clock.now();
    let ctx = ExecutionContext {
        unit: UnitAddress::new(shared.environment.clone(), name.clone()),
        operation: DriverOperation::Reconcile,
        project_dir: project_path(&worktree, shared.project_dir),
        worktree,
        temp_dir,
        env: admitted_env(context.process_env, &unit.spec),
        approved_digest: None,
        deadline: unit_timeout(unit).after(started),
        now: started,
        observed: job.observed,
        tools: context.tools,
        commit_identity: context.commit_identity,
        cache_dir: shared.cache_dir.clone(),
    };
    let outcome = driver.reconcile(&ctx, unit);
    drop(ctx);
    execution.killed = context.base.faults.at(FaultPoint::AfterEffect, Some(&name)) == FaultAction::Kill;
    let finished = context.base.clock.now();
    let _ = std::fs::remove_dir_all(&job.directory);
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
    let previous_receipt = same_incarnation(unit, job.observed).and_then(|observed| observed.receipt.clone());
    let condition = |condition: Condition| ObservedUnit {
        api_version: StateV1,
        kind: ObservedUnitKind,
        unit: unit.unit.clone(),
        receipt: previous_receipt.clone(),
        condition: Some(condition),
    };
    let observed = match outcome {
        Outcome::Succeeded { outputs, artifacts, .. } => {
            let key = unit
                .execution_key
                .clone()
                .expect("an executed unit has an execution key");
            let (outputs, warnings) = recorded_outputs(unit, outputs);
            execution.warnings = warnings;
            let (set, digests) = recorded_artifacts(shared.environment, unit, &key, artifacts);
            execution.result = UnitResult::Executed {
                execution_key: key.clone(),
            };
            execution.artifacts = Some(set);
            ObservedUnit {
                api_version: StateV1,
                kind: ObservedUnitKind,
                unit: unit.unit.clone(),
                receipt: Some(Receipt {
                    execution_key: key,
                    behavior_version: driver.behavior_version(),
                    run: shared.run.clone(),
                    read_desired: shared.read_desired.cloned(),
                    started_at: started,
                    finished_at: finished,
                    versions: BTreeMap::from([("nyl".to_owned(), context.base.version.clone())]),
                    approval: None,
                    provenance: unit.provenance.clone(),
                    outputs,
                    artifacts: digests,
                }),
                condition: None,
            }
        }
        Outcome::AwaitingApproval { .. } => {
            execution.result = UnitResult::AwaitingApproval;
            return execution;
        }
        Outcome::Failed {
            category,
            retryable,
            message,
        } => {
            execution.result = UnitResult::Failed { category, retryable };
            execution.message = Some(message.clone());
            condition(Condition::Failed {
                since: finished,
                run: shared.run.clone(),
                category,
                retryable,
                message,
            })
        }
        Outcome::Uncertain { reason } | Outcome::Waiting { reason, .. } => {
            execution.result = UnitResult::Uncertain { reason: reason.clone() };
            execution.message = Some(reason.clone());
            condition(Condition::Uncertain {
                since: finished,
                run: shared.run.clone(),
                reason,
            })
        }
    };
    execution.checkpoint = Some(Checkpoint {
        desired: unit.clone(),
        observed,
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
