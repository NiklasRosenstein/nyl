//! `plan`, `reconcile`, `status`, and `get -e` of the orchestration shell,
//! through its API, in the scenario test world: real Git source resolution
//! and unit rendering, the built-in Command kind and the fake kinds, and
//! the in-memory or Git state store.

mod scenario;

use std::collections::BTreeSet;

use nyl_core::bindings::UnitInputSource;
use nyl_core::orchestration::codes::Code;
use nyl_core::orchestration::driver::Change;
use nyl_core::orchestration::ports::{SourceRepository, SourceResolver};
use nyl_git::GitAuth;
use nyl_orchestration::run_report::{EnvironmentRunReport, LeavingConsequence, UnitResult, UnitState};
use nyl_orchestration::shell::inspect::{self, GetValue};
use nyl_orchestration::shell::run::{self, Cancellation, RunMode, TeardownTarget};
use nyl_state::source::GitSourceResolver;
use scenario::format::BranchStep;
use scenario::format::CommitStep;
use scenario::shell::{
    codes, command, dev, outcome, project, set, shared_project, unit, FaultyStore, Harness, ENVIRONMENT, NYL_TOML,
};
use scenario::world::World;
use tempfile::TempDir;

fn environment(extra: &str) -> String {
    format!(
        "apiVersion: gitops.nyl/v1\nkind: Environment\nmetadata:\n  name: dev\n  labels: {{tier: dev}}\nspec:\n  unitSelector:\n    matchLabels: {{dev: 'true'}}\n{extra}"
    )
}

/// The dependency wave against the Git store, beyond what the walkthrough
/// "Successful dependency wave" checks: a plan also lists the ready units,
/// a run that changes nothing releases its lease, and once the units ran,
/// status is complete, an output resolves, and an earlier state revision
/// reads the state before the run.
#[test]
fn test_dependency_wave_runs_once_then_changes_nothing() {
    let harness = Harness::new(&shared_project(), true);
    harness.init();
    let initial = harness.tips();

    let plan = harness.plan(&[]);
    assert_eq!(plan.exit.code(), 2, "{plan:#?}");
    assert_eq!(outcome(&plan, "blocked"), set(&["database", "kubernetes"]));
    assert_eq!(outcome(&plan, "ready"), set(&["network", "web-image"]));
    assert_eq!(harness.tips(), initial, "a plan writes nothing");

    let first = harness.reconcile(&[]);
    assert_eq!(first.exit.code(), 0, "{first:#?}");
    let second = harness.reconcile(&[]);
    assert!(second.commits.is_empty(), "{second:#?}");
    assert!(harness.lease_released(), "the no-op run released its lease");

    let status = inspect::status(&harness.context(), &dev(), &harness.location);
    assert_eq!(status.exit().code(), 0, "{status:#?}");

    let snapshot = inspect::snapshot(harness.store.as_ref(), &harness.location, None).unwrap();
    let host: UnitInputSource =
        serde_json::from_value(serde_json::json!({"unit": "database", "output": "host"})).unwrap();
    assert!(matches!(
        inspect::get_value(&dev(), &snapshot, &host, false).unwrap(),
        GetValue::Value(serde_json::Value::String(_))
    ));
    let before = inspect::snapshot(harness.store.as_ref(), &harness.location, Some(&initial)).unwrap();
    assert!(before.desired.is_empty() && before.record.is_some());
}

/// `--unit` reports only the named unit, blocked until its dependencies
/// have current receipts; the walkthrough "unit-jobs" runs each unit in its
/// own invocation.
#[test]
fn test_unit_scoped_runs_report_only_the_named_unit() {
    let harness = Harness::new(&shared_project(), false);
    harness.init();
    let blocked = harness.reconcile(&["database"]);
    assert_eq!(blocked.exit.code(), 2, "{blocked:#?}");
    assert_eq!(outcome(&blocked, "blocked"), set(&["database"]));
    assert_eq!(blocked.units.len(), 1);
}

fn command_project() -> TempDir {
    project(&[
        NYL_TOML,
        ("nyl/environment.yaml", &environment("  allowUnprotectedSource: true\n")),
        (
            "nyl/units.yaml",
            "apiVersion: units.gitops.nyl/v1\nkind: Command\nmetadata:\n  name: producer\n  labels: {dev: 'true'}\nspec:\n  command: [sh, -c, 'printf ''{\"vpcId\": \"vpc-1\", \"extra\": 1}'' > \"$NYL_OUTPUTS\"']\n  outputs:\n    vpcId: {type: string}\n",
        ),
    ])
}

/// A Command unit's undeclared output is reported with
/// `NYL-OUTPUT-UNDECLARED` and left out of its receipt.
#[test]
fn test_undeclared_command_output_is_warned_about_and_not_recorded() {
    let project = command_project();
    let harness = Harness::new(project.path(), false);
    harness.init();
    let report = harness.reconcile(&[]);
    assert_eq!(report.exit.code(), 0, "{report:#?}");
    let producer = &report.units[&unit("producer")];
    assert!(matches!(producer.result, Some(UnitResult::Executed { .. })));
    assert_eq!(codes(&producer.warnings), vec![Code::OutputUndeclared]);
    assert!(producer.warnings[0].message.contains("extra"));
    let snapshot = harness.store.snapshot(&harness.location).unwrap();
    let receipt = snapshot.observed[&unit("producer")].receipt.as_ref().unwrap();
    assert_eq!(receipt.outputs, serde_json::json!({"vpcId": "vpc-1"}));
}

/// `--unit` naming no unit of the ownership set is an error, exit 1, and
/// writes nothing; a plan needs no commit identity and writes nothing.
#[test]
fn test_unknown_unit_fails_and_plan_writes_nothing() {
    let project = command_project();
    let harness = Harness::new(project.path(), false);
    harness.init();
    let before = harness.tips();

    let unknown = harness.reconcile(&["missing"]);
    assert_eq!(unknown.exit.code(), 1, "{unknown:#?}");
    assert_eq!(codes(&unknown.errors), vec![Code::UnitUnknown]);
    assert!(harness.lease_released());

    let plan = harness.plan(&[]);
    assert_eq!(plan.exit.code(), 0, "{plan:#?}");
    assert_eq!(outcome(&plan, "ready"), set(&["producer"]));
    assert_eq!(harness.tips(), before);
    assert!(harness.lease_released());
}

fn infra_project(source: &str, environment_extra: &str) -> TempDir {
    project(&[
        NYL_TOML,
        ("nyl/environment.yaml", &environment(environment_extra)),
        (
            "nyl/units.yaml",
            &format!(
                "apiVersion: units.gitops.nyl/v1\nkind: OpenTofu\nmetadata:\n  name: network\n  labels: {{dev: 'true'}}\nspec:\n  source: {source}\n  backend: {{path: '%SCENARIO_TMP%/network.tfstate'}}\n"
            ),
        ),
        ("infra/main.tf", "# v1\n"),
    ])
}

/// A unit whose source names a revision executes from that revision's
/// files: a change there alone changes its execution key.
#[test]
fn test_change_on_a_pinned_revision_changes_the_key() {
    let project = infra_project("{path: ../infra, revision: pinned}", "  allowUnprotectedSource: true\n");
    let harness = Harness::new(project.path(), false);
    harness.world.branch(&BranchStep {
        name: "pinned".to_owned(),
        from: "main".to_owned(),
    });
    harness.init();
    let executed_key = |report: &EnvironmentRunReport| match &report.units[&unit("network")].result {
        Some(UnitResult::Executed { execution_key }) => execution_key.clone(),
        other => panic!("network did not execute: {other:?}"),
    };
    let first = harness.reconcile(&[]);
    assert_eq!(first.exit.code(), 0, "{first:#?}");
    let first_key = executed_key(&first);

    harness
        .world
        .commit_files("pinned", "Change the pinned source", &[("infra/main.tf", "# v2\n")]);
    let second = harness.reconcile(&[]);
    assert_eq!(second.exit.code(), 0, "{second:#?}");
    assert_ne!(executed_key(&second), first_key);
}

/// A unit's locked commit must be reachable from protectedRefs or its own
/// revision; allowUnprotectedSource waives that only for S.
#[test]
fn test_unreachable_unit_lock_fails_despite_allow_unprotected_source() {
    let project = infra_project("{path: ../infra}", "  allowUnprotectedSource: true\n");
    let harness = Harness::new(project.path(), false);
    harness.world.branch(&BranchStep {
        name: "other".to_owned(),
        from: "main".to_owned(),
    });
    harness
        .world
        .commit_files("other", "Diverge", &[("infra/main.tf", "# other\n")]);
    let other = harness.world.rev_parse("other");
    let units = format!(
        "apiVersion: units.gitops.nyl/v1\nkind: OpenTofu\nmetadata:\n  name: network\n  labels: {{dev: 'true'}}\nspec:\n  source: {{path: ../infra, revision: main, commit: {other}}}\n  backend: {{path: '%SCENARIO_TMP%/network.tfstate'}}\n"
    );
    harness
        .world
        .commit_files("main", "Lock the source", &[("nyl/units.yaml", &units)]);
    harness.init();
    let report = harness.reconcile(&[]);
    assert_eq!(report.exit.code(), 1, "{report:#?}");
    assert_eq!(codes(&report.errors), vec![Code::SourceUnreachable]);
    assert!(report.errors[0].message.contains(&other));
    assert!(report.commits.is_empty());
    assert!(harness.lease_released());
}

fn grouped_project(group_spec: &str) -> TempDir {
    project(&[
        NYL_TOML,
        ("nyl/environment.yaml", &environment("")),
        (
            "nyl/group.yaml",
            &format!(
                "apiVersion: gitops.nyl/v1\nkind: EnvironmentGroup\nmetadata:\n  name: development\nspec:\n  environmentSelector:\n    matchLabels: {{tier: dev}}\n{group_spec}"
            ),
        ),
        (
            "nyl/units.yaml",
            "apiVersion: units.gitops.nyl/v1\nkind: Command\nmetadata:\n  name: producer\n  labels: {dev: 'true'}\nspec:\n  command: [sh, -c, 'true']\n",
        ),
    ])
}

/// An EnvironmentGroup's protectedRefs and allowUnprotectedSource reach the
/// reachability check of S through the defaults-applied entry Environment.
#[test]
fn test_group_protection_defaults_reach_the_source_check() {
    let protected = grouped_project("  protectedRefs: [release]\n");
    let harness = Harness::new(protected.path(), false);
    harness.init();
    let plan = harness.plan(&[]);
    assert_eq!(plan.exit.code(), 1, "{plan:#?}");
    assert_eq!(codes(&plan.errors), vec![Code::SourceUnreachable]);
    assert!(plan.errors[0].message.contains("release"));

    let waived = grouped_project("  protectedRefs: [release]\n  allowUnprotectedSource: true\n");
    let harness = Harness::new(waived.path(), false);
    harness.init();
    let plan = harness.plan(&[]);
    assert_eq!(plan.exit.code(), 0, "{plan:#?}");
}

/// Discovery of a project works in a worktree the source resolver checks
/// out, which is a Git repository of its own borrowing the clone's objects.
#[test]
fn test_discovery_works_in_a_source_resolver_worktree() {
    let world = World::new(&shared_project());
    let clone = world.fresh_clone("main");
    let resolver = GitSourceResolver::new(clone.join(".git"), GitAuth::none());
    let head = resolver.resolve_revision(&SourceRepository::This, "main").unwrap();
    let directory = TempDir::new().unwrap();
    let worktree = directory.path().join("worktree");
    resolver.worktree(&SourceRepository::This, &head, &worktree).unwrap();
    let inventory = nyl::gitops::discover_gitops_inventory(&worktree.join("nyl"), None).unwrap();
    let units = nyl::gitops::units::discover_units(&inventory).unwrap();
    assert_eq!(units.len(), 4);
}

/// A store failure while taking the lease for a later wave, after an
/// earlier wave's units ran and were checkpointed, keeps the run ref with
/// their checkpoints, so the next run imports them instead of executing
/// them again; the lease is released.
#[test]
fn test_store_failure_after_a_checkpoint_keeps_the_run_ref() {
    let mut harness = Harness::new(&shared_project(), false);
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    harness.store = Box::new(FaultyStore {
        lease_updates: calls.clone(),
        fail_lease_updates_from: Some(3),
        ..FaultyStore::default()
    });
    harness.init();
    // The renewals before the first resolution and the first wave succeed;
    // the renewal before re-resolving for the second wave fails.
    calls.store(0, std::sync::atomic::Ordering::SeqCst);
    let report = harness.reconcile(&[]);
    assert_eq!(report.exit.code(), 1, "{report:#?}");
    assert_eq!(codes(&report.errors), vec![Code::StateUnavailable], "{report:#?}");
    let coordination = harness.coordination();
    assert!(coordination.lease.is_none(), "the lease is released");
    let [kept] = coordination.runs.as_slice() else {
        panic!("the run ref is kept: {coordination:#?}");
    };
    assert!(kept.run.finished);
    let checkpointed: BTreeSet<String> = kept
        .checkpoints
        .iter()
        .map(|checkpoint| checkpoint.desired.unit.name.to_string())
        .collect();
    assert_eq!(checkpointed, set(&["network", "web-image"]));
}

/// A plan of `mode` with a fresh clone of `main`.
fn plan_with(harness: &Harness, mode: RunMode) -> EnvironmentRunReport {
    let mut job = harness.job(&[], Cancellation::default());
    job.request.mode = mode;
    harness
        .run_on(harness.store.as_ref(), &job, false, run::plan)
        .environments
        .remove(0)
}

fn teardown_project() -> TempDir {
    project(&[
        NYL_TOML,
        ("nyl/environment.yaml", ENVIRONMENT),
        (
            "nyl/units.yaml",
            &command("svc", "true", "  teardown: [sh, -c, 'true']\n"),
        ),
    ])
}

/// Planning, the leaving section: a unit leaving the ownership set is
/// listed with its consequence, its pending teardown and why it waits, and
/// the destroy plan `--allow-teardown` would carry out; the plan writes
/// nothing.
#[test]
fn test_plan_lists_a_leaving_unit_with_its_destroy_plan() {
    let project = teardown_project();
    let harness = Harness::new(project.path(), false);
    harness.init();
    assert_eq!(harness.reconcile(&[]).exit.code(), 0);
    let steps = TempDir::new().unwrap();
    harness.world.commit(
        &CommitStep {
            branch: "main".to_owned(),
            message: "Remove svc".to_owned(),
            apply: None,
            delete: vec!["nyl/units.yaml".to_owned()],
        },
        steps.path(),
    );
    let before = harness.tips();
    let plan = harness.plan(&[]);
    assert_eq!(plan.exit.code(), 0, "{plan:#?}");
    assert_eq!(plan.leaving[0].consequence, LeavingConsequence::Teardown);
    let svc = &plan.units[&unit("svc")];
    assert_eq!(svc.state, Some(UnitState::PendingTeardown));
    assert_eq!(svc.changes, Some(Change::Destroy));
    assert!(codes(&svc.warnings).contains(&Code::TeardownPending));
    assert_eq!(harness.tips(), before, "a plan writes nothing");
}

/// `nyl plan --teardown` previews what `nyl teardown` removes from the
/// stored documents alone (Q4): it works when source no longer renders.
#[test]
fn test_plan_teardown_previews_from_stored_documents() {
    let project = teardown_project();
    let harness = Harness::new(project.path(), false);
    harness.init();
    assert_eq!(harness.reconcile(&[]).exit.code(), 0);
    harness
        .world
        .commit_files("main", "Break source", &[("nyl/units.yaml", "kind: [\n")]);
    let before = harness.tips();
    let plan = plan_with(
        &harness,
        RunMode::Teardown {
            target: TeardownTarget::Units(std::collections::BTreeSet::from([unit("svc")])),
        },
    );
    assert!(plan.errors.is_empty(), "{plan:#?}");
    assert_eq!(plan.units[&unit("svc")].changes, Some(Change::Destroy), "{plan:#?}");
    assert_eq!(harness.tips(), before);
}
