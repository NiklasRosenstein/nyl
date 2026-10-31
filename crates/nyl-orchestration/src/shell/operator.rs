//! Held-unit repairs through a driver's generic `exec` capability.
//! Contract: [Operator exec](../../../../design/orchestration-core.md#operator-exec).

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;

use nyl_core::orchestration::codes::{Code, Finding};
use nyl_core::orchestration::driver::{DriverOperation, ExecStatus, ExecutionContext, SourceRoot, Supported};
use nyl_core::orchestration::ids::UnitName;
use nyl_core::orchestration::ports::{SourceRepository, TransitionChanges};
use nyl_core::orchestration::state::{DesiredUnit, Lifecycle, Operation, OperatorModification, Request};
use nyl_core::orchestration::time::Duration;
use serde_json::{json, Value};

use super::run::{execution_credentials, project_path, unit_timeout, RunContext, RunRequest};
use super::state::{commit, read, summary, with_lease, Requested};
use super::Context;
use crate::report::StateOperationReport;
use crate::rules::transitions::{apply_to, Applied, Event};

/// A read-only CLI preparation of an explicitly requested worktree spec.
pub struct WorktreeUnit {
    pub unit: DesiredUnit,
    pub worktree: PathBuf,
    pub project_dir: PathBuf,
    pub source_roots: BTreeMap<String, SourceRoot>,
    /// Source commit, dirty flag, snapshot digest, and changed paths; never bytes.
    pub audit: Value,
}

/// Options independent of the unit's native tool.
pub struct ExecOptions {
    pub unit: UnitName,
    pub arguments: Vec<OsString>,
    pub timeout: Option<Duration>,
    pub interactive: bool,
    pub worktree: Option<WorktreeUnit>,
}

fn invalid(message: impl Into<String>) -> Finding {
    Finding::new(
        Code::ExecInvalid,
        message,
        "check the held unit and its declared source and credentials",
    )
}

/// Run a native tool under the environment lease. Refusals always exit 125.
#[allow(clippy::too_many_lines)] // One leased attempt keeps the pre-effect fence and result audit together.
pub fn exec(context: &RunContext<'_>, request: &RunRequest, options: &ExecOptions) -> StateOperationReport {
    let initial = match read(context.base, &request.location, &request.environment) {
        Ok(snapshot) => snapshot,
        Err(finding) => return refused(request, finding),
    };
    let Some(stored) = initial.desired.get(&options.unit) else {
        return refused(
            request,
            Finding::new(
                Code::UnitUnknown,
                format!("unit {} is absent from state", options.unit),
                "select an existing held unit",
            ),
        );
    };
    let timeout = options.timeout.unwrap_or_else(|| unit_timeout(context.drivers, stored));
    let base = Context {
        store: context.base.store,
        clock: context.base.clock,
        ids: context.base.ids,
        faults: context.base.faults,
        runner: context.base.runner.clone(),
        requested_by: context.base.requested_by.clone(),
        version: context.base.version.clone(),
        settings: super::Settings {
            resolution_timeout: timeout,
            ..context.base.settings
        },
    };
    let requested = Requested {
        reason: request.reason.clone(),
        wait: request.wait,
        cancellation: request.cancellation.clone(),
    };
    let mut report = with_lease(
        &base,
        &request.environment,
        Operation::Exec,
        &request.location,
        &requested,
        |held, report| {
            let mut snapshot = read(&base, &request.location, &request.environment)?;
            super::local::check_record(
                base.store,
                snapshot.record.as_ref(),
                &request.environment,
                &request.location,
            )?;
            super::local::check_remote_tips(base.store, &request.location, &request.environment)?;
            let stored = snapshot
                .desired
                .get(&options.unit)
                .ok_or_else(|| invalid("unit disappeared from state"))?;
            if !matches!(stored.lifecycle, Lifecycle::Held { .. }) || stored.unit.uid.is_none() {
                return Err(Finding::new(
                    Code::ExecNotHeld,
                    format!("{} must have a held incarnation before exec", options.unit),
                    "hold the unit before repairing it",
                ));
            }
            let mut unit = options
                .worktree
                .as_ref()
                .map_or_else(|| stored.clone(), |worktree| worktree.unit.clone());
            if unit.unit.kind != stored.unit.kind
                || unit.unit.api_version != stored.unit.api_version
                || unit.unit.name != stored.unit.name
            {
                return Err(invalid("a repair must keep the deployed unit's name and kind"));
            }
            unit.unit = stored.unit.clone();
            unit.lifecycle = stored.lifecycle.clone();
            let driver = context
                .drivers
                .get(&unit.unit.api_version, &unit.unit.kind)
                .ok_or_else(|| invalid(format!("no driver serves {}", unit.unit.kind)))?;
            let terminal = driver
                .capabilities(unit.resolved_spec.as_ref().unwrap_or(&unit.spec))
                .exec
                .ok_or_else(|| {
                    Finding::new(
                        Code::ExecUnsupported,
                        format!("kind {} has no exec capability", unit.unit.kind),
                        "use a kind with native operator support",
                    )
                })?;
            let spec = unit.resolved_spec.as_ref().unwrap_or(&unit.spec);
            driver.validate_spec(spec).map_err(invalid)?;
            let (env, secrets) = execution_credentials(context, driver, spec).map_err(invalid)?;
            let mut claims = BTreeMap::new();
            if let Some(finding) = request.environment_errors.values().next() {
                return Err(finding.clone());
            }
            for (environment, location) in &request.environments {
                let mut state = if *environment == request.environment {
                    snapshot.clone()
                } else {
                    base.store
                        .snapshot(location)
                        .map_err(|error| invalid(format!("cannot check ownership in {environment}: {error}")))?
                };
                if *environment == request.environment {
                    state.desired.insert(options.unit.clone(), unit.clone());
                }
                claims.insert(environment.clone(), state);
            }
            if let Some(finding) =
                crate::rules::desired::check_ownership_with_snapshots(&BTreeMap::new(), &claims, context.drivers)
                    .into_iter()
                    .next()
            {
                return Err(Finding::new(
                    Code::ExecScopeConflict,
                    finding.message,
                    "choose a scope no other unit owns",
                ));
            }
            let directory = context.workspace.join("operator");
            std::fs::create_dir_all(&directory).map_err(|error| invalid(error.to_string()))?;
            let (worktree, project_dir, source_roots) = if let Some(worktree) = &options.worktree {
                (
                    worktree.worktree.clone(),
                    worktree.project_dir.clone(),
                    worktree.source_roots.clone(),
                )
            } else {
                let revision = unit
                    .source_revision
                    .as_ref()
                    .ok_or_else(|| invalid("the unit has no source snapshot"))?;
                let worktree = directory.join("source");
                context
                    .sources
                    .worktree(&SourceRepository::This, revision, &worktree)
                    .map_err(|error| invalid(error.to_string()))?;
                let rendered = context
                    .renderer
                    .render(&worktree, &request.environment, revision)
                    .map_err(|error| invalid(error.to_string()))?;
                let roots = super::sources::prepare_source_roots(
                    context.sources,
                    driver,
                    spec,
                    revision,
                    &rendered.project_dir,
                    &directory.join("contexts"),
                    Some((&worktree, revision)),
                )
                .map_err(invalid)?;
                let project_dir = project_path(&worktree, &rendered.project_dir);
                (worktree, project_dir, roots)
            };
            let now = base.clock.now();
            let modification = OperatorModification {
                request: Request {
                    by: base.requested_by.clone(),
                    at: now,
                    reason: request.reason.clone(),
                },
                status: None,
            };
            let previous_observed = snapshot.observed.get(&options.unit).cloned();
            let temp_dir = directory.join("tmp");
            std::fs::create_dir_all(&temp_dir).map_err(|error| invalid(error.to_string()))?;
            let changed = match apply_to(
                stored,
                snapshot.observed.get(&options.unit),
                Event::OperatorModified {
                    modification: modification.clone(),
                },
            ) {
                Ok(Applied::Changed(changed)) => *changed,
                _ => return Err(invalid("cannot record the operator repair")),
            };
            let mut audit = BTreeMap::new();
            audit.insert(
                "arguments".to_owned(),
                json!(options
                    .arguments
                    .iter()
                    .map(|argument| mask(&argument.to_string_lossy(), env.values().chain(secrets.values())))
                    .collect::<Vec<_>>()),
            );
            audit.insert(
                "specDigest".to_owned(),
                json!(nyl_core::digest::sha256_hex(
                    &nyl_core::digest::canonical_json_bytes(spec).map_err(|error| invalid(error.to_string()))?
                )),
            );
            audit.insert(
                "specDifferences".to_owned(),
                json!(differing_paths(
                    stored.resolved_spec.as_ref().unwrap_or(&stored.spec),
                    spec
                )),
            );
            audit.insert(
                "worktree".to_owned(),
                options
                    .worktree
                    .as_ref()
                    .map_or(Value::Null, |worktree| worktree.audit.clone()),
            );
            let mut started = summary(Operation::Exec, &request.environment, "operator tool starting");
            started.details = audit.clone();
            // Persist the approval fence before a tool can change native state.
            commit(
                &base,
                &request.location,
                held,
                &snapshot,
                started,
                TransitionChanges {
                    observed: BTreeMap::from([(options.unit.clone(), Some(changed))]),
                    ..Default::default()
                },
                &requested,
                report,
            )?;
            snapshot = read(&base, &request.location, &request.environment)?;
            let tools = super::tools::DeadlineRunner {
                tools: context.tools,
                clock: base.clock,
                deadline: timeout.after(now),
            };
            let execution = ExecutionContext {
                unit: nyl_core::orchestration::ids::UnitAddress::new(request.environment.clone(), options.unit.clone()),
                operation: DriverOperation::Exec,
                worktree,
                project_dir,
                source_roots,
                temp_dir,
                env,
                secrets,
                interactive: options.interactive
                    && terminal == nyl_core::orchestration::driver::ExecTerminal::Interactive,
                artifacts: snapshot.artifacts.get(&options.unit).cloned().unwrap_or_default(),
                approved_digest: None,
                deadline: timeout.after(now),
                now,
                observed: snapshot.observed.get(&options.unit),
                tools: &tools,
                commit_identity: context.commit_identity,
                cache_dir: context.cache_dir(),
                teardown_wait: None,
            };
            let status = match driver.exec(&execution, &unit, &options.arguments) {
                Ok(Supported::Supported(status)) => status,
                Ok(Supported::Unsupported) => return Err(invalid("driver refused its declared exec capability")),
                Err(error) if !error.effects_possible() => {
                    let mut failed = summary(Operation::Exec, &request.environment, "operator tool did not start");
                    failed.details = audit.clone();
                    commit(
                        &base,
                        &request.location,
                        held,
                        &snapshot,
                        failed,
                        TransitionChanges {
                            observed: BTreeMap::from([(options.unit.clone(), previous_observed)]),
                            ..Default::default()
                        },
                        &requested,
                        report,
                    )?;
                    return Err(invalid(error.to_string()));
                }
                Err(error) => return Err(invalid(error.to_string())),
            };
            report.details.insert(
                "status".to_owned(),
                serde_json::to_value(status).expect("tool status serializes"),
            );
            report.exit = match status {
                ExecStatus::Exited { code } => u8::try_from(code).unwrap_or(1),
                ExecStatus::Signaled { signal } => u8::try_from(128 + signal).unwrap_or(1),
            };
            let changed = match apply_to(
                &unit,
                snapshot.observed.get(&options.unit),
                Event::OperatorModified {
                    modification: OperatorModification {
                        status: Some(status),
                        ..modification
                    },
                },
            ) {
                Ok(Applied::Changed(changed)) => *changed,
                _ => return Err(invalid("cannot record the operator result")),
            };
            let mut finished = summary(Operation::Exec, &request.environment, "operator tool finished");
            audit.insert(
                "status".to_owned(),
                serde_json::to_value(status).expect("tool status serializes"),
            );
            finished.details = audit.clone();
            commit(
                &base,
                &request.location,
                held,
                &snapshot,
                finished,
                TransitionChanges {
                    observed: BTreeMap::from([(options.unit.clone(), Some(changed))]),
                    ..Default::default()
                },
                &requested,
                report,
            )?;
            report.details = audit;
            report.exit = match status {
                ExecStatus::Exited { code } => u8::try_from(code).unwrap_or(1),
                ExecStatus::Signaled { signal } => u8::try_from(128 + signal).unwrap_or(1),
            };
            Ok(())
        },
    );
    if !report.errors.is_empty() {
        report.exit = report
            .details
            .get("status")
            .and_then(|status| serde_json::from_value::<ExecStatus>(status.clone()).ok())
            .map_or(125, |status| match status {
                ExecStatus::Exited { code } => u8::try_from(code).unwrap_or(1),
                ExecStatus::Signaled { signal } => u8::try_from(128 + signal).unwrap_or(1),
            });
    }
    report
}

fn refused(request: &RunRequest, finding: Finding) -> StateOperationReport {
    let mut report = StateOperationReport::new(request.environment.clone(), "exec").refused(finding);
    report.exit = 125;
    report
}

fn mask<'a>(text: &str, values: impl Iterator<Item = &'a String>) -> String {
    let mut text = text.to_owned();
    let mut values = values.filter(|value| !value.is_empty()).collect::<Vec<_>>();
    values.sort_by_key(|value| std::cmp::Reverse(value.len()));
    for value in values {
        text = text.replace(value, "***");
    }
    text
}

fn differing_paths(before: &Value, after: &Value) -> Vec<String> {
    fn visit(before: &Value, after: &Value, path: &str, paths: &mut Vec<String>) {
        if before == after {
            return;
        }
        if let (Some(before), Some(after)) = (before.as_object(), after.as_object()) {
            let keys = before
                .keys()
                .chain(after.keys())
                .collect::<std::collections::BTreeSet<_>>();
            for key in keys {
                visit(
                    before.get(key).unwrap_or(&Value::Null),
                    after.get(key).unwrap_or(&Value::Null),
                    &format!("{path}/{}", key.replace('~', "~0").replace('/', "~1")),
                    paths,
                );
            }
        } else {
            paths.push(path.to_owned());
        }
    }
    let mut paths = Vec::new();
    visit(before, after, "", &mut paths);
    paths
}
