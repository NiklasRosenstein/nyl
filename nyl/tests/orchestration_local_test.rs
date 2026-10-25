//! Local state next to remote state, through the orchestration shell's API
//! in the scenario test world: the Git state store works in a clone of
//! `project.git`, whose own `nyl/dev/*` branches are the local view.
//!
//! Contract: [Local runs](../../design/orchestration-core.md#local-runs).

mod scenario;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use nyl_core::orchestration::codes::Code;
use nyl_core::orchestration::ids::{CommitId, RunId, UnitName};
use nyl_core::orchestration::ports::IdGenerator as _;
use nyl_core::orchestration::ports::StateStore;
use nyl_core::orchestration::state::{Lease, LeaseKind, Operation, Run, RunKind, StateEvent, StateLocation, StateV1};
use nyl_orchestration::fault::{FaultAction, FaultInjector, FaultPoint};
use nyl_orchestration::report::Outcome;
use nyl_orchestration::run_report::EnvironmentRunReport;
use nyl_orchestration::shell::inspect;
use nyl_orchestration::shell::local::{LeaseScope, LocalRun, LocalStore};
use nyl_orchestration::shell::run::{self, Cancellation};
use nyl_orchestration::shell::state::{self as state_ops, Requested};
use nyl_state::git::GitStateStore;
use scenario::shell::{self, codes, dev, git_store, outcome, set, shared_project, start};
use scenario::world::{git, git_output, lease};

const DESIRED: &str = "refs/heads/nyl/dev/desired";
const OBSERVED: &str = "refs/heads/nyl/dev/observed";
const UNITS: [&str; 4] = ["database", "kubernetes", "network", "web-image"];

fn local_run(lease: LeaseScope) -> Option<LocalRun> {
    Some(LocalRun {
        lease,
        approve: BTreeSet::new(),
    })
}

/// The walkthroughs' project with its state at `project.git`, and a Git
/// state store working in its own clone.
struct Harness {
    shell: shell::Harness,
    /// The clone the store works in, whose `nyl/dev/*` branches are local state.
    clone: PathBuf,
}

impl std::ops::Deref for Harness {
    type Target = shell::Harness;

    fn deref(&self) -> &shell::Harness {
        &self.shell
    }
}

impl std::ops::DerefMut for Harness {
    fn deref_mut(&mut self) -> &mut shell::Harness {
        &mut self.shell
    }
}

impl Harness {
    fn new() -> Self {
        let shell = shell::Harness::new(&shared_project(), true);
        let clone = shell.store_clone.clone().unwrap();
        Self { shell, clone }
    }

    /// The same location on the store clone's own branches.
    fn local_location(&self) -> StateLocation {
        let mut location = self.location.clone();
        StateLocation::LOCAL_REPOSITORY.clone_into(&mut location.repository);
        location
    }

    /// One reconcile from a fresh clone of `main`, against `location`; a
    /// `--local` run works through a [`LocalStore`] as the command line wires it.
    fn reconcile_at(&self, location: &StateLocation, local: Option<LocalRun>) -> EnvironmentRunReport {
        let mut job = self.job(&[], Cancellation::default());
        job.request.location = location.clone();
        job.request.local.clone_from(&local);
        let local_store = local.map(|local| LocalStore::new(self.store.as_ref(), local));
        let store = local_store
            .as_ref()
            .map_or(self.store.as_ref(), |local| local as &dyn StateStore);
        self.run_on(store, &job, true, run::reconcile).environments.remove(0)
    }

    fn reconcile(&self, local: Option<LocalRun>) -> EnvironmentRunReport {
        self.reconcile_at(&self.location, local)
    }

    /// A ref of `project.git`, the remote state repository.
    fn remote_ref(&self, reference: &str) -> Option<String> {
        git2::Repository::open_bare(self.world.bare())
            .unwrap()
            .refname_to_id(reference)
            .ok()
            .map(|oid| oid.to_string())
    }

    /// A ref of the store's clone, such as a local state branch.
    fn local_ref(&self, reference: &str) -> Option<String> {
        git2::Repository::open(&self.clone)
            .unwrap()
            .refname_to_id(reference)
            .ok()
            .map(|oid| oid.to_string())
    }

    fn tip_message(&self, repository: &Path, reference: &str) -> String {
        git_output(repository, &["log", "-1", "--format=%B", reference])
    }

    fn lease(&self, run: &RunId, deadline: chrono::DateTime<chrono::Utc>) -> Lease {
        Lease {
            api_version: StateV1,
            kind: LeaseKind,
            run: run.clone(),
            runner: "https://ci.example.com/runs/1".to_owned(),
            operation: Operation::Reconcile,
            units: vec![UnitName::new("network").unwrap()],
            deadline,
            taken_at: start(),
            broken: None,
        }
    }

    fn run_record(&self, run: &RunId, deadline: chrono::DateTime<chrono::Utc>) -> Run {
        Run {
            api_version: StateV1,
            kind: RunKind,
            run: run.clone(),
            runner: "https://ci.example.com/runs/1".to_owned(),
            operation: Operation::Reconcile,
            source_commit: None,
            read_desired: None,
            read_observed: None,
            deadline,
            units: vec![UnitName::new("network").unwrap()],
            finished: false,
        }
    }

    /// A CI job's store, in a clone of its own.
    fn ci_store(&self) -> GitStateStore {
        git_store(&self.world.fresh_clone("main"), &self.clock)
    }
}

fn executed(report: &EnvironmentRunReport) -> BTreeSet<String> {
    outcome(report, "executed")
}

fn all_units() -> BTreeSet<String> {
    set(&UNITS)
}

/// Reads the remote lease whenever a unit starts executing.
struct LeaseProbe {
    bare: PathBuf,
    seen: Arc<Mutex<Vec<Lease>>>,
}

impl FaultInjector for LeaseProbe {
    fn at(&self, point: FaultPoint, _unit: Option<&UnitName>) -> FaultAction {
        if point == FaultPoint::Executing {
            if let Some(lease) = lease(&self.bare) {
                self.seen.lock().unwrap().push(lease);
            }
        }
        FaultAction::Continue
    }
}

/// A `--local` run executes against the clone's local branches: it commits
/// there with `Nyl-Local: true`, leaves the remote state refs and lease as
/// they were, holds the remote lease without units while it runs, and warns
/// about every unit it executed. `status --local` shows the local view.
/// Once the remote moves, `status` warns that the local branches diverged,
/// and `state reset --local` follows the remote again only with
/// `--discard-all`.
#[test]
fn test_local_run_commits_only_locally_and_reset_follows_the_remote_again() {
    let mut harness = Harness::new();
    harness.init();
    let remote_desired = harness.remote_ref(DESIRED);
    let remote_observed = harness.remote_ref(OBSERVED);

    let seen = Arc::default();
    harness.faults = Box::new(LeaseProbe {
        bare: harness.world.bare(),
        seen: Arc::clone(&seen),
    });
    let report = harness.reconcile(local_run(LeaseScope::Remote));
    assert_eq!(report.exit.code(), 0, "{report:#?}");
    assert_eq!(executed(&report), all_units());
    // One warning per executed unit, naming the remote command that records it.
    let unrecorded = report
        .warnings
        .iter()
        .filter(|finding| finding.code == Code::LocalRunUnrecorded)
        .collect::<Vec<_>>();
    assert_eq!(unrecorded.len(), 4, "{:#?}", report.warnings);
    assert!(unrecorded
        .iter()
        .any(|finding| finding.fix.contains("nyl reconcile -e dev --unit network")));
    // The remote lease was held while units ran, without them.
    let seen = seen.lock().unwrap().clone();
    assert!(!seen.is_empty(), "the remote lease was held during the run");
    assert!(seen.iter().all(|lease| lease.units.is_empty()), "{seen:#?}");

    // Remote state and coordination are untouched; local branches moved.
    assert_eq!(harness.remote_ref(DESIRED), remote_desired);
    assert_eq!(harness.remote_ref(OBSERVED), remote_observed);
    assert!(git_output(&harness.world.bare(), &["for-each-ref", "refs/nyl/"]).is_empty());
    assert_ne!(harness.local_ref(DESIRED), remote_desired);
    assert!(harness.tip_message(&harness.clone, DESIRED).contains("Nyl-Local: true"));
    assert!(harness
        .tip_message(&harness.clone, OBSERVED)
        .contains("Nyl-Local: true"));
    assert!(git_output(&harness.clone, &["for-each-ref", "refs/nyl/"]).is_empty());

    // `status --local` shows the local view; `status` the remote one.
    let local_store = LocalStore::new(harness.store.as_ref(), local_run(LeaseScope::Remote).unwrap());
    let local_status = inspect::status(&harness.context_on(&local_store), &dev(), &harness.location);
    assert_eq!(local_status.exit().code(), 0, "{local_status:#?}");
    assert_eq!(local_status.environments[0].units.len(), 4, "{local_status:#?}");
    let remote_status = inspect::status(&harness.context(), &dev(), &harness.location);
    assert!(remote_status.environments[0].units.is_empty(), "{remote_status:#?}");
    assert_eq!(
        remote_status.environments[0].warnings[0].message,
        "local state of dev has 1 local transition the remote does not record"
    );

    // The remote moves: an ordinary run records the units there.
    let ordinary = harness.reconcile(None);
    assert_eq!(ordinary.exit.code(), 0, "{ordinary:#?}");
    assert_eq!(
        executed(&ordinary),
        all_units(),
        "remote runs do not notice local effects"
    );
    let status = inspect::status(&harness.context(), &dev(), &harness.location);
    let diverged = status.environments[0]
        .warnings
        .iter()
        .find(|finding| finding.code == Code::StateLocalDiverged)
        .unwrap_or_else(|| panic!("{status:#?}"));
    assert_eq!(
        diverged.message,
        "local state of dev has 1 local transition and is 1 behind the remote"
    );
    assert_eq!(status.exit().code(), 0, "divergence is a warning");

    // Reset forgets the local receipts only when told to.
    let context = harness.context();
    let refused = state_ops::reset(&context, &dev(), &harness.location, None);
    assert_eq!(refused.exit, 1, "{refused:?}");
    assert_eq!(codes(&refused.errors), vec![Code::StateResetUndiscarded]);
    assert!(refused.errors[0]
        .message
        .contains("database, kubernetes, network, web-image"));
    let reset = state_ops::reset(
        &context,
        &dev(),
        &harness.location,
        Some(&nyl_orchestration::rules::local::Discard::All),
    );
    assert_eq!(reset.outcome, Outcome::Done, "{reset:?}");
    assert_eq!(reset.details["dropped"].as_array().unwrap().len(), 2, "{reset:?}");
    assert_eq!(harness.local_ref(DESIRED), harness.remote_ref(DESIRED));
    assert_eq!(harness.local_ref(OBSERVED), harness.remote_ref(OBSERVED));
    let status = inspect::status(&context, &dev(), &harness.location);
    assert!(!codes(&status.environments[0].warnings).contains(&Code::StateLocalDiverged));
    let again = state_ops::reset(&context, &dev(), &harness.location, None);
    assert_eq!(again.outcome, Outcome::Unchanged, "{again:?}");
}

/// A CI run holds the remote lease and has a live run ref: a `--local` run
/// waits (exit 2, `NYL-LEASE-HELD`), and `--local-lease` runs anyway, with
/// the clone's own lease, leaving the CI run's refs alone.
#[test]
fn test_local_run_waits_for_a_ci_run_unless_it_takes_the_local_lease() {
    let harness = Harness::new();
    harness.init();
    let ci = harness.ci_store();
    let run = harness.ids.run_id();
    let deadline = start() + chrono::Duration::hours(1);
    ci.take_lease(&harness.location, &dev(), &harness.lease(&run, deadline), None)
        .unwrap();
    ci.write_run(&harness.location, &dev(), &harness.run_record(&run, deadline), None)
        .unwrap();
    let lease_ref = "refs/nyl/dev/lease";
    let run_ref = format!("refs/nyl/dev/runs/{run}");
    let before = (harness.remote_ref(lease_ref), harness.remote_ref(&run_ref));
    assert!(before.0.is_some() && before.1.is_some());

    let held = harness.reconcile(local_run(LeaseScope::Remote));
    assert_eq!(held.exit.code(), 2, "{held:#?}");
    assert_eq!(codes(&held.errors), vec![Code::LeaseHeld]);
    assert!(executed(&held).is_empty());

    let anyway = harness.reconcile(local_run(LeaseScope::Local));
    assert_eq!(anyway.exit.code(), 0, "{anyway:#?}");
    assert_eq!(executed(&anyway), all_units());
    assert_eq!((harness.remote_ref(lease_ref), harness.remote_ref(&run_ref)), before);
    // The clone's own lease was released.
    assert!(git_output(&harness.clone, &["for-each-ref", "refs/nyl/"]).is_empty());
}

/// A dead CI run's ref awaits takeover by remote state: a `--local` run,
/// which never imports remote run refs, refuses and leaves it in place.
#[test]
fn test_local_run_refuses_while_a_dead_remote_run_awaits_takeover() {
    let harness = Harness::new();
    harness.init();
    let ci = harness.ci_store();
    let run = harness.ids.run_id();
    ci.write_run(
        &harness.location,
        &dev(),
        &harness.run_record(&run, start() - chrono::Duration::minutes(1)),
        None,
    )
    .unwrap();
    let run_ref = format!("refs/nyl/dev/runs/{run}");
    let before = harness.remote_ref(&run_ref);

    let refused = harness.reconcile(local_run(LeaseScope::Remote));
    assert_eq!(refused.exit.code(), 1, "{refused:#?}");
    assert_eq!(codes(&refused.errors), vec![Code::LocalRunLeftovers]);
    assert!(refused.errors[0].message.contains(run.as_str()));
    assert!(executed(&refused).is_empty());
    assert_eq!(harness.remote_ref(&run_ref), before);
    assert!(harness.remote_ref("refs/nyl/dev/lease").is_none());
    assert_eq!(harness.local_ref(DESIRED), harness.remote_ref(DESIRED));
}

/// After `nyl lease break`, the lease rule lets a run take over at once,
/// even while the broken holder's run ref is within its deadline. A
/// `--local` run, which never imports that run ref, refuses before taking
/// the lease, so the break stays for the next remote run to take over.
#[test]
fn test_local_run_refuses_after_a_lease_break_and_keeps_the_break() {
    let harness = Harness::new();
    harness.init();
    let ci = harness.ci_store();
    let run = harness.ids.run_id();
    let deadline = start() + chrono::Duration::hours(1);
    ci.take_lease(&harness.location, &dev(), &harness.lease(&run, deadline), None)
        .unwrap();
    ci.write_run(&harness.location, &dev(), &harness.run_record(&run, deadline), None)
        .unwrap();
    let context = harness.context();
    let broken = state_ops::break_lease(&context, &dev(), &harness.location, Some("runner gone".to_owned()));
    assert_eq!(broken.outcome, Outcome::Done, "{broken:?}");
    let lease_ref = "refs/nyl/dev/lease";
    let run_ref = format!("refs/nyl/dev/runs/{run}");
    let before = (harness.remote_ref(lease_ref), harness.remote_ref(&run_ref));
    assert!(before.0.is_some() && before.1.is_some());

    let refused = harness.reconcile(local_run(LeaseScope::Remote));
    assert_eq!(refused.exit.code(), 1, "{refused:#?}");
    assert_eq!(codes(&refused.errors), vec![Code::LocalRunLeftovers]);
    assert!(refused.errors[0].message.contains(run.as_str()));
    assert!(executed(&refused).is_empty());
    assert_eq!((harness.remote_ref(lease_ref), harness.remote_ref(&run_ref)), before);

    // The break stands: an ordinary run takes over at once.
    let takeover = harness.reconcile(None);
    assert!(!codes(&takeover.errors).contains(&Code::LeaseHeld), "{takeover:#?}");
}

/// Local branches pushed by hand put `Nyl-Local` commits at the remote
/// tips: the next ordinary run refuses, naming the commit to restore, and
/// `state restore` writes forward commits restoring it, after which runs
/// proceed from that content.
#[test]
fn test_pushed_local_transitions_refuse_runs_until_restored() {
    let harness = Harness::new();
    harness.init();
    let init_desired = harness.remote_ref(DESIRED).unwrap();
    let local = harness.reconcile(local_run(LeaseScope::Remote));
    assert_eq!(local.exit.code(), 0, "{local:#?}");
    git(
        &harness.clone,
        &[
            "push",
            "--quiet",
            harness.world.project_url().as_str(),
            &format!("{DESIRED}:{DESIRED}"),
            &format!("{OBSERVED}:{OBSERVED}"),
        ],
    );
    let pushed = harness.remote_ref(DESIRED).unwrap();

    let refused = harness.reconcile(None);
    assert_eq!(refused.exit.code(), 1, "{refused:#?}");
    assert_eq!(codes(&refused.errors), vec![Code::StateLocalTip]);
    let fix = &refused.errors[0].fix;
    assert!(
        fix.contains(&format!("nyl state restore -e dev --to {init_desired}")),
        "{fix}"
    );
    assert!(executed(&refused).is_empty());

    // How state operations restore it is state_shell_test's; here the run
    // path refuses and then proceeds from the restored content.
    let context = harness.context();
    // The pushed local commit itself is never restored.
    let local_commit = CommitId::new(pushed.clone()).unwrap();
    let invalid = state_ops::restore(
        &context,
        &dev(),
        &harness.location,
        &local_commit,
        &Requested::default(),
    );
    assert_eq!(codes(&invalid.errors), vec![Code::StateRestoreInvalid]);

    let to = CommitId::new(init_desired.clone()).unwrap();
    let restored = state_ops::restore(&context, &dev(), &harness.location, &to, &Requested::default());
    assert_eq!(restored.outcome, Outcome::Done, "{restored:?}");
    let desired_tip = harness.remote_ref(DESIRED).unwrap();
    // Forward, on top of the pushed commit: never a force push.
    assert_eq!(
        git_output(&harness.world.bare(), &["rev-parse", &format!("{desired_tip}~1")]),
        pushed
    );
    let message = harness.tip_message(&harness.world.bare(), DESIRED);
    assert!(message.contains("Nyl-Operation: state-restore"), "{message}");
    assert!(!message.contains("Nyl-Local"), "{message}");

    let proceeds = harness.reconcile(None);
    assert_eq!(proceeds.exit.code(), 0, "{proceeds:#?}");
    assert_eq!(executed(&proceeds), all_units());
}

/// Without a remote, the clone's branches are the state and runs take the
/// clone's lease. Once the state repository resolves to a remote, a run
/// refuses there until `state move --from local` pushes the history, records
/// the move at the remote, and leaves the local branches following it.
#[test]
fn test_state_without_a_remote_moves_to_one_with_its_history() {
    let harness = Harness::new();
    let local = harness.local_location();
    harness.init_at(&local);
    let first = harness.reconcile_at(&local, None);
    assert_eq!(first.exit.code(), 0, "{first:#?}");
    assert_eq!(executed(&first), all_units());
    assert!(harness.remote_ref(DESIRED).is_none());
    assert!(git_output(&harness.clone, &["for-each-ref", "refs/nyl/"]).is_empty());
    let local_tip = harness.local_ref(DESIRED).unwrap();
    // `--local` needs remote state.
    let unsupported = harness.reconcile_at(&local, local_run(LeaseScope::Remote));
    assert_eq!(unsupported.exit.code(), 1, "{unsupported:#?}");

    let missing = harness.reconcile(None);
    assert_eq!(codes(&missing.errors), vec![Code::StateMissing]);
    assert_eq!(
        missing.errors[0].fix,
        "move the state and its history to the remote with `nyl state move -e dev --from local`"
    );
    // `status` explains it the same way.
    let status = inspect::status(&harness.context(), &dev(), &harness.location);
    assert!(
        status.environments[0].errors[0]
            .fix
            .contains("nyl state move -e dev --from local"),
        "{status:#?}"
    );

    let context = harness.context();
    let moved = state_ops::relocate(&context, &dev(), &harness.location, &local, &Requested::default());
    assert_eq!(moved.outcome, Outcome::Done, "{moved:?}");
    let remote_tip = harness.remote_ref(DESIRED).unwrap();
    assert_eq!(
        git_output(&harness.world.bare(), &["rev-parse", &format!("{remote_tip}~1")]),
        local_tip,
        "the history moved"
    );
    assert_eq!(harness.local_ref(DESIRED), Some(remote_tip));
    let record = harness.store.snapshot(&harness.location).unwrap().record.unwrap();
    assert_eq!(record.location, harness.location);
    assert!(matches!(record.events.last(), Some(StateEvent::StateMoved { .. })));

    let after = harness.reconcile(None);
    assert_eq!(after.exit.code(), 0, "{after:#?}");
    assert!(executed(&after).is_empty(), "receipts moved with the state: {after:#?}");
}
