//! The lifecycle of template instances: removing an expired one, and
//! admitting a new one under `maxInstances`.
//!
//! Contract: [Expiry](../../../design/orchestration-core.md#expiry) and
//! [Instances](../../../design/orchestration-core.md#instances). The decisions
//! are `nyl_orchestration::rules::expiry`'s and the effects the shell's
//! (`teardown --all`, then `state delete`); this module composes them for the
//! commands that share them: `reconcile`, the fleet reconcile, and
//! `state init --template --make-room`.

use std::path::Path;

use nyl_core::orchestration::codes::{Code, Finding};
use nyl_core::orchestration::ids::EnvironmentName;
use nyl_core::orchestration::time::Duration;
use nyl_orchestration::report::Outcome;
use nyl_orchestration::rules::expiry::{self, Admission, Existing, Refusal};
use nyl_orchestration::run_report::{EnvironmentRunReport, ExitCategory, InstanceAction, ReportKind};
use nyl_orchestration::shell::run::{LifecycleOptions, RunMode, TeardownTarget};
use nyl_state::git::CommitAuthor;

use super::approval::ApprovalArgs;
use super::project::{list_instances, listing_location, InstanceRequest};
use super::run::Wiring;
use super::session::{RunOptions, Session, Stop};

/// How a removal is authorized and what it may wait for: the options of the
/// command that removes an expired instance.
#[derive(Debug, Clone)]
#[allow(clippy::struct_excessive_bools)] // Independent command-line switches.
pub struct Removal<'a> {
    /// The project directory the command was given.
    pub path: &'a Path,
    /// `--local`: state is read and written without network access.
    pub offline: bool,
    /// `--allow-teardown`, which authorizes the teardown like the template's `allowTeardown`.
    pub allow_teardown: bool,
    /// `--allow-dependents`.
    pub allow_dependents: bool,
    /// `--allow-incomplete`.
    pub allow_incomplete: bool,
    /// `--fail-fast`.
    pub fail_fast: bool,
    /// Manual approvals of the teardowns.
    pub approval: Option<&'a ApprovalArgs>,
    /// `--reason`.
    pub reason: Option<&'a str>,
    /// `--wait-lease`.
    pub wait: Option<Duration>,
    /// `--author`.
    pub author: Option<&'a str>,
    /// `--source`: take the units' source files from this revision when the
    /// recorded commit cannot be fetched.
    pub source: Option<&'a str>,
}

/// Remove the expired instance `session` works on: `teardown --all`, then
/// `state delete`, the way `nyl state delete --teardown` does, with the
/// teardown authorized by the template's `allowTeardown` or `--allow-teardown`.
/// Without either, the units are marked pending teardown and the instance is
/// reported as such (exit 2). Nothing is removed while resources remain: the
/// state stays until every unit's teardown completed, and the report says
/// which waits remain.
///
/// The report is an ordinary reconcile report of the environment, so a
/// pipeline reads it like any other: its units are the teardown's results.
pub fn remove_expired(
    session: &Session,
    wiring: &Wiring<'_>,
    removal: &Removal<'_>,
) -> Result<EnvironmentRunReport, Stop> {
    let instance = session.resolved.instance.as_ref().expect("only an instance expires");
    // The decision the removal repeats under the lease: this expiry, reached.
    let recorded = instance.record.as_ref().and_then(|record| record.expires_at);
    let decided_on = recorded
        .filter(|_| expiry::expired(recorded, instance.terms.ttl, wiring.clock.now()))
        .ok_or_else(|| {
            Finding::new(
                Code::InstanceRenewed,
                format!("{} is not expired, so nothing is removed", session.resolved.name),
                format!(
                    "run `nyl reconcile -e {}` again; the next run decides on the instance's current expiry",
                    session.resolved.name
                ),
            )
        })?;
    let identity = wiring.commit_identity(&session.resolved.git_dir, removal.author)?;
    let authorized = removal.allow_teardown || instance.terms.allow_teardown;
    let mut request = session.request(&RunOptions {
        units: &[],
        concurrency: nyl_orchestration::shell::run::DEFAULT_CONCURRENCY,
        source: removal.source,
        wait: removal.wait,
        local: None,
        cancellation: wiring.cancellation.clone(),
        mode: RunMode::Teardown {
            target: TeardownTarget::All,
        },
        lifecycle: LifecycleOptions {
            allow_teardown: authorized,
            allow_dependents: removal.allow_dependents,
            allow_incomplete: removal.allow_incomplete,
            expired: Some(decided_on),
            ..LifecycleOptions::default()
        },
        reapply: false,
        fail_fast: removal.fail_fast,
        reason: removal.reason,
    })?;
    if let Some(approval) = removal.approval {
        request.approvals = approval.resolve(session, wiring, &identity, None)?;
    }
    let author = Some(CommitAuthor {
        identity: identity.clone(),
        clock: wiring.clock.clone(),
    });
    let store = if removal.offline {
        session.offline_store(author)?
    } else {
        session.store(author)?
    };
    let deletion = session.run_with(wiring, &store, Some(&identity), &request, |context, request| {
        nyl_orchestration::shell::teardown::state_delete_detailed(context, request, true)
    })?;
    let removed = deletion.report.outcome == Outcome::Done;
    let mut report = deletion
        .teardown
        .and_then(|teardown| teardown.environments.into_iter().next())
        .unwrap_or_else(|| EnvironmentRunReport::new(session.resolved.name.clone()));
    // The units the teardown could not finish are the reason state stays; the
    // deletion's own refusal repeats them, and its errors carry the rest.
    for finding in deletion.report.errors {
        if finding.code == Code::StateDeleteIncarnations {
            report.warnings.push(finding);
        } else if !report.errors.contains(&finding) {
            report.errors.push(finding);
        }
    }
    report.warnings.extend(deletion.report.warnings);
    let action = if removed {
        InstanceAction::Removed
    } else if !authorized {
        report.errors.push(Finding::new(
            Code::InstanceRemovalPending,
            format!(
                "{} is expired, and its teardown waits: the template does not set allowTeardown and --allow-teardown was not given",
                session.resolved.name
            ),
            format!(
                "set allowTeardown: true on EnvironmentTemplate {}, or pass --allow-teardown to the run that removes expired instances",
                instance.template
            ),
        ));
        InstanceAction::RemovalPending
    } else {
        InstanceAction::RemovalIncomplete
    };
    report.finish(ReportKind::ReconcileReport);
    if !removed && report.exit == ExitCategory::Ok {
        // State and resources remain, so the removal is not complete, whatever the units say.
        report.exit = ExitCategory::Incomplete;
        report.complete = false;
    }
    report.instance = session.instance_report(wiring.clock.now()).map(|mut instance| {
        instance.expired = true;
        instance.action = Some(action);
        instance
    });
    Ok(report)
}

/// The live instances of `template` other than `creating`, which count
/// against `maxInstances`; `Err` when they cannot be listed.
fn existing(
    session: &Session,
    template: &str,
    creating: &EnvironmentName,
    offline: bool,
) -> Result<Vec<Existing>, Finding> {
    let declaration = nyl_render::gitops::instances::find_template(&session.inventory, template)
        .expect("the instance's template resolved");
    if listing_location(&session.inventory, declaration).is_none() {
        return Err(Finding::new(
            Code::InstanceNotListable,
            format!(
                "EnvironmentTemplate {template} sets maxInstances, but its instances cannot be listed: its state location depends on the instance name"
            ),
            "end spec.state.path with `{{ environment.name }}` and keep the repository and refs independent of the name, or drop maxInstances",
        ));
    }
    let listing = list_instances(&session.inventory, offline);
    if listing.unavailable.iter().any(|unavailable| unavailable == template) {
        return Err(Finding::new(
            Code::StateUnavailable,
            format!("the instances of EnvironmentTemplate {template} could not be listed, so maxInstances cannot be checked"),
            "check access to the template's state repository",
        ));
    }
    Ok(listing
        .instances
        .into_iter()
        .filter(|listed| listed.descriptor.template == template && listed.name != *creating)
        .map(|listed| Existing {
            name: listed.name,
            expires_at: listed.record.expires_at,
        })
        .collect())
}

/// Admit the new instance `session` works on under its template's
/// `maxInstances`: nothing when the template has no limit or room is left;
/// at the limit, a refusal (`NYL-INSTANCE-LIMIT`, exit 2) that names the
/// expired instances that could go, or with `make_room` their removal first.
/// Returns the instances it removed.
pub fn admit(
    session: &Session,
    wiring: &Wiring<'_>,
    make_room: bool,
    removal: &Removal<'_>,
) -> Result<Vec<EnvironmentName>, Stop> {
    let instance = session
        .resolved
        .instance
        .as_ref()
        .expect("only an instance is admitted");
    let Some(max) = instance.terms.max_instances else {
        return Ok(Vec::new());
    };
    let template = instance.template.as_str();
    let now = wiring.clock.now();
    let existing = existing(session, template, &session.resolved.name, removal.offline)?;
    let counted = existing;
    match expiry::admit(Some(max), instance.terms.ttl, &counted, now, make_room) {
        Admission::Admit => Ok(Vec::new()),
        Admission::Refuse(refusal) => Err(expiry::limit_finding(template, &refusal, &[]).into()),
        Admission::Evict(names) => {
            let mut removed = Vec::new();
            let mut incomplete = Vec::new();
            for name in &names {
                let evictee = Session::open_with(
                    removal.path,
                    name.as_str(),
                    &InstanceRequest {
                        offline: removal.offline,
                        ..InstanceRequest::default()
                    },
                )?;
                // Whatever the evictee's own session decides, it is expired: the
                // rule chose it from its record, and the removal repeats the check under the lease.
                let report = remove_expired(&evictee, wiring, removal)?;
                if report
                    .instance
                    .as_ref()
                    .is_some_and(|instance| instance.action == Some(InstanceAction::Removed))
                {
                    removed.push(name.clone());
                } else {
                    incomplete.push(name.clone());
                }
            }
            if incomplete.is_empty() {
                return Ok(removed);
            }
            let refusal = Refusal {
                max,
                count: counted.len(),
                needed: names.len(),
                evictable: names,
            };
            Err(expiry::limit_finding(template, &refusal, &incomplete).into())
        }
    }
}
