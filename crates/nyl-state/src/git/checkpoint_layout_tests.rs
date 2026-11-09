//! The files of a checkpoint on a run ref, `checkpoints/<n>-<unit>/`, against
//! golden files.
//!
//! A checkpoint is a directory of state documents, each with its own
//! versioned golden file in `nyl-core`. The directory itself has no version:
//! `attestations.yaml` and `observations.yaml` are optional members, present
//! only when the run recorded some for the receipt, so a checkpoint written
//! before they were carried reads as it did, and a reader that does not know
//! them ignores them.
//!
//! `v0.6.1` is frozen: a checkpoint in the layout v0.6.1 wrote, which every
//! later reader must still read and which no regeneration touches. A change
//! to the layout adds a directory and keeps the old ones. `without-evidence`
//! and `with-evidence` are what the current writer produces; after an
//! intended change, regenerate those two with
//! `cargo test -p nyl-state --lib checkpoint_layout -- --ignored`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use nyl_core::orchestration::ports::Checkpoint;
use nyl_core::orchestration::state::{Attestation, DesiredUnit, ObservationRecord, ObservedUnit, Run};
use serde_json::json;

use super::{insert_checkpoint, read_run, to_yaml, Files, RUN_FILE};

const RUN: &str = "0192f0c4-8d0a-7c3e-9a51-3c1f0e7d2b90";
const UID: &str = "0192f0c4-8d0a-7c3e-9a51-000000000001";
const COMMIT: &str = "3e7b9c0a1d2e3f4a5b6c7d8e9f0a1b2c3d4e5f6a";
const KEY: &str = "sha256:9f2c000000000000000000000000000000000000000000000000000000000001";

fn identity() -> serde_json::Value {
    json!({"environment": "dev", "name": "lb", "uid": UID, "apiVersion": "units.gitops.nyl/v1", "kind": "Command"})
}

fn without_evidence() -> Checkpoint {
    let desired: DesiredUnit = serde_json::from_value(json!({
        "apiVersion": "gitops.nyl/v1", "kind": "DesiredUnit", "unit": identity(),
        "lifecycle": {"state": "active"}, "sourceCommit": COMMIT, "sourceRevision": COMMIT,
        "spec": {"command": ["./run.sh"]}, "resolvedSpec": {"command": ["./run.sh"]},
        "executionKey": KEY, "readiness": {"state": "ready"}
    }))
    .unwrap();
    let observed: ObservedUnit = serde_json::from_value(json!({
        "apiVersion": "gitops.nyl/v1", "kind": "ObservedUnit", "unit": identity(),
        "receipt": {"executionKey": KEY, "behaviorVersion": 1, "run": RUN,
                    "startedAt": "2026-09-24T10:00:00Z", "finishedAt": "2026-09-24T10:01:00Z",
                    "outputs": {"version": "1"}},
        "condition": null
    }))
    .unwrap();
    Checkpoint::new(desired, observed, BTreeMap::new())
}

/// The checkpoint of a receipt whose run attested `healthy` and read `ip`
/// (and could not read `port` yet), each at the time it did.
fn with_evidence() -> Checkpoint {
    let mut checkpoint = without_evidence();
    let attestation: Attestation = serde_json::from_value(json!({
        "apiVersion": "gitops.nyl/v1", "kind": "Attestation", "name": "healthy",
        "unit": {"environment": "dev", "name": "lb", "uid": UID}, "executionKey": KEY, "result": "pass",
        "at": "2026-09-24T10:01:00Z", "source": {"driver": "reconcile"}
    }))
    .unwrap();
    checkpoint.attestations = BTreeMap::from([("healthy".to_owned(), attestation)]);
    let observations: ObservationRecord = serde_json::from_value(json!({
        "apiVersion": "gitops.nyl/v1", "kind": "ObservationRecord", "unit": identity(),
        "executionKey": KEY, "run": RUN, "at": "2026-09-24T10:02:00Z",
        "values": {"ip": {"value": "203.0.113.10", "observedAt": "2026-09-24T10:02:00Z"},
                   "port": {"pending": "no port yet"}}
    }))
    .unwrap();
    checkpoint.observations = Some(observations);
    checkpoint
}

fn golden(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden/checkpoint")
        .join(name)
}

/// The files `insert_checkpoint` writes for `checkpoint`, relative to its directory.
fn written(checkpoint: &Checkpoint) -> BTreeMap<String, String> {
    let mut files = Files::new();
    insert_checkpoint(&mut files, 0, checkpoint).unwrap();
    files
        .into_iter()
        .map(|(path, bytes)| {
            let relative = path
                .strip_prefix("checkpoints/0000-lb/")
                .expect("one directory")
                .to_owned();
            (relative, String::from_utf8(bytes).unwrap())
        })
        .collect()
}

fn stored(name: &str) -> BTreeMap<String, String> {
    let directory = golden(name);
    std::fs::read_dir(&directory)
        .unwrap_or_else(|error| panic!("{}: {error}", directory.display()))
        .map(|entry| {
            let path = entry.unwrap().path();
            (
                path.file_name().unwrap().to_string_lossy().into_owned(),
                std::fs::read_to_string(&path).unwrap(),
            )
        })
        .collect()
}

fn run_record() -> Run {
    serde_json::from_value(json!({
        "apiVersion": "gitops.nyl/v1", "kind": "Run", "run": RUN, "runner": "ci",
        "operation": "reconcile", "deadline": "2026-09-24T11:00:00Z", "units": ["lb"], "finished": false
    }))
    .unwrap()
}

/// Reading `files` of a run ref back.
fn read(name: &str, files: &BTreeMap<String, String>) -> Vec<Checkpoint> {
    let mut tree = Files::new();
    tree.insert(RUN_FILE.to_owned(), to_yaml(&run_record()).unwrap());
    for (path, content) in files {
        tree.insert(format!("checkpoints/0000-lb/{path}"), content.clone().into_bytes());
    }
    read_run(name, &tree, "v".to_owned()).unwrap().checkpoints
}

/// Catches a change to the files a checkpoint is written as, or to the
/// documents in them, that does not add a golden directory and keep the old.
#[test]
fn test_checkpoint_files_match_their_golden_files() {
    assert_eq!(written(&without_evidence()), stored("without-evidence"));
    assert_eq!(written(&with_evidence()), stored("with-evidence"));
}

/// Catches a checkpoint written by v0.6.1, which has neither optional file,
/// not reading back (it holds no evidence), and the evidence files not
/// reading back as written: both ways a run ref survives an upgrade.
#[test]
fn test_golden_checkpoints_read_back() {
    assert_eq!(read("v0.6.1", &stored("v0.6.1")), [without_evidence()]);
    assert_eq!(read("current", &stored("with-evidence")), [with_evidence()]);
}

/// Catches a ref past 9999 checkpoints reading back out of order: the index
/// counts past its four digits, where the directory names stop sorting as
/// text, and the later checkpoint of a unit is its result.
#[test]
fn test_checkpoints_read_back_in_the_order_they_were_written_past_four_digits() {
    let mut tree = Files::new();
    tree.insert(RUN_FILE.to_owned(), to_yaml(&run_record()).unwrap());
    let mut expected = Vec::new();
    for index in [2, 9999, 10000, 10001] {
        let mut checkpoint = without_evidence();
        checkpoint.observed.receipt.as_mut().unwrap().outputs = json!({"index": index});
        insert_checkpoint(&mut tree, index, &checkpoint).unwrap();
        expected.push(checkpoint);
    }
    assert_eq!(
        read_run("ordered", &tree, "v".to_owned()).unwrap().checkpoints,
        expected
    );
}

/// A reader that does not know the evidence files ignores them: the files of
/// the checkpoint it does know read the same without them.
#[test]
fn test_checkpoint_evidence_files_are_optional_members() {
    let mut files = stored("with-evidence");
    files.remove("attestations.yaml");
    files.remove("observations.yaml");
    assert_eq!(read("without", &files), [without_evidence()]);
}

#[test]
#[ignore = "regenerates the golden files"]
fn regenerate_checkpoint_layout_goldens() {
    // Never `v0.6.1`: it is frozen.
    for (name, checkpoint) in [
        ("without-evidence", without_evidence()),
        ("with-evidence", with_evidence()),
    ] {
        let directory = golden(name);
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        for (file, content) in written(&checkpoint) {
            std::fs::write(directory.join(file), content).unwrap();
        }
    }
}
