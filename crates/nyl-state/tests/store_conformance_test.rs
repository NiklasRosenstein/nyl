//! One conformance suite for every `StateStore`: the in-memory store, and the
//! Git store against a remote bare repository and against the clone's own
//! branches. The walkthroughs run against the first and the reference
//! scenarios against the others, so they must agree on every rule here.

use std::collections::BTreeMap;

use nyl_core::orchestration::ids::{EnvironmentName, RunId, UnitName};
use nyl_core::orchestration::ports::{
    Checkpoint, Committed, Divergence, LocalDivergence, StateError, StateSide, StateStore, StateTips, Transition,
    TransitionChanges,
};
use nyl_core::orchestration::state::{
    ApprovalIdentity, Attestation, DesiredUnit, Lease, LeaseKind, Observation, ObservationType, ObservedUnit,
    Operation, Run, RunKind, Signal, SignalKind, StateLocation, StateRecord, StateRecordKind, StateV1,
};
use nyl_git::GitAuth;
use nyl_state::git::{CommitAuthor, GitStateStore};
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

fn author() -> CommitAuthor {
    CommitAuthor {
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
            store: Box::new(GitStateStore::new(clone.path().join(".git"), GitAuth::none(), author())),
            repository: format!("file://{}", bare.path().display()),
            _directories: vec![bare, clone],
        }
    };
    let local = {
        let clone = TempDir::new().unwrap();
        git2::Repository::init(clone.path()).unwrap();
        Subject {
            name: "git local",
            store: Box::new(GitStateStore::new(clone.path().join(".git"), GitAuth::none(), author())),
            repository: StateLocation::LOCAL_REPOSITORY.to_owned(),
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

/// Take the lease of `environment` for `RUN_A` and initialize its state.
fn initialized(store: &dyn StateStore, location: &StateLocation, environment: &EnvironmentName) -> Committed {
    let version = store.take_lease(location, environment, &lease(RUN_A), None).unwrap();
    store
        .commit(
            location,
            environment,
            &StateTips::default(),
            &init(location, environment.as_str()),
            &version,
        )
        .unwrap()
}

fn run() -> Run {
    Run {
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
    }
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
        let committed = initialized(store, &location, &dev);
        let (desired, observed) = unit("seed");
        let name = UnitName::new("seed").unwrap();
        let base = store.snapshot(&location).unwrap().tips;
        let changes = seed_changes("seed");
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

/// Catches `get --state-revision` reading the current state instead of
/// the revision it names, or reading a commit the state refs never held.
#[test]
fn test_snapshot_at_reads_an_earlier_commit() {
    for subject in subjects() {
        let store = subject.store.as_ref();
        let location = separate(&subject);
        let dev = environment("dev");
        let first = initialized(store, &location, &dev);
        let earlier = StateTips {
            desired: first.desired.clone(),
            observed: first.observed.clone(),
        };
        let mut moved = init(&location, "dev");
        moved.changes.record.as_mut().unwrap().template = Some("preview".to_owned());
        store
            .commit(&location, &dev, &earlier, &moved, &first.lease_version)
            .unwrap();
        let snapshot = store.snapshot_at(&location, &earlier).unwrap();
        assert_eq!(snapshot.record.unwrap().template, None, "{}", subject.name);
        assert_eq!(
            store.snapshot(&location).unwrap().record.unwrap().template.as_deref(),
            Some("preview"),
            "{}",
            subject.name
        );
        let unknown = StateTips {
            desired: Some(nyl_core::orchestration::ids::CommitId::new("f".repeat(40)).unwrap()),
            observed: None,
        };
        assert!(
            matches!(store.snapshot_at(&location, &unknown), Err(StateError::Unavailable(_))),
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
        let run = run();
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

        store.delete_run(&location, &dev, &run.run, None).unwrap();
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
        let committed = initialized(store, &from, &dev);
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
    let store = GitStateStore::new(clone.path().join(".git"), GitAuth::none(), author());
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

#[test]
fn test_observations_and_attestations_round_trip_on_the_observed_ref_only() {
    for subject in subjects() {
        let store = subject.store.as_ref();
        let location = separate(&subject);
        let dev = environment("dev");
        let committed = initialized(store, &location, &dev);
        let name = UnitName::new("seed").unwrap();
        let identity = json!({"environment": "dev", "name": "seed", "uid": UID,
                              "apiVersion": "units.gitops.nyl/v1", "kind": "Command"});
        let observation: Observation = serde_json::from_value(json!({
            "apiVersion": "gitops.nyl/v1", "kind": "Observation", "unit": identity, "type": "drift",
            "executionKey": KEY, "at": "2026-09-24T10:00:00Z", "result": {"drift": "clean"}
        }))
        .unwrap();
        let attestation: Attestation = serde_json::from_value(json!({
            "apiVersion": "gitops.nyl/v1", "kind": "Attestation", "name": "smoke",
            "unit": {"environment": "dev", "name": "seed", "uid": UID}, "executionKey": KEY, "result": "pass",
            "at": "2026-09-24T10:00:00Z", "source": {"driver": "Command"}
        }))
        .unwrap();
        let before = store.snapshot(&location).unwrap().tips;
        let changes = TransitionChanges {
            observations: BTreeMap::from([(
                name.clone(),
                Some(BTreeMap::from([(ObservationType::Drift, observation.clone())])),
            )]),
            attestations: BTreeMap::from([(
                name.clone(),
                Some(BTreeMap::from([("smoke".to_owned(), attestation.clone())])),
            )]),
            ..TransitionChanges::default()
        };
        let written = store
            .commit(&location, &dev, &before, &transition(changes), &committed.lease_version)
            .unwrap();
        assert_eq!(written.desired, None, "{}: the desired ref is untouched", subject.name);
        assert!(written.observed.is_some(), "{}", subject.name);
        let snapshot = store.snapshot(&location).unwrap();
        assert_eq!(snapshot.tips.desired, before.desired, "{}", subject.name);
        assert_eq!(
            snapshot.observations[&name][&ObservationType::Drift],
            observation,
            "{}",
            subject.name
        );
        assert_eq!(snapshot.attestations[&name]["smoke"], attestation, "{}", subject.name);

        let removal = TransitionChanges {
            observations: BTreeMap::from([(name.clone(), None)]),
            attestations: BTreeMap::from([(name.clone(), None)]),
            ..TransitionChanges::default()
        };
        store
            .commit(
                &location,
                &dev,
                &snapshot.tips,
                &transition(removal),
                &written.lease_version,
            )
            .unwrap();
        let snapshot = store.snapshot(&location).unwrap();
        assert!(snapshot.observations.is_empty(), "{}", subject.name);
        assert!(snapshot.attestations.is_empty(), "{}", subject.name);
    }
}

#[test]
fn test_created_units_ride_on_run_refs_and_retired_runs_become_evidence() {
    for subject in subjects() {
        let store = subject.store.as_ref();
        let location = separate(&subject);
        let dev = environment("dev");
        let run = run();
        let (desired, observed) = unit("seed");
        store
            .write_run_created(&location, &dev, &run, std::slice::from_ref(&desired))
            .unwrap();
        let checkpoint = Checkpoint {
            desired: desired.clone(),
            observed,
            artifacts: BTreeMap::new(),
        };
        store.write_run(&location, &dev, &run, Some(&checkpoint)).unwrap();
        let coordination = store.coordination(&location, &dev).unwrap();
        assert_eq!(coordination.runs[0].created, vec![desired.clone()], "{}", subject.name);
        assert!(coordination.evidence.is_empty(), "{}", subject.name);

        // A run ref that moved since it was read is neither retired nor deleted.
        let read = coordination.runs[0].version.clone();
        let mut moved = run.clone();
        moved.units.clear();
        store.write_run(&location, &dev, &moved, None).unwrap();
        for consumed in [
            store.retire_run(&location, &dev, &run.run, Some(&read)),
            store.delete_run(&location, &dev, &run.run, Some(&read)),
        ] {
            assert!(matches!(consumed, Err(StateError::Conflict { .. })), "{}", subject.name);
        }
        let coordination = store.coordination(&location, &dev).unwrap();
        assert_eq!(coordination.runs.len(), 1, "{}", subject.name);
        assert!(coordination.evidence.is_empty(), "{}", subject.name);

        // Retiring moves the run ref to evidence, which no longer counts as a run.
        let current = coordination.runs[0].version.clone();
        store.retire_run(&location, &dev, &run.run, Some(&current)).unwrap();
        let coordination = store.coordination(&location, &dev).unwrap();
        assert!(coordination.runs.is_empty(), "{}", subject.name);
        assert_eq!(coordination.evidence.len(), 1, "{}", subject.name);
        assert_eq!(
            coordination.evidence[0].checkpoints,
            vec![checkpoint.clone()],
            "{}",
            subject.name
        );
        assert_eq!(coordination.evidence[0].created, vec![desired], "{}", subject.name);

        // A late checkpoint on a recreated run ref is appended when retired again.
        let mut late = run.clone();
        late.finished = true;
        store.write_run(&location, &dev, &late, Some(&checkpoint)).unwrap();
        store.retire_run(&location, &dev, &run.run, None).unwrap();
        let coordination = store.coordination(&location, &dev).unwrap();
        assert!(coordination.runs.is_empty(), "{}", subject.name);
        assert_eq!(coordination.evidence[0].checkpoints.len(), 2, "{}", subject.name);
        assert!(coordination.evidence[0].run.finished, "{}", subject.name);

        store.delete_evidence(&location, &dev, &run.run).unwrap();
        assert!(
            store.coordination(&location, &dev).unwrap().evidence.is_empty(),
            "{}",
            subject.name
        );
    }
}

/// A run writes its run ref on top of the tip it last pushed. Another
/// clone moving the ref (a takeover retiring it) makes that write fall
/// back to the remote's tip, as every write did before.
#[test]
fn test_run_ref_writes_follow_a_ref_another_clone_moved() {
    let bare = TempDir::new().unwrap();
    git2::Repository::init_bare(bare.path()).unwrap();
    let clones = [TempDir::new().unwrap(), TempDir::new().unwrap()];
    let [running, taking] = clones.each_ref().map(|clone| {
        git2::Repository::init(clone.path()).unwrap();
        GitStateStore::new(clone.path().join(".git"), GitAuth::none(), author())
    });
    let location = location(
        &format!("file://{}", bare.path().display()),
        "nyl/dev/desired",
        "nyl/dev/observed",
        "",
    );
    let dev = environment("dev");
    let run = run();
    let (desired, observed) = unit("seed");
    let checkpoint = Checkpoint {
        desired,
        observed,
        artifacts: BTreeMap::new(),
    };
    running.write_run(&location, &dev, &run, Some(&checkpoint)).unwrap();

    // Another clone writes the run ref it never wrote, then retires it.
    taking.write_run(&location, &dev, &run, None).unwrap();
    let read = taking.coordination(&location, &dev).unwrap().runs[0].version.clone();
    assert_eq!(
        taking.coordination(&location, &dev).unwrap().runs[0].checkpoints,
        vec![checkpoint.clone()]
    );
    taking.retire_run(&location, &dev, &run.run, Some(&read)).unwrap();

    // The run's late checkpoint recreates the run ref it last saw elsewhere.
    running.write_run(&location, &dev, &run, Some(&checkpoint)).unwrap();
    let coordination = taking.coordination(&location, &dev).unwrap();
    assert_eq!(coordination.runs.len(), 1);
    assert_eq!(coordination.runs[0].checkpoints, vec![checkpoint.clone()]);
    assert_eq!(coordination.evidence.len(), 1);

    // A phase update that moves the lease too follows the moved run ref.
    let mut version = running.take_lease(&location, &dev, &lease(RUN_A), None).unwrap();
    taking.write_run(&location, &dev, &run, None).unwrap();
    running
        .update_lease_and_run(
            &location,
            &dev,
            &lease(RUN_A),
            &mut version,
            &run,
            std::slice::from_ref(&checkpoint.desired),
        )
        .unwrap();
    let coordination = taking.coordination(&location, &dev).unwrap();
    assert_eq!(coordination.lease.unwrap().version, version);
    assert_eq!(coordination.runs[0].checkpoints, vec![checkpoint.clone()]);
    assert_eq!(coordination.runs[0].created, vec![checkpoint.desired]);
}

/// The phase update moves the lease and writes the run record with the
/// units the run created, or changes neither when the lease moved.
#[test]
fn test_lease_and_run_record_move_together_or_not_at_all() {
    for subject in subjects() {
        let store = subject.store.as_ref();
        let location = separate(&subject);
        let dev = environment("dev");
        let taken = store.take_lease(&location, &dev, &lease(RUN_A), None).unwrap();
        let mut moved = lease(RUN_A);
        moved.units = vec![UnitName::new("seed").unwrap()];
        let (desired, _) = unit("seed");
        let mut version = taken.clone();
        store
            .update_lease_and_run(
                &location,
                &dev,
                &moved,
                &mut version,
                &run(),
                std::slice::from_ref(&desired),
            )
            .unwrap();
        let coordination = store.coordination(&location, &dev).unwrap();
        let held = coordination.lease.unwrap();
        assert_eq!((held.lease, held.version), (moved, version.clone()), "{}", subject.name);
        assert_eq!(coordination.runs[0].run, run(), "{}", subject.name);
        assert_eq!(coordination.runs[0].created, vec![desired.clone()], "{}", subject.name);

        // From a lease version that is no longer current, nothing moves.
        let mut stale = taken;
        let mut finished = run();
        finished.finished = true;
        let refused = store.update_lease_and_run(&location, &dev, &lease(RUN_A), &mut stale, &finished, &[]);
        assert!(matches!(refused, Err(StateError::LeaseLost)), "{}", subject.name);
        let coordination = store.coordination(&location, &dev).unwrap();
        assert_eq!(coordination.lease.unwrap().version, version, "{}", subject.name);
        assert!(!coordination.runs[0].run.finished, "{}", subject.name);
    }
}

/// The clone's local branches of a remote location's state refs.
fn view(location: &StateLocation) -> StateLocation {
    StateLocation {
        repository: StateLocation::LOCAL_REPOSITORY.to_owned(),
        ..location.clone()
    }
}

fn seed_changes(name: &str) -> TransitionChanges {
    let (desired, observed) = unit(name);
    let unit = UnitName::new(name).unwrap();
    TransitionChanges {
        desired: BTreeMap::from([(unit.clone(), Some(desired))]),
        observed: BTreeMap::from([(unit, Some(observed))]),
        ..TransitionChanges::default()
    }
}

/// `check_remote_tips` and `state restore` read raw messages from `log`:
/// catches a reordered, truncated, or rewritten history.
#[test]
fn test_log_returns_first_parent_history_newest_first_with_raw_messages() {
    for subject in subjects() {
        let store = subject.store.as_ref();
        let location = separate(&subject);
        let dev = environment("dev");
        assert!(
            store.log(&location, StateSide::Desired, 10).unwrap().is_empty(),
            "{}: a missing ref has an empty log",
            subject.name
        );
        let first = initialized(store, &location, &dev);
        let mut second = transition(seed_changes("seed"));
        second.message = "nyl: second\n\nNyl-Operation: reconcile\nNyl-Local: true\n".to_owned();
        let tips = store.snapshot(&location).unwrap().tips;
        let committed = store
            .commit(&location, &dev, &tips, &second, &first.lease_version)
            .unwrap();

        let log = store.log(&location, StateSide::Desired, 10).unwrap();
        let commits = log.iter().map(|entry| entry.commit.clone()).collect::<Vec<_>>();
        assert_eq!(
            commits,
            vec![committed.desired.clone().unwrap(), first.desired.clone().unwrap()],
            "{}",
            subject.name
        );
        assert_eq!(log[0].message, second.message, "{}", subject.name);
        assert_eq!(log[1].message, init(&location, "dev").message, "{}", subject.name);
        let newest = store.log(&location, StateSide::Observed, 1).unwrap();
        assert_eq!(newest.len(), 1, "{}", subject.name);
        assert_eq!(Some(&newest[0].commit), committed.observed.as_ref(), "{}", subject.name);
    }
}

/// A `--local` run commits to the clone's local branches only, fenced by
/// the lease; the branches then diverge from remote state until
/// `state reset --local` moves them back. Catches a local run pushing
/// state, writing after losing its lease, a read silently moving a
/// diverged local branch, and a reset that overwrites unseen local work.
#[test]
fn test_local_commits_stay_local_diverge_and_reset_by_compare_and_swap() {
    for subject in subjects() {
        let store = subject.store.as_ref();
        let location = separate(&subject);
        let dev = environment("dev");
        let first = initialized(store, &location, &dev);
        let remote = store.snapshot(&location).unwrap().tips;
        let mut local_transition = transition(seed_changes("seed"));
        local_transition.message = "nyl: local\n\nNyl-Local: true\n".to_owned();

        if subject.repository == StateLocation::LOCAL_REPOSITORY {
            // The clone's own branches are the state: nothing to diverge from.
            assert_eq!(
                store.local_divergence(&location).unwrap(),
                LocalDivergence::default(),
                "{}",
                subject.name
            );
            assert!(matches!(
                store.reset_local(&location, &remote),
                Err(StateError::Unavailable(_))
            ));
            assert!(matches!(
                store.commit_local(
                    &location,
                    &dev,
                    &remote,
                    &local_transition,
                    &location,
                    &first.lease_version
                ),
                Err(StateError::Unavailable(_))
            ));
            continue;
        }

        let local = view(&location);
        let even = Some(Divergence { ahead: 0, behind: 0 });
        assert_eq!(
            store.local_divergence(&location).unwrap(),
            LocalDivergence {
                desired: even,
                observed: even
            },
            "{}",
            subject.name
        );
        assert_eq!(
            store.snapshot(&local).unwrap().tips,
            remote,
            "{}: reads follow remote state",
            subject.name
        );

        let committed = store
            .commit_local(
                &location,
                &dev,
                &remote,
                &local_transition,
                &location,
                &first.lease_version,
            )
            .unwrap();
        assert!(committed.desired.is_some(), "{}", subject.name);
        assert_eq!(
            store.snapshot(&location).unwrap().tips,
            remote,
            "{}: remote state is never pushed",
            subject.name
        );
        let local_tips = store.snapshot(&local).unwrap().tips;
        assert_eq!(local_tips.desired, committed.desired, "{}", subject.name);
        assert_eq!(
            store.log(&local, StateSide::Desired, 1).unwrap()[0].message,
            local_transition.message,
            "{}",
            subject.name
        );
        assert_eq!(
            store.local_divergence(&location).unwrap().desired,
            Some(Divergence { ahead: 1, behind: 0 }),
            "{}",
            subject.name
        );

        // Remote state moves on; reading it leaves the diverged branch alone.
        let mut remote_transition = init(&location, "dev");
        remote_transition.changes.record.as_mut().unwrap().template = Some("moved".to_owned());
        let moved = store
            .commit(&location, &dev, &remote, &remote_transition, &committed.lease_version)
            .unwrap();
        let remote = store.snapshot(&location).unwrap().tips;
        assert_eq!(remote.desired, moved.desired, "{}", subject.name);
        assert_eq!(
            store.snapshot(&local).unwrap().tips,
            local_tips,
            "{}: a diverged local branch is never moved by a read",
            subject.name
        );
        assert_eq!(
            store.local_divergence(&location).unwrap().desired,
            Some(Divergence { ahead: 1, behind: 1 }),
            "{}",
            subject.name
        );

        // A local run that lost its lease writes nothing.
        let held = store.coordination(&location, &dev).unwrap().lease.unwrap().version;
        let taken = store.take_lease(&location, &dev, &lease(RUN_B), Some(&held)).unwrap();
        let mut late = transition(seed_changes("late"));
        late.message = "nyl: late\n\nNyl-Local: true\n".to_owned();
        assert!(
            matches!(
                store.commit_local(&location, &dev, &local_tips, &late, &location, &held),
                Err(StateError::LeaseLost)
            ),
            "{}: a taken-over lease",
            subject.name
        );
        store.delete_lease(&location, &dev).unwrap();
        assert!(
            matches!(
                store.commit_local(&location, &dev, &local_tips, &late, &location, &taken),
                Err(StateError::LeaseLost)
            ),
            "{}: a deleted lease",
            subject.name
        );
        assert_eq!(store.snapshot(&local).unwrap().tips, local_tips, "{}", subject.name);

        // The reset compares against the local tips the caller read.
        assert!(
            matches!(
                store.reset_local(&location, &StateTips::default()),
                Err(StateError::Conflict { .. })
            ),
            "{}: a stale expectation",
            subject.name
        );
        assert!(matches!(
            store.reset_local(&local, &local_tips),
            Err(StateError::Unavailable(_))
        ));
        assert_eq!(store.snapshot(&local).unwrap().tips, local_tips, "{}", subject.name);
        let reset = store.reset_local(&location, &local_tips).unwrap();
        assert_eq!(reset, remote, "{}", subject.name);
        assert_eq!(store.snapshot(&local).unwrap().tips, remote, "{}", subject.name);
        assert_eq!(
            store.local_divergence(&location).unwrap(),
            LocalDivergence {
                desired: even,
                observed: even
            },
            "{}",
            subject.name
        );

        // `--local-lease`: the clone's own lease fences the local commit.
        let local_lease = store.take_lease(&local, &dev, &lease(RUN_A), None).unwrap();
        let committed = store
            .commit_local(&location, &dev, &reset, &late, &local, &local_lease)
            .unwrap();
        let held = store.coordination(&local, &dev).unwrap().lease.unwrap().version;
        assert_eq!(held, committed.lease_version, "{}: the lease moved", subject.name);
        assert_eq!(store.snapshot(&location).unwrap().tips, remote, "{}", subject.name);
        assert!(store.coordination(&location, &dev).unwrap().lease.is_none());
    }
}

/// The guards of the local writes, which each store implements on its own:
/// catches a stale `base` moving the lease before it is refused, a full
/// `refs/…` state ref (which has no local branch) being reset or committed
/// to, and a reset leaving a local branch whose remote ref is gone.
#[test]
fn test_local_writes_refuse_a_stale_base_and_full_refs_and_drop_branches_without_remote() {
    for subject in subjects() {
        if subject.repository == StateLocation::LOCAL_REPOSITORY {
            // Local writes are refused outright for this subject (see
            // `test_local_commits_stay_local_diverge_and_reset_by_compare_and_swap`).
            continue;
        }
        let store = subject.store.as_ref();

        // (a) A stale base is a conflict, and the lease does not move.
        let dev_location = separate(&subject);
        let dev_local = view(&dev_location);
        let dev = environment("dev");
        let first = initialized(store, &dev_location, &dev);
        store.snapshot(&dev_location).unwrap();
        let local_tips = store.snapshot(&dev_local).unwrap().tips;
        assert!(local_tips.desired.is_some(), "{}", subject.name);
        let mut local_transition = transition(seed_changes("seed"));
        local_transition.message = "nyl: local\n\nNyl-Local: true\n".to_owned();
        assert!(
            matches!(
                store.commit_local(
                    &dev_location,
                    &dev,
                    &StateTips::default(),
                    &local_transition,
                    &dev_location,
                    &first.lease_version
                ),
                Err(StateError::Conflict { .. })
            ),
            "{}: a base that is not the local tip",
            subject.name
        );
        assert_eq!(
            store.coordination(&dev_location, &dev).unwrap().lease.unwrap().version,
            first.lease_version,
            "{}: the lease moved before the stale base was refused",
            subject.name
        );
        assert_eq!(store.snapshot(&dev_local).unwrap().tips, local_tips, "{}", subject.name);

        // (b) A full `refs/…` state ref outside refs/heads/ has no local branch.
        let full = location(&subject.repository, "refs/state/full/desired", "nyl/full/observed", "");
        let full_env = environment("full");
        let version = store.take_lease(&full, &full_env, &lease(RUN_A), None).unwrap();
        let mut both_sides = init(&full, "full");
        both_sides.changes = TransitionChanges {
            record: both_sides.changes.record,
            ..seed_changes("seed")
        };
        let committed = store
            .commit(&full, &full_env, &StateTips::default(), &both_sides, &version)
            .unwrap();
        let remote = store.snapshot(&full).unwrap().tips;
        assert!(remote.observed.is_some(), "{}", subject.name);
        assert_eq!(
            store.local_divergence(&full).unwrap(),
            LocalDivergence {
                desired: None,
                observed: Some(Divergence { ahead: 0, behind: 0 }),
            },
            "{}",
            subject.name
        );
        assert!(
            matches!(store.reset_local(&full, &remote), Err(StateError::Unavailable(_))),
            "{}: reset of a full ref",
            subject.name
        );
        assert!(
            matches!(
                store.commit_local(
                    &full,
                    &full_env,
                    &remote,
                    &local_transition,
                    &full,
                    &committed.lease_version
                ),
                Err(StateError::Unavailable(_))
            ),
            "{}: local commit to a full ref",
            subject.name
        );
        assert_eq!(
            store.coordination(&full, &full_env).unwrap().lease.unwrap().version,
            committed.lease_version,
            "{}: a refused local commit moved the lease",
            subject.name
        );

        // (c) A reset removes a local branch whose remote ref is missing.
        let fresh = location(&subject.repository, "nyl/fresh/desired", "nyl/fresh/observed", "");
        let fresh_local = view(&fresh);
        let fresh_env = environment("fresh");
        let version = store.take_lease(&fresh, &fresh_env, &lease(RUN_A), None).unwrap();
        let committed = store
            .commit_local(
                &fresh,
                &fresh_env,
                &StateTips::default(),
                &init(&fresh, "fresh"),
                &fresh,
                &version,
            )
            .unwrap();
        let tips = store.snapshot(&fresh_local).unwrap().tips;
        assert!(tips.desired.is_some(), "{}", subject.name);
        assert_eq!(tips.desired, committed.desired, "{}", subject.name);
        assert_eq!(
            store.snapshot(&fresh).unwrap().tips,
            StateTips::default(),
            "{}: remote state was written",
            subject.name
        );
        assert_eq!(
            store.local_divergence(&fresh).unwrap(),
            LocalDivergence::default(),
            "{}: no divergence without a remote ref",
            subject.name
        );
        assert_eq!(
            store.reset_local(&fresh, &tips).unwrap(),
            StateTips::default(),
            "{}",
            subject.name
        );
        assert_eq!(
            store.snapshot(&fresh_local).unwrap().tips,
            StateTips::default(),
            "{}: the local branches survived the reset",
            subject.name
        );
    }
}
