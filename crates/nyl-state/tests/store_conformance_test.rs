//! One conformance suite for every `StateStore`: the in-memory store, and the
//! Git store against a remote bare repository and against the clone's own
//! branches. The walkthroughs run against the first and the reference
//! scenarios against the others, so they must agree on every rule here.

use std::collections::BTreeMap;

use nyl_core::orchestration::ids::{EnvironmentName, RunId, UnitName};
use nyl_core::orchestration::ports::{Checkpoint, StateError, StateStore, StateTips, Transition, TransitionChanges};
use nyl_core::orchestration::state::{
    ApprovalIdentity, DesiredUnit, Lease, LeaseKind, ObservedUnit, Operation, Run, RunKind, Signal, SignalKind,
    StateLocation, StateRecord, StateRecordKind, StateV1,
};
use nyl_git::GitAuth;
use nyl_state::git::{CommitSigner, GitStateStore, LOCAL_REPOSITORY};
use nyl_state::memory::MemoryStateStore;
use serde_json::json;
use tempfile::TempDir;

/// A clock standing still, so commits are reproducible.
struct FixedClock;

impl nyl_core::orchestration::ports::Clock for FixedClock {
    fn now(&self) -> nyl_core::orchestration::time::Timestamp {
        chrono::DateTime::parse_from_rfc3339("2026-09-24T10:00:00Z")
            .unwrap()
            .to_utc()
    }

    fn wait(&self, _duration: nyl_core::orchestration::time::Duration) {}
}

fn signer() -> CommitSigner {
    CommitSigner {
        identity: "Scenario <scenario@example.invalid>".parse().unwrap(),
        clock: std::sync::Arc::new(FixedClock),
    }
}

const RUN_A: &str = "0192f0c4-8d0a-7c3e-9a51-3c1f0e7d2b90";
const RUN_B: &str = "0192f0c4-8d0a-7c3e-9a51-3c1f0e7d2b91";
const UID: &str = "0192f0c4-8d0a-7c3e-9a51-000000000001";
const COMMIT: &str = "3e7b9c0a1d2e3f4a5b6c7d8e9f0a1b2c3d4e5f6a";
const KEY: &str = "sha256:9f2c000000000000000000000000000000000000000000000000000000000001";

/// A store under test, the repository URL its locations name, and what
/// keeps its directories alive.
struct Subject {
    name: &'static str,
    store: Box<dyn StateStore>,
    repository: String,
    _directories: Vec<TempDir>,
}

fn subjects() -> Vec<Subject> {
    let memory = Subject {
        name: "memory",
        store: Box::new(MemoryStateStore::new()),
        repository: "file:///project.git".to_owned(),
        _directories: Vec::new(),
    };
    let remote = {
        let bare = TempDir::new().unwrap();
        git2::Repository::init_bare(bare.path()).unwrap();
        let clone = TempDir::new().unwrap();
        git2::Repository::init(clone.path()).unwrap();
        Subject {
            name: "git remote",
            store: Box::new(GitStateStore::new(clone.path().join(".git"), GitAuth::none(), signer())),
            repository: format!("file://{}", bare.path().display()),
            _directories: vec![bare, clone],
        }
    };
    let local = {
        let clone = TempDir::new().unwrap();
        git2::Repository::init(clone.path()).unwrap();
        Subject {
            name: "git local",
            store: Box::new(GitStateStore::new(clone.path().join(".git"), GitAuth::none(), signer())),
            repository: LOCAL_REPOSITORY.to_owned(),
            _directories: vec![clone],
        }
    };
    vec![memory, remote, local]
}

fn location(repository: &str, desired: &str, observed: &str, path: &str) -> StateLocation {
    StateLocation {
        repository: repository.to_owned(),
        desired_ref: desired.to_owned(),
        observed_ref: observed.to_owned(),
        path: path.to_owned(),
        coordination_ref_prefix: "refs/nyl/".to_owned(),
    }
}

fn separate(subject: &Subject) -> StateLocation {
    location(&subject.repository, "nyl/dev/desired", "nyl/dev/observed", "")
}

fn environment(name: &str) -> EnvironmentName {
    EnvironmentName::new(name).unwrap()
}

fn at() -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::parse_from_rfc3339("2026-09-24T10:00:00Z")
        .unwrap()
        .to_utc()
}

fn lease(run: &str) -> Lease {
    Lease {
        api_version: StateV1,
        kind: LeaseKind,
        run: RunId::new(run).unwrap(),
        runner: "test".to_owned(),
        operation: Operation::Reconcile,
        units: Vec::new(),
        deadline: at(),
        taken_at: at(),
        broken: None,
    }
}

fn record(location: &StateLocation, name: &str) -> StateRecord {
    StateRecord {
        api_version: StateV1,
        kind: StateRecordKind,
        environment: environment(name),
        template: None,
        params: BTreeMap::new(),
        expires_at: None,
        location: location.clone(),
        events: Vec::new(),
    }
}

fn unit(name: &str) -> (DesiredUnit, ObservedUnit) {
    let identity =
        json!({"environment": "dev", "name": name, "uid": UID, "apiVersion": "units.gitops.nyl/v1", "kind": "Command"});
    let desired = serde_json::from_value(json!({
        "apiVersion": "gitops.nyl/v1", "kind": "DesiredUnit", "unit": identity,
        "lifecycle": {"state": "active"}, "sourceCommit": COMMIT, "sourceRevision": COMMIT,
        "spec": {"command": ["./run.sh"]}, "resolvedSpec": {"command": ["./run.sh"]},
        "executionKey": KEY, "readiness": {"state": "ready"}
    }))
    .unwrap();
    let observed = serde_json::from_value(json!({
        "apiVersion": "gitops.nyl/v1", "kind": "ObservedUnit", "unit": identity,
        "receipt": {"executionKey": KEY, "behaviorVersion": 1, "run": RUN_A,
                    "startedAt": "2026-09-24T10:00:00Z", "finishedAt": "2026-09-24T10:01:00Z",
                    "outputs": {"version": "1"}},
        "condition": null
    }))
    .unwrap();
    (desired, observed)
}

fn transition(changes: TransitionChanges) -> Transition {
    Transition {
        message: "nyl: test\n\nNyl-Operation: reconcile\n".to_owned(),
        changes,
    }
}

fn init(location: &StateLocation, name: &str) -> Transition {
    transition(TransitionChanges {
        record: Some(record(location, name)),
        ..TransitionChanges::default()
    })
}

#[test]
fn test_leases_admit_one_holder_and_a_takeover_names_what_it_replaces() {
    for subject in subjects() {
        let store = subject.store.as_ref();
        let location = separate(&subject);
        let first = store
            .take_lease(&location, &environment("dev"), &lease(RUN_A), None)
            .unwrap();
        assert!(
            matches!(
                store.take_lease(&location, &environment("dev"), &lease(RUN_B), None),
                Err(StateError::LeaseRace)
            ),
            "{}",
            subject.name
        );
        let held = store
            .coordination(&location, &environment("dev"))
            .unwrap()
            .lease
            .unwrap();
        assert_eq!(held.version, first, "{}", subject.name);
        assert_eq!(held.lease.run.as_str(), RUN_A, "{}", subject.name);

        let taken = store
            .take_lease(&location, &environment("dev"), &lease(RUN_B), Some(&first))
            .unwrap();
        assert!(
            matches!(
                store.update_lease(&location, &environment("dev"), &lease(RUN_A), &first),
                Err(StateError::LeaseLost)
            ),
            "{}",
            subject.name
        );
        let updated = store
            .update_lease(&location, &environment("dev"), &lease(RUN_B), &taken)
            .unwrap();
        store.release_lease(&location, &environment("dev"), &updated).unwrap();
        assert!(store
            .coordination(&location, &environment("dev"))
            .unwrap()
            .lease
            .is_none());
    }
}

#[test]
fn test_transitions_are_fenced_by_the_lease_and_the_tips_they_read() {
    for subject in subjects() {
        let store = subject.store.as_ref();
        let location = separate(&subject);
        let dev = environment("dev");
        let version = store.take_lease(&location, &dev, &lease(RUN_A), None).unwrap();
        store.delete_lease(&location, &dev).unwrap();
        assert!(
            matches!(
                store.commit(
                    &location,
                    &dev,
                    &StateTips::default(),
                    &init(&location, "dev"),
                    &version
                ),
                Err(StateError::LeaseLost)
            ),
            "{}: a holder whose lease was deleted pushes nothing",
            subject.name
        );
        assert!(store.snapshot(&location).unwrap().record.is_none(), "{}", subject.name);

        let version = store.take_lease(&location, &dev, &lease(RUN_B), None).unwrap();
        let committed = store
            .commit(
                &location,
                &dev,
                &StateTips::default(),
                &init(&location, "dev"),
                &version,
            )
            .unwrap();
        assert!(
            committed.desired.is_some() && committed.observed.is_some(),
            "{}",
            subject.name
        );
        // A second run's transition, built on the tips before the first.
        let mut retry = init(&location, "dev");
        retry.message = "nyl: another run\n".to_owned();
        // The old lease version stopped holding when the push moved it.
        assert!(matches!(
            store.release_lease(&location, &dev, &version),
            Err(StateError::LeaseLost)
        ));
        assert!(
            matches!(
                store.commit(&location, &dev, &StateTips::default(), &retry, &committed.lease_version),
                Err(StateError::Conflict { .. })
            ),
            "{}: a commit on moved tips is a conflict",
            subject.name
        );
        store.release_lease(&location, &dev, &committed.lease_version).unwrap();

        // A transition that changes nothing still tells a taken-over run it lost.
        let stale = store.take_lease(&location, &dev, &lease(RUN_A), None).unwrap();
        let tips = store.snapshot(&location).unwrap().tips;
        let held = store.coordination(&location, &dev).unwrap().lease.unwrap().version;
        store.take_lease(&location, &dev, &lease(RUN_B), Some(&held)).unwrap();
        assert!(
            matches!(
                store.commit(
                    &location,
                    &dev,
                    &tips,
                    &transition(TransitionChanges::default()),
                    &stale
                ),
                Err(StateError::LeaseLost)
            ),
            "{}: an unchanged transition is fenced too",
            subject.name
        );
    }
}

#[test]
fn test_snapshots_read_back_what_transitions_wrote_and_unchanged_state_writes_nothing() {
    for subject in subjects() {
        let store = subject.store.as_ref();
        let location = separate(&subject);
        let dev = environment("dev");
        let version = store.take_lease(&location, &dev, &lease(RUN_A), None).unwrap();
        let committed = store
            .commit(
                &location,
                &dev,
                &StateTips::default(),
                &init(&location, "dev"),
                &version,
            )
            .unwrap();
        let (desired, observed) = unit("seed");
        let name = UnitName::new("seed").unwrap();
        let base = store.snapshot(&location).unwrap().tips;
        let changes = TransitionChanges {
            desired: BTreeMap::from([(name.clone(), Some(desired.clone()))]),
            observed: BTreeMap::from([(name.clone(), Some(observed.clone()))]),
            ..TransitionChanges::default()
        };
        let committed = store
            .commit(
                &location,
                &dev,
                &base,
                &transition(changes.clone()),
                &committed.lease_version,
            )
            .unwrap();
        let snapshot = store.snapshot(&location).unwrap();
        assert_eq!(snapshot.desired.get(&name), Some(&desired), "{}", subject.name);
        assert_eq!(snapshot.observed.get(&name), Some(&observed), "{}", subject.name);
        assert_eq!(snapshot.record.unwrap().environment, dev);

        // Writing the same documents again changes nothing and commits nothing.
        let again = store
            .commit(
                &location,
                &dev,
                &snapshot.tips,
                &transition(changes),
                &committed.lease_version,
            )
            .unwrap();
        assert_eq!((again.desired, again.observed), (None, None), "{}", subject.name);
        assert_eq!(
            store.snapshot(&location).unwrap().tips,
            snapshot.tips,
            "{}",
            subject.name
        );
    }
}

#[test]
fn test_environments_sharing_a_ref_through_path_keep_their_own_directories() {
    for subject in subjects() {
        let store = subject.store.as_ref();
        let a = location(&subject.repository, "nyl/previews", "nyl/previews", "pr-1");
        let b = location(&subject.repository, "nyl/previews", "nyl/previews", "pr-2");
        for (location, name, run) in [(&a, "pr-1", RUN_A), (&b, "pr-2", RUN_B)] {
            let environment = environment(name);
            let version = store.take_lease(location, &environment, &lease(run), None).unwrap();
            let base = store.snapshot(location).unwrap().tips;
            let committed = store
                .commit(location, &environment, &base, &init(location, name), &version)
                .unwrap();
            assert_eq!(
                committed.desired, committed.observed,
                "{}: one ref, one commit",
                subject.name
            );
            store
                .release_lease(location, &environment, &committed.lease_version)
                .unwrap();
        }
        assert_eq!(store.snapshot(&a).unwrap().record.unwrap().environment.as_str(), "pr-1");
        assert_eq!(store.snapshot(&b).unwrap().record.unwrap().environment.as_str(), "pr-2");
    }
}

#[test]
fn test_concurrent_writers_sharing_a_ref_rebase_unless_their_directory_changed() {
    for subject in subjects() {
        let store = subject.store.as_ref();
        let a = location(&subject.repository, "nyl/previews", "nyl/previews", "pr-1");
        let b = location(&subject.repository, "nyl/previews", "nyl/previews", "pr-2");
        let (pr1, pr2) = (environment("pr-1"), environment("pr-2"));
        let lease_a = store.take_lease(&a, &pr1, &lease(RUN_A), None).unwrap();
        let lease_b = store.take_lease(&b, &pr2, &lease(RUN_B), None).unwrap();
        // Both read the ref before either commits.
        let base_a = store.snapshot(&a).unwrap().tips;
        let base_b = store.snapshot(&b).unwrap().tips;
        let first = store.commit(&a, &pr1, &base_a, &init(&a, "pr-1"), &lease_a).unwrap();
        let second = store.commit(&b, &pr2, &base_b, &init(&b, "pr-2"), &lease_b);
        let second = second.unwrap_or_else(|error| panic!("{}: disjoint paths rebase: {error}", subject.name));
        assert_eq!(store.snapshot(&a).unwrap().record.unwrap().environment.as_str(), "pr-1");
        assert_eq!(store.snapshot(&b).unwrap().record.unwrap().environment.as_str(), "pr-2");

        // A transition built before pr-1's directory changed is a conflict.
        let mut stale = init(&a, "pr-1");
        stale.message = "nyl: stale\n".to_owned();
        assert!(
            matches!(
                store.commit(&a, &pr1, &base_a, &stale, &first.lease_version),
                Err(StateError::Conflict { .. })
            ),
            "{}",
            subject.name
        );
        store.release_lease(&a, &pr1, &first.lease_version).unwrap();
        store.release_lease(&b, &pr2, &second.lease_version).unwrap();
    }
}

#[test]
fn test_run_refs_carry_checkpoints_and_signals_until_deleted() {
    for subject in subjects() {
        let store = subject.store.as_ref();
        let location = separate(&subject);
        let dev = environment("dev");
        let run = Run {
            api_version: StateV1,
            kind: RunKind,
            run: RunId::new(RUN_A).unwrap(),
            runner: "test".to_owned(),
            operation: Operation::Reconcile,
            source_commit: None,
            read_desired: None,
            read_observed: None,
            deadline: at(),
            units: vec![UnitName::new("seed").unwrap()],
            finished: false,
        };
        store.write_run(&location, &dev, &run, None).unwrap();
        let (desired, observed) = unit("seed");
        let checkpoint = Checkpoint {
            desired,
            observed,
            artifacts: BTreeMap::new(),
        };
        store.write_run(&location, &dev, &run, Some(&checkpoint)).unwrap();
        let mut finished = run.clone();
        finished.finished = true;
        store.write_run(&location, &dev, &finished, None).unwrap();

        let runs = store.coordination(&location, &dev).unwrap().runs;
        assert_eq!(runs.len(), 1, "{}", subject.name);
        assert!(runs[0].run.finished, "{}", subject.name);
        assert_eq!(runs[0].checkpoints, vec![checkpoint], "{}", subject.name);

        let signal = Signal {
            api_version: StateV1,
            kind: SignalKind,
            run: run.run.clone(),
            unit: UnitName::new("kubernetes").unwrap(),
            phase1_commit: nyl_core::orchestration::ids::CommitId::new(COMMIT).unwrap(),
            by: "alice".to_owned(),
            identity: ApprovalIdentity::Local,
            reason: None,
            at: at(),
            from: "workstation".to_owned(),
        };
        store.send_signal(&location, &dev, &signal).unwrap();
        assert_eq!(store.signals(&location, &dev, &run.run).unwrap(), vec![signal]);

        store.delete_run(&location, &dev, &run.run).unwrap();
        assert!(
            store.coordination(&location, &dev).unwrap().runs.is_empty(),
            "{}",
            subject.name
        );
        assert!(
            store.signals(&location, &dev, &run.run).unwrap().is_empty(),
            "{}",
            subject.name
        );
    }
}

#[test]
fn test_imported_history_completes_when_repeated_and_refuses_a_diverged_target() {
    for subject in subjects() {
        let store = subject.store.as_ref();
        let from = separate(&subject);
        let to = location(&subject.repository, "state/dev/desired", "state/dev/observed", "");
        let dev = environment("dev");
        let version = store.take_lease(&from, &dev, &lease(RUN_A), None).unwrap();
        let committed = store
            .commit(&from, &dev, &StateTips::default(), &init(&from, "dev"), &version)
            .unwrap();
        store.import_history(&from, &to).unwrap();
        store.import_history(&from, &to).unwrap();
        let moved = store.snapshot(&to).unwrap();
        assert_eq!(
            moved.tips.desired, committed.desired,
            "{}: history, not a copy",
            subject.name
        );

        // Once the source moves on, the target no longer matches it.
        let base = store.snapshot(&from).unwrap().tips;
        let (desired, _) = unit("seed");
        store
            .commit(
                &from,
                &dev,
                &base,
                &transition(TransitionChanges {
                    desired: BTreeMap::from([(UnitName::new("seed").unwrap(), Some(desired))]),
                    ..TransitionChanges::default()
                }),
                &committed.lease_version,
            )
            .unwrap();
        assert!(
            matches!(store.import_history(&from, &to), Err(StateError::Conflict { .. })),
            "{}",
            subject.name
        );
    }
}

/// The clone's own branches follow remote state, and a rewritten state ref
/// stops the store.
#[test]
fn test_remote_state_fast_forwards_local_branches_and_refuses_rewrites() {
    let bare = TempDir::new().unwrap();
    let remote = git2::Repository::init_bare(bare.path()).unwrap();
    let clone = TempDir::new().unwrap();
    let local = git2::Repository::init(clone.path()).unwrap();
    let store = GitStateStore::new(clone.path().join(".git"), GitAuth::none(), signer());
    let url = format!("file://{}", bare.path().display());
    let location = location(&url, "nyl/dev/desired", "nyl/dev/observed", "");
    let dev = environment("dev");
    let version = store.take_lease(&location, &dev, &lease(RUN_A), None).unwrap();
    let committed = store
        .commit(
            &location,
            &dev,
            &StateTips::default(),
            &init(&location, "dev"),
            &version,
        )
        .unwrap();
    let desired = committed.desired.unwrap();
    assert_eq!(
        local.refname_to_id("refs/heads/nyl/dev/desired").unwrap().to_string(),
        desired.as_str()
    );

    // Someone rewrites the remote state ref by hand.
    let signature = git2::Signature::now("Someone", "someone@example.invalid").unwrap();
    let tree = remote
        .find_tree(remote.treebuilder(None).unwrap().write().unwrap())
        .unwrap();
    let rewritten = remote
        .commit(None, &signature, &signature, "rewrite", &tree, &[])
        .unwrap();
    remote
        .reference("refs/heads/nyl/dev/desired", rewritten, true, "rewrite")
        .unwrap();
    // Every later snapshot refuses it too, not only the first.
    for _ in 0..2 {
        assert!(matches!(
            store.snapshot(&location),
            Err(StateError::NotFastForward { .. })
        ));
    }
    // The diverged local branch is never moved.
    assert_eq!(
        local.refname_to_id("refs/heads/nyl/dev/desired").unwrap().to_string(),
        desired.as_str()
    );
}
