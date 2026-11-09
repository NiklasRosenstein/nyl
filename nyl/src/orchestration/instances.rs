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
use nyl_orchestration::rules::expiry::{self, Admission, EvictionResult, Existing};
use nyl_orchestration::run_report::{EnvironmentRunReport, InstanceAction};
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
    let decided_on = expiry::decided_expiry(
        &session.resolved.name,
        instance.record.as_ref().and_then(|record| record.expires_at),
        instance.terms.ttl,
        wiring.clock.now(),
    )?;
    let identity = wiring.commit_identity(&session.resolved.git_dir, removal.author)?;
    let authorized = expiry::removal_authorized(instance.terms.allow_teardown, removal.allow_teardown);
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
    Ok(nyl_orchestration::shell::removal::removal_report(
        &session.resolved.name,
        &instance.template,
        deletion,
        authorized,
        session.instance_report(wiring.clock.now()),
    ))
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
    if listing_location(&session.inventory, declaration)?.is_none() {
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
        Admission::Refuse(refusal) => Err(expiry::limit_finding(template, &refusal).into()),
        Admission::Evict(mut eviction) => {
            // The evictees are other pull requests' previews: they inherit neither this
            // job's `--source` nor its approvals, and the template's `allowTeardown`
            // that authorizes their removal is the one this job's checkout declares.
            let evicting = Removal {
                source: None,
                approval: None,
                ..removal.clone()
            };
            while let Some(name) = eviction.next().cloned() {
                let result = evict(&name, wiring, &evicting);
                eviction.record(&name, result?);
            }
            match eviction.refusal() {
                None => Ok(eviction.removed().to_vec()),
                Some(refusal) => Err(expiry::limit_finding(template, &refusal).into()),
            }
        }
    }
}

/// Remove the expired instance `name` to make room, and what came of it.
fn evict(name: &EnvironmentName, wiring: &Wiring<'_>, removal: &Removal<'_>) -> Result<EvictionResult, Stop> {
    let session = Session::open_with(
        removal.path,
        name.as_str(),
        &InstanceRequest {
            offline: removal.offline,
            ..InstanceRequest::default()
        },
    )?;
    // The rule chose it from a listing; the removal decides again on the record it reads, and under the lease.
    match remove_expired(&session, wiring, removal) {
        Ok(report) => {
            let codes = report.errors.iter().map(|finding| finding.code).collect::<Vec<_>>();
            Ok(expiry::eviction_result(
                report
                    .instance
                    .as_ref()
                    .and_then(|instance| instance.action)
                    .unwrap_or(InstanceAction::RemovalIncomplete),
                &codes,
            ))
        }
        Err(Stop::Finding(finding)) => Ok(expiry::eviction_result_of_stop(finding.code)),
        Err(error @ Stop::Error(_)) => Err(error),
    }
}
