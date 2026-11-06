//! The generic rules of expired-instance removal in the orchestration shell,
//! with the fake kinds: the removal's teardowns are caused by expiry and need
//! `allow_teardown`, nothing is deleted while a unit has an incarnation, and
//! the decision is repeated under the lease so an instance renewed meanwhile
//! is kept. The environment here is declared, because the shell knows no
//! difference: an instance is an environment whose record carries an expiry.
//!
//! Contract: [Expiry](../../design/orchestration-core.md#expiry).

mod scenario;

use nyl_core::orchestration::codes::Code;
use nyl_core::orchestration::ports::{Transition, TransitionChanges};
use nyl_core::orchestration::state::{DeletionReason, Lifecycle};
use nyl_core::orchestration::time::Timestamp;
use nyl_orchestration::report::{Outcome, StateOperationReport};
use nyl_orchestration::shell::run::{Cancellation, LifecycleOptions, RunContext, RunMode, TeardownTarget};
use nyl_orchestration::shell::state::{renew_instance, Requested};
use nyl_orchestration::shell::teardown::{state_delete_detailed, Deletion};
use nyl_units::process::ProcessRunner;
use scenario::shell::{self, dev, shared_project};

fn at(text: &str) -> Timestamp {
    chrono::DateTime::parse_from_rfc3339(text).unwrap().to_utc()
}

/// A reconciled `dev` whose record claims a template and `expires_at`, as an instance's does.
fn expiring(expires_at: &str) -> shell::Harness {
    let harness = shell::Harness::new(&shared_project(), true);
    harness.init();
    assert!(harness.reconcile(&[]).errors.is_empty());
    let snapshot = harness.snapshot();
    let mut record = snapshot.record.clone().unwrap();
    record.template = Some("preview".to_owned());
    record.expires_at = Some(at(expires_at));
    let held = harness
        .store
        .take_lease(&harness.location, &dev(), &lease(&harness), None)
        .unwrap();
    let committed = harness
        .store
        .commit(
            &harness.location,
            &dev(),
            &snapshot.tips,
            &Transition {
                message: "nyl: state-init dev (staged)\n\nNyl-Operation: state-init\n".to_owned(),
                changes: TransitionChanges {
                    record: Some(record),
                    ..TransitionChanges::default()
                },
            },
            &held,
        )
        .unwrap();
    harness
        .store
        .release_lease(&harness.location, &dev(), &committed.lease_version)
        .unwrap();
    harness
}

fn lease(harness: &shell::Harness) -> nyl_core::orchestration::state::Lease {
    use nyl_core::orchestration::ports::Clock as _;
    let now = harness.clock.now();
    nyl_core::orchestration::state::Lease {
        api_version: nyl_core::orchestration::state::StateV1,
        kind: nyl_core::orchestration::state::LeaseKind,
        run: nyl_core::orchestration::ids::RunId::new("00000000-0000-7000-9000-0000000000fe").unwrap(),
        runner: "https://ci.example.com/runs/7".to_owned(),
        operation: nyl_core::orchestration::state::Operation::StateInit,
        units: Vec::new(),
        deadline: now + chrono::Duration::minutes(5),
        taken_at: now,
        broken: None,
    }
}

/// `state delete --teardown` as the removal of an instance expired at
/// `decided_on`, with `allow_teardown` as its authorization.
fn remove(harness: &shell::Harness, decided_on: &str, allow_teardown: bool) -> Deletion {
    let mut job = harness.job(&[], Cancellation::default());
    job.request.mode = RunMode::Teardown {
        target: TeardownTarget::All,
    };
    job.request.lifecycle = LifecycleOptions {
        allow_teardown,
        expired: Some(at(decided_on)),
        ..LifecycleOptions::default()
    };
    let base = harness.context();
    let tools = ProcessRunner::new();
    let context = RunContext {
        instances: &nyl_core::orchestration::instance::NO_INSTANCES,
        base: &base,
        sources: &job.sources,
        renderer: &job.renderer,
        drivers: &harness.drivers,
        tools: &tools,
        workspace: job.workspace.path(),
        process_env: &harness.process_env,
        secrets: None,
        commit_identity: Some(&harness.identity),
        progress: None,
        prompt: None,
    };
    state_delete_detailed(&context, &job.request, true)
}

fn deleting_reasons(harness: &shell::Harness) -> Vec<DeletionReason> {
    harness
        .snapshot()
        .desired
        .values()
        .filter_map(|unit| match &unit.lifecycle {
            Lifecycle::Deleting { deletion } => Some(deletion.reason),
            _ => None,
        })
        .collect()
}

/// The teardowns of an expired removal are caused by expiry, so they wait for
/// the authorization nobody gave by naming them: the units are recorded as
/// pending teardown with the reason `expired`, nothing is destroyed, and the
/// state stays; with the authorization the same removal completes and deletes
/// the state. Catches an unattended removal that destroys without
/// `allowTeardown`, and a deletion that outruns the teardowns.
#[test]
fn test_an_expired_removal_needs_allow_teardown_and_deletes_only_after_every_teardown() {
    let harness = expiring("2026-10-01T10:00:00Z");
    let waiting = remove(&harness, "2026-10-01T10:00:00Z", false);
    assert_ne!(waiting.report.outcome, Outcome::Done, "{:?}", waiting.report);
    assert_eq!(waiting.report.exit, 2, "{:?}", waiting.report);
    let teardown = waiting.teardown.expect("the teardown ran first");
    assert!(
        teardown.environments[0]
            .units
            .values()
            .all(|unit| unit.outcome_name() == Some("pending-teardown")),
        "{:?}",
        teardown.environments[0].units
    );
    let reasons = deleting_reasons(&harness);
    assert!(!reasons.is_empty() && reasons.iter().all(|reason| *reason == DeletionReason::Expired));
    assert!(harness.snapshot().record.unwrap().deleted().is_none());

    let removed = remove(&harness, "2026-10-01T10:00:00Z", true);
    // Authorized, the teardowns start; the publication's manual wait still
    // stops the removal after its first phase, so the state stays.
    let teardown = removed.teardown.expect("the teardown ran first");
    let units = &teardown.environments[0].units;
    assert_eq!(
        units[&shell::unit("kubernetes")].outcome_name(),
        Some("tearing-down"),
        "{units:?}"
    );
    assert!(harness.snapshot().record.unwrap().deleted().is_none());
}

/// The decision repeats under the lease: a record whose expiry is no longer
/// the one the removal was decided on means another run renewed the instance,
/// and nothing is torn down (`NYL-INSTANCE-RENEWED`, exit 2), even for an
/// environment without units. Catches the race between a pull request's
/// renewal and the sweep.
#[test]
fn test_a_removal_decided_on_an_older_expiry_is_refused_under_the_lease() {
    let harness = expiring("2026-10-08T10:00:00Z");
    let refused = remove(&harness, "2026-10-01T10:00:00Z", true);
    assert_eq!(refused.report.exit, 2, "{:?}", refused.report);
    let codes = refused
        .teardown
        .iter()
        .flat_map(|report| {
            report
                .environments
                .iter()
                .flat_map(|environment| environment.errors.iter())
        })
        .chain(refused.report.errors.iter())
        .map(|finding| finding.code)
        .collect::<Vec<_>>();
    assert!(codes.contains(&Code::InstanceRenewed), "{codes:?}");
    assert!(deleting_reasons(&harness).is_empty(), "nothing is marked for teardown");
    assert!(harness.snapshot().record.unwrap().deleted().is_none());
    assert!(harness.snapshot().desired.values().all(|unit| unit.unit.uid.is_some()));
}

fn renew(harness: &shell::Harness, ttl_days: Option<u64>) -> StateOperationReport {
    let ttl = ttl_days.map(|days| nyl_core::orchestration::time::Duration::from_secs(days * 86_400));
    renew_instance(
        &harness.context(),
        &dev(),
        &harness.location,
        ttl,
        &Requested::default(),
    )
}

/// Renewing moves the expiry to now plus `ttl` in one commit, writes nothing
/// when it already is that time, and applies to instances only. Catches an
/// expiry that is not recorded, a commit per no-op, and `--renew` accepted for
/// a declared environment.
#[test]
fn test_renewing_moves_the_expiry_once_and_refuses_a_declared_environment() {
    let declared = shell::Harness::new(&shared_project(), true);
    declared.init();
    let refused = renew(&declared, Some(7));
    assert_eq!(refused.errors[0].code, Code::InstanceOptionInvalid, "{refused:?}");

    let harness = expiring("2026-10-01T10:00:00Z");
    let before = harness.tips();
    let renewed = renew(&harness, Some(14));
    assert_eq!(renewed.exit, 0, "{renewed:?}");
    assert_eq!(renewed.outcome, Outcome::Done);
    let expires = harness.snapshot().record.unwrap().expires_at;
    assert_eq!(
        expires,
        Some(at("2026-10-08T10:00:00Z")),
        "the start time plus fourteen days"
    );
    assert_ne!(harness.tips(), before);

    let tips = harness.tips();
    assert_eq!(renew(&harness, Some(14)).outcome, Outcome::Unchanged);
    assert_eq!(harness.tips(), tips, "an unchanged expiry writes no commit");
}
