//! What an Argo CD Application reports, reduced to what attestation reads:
//! the revision it runs, its live health, and whether it is being deleted.
//!
//! Contract: [Kubernetes publications](../../../../../design/promotion.md#kubernetes-publications),
//! running revision. Both access methods return the Application as Argo CD
//! serves it (`metadata`, `spec`, `status`), so this one parser serves them.
//!
//! - The **running revision** is that of the newest `status.history` entry
//!   (the highest `id`), which Argo CD appends only for full, non-dry-run
//!   syncs. Synced/OutOfSync is never read: it compares against the branch
//!   head, so an Application still running an older publication reports
//!   OutOfSync as soon as a newer one is published. The sync *result*
//!   (`status.operationState.syncResult`) is not read either, because Argo
//!   CD also records selective and dry-run syncs there as succeeded.
//! - An operation newer than that entry (or as new: timestamps have second
//!   resolution) that is **not** a completed full sync leaves the Application
//!   running a mix of revisions, which proves no revision: a selective sync
//!   (`operation.sync.resources`), a full sync that failed or errored (it may
//!   have applied part of its revision), and one still running or
//!   terminating. A dry run changes nothing itself, but `operationState`
//!   holds only the latest operation, so a dry run after the entry may hide a
//!   selective sync before it: it counts as a mix too.
//! - **Health** is `status.health.status`, Argo CD's live health, as
//!   reported.

use chrono::{DateTime, Utc};
use serde_json::Value;

/// Why an Application runs no single revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mixed {
    /// A selective sync newer than the newest history entry.
    SelectiveSync,
    /// A full sync newer than the newest history entry that has not
    /// succeeded, with its phase (`Running`, `Failed`, `Error`, ...), or an
    /// operation that is no sync (`non-sync operation`).
    UnfinishedSync(String),
    /// A dry run is the latest operation after the newest history entry, so
    /// an earlier selective sync may be hidden behind it.
    DryRun,
}

/// The revision an Application runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Running {
    /// No history entry: no full sync has completed.
    NeverSynced,
    /// A single revision: the newest history entry's.
    Revision {
        revision: String,
        /// The `source` the entry synced, when it records one.
        source: Option<Value>,
    },
    /// A newer operation leaves a mix of revisions.
    Mixed { history: Option<String>, why: Mixed },
    /// The newest history entry names no single revision.
    Unreadable(String),
}

/// An Application as attestation reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveApplication {
    /// `metadata.deletionTimestamp` is set: Argo CD is removing it.
    pub deleting: bool,
    /// `status.health.status`, such as `Healthy`; `None` before Argo CD reports one.
    pub health: Option<String>,
    /// `spec.source` as it is now.
    pub source: Option<Value>,
    pub running: Running,
}

impl LiveApplication {
    /// Read an Application document. A document that is no Application
    /// object is an error; one without `status` has simply never synced.
    pub fn parse(application: &Value) -> Result<Self, String> {
        if !application.is_object() {
            return Err("an Application must be a JSON object".to_owned());
        }
        let text = |pointer: &str| application.pointer(pointer).and_then(Value::as_str).map(str::to_owned);
        Ok(Self {
            deleting: application
                .pointer("/metadata/deletionTimestamp")
                .is_some_and(|value| !value.is_null()),
            health: text("/status/health/status").filter(|status| !status.is_empty()),
            source: application.pointer("/spec/source").cloned(),
            running: running(application),
        })
    }
}

fn running(application: &Value) -> Running {
    let history = application
        .pointer("/status/history")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let Some(newest) = newest_entry(history) else {
        // Without an entry, a selective or unfinished operation changes nothing we can prove.
        return Running::NeverSynced;
    };
    let revision = single_revision(newest);
    if let Some(why) = newer_operation(application, newest) {
        return Running::Mixed { history: revision, why };
    }
    match revision {
        Some(revision) => Running::Revision {
            revision,
            source: newest
                .get("source")
                .or_else(|| newest.pointer("/sources/0"))
                .cloned(),
        },
        None => Running::Unreadable(
            "the newest status.history entry names no single revision (a multi-source Application is not one Nyl generates)"
                .to_owned(),
        ),
    }
}

/// The entry with the highest `id`; without ids, the last one, since Argo
/// CD lists history oldest first.
fn newest_entry(history: &[Value]) -> Option<&Value> {
    if history.iter().all(|entry| entry.get("id").is_none()) {
        return history.last();
    }
    history
        .iter()
        .max_by_key(|entry| entry.get("id").and_then(Value::as_i64).unwrap_or(i64::MIN))
}

fn single_revision(entry: &Value) -> Option<String> {
    if let Some(revision) = entry
        .get("revision")
        .and_then(Value::as_str)
        .filter(|revision| !revision.is_empty())
    {
        return Some(revision.to_owned());
    }
    match entry.get("revisions").and_then(Value::as_array).map(Vec::as_slice) {
        Some([only]) => only.as_str().map(str::to_owned),
        _ => None,
    }
}

/// An operation newer than the history entry that leaves a mix, if any: any
/// operation as new as the entry that is not a completed full sync.
fn newer_operation(application: &Value, newest: &Value) -> Option<Mixed> {
    let operation = application.pointer("/status/operationState")?;
    let time = |value: &Value, field: &str| {
        value
            .get(field)
            .and_then(Value::as_str)
            .and_then(|time| DateTime::parse_from_rfc3339(time).ok())
            .map(|time| time.with_timezone(&Utc))
    };
    // History records when the sync finished (`deployedAt`) and began
    // (`deployStartedAt`); the operation, when it began.
    let history_time = time(newest, "deployedAt").or_else(|| time(newest, "deployStartedAt"));
    let started = time(operation, "startedAt");
    // `>=`: times have second resolution, so an operation that began in the
    // second the entry finished may be a later one. Times that cannot be
    // compared leave the order unknown: assume newer, so an unreadable
    // operation never vouches for a revision. The operation that produced the
    // entry is itself a completed full sync, which is no mix.
    let newer = match (started, history_time) {
        (Some(started), Some(history)) => started >= history,
        _ => true,
    };
    if !newer {
        return None;
    }
    let Some(sync) = operation.pointer("/operation/sync") else {
        return Some(Mixed::UnfinishedSync("non-sync operation".to_owned()));
    };
    let selective = sync
        .get("resources")
        .and_then(Value::as_array)
        .is_some_and(|resources| !resources.is_empty());
    if selective {
        return Some(Mixed::SelectiveSync);
    }
    if sync.get("dryRun").and_then(Value::as_bool) == Some(true) {
        return Some(Mixed::DryRun);
    }
    match operation.get("phase").and_then(Value::as_str) {
        Some("Succeeded") => None,
        Some(phase) => Some(Mixed::UnfinishedSync(phase.to_owned())),
        None => Some(Mixed::UnfinishedSync("unknown".to_owned())),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn application(status: Value) -> Value {
        let mut application = json!({"metadata": {"name": "web"}, "spec": {"source": {"path": "dev/web/web"}}});
        application["status"] = status;
        application
    }

    fn entry(id: i64, revision: &str, deployed: &str) -> Value {
        json!({"id": id, "revision": revision, "deployedAt": deployed, "deployStartedAt": deployed,
               "source": {"path": "dev/web/web", "repoURL": "https://example.invalid/deploy.git"}})
    }

    fn parse(status: Value) -> LiveApplication {
        LiveApplication::parse(&application(status)).unwrap()
    }

    /// Catches the running revision coming from the sync status or the
    /// operation's result instead of the newest history entry, and an older
    /// entry beating a newer one when Argo CD lists them out of order.
    #[test]
    fn test_running_revision_is_the_newest_history_entry() {
        let live = parse(json!({
            "sync": {"status": "OutOfSync", "revision": "branch-head"},
            "health": {"status": "Healthy"},
            "history": [entry(3, "c3", "2026-01-03T00:00:00Z"), entry(1, "c1", "2026-01-01T00:00:00Z"),
                        entry(2, "c2", "2026-01-02T00:00:00Z")],
            "operationState": {"phase": "Succeeded", "startedAt": "2026-01-03T00:00:00Z",
                               "operation": {"sync": {"revision": "c3"}}, "syncResult": {"revision": "c3"}},
        }));
        assert_eq!(
            live.running,
            Running::Revision {
                revision: "c3".to_owned(),
                source: Some(json!({"path": "dev/web/web", "repoURL": "https://example.invalid/deploy.git"}))
            }
        );
        assert_eq!(live.health.as_deref(), Some("Healthy"));
        assert_eq!(live.source.as_ref().unwrap()["path"], "dev/web/web");
        assert!(!live.deleting);
    }

    /// Catches an Application that never synced reporting a revision, and a
    /// status-less (just created) one failing to parse.
    #[test]
    fn test_an_application_without_history_never_synced() {
        assert_eq!(
            parse(json!({"health": {"status": "Missing"}})).running,
            Running::NeverSynced
        );
        let bare = LiveApplication::parse(&json!({"metadata": {"name": "web"}})).unwrap();
        assert_eq!(bare.running, Running::NeverSynced);
        assert_eq!(bare.health, None);
        assert!(LiveApplication::parse(&json!("not an application")).is_err());
    }

    /// Catches a selective sync newer than the history vouching for the
    /// history's revision, although it applied some resources of another.
    #[test]
    fn test_a_newer_selective_sync_leaves_a_mix() {
        let live = parse(json!({
            "history": [entry(1, "c1", "2026-01-01T00:00:00Z")],
            "operationState": {"phase": "Succeeded", "startedAt": "2026-01-02T00:00:00Z",
                               "operation": {"sync": {"revision": "c2", "resources": [{"kind": "Deployment", "name": "web"}]}},
                               "syncResult": {"revision": "c2"}},
        }));
        assert_eq!(
            live.running,
            Running::Mixed {
                history: Some("c1".to_owned()),
                why: Mixed::SelectiveSync
            }
        );
    }

    /// Catches a failed or running full sync after the newest entry counted
    /// as the entry's revision: it may have applied part of its own.
    #[test]
    fn test_a_newer_unfinished_full_sync_leaves_a_mix() {
        for phase in ["Failed", "Error", "Running", "Terminating"] {
            let live = parse(json!({
                "history": [entry(1, "c1", "2026-01-01T00:00:00Z")],
                "operationState": {"phase": phase, "startedAt": "2026-01-02T00:00:00Z",
                                   "operation": {"sync": {"revision": "c2"}}},
            }));
            assert_eq!(
                live.running,
                Running::Mixed {
                    history: Some("c1".to_owned()),
                    why: Mixed::UnfinishedSync(phase.to_owned())
                },
                "{phase}"
            );
        }
    }

    /// Catches an operation older than the newest entry, or a completed full
    /// sync, being mistaken for a mix.
    #[test]
    fn test_older_operations_and_completed_full_syncs_leave_no_mix() {
        let selective_before = json!({
            "history": [entry(2, "c2", "2026-01-02T00:00:00Z")],
            "operationState": {"phase": "Succeeded", "startedAt": "2026-01-01T00:00:00Z",
                               "operation": {"sync": {"resources": [{"kind": "Deployment"}]}}},
        });
        let full_succeeded = json!({
            "history": [entry(1, "c1", "2026-01-01T00:00:00Z")],
            "operationState": {"phase": "Succeeded", "startedAt": "2026-01-02T00:00:00Z",
                               "operation": {"sync": {"revision": "c2"}}},
        });
        // The operation that produced the entry, in the same second.
        let producer = json!({
            "history": [entry(1, "c1", "2026-01-01T00:00:00Z")],
            "operationState": {"phase": "Succeeded", "startedAt": "2026-01-01T00:00:00Z",
                               "operation": {"sync": {"revision": "c1"}}},
        });
        for status in [selective_before, full_succeeded, producer] {
            assert!(
                matches!(parse(status.clone()).running, Running::Revision { .. }),
                "{status}"
            );
        }
    }

    /// Catches a dry run hiding an earlier selective sync: `operationState`
    /// holds only the latest operation, so the history `c`, a selective
    /// sync of `Deployment/web` at `d`, and then `--dry-run` would read as
    /// running `c` while `web` runs `d`. A dry run newer than the entry
    /// leaves a mix, and so does a selective sync that began in the second
    /// the entry finished.
    #[test]
    fn test_a_dry_run_after_the_history_hides_an_earlier_selective_sync() {
        let dry_run = json!({
            "history": [entry(5, "c", "2026-01-01T00:00:00Z")],
            "operationState": {"phase": "Succeeded", "startedAt": "2026-01-03T00:00:00Z",
                               "operation": {"sync": {"dryRun": true, "revision": "d"}}},
        });
        assert_eq!(
            parse(dry_run).running,
            Running::Mixed {
                history: Some("c".to_owned()),
                why: Mixed::DryRun
            }
        );
        let same_second = json!({
            "history": [entry(5, "c", "2026-01-01T00:00:00Z")],
            "operationState": {"phase": "Succeeded", "startedAt": "2026-01-01T00:00:00Z",
                               "operation": {"sync": {"resources": [{"kind": "Deployment", "name": "web"}]}}},
        });
        assert!(matches!(
            parse(same_second).running,
            Running::Mixed {
                why: Mixed::SelectiveSync,
                ..
            }
        ));
        let other = json!({
            "history": [entry(5, "c", "2026-01-01T00:00:00Z")],
            "operationState": {"phase": "Succeeded", "startedAt": "2026-01-03T00:00:00Z", "operation": {}},
        });
        assert!(matches!(
            parse(other).running,
            Running::Mixed {
                why: Mixed::UnfinishedSync(_),
                ..
            }
        ));
    }

    /// Catches an operation whose time cannot be compared with the entry's
    /// vouching for a revision.
    #[test]
    fn test_an_incomparable_selective_operation_is_assumed_newer() {
        let live = parse(json!({
            "history": [entry(1, "c1", "2026-01-01T00:00:00Z")],
            "operationState": {"phase": "Succeeded", "operation": {"sync": {"resources": [{"kind": "Deployment"}]}}},
        }));
        assert!(matches!(
            live.running,
            Running::Mixed {
                why: Mixed::SelectiveSync,
                ..
            }
        ));
    }

    #[test]
    fn test_multi_revision_entries_and_deletion() {
        let single = LiveApplication::parse(&json!({
            "metadata": {"deletionTimestamp": "2026-01-01T00:00:00Z"},
            "status": {"history": [{"id": 1, "revisions": ["c1"]}]},
        }))
        .unwrap();
        assert!(single.deleting);
        assert!(matches!(single.running, Running::Revision { ref revision, .. } if revision == "c1"));
        let multi = parse(json!({"history": [{"id": 1, "revisions": ["c1", "c2"]}]}));
        assert!(matches!(multi.running, Running::Unreadable(_)));
    }
}
