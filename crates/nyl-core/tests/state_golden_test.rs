//! Golden files for every state document kind and format version.
//!
//! Each golden file is the YAML Nyl writes for a representative document of
//! one kind. The test fails when serialization changes or when a golden file
//! no longer parses, so a format change is always deliberate: it adds a
//! version, a migration, and a new golden file, and keeps the old one.
//! `DesiredUnit` and `ObservedUnit` are at `gitops.nyl/v2` (typed blocked
//! reasons); their `.v1.yaml` files are what v0.6.1 wrote, kept as the inputs
//! of the migration and never regenerated. The other kinds are at `v1`, which
//! is amended in place until a release changes it.
//!
//! After an intended change, regenerate with
//! `cargo test -p nyl-core --test state_golden_test -- --ignored`.

use std::path::{Path, PathBuf};

use nyl_core::orchestration::state::{
    ArtifactDocument, AttestationRecord, BlockedReason, DesiredUnit, DriftRecord, EnvironmentRecord, Lease,
    ObservationRecord, ObservedUnit, Readiness, Run, Signal, StateRecord,
};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{json, Value};

const RUN: &str = "0192f0c4-8d0a-7c3e-9a51-3c1f0e7d2b90";
const UID: &str = "0192f0c4-8d0a-7c3e-9a51-000000000001";
const COMMIT: &str = "3e7b9c0a1d2e3f4a5b6c7d8e9f0a1b2c3d4e5f6a";
const KEY: &str = "sha256:9f2c000000000000000000000000000000000000000000000000000000000001";

fn location() -> Value {
    json!({
        "repository": "https://example.invalid/platform.git",
        "desiredRef": "nyl/dev/desired",
        "observedRef": "nyl/dev/observed",
        "coordinationRefPrefix": "refs/nyl/"
    })
}

fn identity(name: &str, kind: &str) -> Value {
    json!({"environment": "dev", "name": name, "uid": UID, "apiVersion": "units.gitops.nyl/v1", "kind": kind})
}

fn receipt() -> Value {
    json!({
        "executionKey": KEY,
        "behaviorVersion": 1,
        "run": RUN,
        "readDesired": COMMIT,
        "startedAt": "2026-09-24T10:00:00Z",
        "finishedAt": "2026-09-24T10:07:12Z",
        "versions": {"nyl": "0.7.0", "tofu": "1.9.5"},
        "approval": {"by": ["alice"], "identity": "verified", "source": "github-environment production", "digest": KEY.replace("9f2c", "aaaa"), "reviewed": true},
        "provenance": {"/variables/vpc_id": {"unit": "network", "uid": UID, "executionKey": KEY, "observedCommit": COMMIT}},
        "outputs": {"host": "db.dev.internal", "port": 5432}
    })
}

/// One sample per golden file: its file name, and the document as JSON.
fn samples() -> Vec<(&'static str, Value)> {
    let mut samples = vec![
        (
            "state-record.v1.yaml",
            json!({
                "apiVersion": "gitops.nyl/v1", "kind": "StateRecord", "environment": "dev",
                "location": location(),
                "events": [{"type": "state-initialized", "at": "2026-09-24T09:00:00Z", "by": "alice"}]
            }),
        ),
        (
            "state-record-instance.v1.yaml",
            json!({
                "apiVersion": "gitops.nyl/v1", "kind": "StateRecord", "environment": "pr-123",
                "template": "preview",
                "params": {"pr": 123, "size": "small"},
                "expiresAt": "2026-10-01T09:00:00Z",
                "sourceCommit": COMMIT,
                "sourceRef": "refs/pull/123/head",
                "keepId": "0192f0c4-8d0a-7c3e-9a51-3c1f0e7d2b90",
                "location": {
                    "repository": "https://example.invalid/platform.git",
                    "desiredRef": "nyl/previews",
                    "observedRef": "nyl/previews",
                    "path": "pr-123",
                    "coordinationRefPrefix": "refs/nyl/"
                },
                "events": [{"type": "state-initialized", "at": "2026-09-24T09:00:00Z", "by": "alice"}]
            }),
        ),
        (
            "state-record-deleted.v1.yaml",
            json!({
                "apiVersion": "gitops.nyl/v1", "kind": "StateRecord", "environment": "dev",
                "location": location(),
                "events": [
                    {"type": "state-initialized", "at": "2026-09-24T09:00:00Z", "by": "alice"},
                    {"type": "state-deleted", "at": "2026-10-01T09:00:00Z", "by": "alice", "reason": "decommissioned"}
                ]
            }),
        ),
        (
            "environment-record.v1.yaml",
            json!({
                "apiVersion": "gitops.nyl/v1", "kind": "EnvironmentRecord", "environment": "dev",
                "spec": {"unitSelector": {"matchLabels": {"dev": "true"}}, "values": {"target": "dev"}},
                "sourceCommit": COMMIT, "sourceChosenBy": {"by": "entry"},
                "ownership": {"database": UID, "seed": null}
            }),
        ),
        (
            "desired-unit.v2.yaml",
            json!({
                "apiVersion": "gitops.nyl/v2", "kind": "DesiredUnit", "unit": identity("database", "OpenTofu"),
                "lifecycle": {"state": "active"}, "sourceCommit": COMMIT, "sourceRevision": COMMIT,
                "spec": {"source": {"path": "infra/database"}, "variables": {"vpc_id": {"fromUnit": {"unit": "network", "output": "vpcId"}}}},
                "resolvedSpec": {"source": {"path": "/infra/database", "commit": COMMIT}, "variables": {"vpc_id": "vpc-0abc123"}},
                "provenance": {"/variables/vpc_id": {"unit": "network", "uid": UID, "executionKey": KEY, "run": RUN}},
                "executionKey": KEY, "readiness": {"state": "ready"}
            }),
        ),
        (
            "desired-unit-held-deleting.v2.yaml",
            json!({
                "apiVersion": "gitops.nyl/v2", "kind": "DesiredUnit", "unit": identity("cache", "Command"),
                "lifecycle": {
                    "state": "held",
                    "hold": {"since": "2026-09-24T12:05:00Z", "by": "alice", "reason": "incident 4711",
                             "pending": {"sourceCommit": COMMIT, "executionKey": KEY, "deletion": true}},
                    "deletion": {"reason": "omission", "intent": "teardown", "teardown": "pending",
                                 "requested": {"by": "alice", "at": "2026-09-24T12:00:00Z", "reason": "decommission"}}
                },
                "sourceCommit": COMMIT, "sourceRevision": COMMIT, "spec": {"command": ["./cache.sh"]},
                "readiness": {"state": "blocked", "reasons": [{"pointer": "/values/host", "blocker": {"type": "no-current-receipt", "producer": {"unit": "network"}}}]}
            }),
        ),
        (
            // Every kind of blocker, with producers of the consumer's own
            // environment and of another one.
            "desired-unit-blocked.v2.yaml",
            json!({
                "apiVersion": "gitops.nyl/v2", "kind": "DesiredUnit", "unit": identity("app", "Command"),
                "lifecycle": {"state": "active"}, "sourceCommit": COMMIT, "sourceRevision": COMMIT,
                "spec": {"values": {"host": {"fromUnit": {"unit": "network", "output": "host"}}}},
                "readiness": {"state": "blocked", "reasons": [
                    {"pointer": "/values/host", "blocker": {"type": "no-current-receipt", "producer": {"unit": "network"}}},
                    {"pointer": "/dependsOn/0", "blocker": {"type": "no-current-receipt", "producer": {"environment": "infra", "unit": "network"}}},
                    {"pointer": "/dependsOn/1", "blocker": {"type": "attestation", "producer": {"environment": "infra", "unit": "network"}, "name": "healthy", "state": "pending"}},
                    {"pointer": "/dependsOn/2", "blocker": {"type": "attestation", "producer": {"unit": "network"}, "name": "accepted", "state": "failing"}},
                    {"pointer": "/values/ip", "blocker": {"type": "observation", "producer": {"environment": "infra", "unit": "kubernetes"}, "name": "ingressIp", "why": "field-not-set: /status/loadBalancer/ingress/0/ip"}},
                    {"blocker": {"type": "other", "message": "the deleting incarnation of app (kind Command) is removed first"}}
                ]}
            }),
        ),
        (
            "desired-unit-held-marker.v2.yaml",
            json!({
                "apiVersion": "gitops.nyl/v2", "kind": "DesiredUnit",
                "unit": {"environment": "dev", "name": "cache"},
                "lifecycle": {"state": "held", "hold": {"since": "2026-09-24T12:05:00Z", "by": "alice", "reason": "incident 4711"}},
                "readiness": {"state": "ready"}
            }),
        ),
        (
            "desired-unit-retained.v2.yaml",
            json!({
                "apiVersion": "gitops.nyl/v2", "kind": "DesiredUnit", "unit": identity("worker", "Command"),
                "lifecycle": {"state": "retained", "tombstone": {"since": "2026-09-24T12:00:00Z", "receipt": receipt()}},
                "sourceCommit": COMMIT, "sourceRevision": COMMIT, "spec": {"command": ["./worker.sh"]},
                "executionKey": KEY, "readiness": {"state": "ready"}
            }),
        ),
        (
            "observed-unit.v2.yaml",
            json!({
                "apiVersion": "gitops.nyl/v2", "kind": "ObservedUnit", "unit": identity("database", "OpenTofu"),
                "receipt": receipt(),
                "desired": {
                    "apiVersion": "gitops.nyl/v2", "kind": "DesiredUnit", "unit": identity("database", "OpenTofu"),
                    "lifecycle": {"state": "active"}, "sourceCommit": COMMIT, "sourceRevision": COMMIT,
                    "spec": {"source": {"path": "infra/database"}},
                    "resolvedSpec": {"source": {"path": "/infra/database", "commit": COMMIT}},
                    "executionKey": KEY, "readiness": {"state": "ready"}
                },
                "condition": {"state": "failed", "since": "2026-09-24T11:12:00Z", "run": RUN, "category": "tool-error",
                              "retryable": false, "message": "tofu apply exited 1"}
            }),
        ),
        (
            // A publication's two-phase teardown waiting after phase 1: the
            // receipt and its desired copy stay until the teardown completes.
            "observed-unit-waiting.v2.yaml",
            json!({
                "apiVersion": "gitops.nyl/v2", "kind": "ObservedUnit", "unit": identity("kubernetes", "KubernetesPublication"),
                "receipt": {"executionKey": KEY, "behaviorVersion": 1, "run": RUN,
                            "startedAt": "2026-09-24T10:00:00Z", "finishedAt": "2026-09-24T10:01:00Z"},
                "desired": {
                    "apiVersion": "gitops.nyl/v2", "kind": "DesiredUnit", "unit": identity("kubernetes", "KubernetesPublication"),
                    "lifecycle": {"state": "active"}, "sourceCommit": COMMIT, "sourceRevision": COMMIT,
                    "spec": {"target": "dev", "teardownWait": {"strategy": "delay", "duration": "10m"}},
                    "resolvedSpec": {"target": "dev", "teardownWait": {"strategy": "delay", "duration": "10m"}},
                    "executionKey": KEY, "readiness": {"state": "ready"}
                },
                "condition": null,
                "waiting": {"phase": "applications-removed", "reason": "waiting 10m for Argo CD to delete the Applications",
                            "since": "2026-09-25T12:00:00Z", "until": "2026-09-25T12:10:00Z", "phase1Commit": COMMIT, "run": RUN}
            }),
        ),
        (
            "artifact-container-image.v1.yaml",
            json!({
                "apiVersion": "artifacts.gitops.nyl/v1", "kind": "ContainerImage",
                "metadata": {"name": "image", "unit": {"environment": "dev", "name": "web-image", "uid": UID}},
                "executionKey": KEY,
                "spec": {"repository": "registry.example.com/web", "digest": KEY,
                         "reference": format!("registry.example.com/web@{KEY}"),
                         "platforms": ["linux/amd64"], "tags": ["nyl-dev-9f2c000000000000"]}
            }),
        ),
        (
            "drift-record.v1.yaml",
            json!({
                "apiVersion": "gitops.nyl/v1", "kind": "DriftRecord", "unit": identity("database", "OpenTofu"),
                "executionKey": KEY, "at": "2026-09-25T16:40:00Z",
                "result": {"drift": "drifted", "summary": "applying would update"}
            }),
        ),
        (
            "observation-record.v1.yaml",
            json!({
                "apiVersion": "gitops.nyl/v1", "kind": "ObservationRecord", "unit": identity("kubernetes", "KubernetesPublication"),
                "executionKey": KEY, "run": RUN, "at": "2026-09-25T16:40:00Z",
                "values": {
                    "ingressIp": {"value": "203.0.113.10", "observedAt": "2026-09-25T16:40:00Z"},
                    "ingressPort": {"pending": "field-not-set: /status/loadBalancer/ingress/0/port"}
                }
            }),
        ),
        (
            "desired-unit-observed-reference.v2.yaml",
            json!({
                "apiVersion": "gitops.nyl/v2", "kind": "DesiredUnit", "unit": identity("dns", "Command"),
                "lifecycle": {"state": "active"}, "sourceCommit": COMMIT, "sourceRevision": COMMIT,
                "spec": {"values": {"address": {"fromUnit": {"unit": "kubernetes", "observation": "ingressIp"}}}},
                "resolvedSpec": {"values": {"address": "203.0.113.10"}},
                "provenance": {"/values/address": {"unit": "kubernetes", "uid": UID, "executionKey": KEY,
                                                    "observedCommit": COMMIT, "observation": "ingressIp",
                                                    "observedAt": "2026-09-25T16:40:00Z"}},
                "executionKey": KEY, "readiness": {"state": "ready"}
            }),
        ),
        (
            "attestation-record.v1.yaml",
            json!({
                "apiVersion": "gitops.nyl/v1", "kind": "AttestationRecord",
                "unit": {"environment": "dev", "name": "kubernetes", "uid": UID},
                "attestations": {
                    "accepted": {"executionKey": KEY, "result": "pass", "at": "2026-09-25T16:40:00Z",
                                 "source": {"driver": "attest"}, "reason": null},
                    "healthy": {"executionKey": KEY, "result": "fail", "at": "2026-09-25T16:40:00Z",
                                "source": {"driver": "attest"}, "reason": "Degraded",
                                "details": {"applications": {"web": {"health": "Degraded"}}}}
                }
            }),
        ),
        (
            "lease.v1.yaml",
            json!({
                "apiVersion": "gitops.nyl/v1", "kind": "Lease", "run": RUN, "runner": "https://ci.example.com/runs/1234",
                "operation": "reconcile", "units": ["database"], "deadline": "2026-09-24T11:10:00Z",
                "takenAt": "2026-09-24T10:00:00Z"
            }),
        ),
        (
            "lease-broken.v1.yaml",
            json!({
                "apiVersion": "gitops.nyl/v1", "kind": "Lease", "run": RUN, "runner": "https://ci.example.com/runs/1234",
                "operation": "reconcile", "deadline": "2026-09-24T11:10:00Z", "takenAt": "2026-09-24T10:00:00Z",
                "broken": {"by": "alice", "at": "2026-09-24T10:30:00Z", "reason": "runner lost"}
            }),
        ),
        (
            "run.v1.yaml",
            json!({
                "apiVersion": "gitops.nyl/v1", "kind": "Run", "run": RUN, "runner": "https://ci.example.com/runs/1234",
                "operation": "reconcile", "sourceCommit": COMMIT, "readDesired": COMMIT, "readObserved": COMMIT,
                "deadline": "2026-09-24T11:10:00Z", "units": ["database"], "finished": true
            }),
        ),
        (
            "signal.v1.yaml",
            json!({
                "apiVersion": "gitops.nyl/v1", "kind": "Signal", "run": RUN, "action": "confirm-removed",
                "unit": "kubernetes", "phase1Commit": COMMIT, "by": "alice", "identity": "local", "reason": "checked in Argo CD",
                "at": "2026-09-24T12:30:00Z", "from": "alice@workstation"
            }),
        ),
        (
            "signal-hold.v1.yaml",
            json!({
                "apiVersion": "gitops.nyl/v1", "kind": "Signal", "run": RUN, "action": "hold", "unit": "database",
                "by": "alice", "identity": "local", "reason": "incident 4711",
                "at": "2026-09-24T12:30:00Z", "from": "alice@workstation"
            }),
        ),
    ];
    let mut repaired = samples
        .iter()
        .find(|(name, _)| *name == "observed-unit.v2.yaml")
        .unwrap()
        .1
        .clone();
    repaired["operatorModified"] = json!({"by":"operator","at":"2026-10-28T00:00:00Z","reason":"restore native state","status":{"status":"exited","code":0}});
    repaired["retry"] = json!({"state":"acknowledged","context":{"version":1,"resource":"test-owned-context"},"request":{"by":"operator","at":"2026-10-28T00:00:00Z","reason":"verified the resource"}});
    let mut required = repaired.clone();
    required["retry"] = json!({"state":"required","context":{"version":1,"resource":"test-owned-context"}});
    samples.push(("observed-unit-retry-required.v2.yaml", required));
    samples.push(("observed-unit-repaired.v2.yaml", repaired));
    samples
}

fn golden_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/state")
}

/// Parse a sample through its kind's type and write it as Nyl would.
fn render(name: &str, sample: &Value) -> String {
    fn typed<T: DeserializeOwned + Serialize>(name: &str, sample: &Value) -> String {
        let document: T = serde_json::from_value(sample.clone()).unwrap_or_else(|error| panic!("{name}: {error}"));
        serde_saphyr::to_string(&document).unwrap()
    }
    match sample["kind"].as_str().unwrap() {
        "StateRecord" => typed::<StateRecord>(name, sample),
        "EnvironmentRecord" => typed::<EnvironmentRecord>(name, sample),
        "DesiredUnit" => typed::<DesiredUnit>(name, sample),
        "ObservedUnit" => typed::<ObservedUnit>(name, sample),
        "ContainerImage" => typed::<ArtifactDocument>(name, sample),
        "DriftRecord" => typed::<DriftRecord>(name, sample),
        "ObservationRecord" => typed::<ObservationRecord>(name, sample),
        "AttestationRecord" => typed::<AttestationRecord>(name, sample),
        "Lease" => typed::<Lease>(name, sample),
        "Run" => typed::<Run>(name, sample),
        "Signal" => typed::<Signal>(name, sample),
        other => panic!("no type for kind {other}"),
    }
}

/// Parse a golden file through its kind's type and write it again.
fn reparse(name: &str, text: &str) -> String {
    let value: Value = serde_saphyr::from_str(text).unwrap_or_else(|error| panic!("{name}: {error}"));
    render(name, &value)
}

#[test]
fn test_state_documents_match_their_golden_files() {
    for (name, sample) in samples() {
        let path = golden_dir().join(name);
        let golden = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("{} is missing ({error}); regenerate it", path.display()));
        assert_eq!(
            render(name, &sample),
            golden,
            "{name} changed; a format change adds a version"
        );
        assert_eq!(reparse(name, &golden), golden, "{name} does not round-trip");
    }
}

#[test]
fn test_readers_reject_unknown_format_versions_and_kinds() {
    let mut lease = samples()
        .into_iter()
        .find(|(name, _)| *name == "lease.v1.yaml")
        .unwrap()
        .1;
    lease["apiVersion"] = json!("gitops.nyl/v2");
    let error = serde_json::from_value::<Lease>(lease.clone()).unwrap_err().to_string();
    assert!(error.contains("upgrade Nyl"), "{error}");
    lease["apiVersion"] = json!("gitops.nyl/v1");
    lease["kind"] = json!("Run");
    assert!(serde_json::from_value::<Lease>(lease).is_err());
}

#[test]
fn test_migrated_kinds_read_their_older_version_and_reject_newer_ones() {
    let mut desired = samples()
        .into_iter()
        .find(|(name, _)| *name == "desired-unit.v2.yaml")
        .unwrap()
        .1;
    for version in ["gitops.nyl/v1", "gitops.nyl/v2"] {
        desired["apiVersion"] = json!(version);
        assert!(
            serde_json::from_value::<DesiredUnit>(desired.clone()).is_ok(),
            "{version}"
        );
    }
    desired["apiVersion"] = json!("gitops.nyl/v3");
    let error = serde_json::from_value::<DesiredUnit>(desired).unwrap_err().to_string();
    assert!(error.contains("upgrade Nyl"), "{error}");
}

#[test]
fn test_observed_v2_preserves_evidence_and_rejects_unknown_versions() {
    for name in ["observed-unit", "observed-unit-waiting", "observed-unit-repaired"] {
        let original = std::fs::read_to_string(golden_dir().join(format!("{name}.v2.yaml"))).unwrap();
        let document: ObservedUnit = serde_saphyr::from_str(&original).unwrap();
        assert_eq!(serde_saphyr::to_string(&document).unwrap(), original);
        let mut unknown = serde_json::to_value(document).unwrap();
        unknown["apiVersion"] = json!("gitops.nyl/v3");
        assert!(serde_json::from_value::<ObservedUnit>(unknown).is_err());
    }
}

/// The `gitops.nyl/v1` documents of the kinds that moved to `gitops.nyl/v2`,
/// and the current golden file each one migrates to. The v1 files are what
/// Nyl wrote before the move and are never regenerated, so they keep every
/// migration covered. `desired-unit-blocked.v1.yaml` holds every wording a
/// blocked reason had.
const MIGRATED: [(&str, &str); 8] = [
    ("desired-unit.v1.yaml", "desired-unit.v2.yaml"),
    (
        "desired-unit-held-deleting.v1.yaml",
        "desired-unit-held-deleting.v2.yaml",
    ),
    ("desired-unit-held-marker.v1.yaml", "desired-unit-held-marker.v2.yaml"),
    ("desired-unit-retained.v1.yaml", "desired-unit-retained.v2.yaml"),
    (
        "desired-unit-observed-reference.v1.yaml",
        "desired-unit-observed-reference.v2.yaml",
    ),
    ("desired-unit-blocked.v1.yaml", "desired-unit-blocked.v2.yaml"),
    ("observed-unit.v1.yaml", "observed-unit.v2.yaml"),
    ("observed-unit-waiting.v1.yaml", "observed-unit-waiting.v2.yaml"),
];

/// Catches a migration that changes more than the format: a stored
/// `gitops.nyl/v1` document must read and write back as exactly the current
/// golden document, with its blocked reasons typed.
#[test]
fn test_v1_documents_migrate_to_the_current_golden_files() {
    for (old, current) in MIGRATED {
        let text = std::fs::read_to_string(golden_dir().join(old)).unwrap();
        let value: Value = serde_saphyr::from_str(&text).unwrap_or_else(|error| panic!("{old}: {error}"));
        assert_eq!(value["apiVersion"], "gitops.nyl/v1", "{old} is a v1 document");
        let golden = std::fs::read_to_string(golden_dir().join(current)).unwrap();
        assert_eq!(render(old, &value), golden, "{old} must migrate to {current}");
    }
}

/// Catches a migration that rewrites what a blocked reason says: the typed
/// blockers of the migrated document render the text the v1 document stored.
#[test]
fn test_migrated_blocked_reasons_render_the_text_they_stored() {
    let text = std::fs::read_to_string(golden_dir().join("desired-unit-blocked.v1.yaml")).unwrap();
    let stored: Value = serde_saphyr::from_str(&text).unwrap();
    let document: DesiredUnit = serde_saphyr::from_str(&text).unwrap();
    let Readiness::Blocked { reasons } = &document.readiness else {
        panic!("the document is blocked");
    };
    let stored: Vec<&str> = stored["readiness"]["reasons"]
        .as_array()
        .unwrap()
        .iter()
        .map(|reason| reason["reason"].as_str().unwrap())
        .collect();
    let rendered: Vec<String> = reasons.iter().map(BlockedReason::message).collect();
    assert_eq!(rendered, stored);
}

#[test]
#[ignore = "writes the golden files; run after an intended format change"]
fn regenerate_state_golden_files() {
    std::fs::create_dir_all(golden_dir()).unwrap();
    for (name, sample) in samples() {
        std::fs::write(golden_dir().join(name), render(name, &sample)).unwrap();
    }
}

#[test]
fn test_artifact_digest_survives_a_yaml_round_trip() {
    // The shell digests the document it writes; resolution digests the one
    // it reads back. A digest that changed across YAML would block every
    // artifact reference with a digest mismatch.
    let text = std::fs::read_to_string(golden_dir().join("artifact-container-image.v1.yaml")).unwrap();
    let document: nyl_core::orchestration::state::ArtifactDocument = serde_saphyr::from_str(&text).unwrap();
    let written = serde_saphyr::to_string(&document).unwrap();
    let read: nyl_core::orchestration::state::ArtifactDocument = serde_saphyr::from_str(&written).unwrap();
    assert!(document.digest().starts_with("sha256:"));
    assert_eq!(read.digest(), document.digest());
}
