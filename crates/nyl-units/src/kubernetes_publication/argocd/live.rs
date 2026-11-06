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
//! - An operation newer than that entry that is **not** a complete, finished
//!   sync leaves the Application running a mix of revisions, which proves no
//!   revision: a selective sync (`operation.sync.resources`), a full sync
//!   that failed or errored (it may have applied part of its revision), and
//!   one still running or terminating. A dry run changes nothing and is
//!   ignored.
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
    /// succeeded, with its phase (`Running`, `Failed`, `Error`, ...).
    UnfinishedSync(String),
}

/// The revision an Application runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Running {
    /// No history entry: no full sync has completed.
    NeverSynced,
    /// A single revision: the newest history entry's.
    Revision {
        revision: String,
        /// The `source.path` the entry synced, when it records one.
        source_path: Option<String>,
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
    /// `spec.source.path` as it is now.
    pub source_path: Option<String>,
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
            source_path: text("/spec/source/path"),
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
            source_path: newest
                .pointer("/source/path")
                .or_else(|| newest.pointer("/sources/0/path"))
                .and_then(Value::as_str)
                .map(str::to_owned),
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

/// An operation newer than the history entry that leaves a mix, if any.
fn newer_operation(application: &Value, newest: &Value) -> Option<Mixed> {
    let operation = application.pointer("/status/operationState")?;
    let sync = operation.pointer("/operation/sync")?;
    if sync.get("dryRun").and_then(Value::as_bool) == Some(true) {
        return None;
    }
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
    // Times that cannot be compared leave the order unknown: assume newer, so
    // an unreadable operation never vouches for a revision.
    let newer = match (started, history_time) {
        (Some(started), Some(history)) => started > history,
        _ => true,
    };
    if !newer {
        return None;
    }
    let selective = sync
        .get("resources")
        .and_then(Value::as_array)
        .is_some_and(|resources| !resources.is_empty());
    if selective {
        return Some(Mixed::SelectiveSync);
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
                source_path: Some("dev/web/web".to_owned())
            }
        );
        assert_eq!(live.health.as_deref(), Some("Healthy"));
        assert_eq!(live.source_path.as_deref(), Some("dev/web/web"));
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

    /// Catches the operation behind the newest entry, a dry run, or a
    /// successful full sync being mistaken for a mix.
    #[test]
    fn test_older_operations_dry_runs_and_full_syncs_leave_no_mix() {
        let selective_before = json!({
            "history": [entry(2, "c2", "2026-01-02T00:00:00Z")],
            "operationState": {"phase": "Succeeded", "startedAt": "2026-01-01T00:00:00Z",
                               "operation": {"sync": {"resources": [{"kind": "Deployment"}]}}},
        });
        let dry_run = json!({
            "history": [entry(1, "c1", "2026-01-01T00:00:00Z")],
            "operationState": {"phase": "Succeeded", "startedAt": "2026-01-02T00:00:00Z",
                               "operation": {"sync": {"dryRun": true, "resources": [{"kind": "Deployment"}]}}},
        });
        let full_succeeded = json!({
            "history": [entry(1, "c1", "2026-01-01T00:00:00Z")],
            "operationState": {"phase": "Succeeded", "startedAt": "2026-01-02T00:00:00Z",
                               "operation": {"sync": {"revision": "c2"}}},
        });
        for status in [selective_before, dry_run, full_succeeded] {
            assert!(
                matches!(parse(status.clone()).running, Running::Revision { .. }),
                "{status}"
            );
        }
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
