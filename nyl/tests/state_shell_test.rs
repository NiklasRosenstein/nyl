//! The state operations of the orchestration shell, against the in-memory
//! store and the Git store: `state init`, `state forget`, `state move`,
//! `state reset`, `state restore`, and `lease break`, with the lease rules
//! they share; a broken lease refuses them until `nyl recover -e <env>`
//! imports it, which the tests run in the scenario world.

mod scenario;

use std::collections::BTreeMap;

use nyl_core::orchestration::codes::Code;
use nyl_core::orchestration::ids::{EnvironmentName, RunId, UnitName};
use nyl_core::orchestration::ports::{Clock as _, IdGenerator as _, StateStore, Transition, TransitionChanges};
use nyl_core::orchestration::state::{Lease, LeaseKind, Operation, Run, RunKind, StateEvent, StateLocation, StateV1};
use nyl_core::orchestration::time::Duration;
use nyl_drivers::process::ProcessRunner;
use nyl_git::GitAuth;
use nyl_orchestration::fault::NoFaults;
use nyl_orchestration::report::{Outcome, StateOperationReport};
use nyl_orchestration::shell::recover::recover_environment;
use nyl_orchestration::shell::run::{Cancellation, RunContext, RunMode};
use nyl_orchestration::shell::state::{self, Requested};
use nyl_orchestration::shell::{Context, Settings};
use nyl_orchestration::testing::{FakeClock, SequentialIds};
use nyl_state::git::{CommitAuthor, GitStateStore};
use nyl_state::memory::MemoryStateStore;
use scenario::shell;
use scenario::world::git_output;
use serde_json::json;
use tempfile::TempDir;

const UID: &str = "0192f0c4-8d0a-7c3e-9a51-000000000001";
const COMMIT: &str = "3e7b9c0a1d2e3f4a5b6c7d8e9f0a1b2c3d4e5f6a";
const KEY: &str = "sha256:9f2c000000000000000000000000000000000000000000000000000000000001";

struct World {
    name: &'static str,
    store: Box<dyn StateStore>,
    repository: String,
    clock: FakeClock,
    ids: SequentialIds,
    /// The bare repository of a remote Git store, for reading commits.
    bare: Option<std::path::PathBuf>,
    _directories: Vec<TempDir>,
}

fn start() -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::parse_from_rfc3339("2026-09-24T10:00:00Z")
        .unwrap()
        .to_utc()
}

fn worlds() -> Vec<World> {
    let bare = TempDir::new().unwrap();
    git2::Repository::init_bare(bare.path()).unwrap();
    let clone = TempDir::new().unwrap();
    git2::Repository::init(clone.path()).unwrap();
    vec![
        World {
            name: "memory",
            store: Box::new(MemoryStateStore::new()),
            repository: "file:///project.git".to_owned(),
            clock: FakeClock::new(start()),
            ids: SequentialIds::new(),
            bare: None,
            _directories: Vec::new(),
        },
        World {
            name: "git",
            store: Box::new(GitStateStore::new(
                clone.path().join(".git"),
                GitAuth::none(),
                CommitAuthor {
                    identity: "Scenario <scenario@example.invalid>".parse().unwrap(),
                    clock: std::sync::Arc::new(FakeClock::new(start())),
                },
            )),
            repository: format!("file://{}", bare.path().display()),
            clock: FakeClock::new(start()),
            ids: SequentialIds::new(),
            bare: Some(bare.path().to_path_buf()),
            _directories: vec![bare, clone],
        },
    ]
}

impl World {
    fn context(&self) -> Context<'_> {
        Context {
            store: self.store.as_ref(),
            clock: &self.clock,
            ids: &self.ids,
            faults: &NoFaults,
            runner: "test-runner".to_owned(),
            requested_by: "alice".to_owned(),
            version: "0.0.0-test".to_owned(),
            settings: Settings::default(),
        }
    }

    fn location(&self, desired: &str, observed: &str) -> StateLocation {
        StateLocation {
            repository: self.repository.clone(),
            desired_ref: desired.to_owned(),
            observed_ref: observed.to_owned(),
            path: String::new(),
            coordination_ref_prefix: "refs/nyl/".to_owned(),
        }
    }

    fn dev(&self) -> StateLocation {
        self.location("nyl/dev/desired", "nyl/dev/observed")
    }

    /// The message of a ref's tip commit, from the remote repository.
    fn tip_message(&self, reference: &str) -> Option<String> {
        let repository = git2::Repository::open_bare(self.bare.as_ref()?).ok()?;
        let commit = repository.find_reference(reference).ok()?.peel_to_commit().ok()?;
        commit.message().ok().map(str::to_owned)
    }
}

fn dev() -> EnvironmentName {
    EnvironmentName::new("dev").unwrap()
}

fn requested() -> Requested {
    Requested::default()
}

/// An observed unit with a receipt, as a run records it.
fn observed_unit(unit: &str) -> nyl_core::orchestration::state::ObservedUnit {
    let mut document: serde_json::Value = serde_saphyr::from_str(include_str!(
        "../../crates/nyl-core/tests/golden/state/observed-unit.v1.yaml"
    ))
    .unwrap();
    document["unit"]["name"] = json!(unit);
    serde_json::from_value(document).unwrap()
}

/// Put a unit with the given lifecycle into state directly, as a run would.
fn seed_unit(world: &World, location: &StateLocation, unit: &str, lifecycle: serde_json::Value) {
    commit_unit(world, location, unit, lifecycle, "nyl: reconcile dev\n");
}

/// A transition commit message of `run`, with the trailers every
/// transition carries, and `Nyl-Local: true` for a local run's.
fn transition_message(run: &RunId, local: bool) -> String {
    let mut message = format!(
        "nyl: reconcile dev\n\nunits: {{}}\n\nNyl-Operation: reconcile\nNyl-Run-Id: {run}\nNyl-Environment: dev\n"
    );
    if local {
        message.push_str("Nyl-Local: true\n");
    }
    message
}

/// Put an active unit into state with one commit per ref carrying `message`.
fn commit_unit(world: &World, location: &StateLocation, unit: &str, lifecycle: serde_json::Value, message: &str) {
    let identity =
        json!({"environment": "dev", "name": unit, "uid": UID, "apiVersion": "units.gitops.nyl/v1", "kind": "Command"});
    let desired = serde_json::from_value(json!({
        "apiVersion": "gitops.nyl/v1", "kind": "DesiredUnit", "unit": identity,
        "lifecycle": lifecycle, "sourceCommit": COMMIT, "sourceRevision": COMMIT,
        "spec": {"command": ["./run.sh"]}, "executionKey": KEY, "readiness": {"state": "ready"}
    }))
    .unwrap();
    let lease = Lease {
        api_version: StateV1,
        kind: LeaseKind,
        run: world.ids.run_id(),
        runner: "seed".to_owned(),
        operation: Operation::Reconcile,
        units: Vec::new(),
        deadline: world.clock.now() + chrono::Duration::minutes(5),
        taken_at: world.clock.now(),
        broken: None,
    };
    let store = world.store.as_ref();
    let version = store
        .take_lease(
            location,
            &dev(),
            &lease,
            store
                .coordination(location, &dev())
                .unwrap()
                .lease
                .map(|held| held.version)
                .as_deref(),
        )
        .unwrap();
    let base = store.snapshot(location).unwrap().tips;
    let committed = store
        .commit(
            location,
            &dev(),
            &base,
            &Transition {
                message: message.to_owned(),
                changes: TransitionChanges {
                    desired: BTreeMap::from([(UnitName::new(unit).unwrap(), Some(desired))]),
                    observed: BTreeMap::from([(UnitName::new(unit).unwrap(), Some(observed_unit(unit)))]),
                    ..TransitionChanges::default()
                },
            },
            &version,
        )
        .unwrap();
    store.release_lease(location, &dev(), &committed.lease_version).unwrap();
}

#[test]
fn test_state_init_creates_state_once_and_fresh_starts_over() {
    for world in worlds() {
        let context = world.context();
        let location = world.dev();
        let first = state::init(&context, &dev(), &location, false, &requested());
        assert_eq!(
            (first.outcome, first.exit),
            (Outcome::Done, 0),
            "{}: {first:?}",
            world.name
        );
        assert_eq!(first.commits.len(), 2, "{}: one commit per state ref", world.name);
        let snapshot = world.store.snapshot(&location).unwrap();
        let record = snapshot.record.unwrap();
        assert_eq!(record.location, location);
        assert!(
            matches!(record.events.as_slice(), [StateEvent::StateInitialized { fresh: false, by, .. }] if by == "alice")
        );

        // Running it again changes nothing and writes nothing.
        let again = state::init(&context, &dev(), &location, false, &requested());
        assert_eq!((again.outcome, again.exit), (Outcome::Unchanged, 0), "{}", world.name);
        assert_eq!(world.store.snapshot(&location).unwrap().tips, snapshot.tips);

        // A fresh start drops every unit, so each becomes a new incarnation.
        seed_unit(&world, &location, "seed", json!({"state": "active"}));
        let fresh = state::init(&context, &dev(), &location, true, &requested());
        assert_eq!(fresh.outcome, Outcome::Done, "{}: {fresh:?}", world.name);
        let snapshot = world.store.snapshot(&location).unwrap();
        assert!(snapshot.desired.is_empty(), "{}", world.name);
        assert!(matches!(
            snapshot.record.unwrap().events.last(),
            Some(StateEvent::StateInitialized { fresh: true, .. })
        ));
        // The lease was released after the operation.
        assert!(world.store.coordination(&location, &dev()).unwrap().lease.is_none());

        if let Some(message) = world.tip_message("refs/heads/nyl/dev/desired") {
            assert!(message.starts_with("nyl: state-init dev (fresh)"), "{message}");
            assert!(message.contains("\nNyl-Operation: state-init\n"), "{message}");
            assert!(message.contains("\nNyl-Requested-By: alice\n"), "{message}");
        }
    }
}

#[test]
fn test_a_held_lease_refuses_with_exit_two_or_waits() {
    for world in worlds() {
        let context = world.context();
        let location = world.dev();
        let holder = Lease {
            api_version: StateV1,
            kind: LeaseKind,
            run: world.ids.run_id(),
            runner: "https://ci.example.com/runs/1".to_owned(),
            operation: Operation::Reconcile,
            units: Vec::new(),
            deadline: world.clock.now() + chrono::Duration::minutes(5),
            taken_at: world.clock.now(),
            broken: None,
        };
        world.store.take_lease(&location, &dev(), &holder, None).unwrap();
        let refused = state::init(&context, &dev(), &location, false, &requested());
        assert_eq!(refused.exit, 2, "{}", world.name);
        assert_eq!(refused.errors[0].code, Code::LeaseHeld);
        assert!(
            refused.errors[0].message.contains("https://ci.example.com/runs/1"),
            "{refused:?}"
        );

        // A signal stops the wait ("Runs and leases" item 7): the operation
        // refuses with NYL-RUN-CANCELLED instead of waiting out --wait-lease.
        let cancellation = nyl_orchestration::shell::run::Cancellation::new();
        cancellation.cancel(nyl_orchestration::shell::run::SIGINT);
        let cancelled = state::init(
            &context,
            &dev(),
            &location,
            false,
            &Requested {
                reason: None,
                wait: Some(Duration::from_mins(30)),
                cancellation,
            },
        );
        assert_eq!(
            cancelled.errors[0].code,
            Code::RunCancelled,
            "{}: {cancelled:?}",
            world.name
        );
        assert_eq!(
            world
                .store
                .coordination(&location, &dev())
                .unwrap()
                .lease
                .unwrap()
                .lease,
            holder
        );

        // Waiting long enough outlasts the holder's deadline: the test clock
        // advances by each poll instead of sleeping.
        let waited = state::init(
            &context,
            &dev(),
            &location,
            false,
            &Requested {
                reason: None,
                wait: Some(Duration::from_mins(30)),
                ..Requested::default()
            },
        );
        assert_eq!(waited.outcome, Outcome::Done, "{}: {waited:?}", world.name);
    }
}

/// `nyl recover -e dev` through the shell, on the shared run path.
fn recover(harness: &shell::Harness) -> StateOperationReport {
    let mut job = harness.job(&[], Cancellation::default());
    job.request.mode = RunMode::Recover { unit: None };
    let base = harness.context();
    let tools = ProcessRunner::new();
    let context = RunContext {
        base: &base,
        sources: &job.sources,
        renderer: &job.renderer,
        drivers: &harness.drivers,
        tools: &tools,
        workspace: job.workspace.path(),
        process_env: &harness.process_env,
        commit_identity: Some(&harness.identity),
        observer: None,
        prompt: None,
    };
    recover_environment(&context, &job.request)
}

/// Break the lease of a holder that died without a run ref, as `nyl lease
/// break` does for a dead `operation`.
fn break_dead_holder(harness: &shell::Harness, operation: Operation) {
    let holder = Lease {
        api_version: StateV1,
        kind: LeaseKind,
        run: RunId::new("00000000-0000-7000-9000-0000000000fd").unwrap(),
        runner: "lost runner".to_owned(),
        operation,
        units: Vec::new(),
        deadline: harness.clock.now() + chrono::Duration::hours(1),
        taken_at: harness.clock.now(),
        broken: None,
    };
    harness
        .store
        .take_lease(&harness.location, &dev(), &holder, None)
        .unwrap();
    let broken = state::break_lease(
        &harness.context(),
        &dev(),
        &harness.location,
        Some("runner lost".to_owned()),
    );
    assert_eq!(broken.outcome, Outcome::Done, "{broken:?}");
    assert!(!broken.warnings.is_empty(), "breaking warns about a live holder");
}

/// "Runs and leases" items 1 and 8 (Q23): no state operation takes a
/// broken lease over. `state init`, `forget`, `move`, and `restore` each
/// exit 2 with `NYL-LEASE-BROKEN`, naming `nyl recover -e dev`, and write
/// nothing; `nyl recover -e dev` imports the run of the broken holder (a
/// `hold`, which leaves no run ref and no units), records
/// the break once in an observed commit although nothing else changed
/// (Q39), and the operations proceed.
#[test]
fn test_a_broken_lease_refuses_state_operations_until_recover_imports_it() {
    for git_store in [false, true] {
        let harness = shell::Harness::new(&shell::shared_project(), git_store);
        harness.init();
        assert_eq!(harness.reconcile(&[]).exit.code(), 0);
        break_dead_holder(&harness, Operation::Hold);
        let context = harness.context();
        let location = harness.location.clone();
        let snapshot = harness.snapshot();
        let restorable = snapshot.tips.desired.clone().unwrap();
        let mut old = location.clone();
        "nyl/old/desired".clone_into(&mut old.desired_ref);
        "nyl/old/observed".clone_into(&mut old.observed_ref);
        let database = UnitName::new("database").unwrap();

        let refusals = [
            state::init(&context, &dev(), &location, true, &requested()),
            state::forget(&context, &dev(), &location, &database, &requested()),
            state::relocate(&context, &dev(), &location, &old, &requested()),
            state::restore(&context, &dev(), &location, &restorable, &requested()),
        ];
        for refused in &refusals {
            assert_eq!(refused.exit, 2, "{git_store}: {refused:?}");
            assert_eq!(refused.errors[0].code, Code::LeaseBroken, "{refused:?}");
            assert!(refused.errors[0].fix.contains("nyl recover -e dev"), "{refused:?}");
        }
        assert_eq!(harness.tips(), snapshot.tips, "{git_store}: nothing was written");

        let recovered = recover(&harness);
        assert_eq!(recovered.exit, 0, "{git_store}: {recovered:?}");
        assert_eq!(recovered.outcome, Outcome::Done);
        assert_eq!(
            recovered.commits.keys().copied().collect::<Vec<_>>(),
            vec!["observed"],
            "{git_store}: one observed commit records the break: {recovered:?}"
        );
        assert_eq!(recovered.details["leaseBroken"]["reason"], "runner lost");
        assert_eq!(harness.snapshot().desired, snapshot.desired, "{git_store}");
        assert_eq!(harness.snapshot().observed, snapshot.observed, "{git_store}");
        if git_store {
            let message = git_output(
                &harness.world.bare(),
                &["log", "-1", "--format=%B", "refs/heads/nyl/dev/observed"],
            );
            assert!(message.starts_with("nyl: recover dev"), "{message}");
            assert!(
                message.contains("\nleaseBroken: {") && message.contains("runner lost"),
                "{message}"
            );
        }
        assert!(harness.coordination().lease.is_none());

        let forget = state::forget(&context, &dev(), &location, &database, &requested());
        assert_eq!(
            forget.errors[0].code,
            Code::StateForgetRefused,
            "{git_store}: {forget:?}"
        );
        let fresh = state::init(&context, &dev(), &location, true, &requested());
        assert_eq!(fresh.outcome, Outcome::Done, "{git_store}: {fresh:?}");
    }
}

/// `nyl lease break` also breaks a live run whose lease ref is gone; a
/// state operation then refuses until `nyl recover -e dev` imports and
/// consumes that run's ref.
#[test]
fn test_lease_break_also_breaks_a_live_run_without_a_lease() {
    let harness = shell::Harness::new(&shell::shared_project(), false);
    harness.init();
    let context = harness.context();
    let location = harness.location.clone();
    // The runner died after its lease ref was deleted; its run ref lives on.
    let run = Run {
        api_version: StateV1,
        kind: RunKind,
        run: RunId::new("00000000-0000-7000-9000-0000000000fc").unwrap(),
        runner: "lost runner".to_owned(),
        operation: Operation::Reconcile,
        source_commit: None,
        read_desired: None,
        read_observed: None,
        deadline: harness.clock.now() + chrono::Duration::hours(3),
        units: Vec::new(),
        finished: false,
    };
    harness.store.write_run(&location, &dev(), &run, None).unwrap();
    let held = state::init(&context, &dev(), &location, true, &requested());
    assert_eq!(
        held.errors[0].code,
        Code::LeaseHeld,
        "the live run holds the environment"
    );

    let broken = state::break_lease(&context, &dev(), &location, Some("runner lost".to_owned()));
    assert_eq!(broken.outcome, Outcome::Done, "{broken:?}");
    let refused = state::init(&context, &dev(), &location, true, &requested());
    assert_eq!(refused.errors[0].code, Code::LeaseBroken, "{refused:?}");

    let recovered = recover(&harness);
    assert_eq!(recovered.exit, 0, "{recovered:?}");
    let coordination = harness.coordination();
    assert!(
        coordination.lease.is_none() && coordination.runs.is_empty(),
        "the run ref is consumed: {coordination:?}"
    );
    let fresh = state::init(&context, &dev(), &location, true, &requested());
    assert_eq!(fresh.outcome, Outcome::Done, "{fresh:?}");
}

#[test]
fn test_state_forget_drops_only_retained_or_pending_teardown_units() {
    for world in worlds() {
        let context = world.context();
        let location = world.dev();
        state::init(&context, &dev(), &location, false, &requested());
        seed_unit(&world, &location, "worker", json!({"state": "active"}));
        let refused = state::forget(
            &context,
            &dev(),
            &location,
            &UnitName::new("worker").unwrap(),
            &requested(),
        );
        assert_eq!(refused.errors[0].code, Code::StateForgetRefused, "{}", world.name);
        let unknown = state::forget(
            &context,
            &dev(),
            &location,
            &UnitName::new("nothing").unwrap(),
            &requested(),
        );
        assert_eq!(unknown.errors[0].code, Code::UnitUnknown, "{}", world.name);

        seed_unit(
            &world,
            &location,
            "cache",
            json!({"state": "deleting", "deletion": {"reason": "omission", "intent": "teardown", "teardown": "pending"}}),
        );
        let forgotten = state::forget(
            &context,
            &dev(),
            &location,
            &UnitName::new("cache").unwrap(),
            &requested(),
        );
        assert_eq!(forgotten.outcome, Outcome::Done, "{}: {forgotten:?}", world.name);
        let snapshot = world.store.snapshot(&location).unwrap();
        assert!(!snapshot.desired.contains_key(&UnitName::new("cache").unwrap()));
        assert!(snapshot.desired.contains_key(&UnitName::new("worker").unwrap()));
    }
}

/// Local transitions pushed to the remote state refs by hand stop every
/// state-writing operation, naming the commit to restore; `state restore`
/// writes forward commits with that commit's content, records the rejected
/// local transitions and the `state-restored` event, and then changes
/// nothing when run again.
#[test]
fn test_state_restore_reverts_pushed_local_transitions_with_forward_commits() {
    for world in worlds() {
        let context = world.context();
        let location = world.dev();
        state::init(&context, &dev(), &location, false, &requested());
        let active = || json!({"state": "active"});
        commit_unit(
            &world,
            &location,
            "web",
            active(),
            &transition_message(&world.ids.run_id(), false),
        );
        let restorable = world.store.snapshot(&location).unwrap().tips.desired.unwrap();
        commit_unit(
            &world,
            &location,
            "extra",
            active(),
            &transition_message(&world.ids.run_id(), true),
        );

        let web = UnitName::new("web").unwrap();
        let refused = state::forget(&context, &dev(), &location, &web, &requested());
        assert_eq!(
            refused.errors[0].code,
            Code::StateLocalTip,
            "{}: {refused:?}",
            world.name
        );
        assert!(
            refused.errors[0].fix.contains(&format!("--to {restorable}")),
            "{}: {}",
            world.name,
            refused.errors[0].fix
        );

        let restored = state::restore(&context, &dev(), &location, &restorable, &requested());
        assert_eq!(restored.outcome, Outcome::Done, "{}: {restored:?}", world.name);
        assert_eq!(restored.commits.len(), 2, "{}: {restored:?}", world.name);
        assert_eq!(
            restored.details["rejected"].as_array().unwrap().len(),
            2,
            "{}",
            world.name
        );
        let snapshot = world.store.snapshot(&location).unwrap();
        fn units<T>(map: &BTreeMap<UnitName, T>) -> Vec<String> {
            map.keys().map(ToString::to_string).collect()
        }
        assert_eq!(units(&snapshot.desired), vec!["web"], "{}", world.name);
        assert_eq!(units(&snapshot.observed), vec!["web"], "{}", world.name);
        assert!(matches!(
            snapshot.record.as_ref().unwrap().events.last(),
            Some(StateEvent::StateRestored { to, .. }) if *to == restorable
        ));
        if let Some(message) = world.tip_message("refs/heads/nyl/dev/observed") {
            assert!(message.contains("Nyl-Operation: state-restore"), "{message}");
        }
        let again = state::restore(&context, &dev(), &location, &restorable, &requested());
        assert_eq!(again.outcome, Outcome::Unchanged, "{}: {again:?}", world.name);
        // Operations proceed again.
        let forget = state::forget(&context, &dev(), &location, &web, &requested());
        assert_eq!(forget.errors[0].code, Code::StateForgetRefused, "{}", world.name);
    }
}

/// `state reset --local` changes only the clone's branches of remote
/// state: it refuses state that lives on them, and changes nothing while
/// they follow the remote.
#[test]
fn test_state_reset_needs_remote_state_and_changes_nothing_in_sync() {
    for world in worlds() {
        let context = world.context();
        let location = world.dev();
        state::init(&context, &dev(), &location, false, &requested());
        let unchanged = state::reset(&context, &dev(), &location, None);
        assert_eq!(unchanged.outcome, Outcome::Unchanged, "{}: {unchanged:?}", world.name);
        let mut local = location.clone();
        StateLocation::LOCAL_REPOSITORY.clone_into(&mut local.repository);
        let refused = state::reset(&context, &dev(), &local, None);
        assert_eq!(refused.errors[0].code, Code::StateResetUnsupported, "{}", world.name);
        assert!(refused.errors[0].fix.contains("--from local"));
    }
}

#[test]
fn test_operations_refuse_a_missing_or_foreign_state() {
    for world in worlds() {
        let context = world.context();
        let location = world.dev();
        let missing = state::forget(
            &context,
            &dev(),
            &location,
            &UnitName::new("cache").unwrap(),
            &requested(),
        );
        assert_eq!(missing.errors[0].code, Code::StateMissing, "{}", world.name);
        assert!(missing.errors[0].fix.contains("nyl state init -e dev"));

        let prod = EnvironmentName::new("prod").unwrap();
        state::init(&context, &prod, &location, false, &requested());
        let foreign = state::init(&context, &dev(), &location, false, &requested());
        assert_eq!(foreign.errors[0].code, Code::StateEnvironmentMismatch, "{}", world.name);
    }
}

#[test]
fn test_state_move_keeps_history_marks_the_old_location_and_completes_when_repeated() {
    for world in worlds() {
        let context = world.context();
        let old = world.dev();
        state::init(&context, &dev(), &old, false, &requested());
        seed_unit(&world, &old, "seed", json!({"state": "active"}));
        let new = world.location("state/dev/desired", "state/dev/observed");

        let moved = state::relocate(&context, &dev(), &new, &old, &requested());
        assert_eq!(moved.outcome, Outcome::Done, "{}: {moved:?}", world.name);
        let snapshot = world.store.snapshot(&new).unwrap();
        assert!(
            snapshot.desired.contains_key(&UnitName::new("seed").unwrap()),
            "{}: units keep their state",
            world.name
        );
        let record = snapshot.record.unwrap();
        assert_eq!(record.location, new);
        nyl_orchestration::rules::state_location::check_record(Some(&record), &dev(), &new).unwrap();

        // Every command against the old location now refuses and names the new one.
        let old_record = world.store.snapshot(&old).unwrap().record.unwrap();
        let finding =
            nyl_orchestration::rules::state_location::check_record(Some(&old_record), &dev(), &old).unwrap_err();
        assert_eq!(finding.code, Code::StateMoved, "{}", world.name);
        assert!(finding.message.contains("state/dev/desired"), "{finding:?}");

        let again = state::relocate(&context, &dev(), &new, &old, &requested());
        assert_eq!(again.outcome, Outcome::Unchanged, "{}: {again:?}", world.name);
    }
}

#[test]
fn test_state_move_onto_one_ref_keeps_desired_and_observed_state() {
    for world in worlds() {
        let context = world.context();
        let old = world.dev();
        state::init(&context, &dev(), &old, false, &requested());
        seed_unit(&world, &old, "seed", json!({"state": "active"}));
        let new = world.location("state/dev", "state/dev");

        let moved = state::relocate(&context, &dev(), &new, &old, &requested());
        assert_eq!(moved.outcome, Outcome::Done, "{}: {moved:?}", world.name);
        let snapshot = world.store.snapshot(&new).unwrap();
        let seed = UnitName::new("seed").unwrap();
        assert!(snapshot.desired.contains_key(&seed), "{}", world.name);
        assert!(
            snapshot.observed.contains_key(&seed),
            "{}: receipts move too",
            world.name
        );
    }
}

#[test]
fn test_state_move_refuses_a_location_holding_other_state() {
    for world in worlds() {
        let context = world.context();
        let old = world.dev();
        state::init(&context, &dev(), &old, false, &requested());
        let mut new = world.location("shared/desired", "shared/observed");
        new.path = "envs/dev".to_owned();
        let prod = EnvironmentName::new("prod").unwrap();
        state::init(&context, &prod, &new, false, &requested());

        let moved = state::relocate(&context, &dev(), &new, &old, &requested());
        assert_eq!(
            moved.errors[0].code,
            Code::StateLocationMismatch,
            "{}: {moved:?}",
            world.name
        );
        // The old location stays usable: nothing marked it as moved.
        let record = world.store.snapshot(&old).unwrap().record.unwrap();
        nyl_orchestration::rules::state_location::check_record(Some(&record), &dev(), &old).unwrap();
    }
}

#[test]
fn test_moving_the_coordination_prefix_records_it_in_state() {
    for world in worlds() {
        let context = world.context();
        let old = world.dev();
        state::init(&context, &dev(), &old, false, &requested());
        let mut new = old.clone();
        new.coordination_ref_prefix = "refs/heads/nyl-run/".to_owned();
        // Before the move, the configured prefix does not match state.yaml.
        let record = world.store.snapshot(&new).unwrap().record.unwrap();
        assert_eq!(
            nyl_orchestration::rules::state_location::check_record(Some(&record), &dev(), &new)
                .unwrap_err()
                .code,
            Code::StateLocationMismatch
        );
        let moved = state::relocate(&context, &dev(), &new, &old, &requested());
        assert_eq!(moved.outcome, Outcome::Done, "{}: {moved:?}", world.name);
        let record = world.store.snapshot(&new).unwrap().record.unwrap();
        nyl_orchestration::rules::state_location::check_record(Some(&record), &dev(), &new).unwrap();
    }
}
