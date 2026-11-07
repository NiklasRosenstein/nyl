//! What an Argo CD Application reports, mapped into what a reading holds
//! ([`Observed`]): the revision it runs, its live health, its source, and
//! whether it is being deleted.
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
//!   running a mix of revisions, which proves no revision
//!   ([`Running::Mixed`]): a selective sync (`operation.sync.resources`), a
//!   full sync that failed or errored (it may have applied part of its
//!   revision), and one still running or terminating. A dry run changes
//!   nothing itself, but `operationState` holds only the latest operation, so
//!   a dry run after the entry may hide a selective sync before it: it counts
//!   as a mix too.
//! - **Health** is `status.health.status`, Argo CD's live health, mapped to
//!   [`Health`]: `Healthy`, `Progressing`, and `Degraded` as reported, and
//!   anything else (`Suspended`, `Missing`, `Unknown`, nothing) as
//!   [`Health::Unknown`], which is never a pass.
//! - The **source** is `spec.source` as it is now and the `source` the newest
//!   history entry synced, each as a [`Source`](nyl_render::gitops::observation::Source).

use chrono::{DateTime, Utc};
use nyl_render::gitops::observation::argocd_source;
use serde_json::Value;

use crate::kubernetes_publication::reconciler::{Health, Observed, Running};

/// Why an Application runs no single revision.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Why {
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

impl Why {
    fn reason(self) -> String {
        match self {
            Self::SelectiveSync => {
                "a selective sync newer than its last full sync leaves it running a mix of revisions: sync the whole Application".to_owned()
            }
            Self::DryRun => {
                "a dry run is its latest operation, so an earlier selective sync may be hidden behind it: sync the whole Application".to_owned()
            }
            Self::UnfinishedSync(phase) => format!(
                "a full sync newer than its last completed one is {phase}, so it may run part of another revision: \
                 wait for it, or sync again"
            ),
        }
    }
}

/// Read an Application document as a reading holds it. A document that is
/// no Application object is an error; one without `status` has simply never
/// synced.
pub fn parse(application: &Value) -> Result<Observed, String> {
    if !application.is_object() {
        return Err("an Application must be a JSON object".to_owned());
    }
    let (running, synced_source) = running(application);
    Ok(Observed {
        running,
        health: health(application),
        reported: application
            .pointer("/status/health/status")
            .and_then(Value::as_str)
            .filter(|status| !status.is_empty())
            .map(str::to_owned),
        deleting: application
            .pointer("/metadata/deletionTimestamp")
            .is_some_and(|value| !value.is_null()),
        // Argo CD's Degraded passes: controllers retry, and so does a wait.
        terminal: false,
        source: application
            .pointer("/spec/source")
            .filter(|source| source.is_object())
            .map(argocd_source),
        synced_source,
    })
}

fn health(application: &Value) -> Health {
    match application.pointer("/status/health/status").and_then(Value::as_str) {
        Some("Healthy") => Health::Healthy,
        Some("Progressing") => Health::Progressing,
        Some("Degraded") => Health::Degraded,
        _ => Health::Unknown,
    }
}

/// The running revision, and the source the newest history entry synced
/// when it names a single revision.
fn running(application: &Value) -> (Running, Option<nyl_render::gitops::observation::Source>) {
    let history = application
        .pointer("/status/history")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let Some(newest) = newest_entry(history) else {
        // Without an entry, a selective or unfinished operation changes nothing we can prove.
        return (Running::None, None);
    };
    let revision = single_revision(newest);
    if let Some(why) = newer_operation(application, newest) {
        return (
            Running::Mixed {
                last: revision,
                reason: why.reason(),
            },
            None,
        );
    }
    match revision {
        Some(revision) => (
            Running::Exact(revision),
            newest
                .get("source")
                .or_else(|| newest.pointer("/sources/0"))
                .filter(|source| source.is_object())
                .map(argocd_source),
        ),
        None => (
            Running::Unreadable(
                "the newest status.history entry names no single revision (a multi-source Application is not one Nyl generates)"
                    .to_owned(),
            ),
            None,
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
fn newer_operation(application: &Value, newest: &Value) -> Option<Why> {
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
        return Some(Why::UnfinishedSync("non-sync operation".to_owned()));
    };
    let selective = sync
        .get("resources")
        .and_then(Value::as_array)
        .is_some_and(|resources| !resources.is_empty());
    if selective {
        return Some(Why::SelectiveSync);
    }
    if sync.get("dryRun").and_then(Value::as_bool) == Some(true) {
        return Some(Why::DryRun);
    }
    match operation.get("phase").and_then(Value::as_str) {
        Some("Succeeded") => None,
        Some(phase) => Some(Why::UnfinishedSync(phase.to_owned())),
        None => Some(Why::UnfinishedSync("unknown".to_owned())),
    }
}

#[cfg(test)]
mod tests {
    use nyl_render::gitops::observation::Source;
    use serde_json::json;

    use super::*;

    fn application(status: Value) -> Value {
        let mut application = json!({"metadata": {"name": "web"}, "spec": {"source": {
            "repoURL": "https://example.invalid/deploy.git", "path": "dev/web/web", "targetRevision": "deploy",
            "directory": {"recurse": true}}}});
        application["status"] = status;
        application
    }

    fn entry(id: i64, revision: &str, deployed: &str) -> Value {
        json!({"id": id, "revision": revision, "deployedAt": deployed, "deployStartedAt": deployed,
               "source": {"path": "dev/web/web", "repoURL": "https://example.invalid/deploy.git"}})
    }

    fn parse_status(status: Value) -> Observed {
        parse(&application(status)).unwrap()
    }

    fn mixed(observed: &Observed) -> (Option<&str>, &str) {
        match &observed.running {
            Running::Mixed { last, reason } => (last.as_deref(), reason),
            other => panic!("expected a mix, got {other:?}"),
        }
    }

    /// Catches the running revision coming from the sync status or the
    /// operation's result instead of the newest history entry, and an older
    /// entry beating a newer one when Argo CD lists them out of order.
    #[test]
    fn test_running_revision_is_the_newest_history_entry() {
        let live = parse_status(json!({
            "sync": {"status": "OutOfSync", "revision": "branch-head"},
            "health": {"status": "Healthy"},
            "history": [entry(3, "c3", "2026-01-03T00:00:00Z"), entry(1, "c1", "2026-01-01T00:00:00Z"),
                        entry(2, "c2", "2026-01-02T00:00:00Z")],
            "operationState": {"phase": "Succeeded", "startedAt": "2026-01-03T00:00:00Z",
                               "operation": {"sync": {"revision": "c3"}}, "syncResult": {"revision": "c3"}},
        }));
        assert_eq!(live.running, Running::Exact("c3".to_owned()));
        assert_eq!(
            live.synced_source,
            Some(Source {
                repository: "https://example.invalid/deploy.git".to_owned(),
                path: "dev/web/web".to_owned(),
                options: Value::Null,
            })
        );
        assert_eq!(live.health, Health::Healthy);
        // The configured source, without the revision it tracks.
        let source = live.source.unwrap();
        assert_eq!(source.path, "dev/web/web");
        assert_eq!(source.options, json!({"directory": {"recurse": true}}));
        assert!(!live.deleting);
    }

    /// Catches an Application that never synced reporting a revision, a
    /// status-less (just created) one failing to parse, and a health Argo CD
    /// does not report reading as a pass.
    #[test]
    fn test_an_application_without_history_never_synced() {
        let never = parse_status(json!({"health": {"status": "Missing"}}));
        assert_eq!(never.running, Running::None);
        assert_eq!(never.health, Health::Unknown);
        let bare = parse(&json!({"metadata": {"name": "web"}})).unwrap();
        assert_eq!(bare.running, Running::None);
        assert_eq!(bare.health, Health::Unknown);
        assert_eq!(bare.source, None);
        assert!(parse(&json!("not an application")).is_err());
    }

    /// Catches Argo CD's health vocabulary leaking past the mapping: the
    /// three it shares with the neutral reading pass through, and `Suspended`,
    /// `Missing`, `Unknown`, and anything new are Unknown, never Healthy.
    #[test]
    fn test_health_maps_to_the_neutral_vocabulary() {
        for (reported, expected) in [
            ("Healthy", Health::Healthy),
            ("Progressing", Health::Progressing),
            ("Degraded", Health::Degraded),
            ("Suspended", Health::Unknown),
            ("Missing", Health::Unknown),
            ("Unknown", Health::Unknown),
            ("Whatever", Health::Unknown),
        ] {
            assert_eq!(
                parse_status(json!({"health": {"status": reported}})).health,
                expected,
                "{reported}"
            );
        }
    }

    /// Catches a selective sync newer than the history vouching for the
    /// history's revision, although it applied some resources of another.
    #[test]
    fn test_a_newer_selective_sync_leaves_a_mix() {
        let live = parse_status(json!({
            "history": [entry(1, "c1", "2026-01-01T00:00:00Z")],
            "operationState": {"phase": "Succeeded", "startedAt": "2026-01-02T00:00:00Z",
                               "operation": {"sync": {"revision": "c2", "resources": [{"kind": "Deployment", "name": "web"}]}},
                               "syncResult": {"revision": "c2"}},
        }));
        let (last, reason) = mixed(&live);
        assert_eq!(last, Some("c1"));
        assert!(
            reason.contains("selective sync") && reason.contains("sync the whole Application"),
            "{reason}"
        );
        assert_eq!(live.synced_source, None);
    }

    /// Catches a failed or running full sync after the newest entry counted
    /// as the entry's revision: it may have applied part of its own.
    #[test]
    fn test_a_newer_unfinished_full_sync_leaves_a_mix() {
        for phase in ["Failed", "Error", "Running", "Terminating"] {
            let live = parse_status(json!({
                "history": [entry(1, "c1", "2026-01-01T00:00:00Z")],
                "operationState": {"phase": phase, "startedAt": "2026-01-02T00:00:00Z",
                                   "operation": {"sync": {"revision": "c2"}}},
            }));
            let (last, reason) = mixed(&live);
            assert_eq!(last, Some("c1"), "{phase}");
            assert!(reason.contains(&format!("is {phase}")), "{reason}");
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
                matches!(parse_status(status.clone()).running, Running::Exact(_)),
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
        let observed = parse_status(dry_run);
        let (last, reason) = mixed(&observed);
        assert_eq!(last, Some("c"));
        assert!(reason.contains("dry run"), "{reason}");
        let same_second = json!({
            "history": [entry(5, "c", "2026-01-01T00:00:00Z")],
            "operationState": {"phase": "Succeeded", "startedAt": "2026-01-01T00:00:00Z",
                               "operation": {"sync": {"resources": [{"kind": "Deployment", "name": "web"}]}}},
        });
        assert!(mixed(&parse_status(same_second)).1.contains("selective sync"));
        let other = json!({
            "history": [entry(5, "c", "2026-01-01T00:00:00Z")],
            "operationState": {"phase": "Succeeded", "startedAt": "2026-01-03T00:00:00Z", "operation": {}},
        });
        assert!(mixed(&parse_status(other)).1.contains("non-sync operation"));
    }

    /// Catches an operation whose time cannot be compared with the entry's
    /// vouching for a revision.
    #[test]
    fn test_an_incomparable_selective_operation_is_assumed_newer() {
        let live = parse_status(json!({
            "history": [entry(1, "c1", "2026-01-01T00:00:00Z")],
            "operationState": {"phase": "Succeeded", "operation": {"sync": {"resources": [{"kind": "Deployment"}]}}},
        }));
        assert!(mixed(&live).1.contains("selective sync"));
    }

    #[test]
    fn test_multi_revision_entries_and_deletion() {
        let single = parse(&json!({
            "metadata": {"deletionTimestamp": "2026-01-01T00:00:00Z"},
            "status": {"history": [{"id": 1, "revisions": ["c1"]}]},
        }))
        .unwrap();
        assert!(single.deleting);
        assert_eq!(single.running, Running::Exact("c1".to_owned()));
        let multi = parse_status(json!({"history": [{"id": 1, "revisions": ["c1", "c2"]}]}));
        assert!(matches!(multi.running, Running::Unreadable(_)));
    }

    /// Catches the source an edit left in the cluster hiding behind what
    /// Argo CD defaults: the configured source and the one the last sync took
    /// are both kept, without `targetRevision`, nulls, and empty containers.
    #[test]
    fn test_sources_drop_what_argocd_defaults() {
        let mut document = application(json!({"history": [entry(1, "c1", "2026-01-01T00:00:00Z")]}));
        document["spec"]["source"]["targetRevision"] = json!("another");
        document["spec"]["source"]["helm"] = json!({});
        document["spec"]["source"]["plugin"] = Value::Null;
        let observed = parse(&document).unwrap();
        assert_eq!(
            observed.source.unwrap().options,
            json!({"directory": {"recurse": true}})
        );
    }
}
