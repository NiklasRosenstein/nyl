//! A workaround for a race in Argo CD that leaks its hook finalizer, so that the
//! suite can test Nyl. Remove this module once Argo CD fixes the race.
//!
//! Argo CD (seen with v3.5.7) puts `argocd.argoproj.io/hook-finalizer` on every
//! hook it creates and removes it once the hook completed. When a hook with a
//! fixed `metadata.name` runs again, its default `BeforeHookCreation` policy
//! deletes the previous object and creates a new one under the same name. In
//! gitops-engine's `pkg/sync/sync_context.go`:
//!
//! - `liveObj` matches a task to a cached live object by group, kind,
//!   namespace, and name only, never by UID. While the controller's cluster
//!   cache still holds the deleted predecessor (its delete answered NotFound,
//!   so the new hook was created in the same pass), the next pass takes that
//!   stale, completed copy as the live state of the new hook.
//! - `getOperationPhase` then reports the hook Succeeded from the predecessor's
//!   status, and the operation finishes while the new Job still runs.
//! - `removeHookFinalizer` edits the stale copy, which has no finalizer any
//!   more, finds nothing to remove, and returns without calling the API server.
//!
//! The new Job keeps the finalizer. When the Application is deleted with
//! cascade, the Job never goes, and Argo CD logs "2 objects remaining for
//! deletion" until a teardown that waits for it times out. Nyl reports that
//! correctly (`applications-remain`), so without this workaround the suite
//! would fail on Argo CD's bug, not on Nyl's behavior.
//!
//! The suite plays the operator here: after Argo CD's last operation on the
//! Application finished, a finished hook Job of that Application that still
//! carries the finalizer has leaked it, and the suite removes that one
//! finalizer, logging the Job and its UID each time, so a CI log shows whether
//! the race happened.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde_json::Value;

use crate::error::{Error, Result};
use crate::fixture::{ARGOCD_NAMESPACE, NAMESPACE};
use crate::kubectl::Kubectl;
use crate::wait::{Poll, Waiter};

/// The finalizer Argo CD puts on the hooks it creates (gitops-engine's
/// `hook.HookFinalizer`).
pub const HOOK_FINALIZER: &str = "argocd.argoproj.io/hook-finalizer";

/// The annotation Argo CD tracks the objects of an Application by:
/// `<application>:<group>/<kind>:<namespace>/<name>`.
const TRACKING_ID: &str = "argocd.argoproj.io/tracking-id";
/// The label Argo CD tracks them by with label tracking.
const INSTANCE_LABEL: &str = "app.kubernetes.io/instance";

/// What the workaround makes of one Job after the Application's sync.
#[derive(Debug, PartialEq, Eq)]
pub enum HookJob {
    /// Not the workaround's concern: it does not carry the hook finalizer, does
    /// not belong to the Application, or the Application's last operation has not
    /// finished, so Argo CD may still remove the finalizer itself.
    Untouched,
    /// It carries the finalizer and has not finished, although the operation
    /// that ran it has: the race happened, and the Job must finish first.
    Running { name: String, uid: String },
    /// It finished and carries the finalizer after the operation finished: the
    /// leaked finalizer, at `index` in `metadata.finalizers`.
    Leaked {
        name: String,
        uid: String,
        condition: String,
        index: usize,
    },
}

/// How Argo CD's last operation on `application` ended, for example
/// `Succeeded at 2026-11-09T15:52:28Z`, or `None` while one is queued or
/// running, or when none ever ran.
pub fn last_operation_finished(application: &Value) -> Option<String> {
    if application
        .get("operation")
        .is_some_and(|operation| !operation.is_null())
    {
        return None;
    }
    let state = application.pointer("/status/operationState")?;
    let phase = state.get("phase").and_then(Value::as_str)?;
    let finished_at = state.get("finishedAt").and_then(Value::as_str)?;
    matches!(phase, "Succeeded" | "Failed" | "Error").then(|| format!("{phase} at {finished_at}"))
}

/// The Job condition that says it finished (`Complete` or `Failed`), if one holds.
fn finished_condition(job: &Value) -> Option<String> {
    job.pointer("/status/conditions")?
        .as_array()?
        .iter()
        .filter(|condition| condition.get("status").and_then(Value::as_str) == Some("True"))
        .filter_map(|condition| condition.get("type").and_then(Value::as_str))
        .find(|kind| matches!(*kind, "Complete" | "Failed"))
        .map(str::to_owned)
}

/// Whether Argo CD tracks `object` as part of `application`.
fn tracked_by(object: &Value, application: &str) -> bool {
    let by_annotation = object
        .pointer("/metadata/annotations")
        .and_then(|annotations| annotations.get(TRACKING_ID))
        .and_then(Value::as_str)
        .is_some_and(|id| id.split_once(':').is_some_and(|(owner, _)| owner == application));
    let by_label = object
        .pointer("/metadata/labels")
        .and_then(|labels| labels.get(INSTANCE_LABEL))
        .and_then(Value::as_str)
        == Some(application);
    by_annotation || by_label
}

/// Which Jobs qualify for the workaround: a Job of `application_name` that
/// carries the hook finalizer, once the Application (`application`) has
/// finished its last operation. Argo CD removes the finalizer of every hook
/// before it finishes the operation that ran it, so after that it has leaked.
pub fn classify(job: &Value, application: &Value, application_name: &str) -> HookJob {
    if job.get("kind").and_then(Value::as_str) != Some("Job") || last_operation_finished(application).is_none() {
        return HookJob::Untouched;
    }
    let Some(index) = job
        .pointer("/metadata/finalizers")
        .and_then(Value::as_array)
        .and_then(|finalizers| finalizers.iter().position(|name| name == HOOK_FINALIZER))
    else {
        return HookJob::Untouched;
    };
    if !tracked_by(job, application_name) {
        return HookJob::Untouched;
    }
    let text = |pointer: &str| {
        job.pointer(pointer)
            .and_then(Value::as_str)
            .unwrap_or("(none)")
            .to_owned()
    };
    let (name, uid) = (text("/metadata/name"), text("/metadata/uid"));
    match finished_condition(job) {
        Some(condition) => HookJob::Leaked {
            name,
            uid,
            condition,
            index,
        },
        None => HookJob::Running { name, uid },
    }
}

/// Remove the leaked finalizer at `index` of Job `name`, only while it is still
/// that finalizer there (a JSON patch `test`). Whether it was removed.
fn release(kubectl: &Kubectl, name: &str, index: usize) -> bool {
    let path = format!("/metadata/finalizers/{index}");
    let patch = serde_json::json!([
        {"op": "test", "path": path, "value": HOOK_FINALIZER},
        {"op": "remove", "path": path},
    ])
    .to_string();
    kubectl
        .cmd(&[
            "patch",
            "job",
            name,
            "--namespace",
            NAMESPACE,
            "--type",
            "json",
            "--patch",
            &patch,
        ])
        .run()
        .is_ok_and(|output| output.success())
}

/// After Argo CD synced `application`: wait until its last operation finished,
/// then remove the hook finalizer Argo CD leaked on any finished hook Job of it
/// (see the module documentation), waiting for a Job that still runs. Logs every
/// Job it acts on.
pub fn release_leaked_hook_finalizers(kubectl: &Kubectl, application: &str, stop: &AtomicBool) -> Result<()> {
    Waiter::new(
        format!("Argo CD's last operation on {application} to finish with no hook finalizer left behind"),
        Duration::from_secs(5 * 60),
    )
    .backoff(Duration::from_secs(1), Duration::from_secs(5))
    .until(|| {
        if stop.load(Ordering::SeqCst) {
            return Err(Error::new("the reconcile failed, so nothing waits for the Application"));
        }
        let Some(found) =
            kubectl.get_optional(&["applications.argoproj.io", application, "--namespace", ARGOCD_NAMESPACE])?
        else {
            return Ok(Poll::Ready(()));
        };
        let Some(finished) = last_operation_finished(&found) else {
            return Ok(Poll::pending(format!(
                "the last operation of {application} has not finished"
            )));
        };
        let jobs = kubectl.json(&["get", "jobs", "--namespace", NAMESPACE])?;
        let mut pending = Vec::new();
        for job in jobs.get("items").and_then(Value::as_array).into_iter().flatten() {
            match classify(job, &found, application) {
                HookJob::Untouched => {}
                HookJob::Running { name, uid } => pending.push(format!(
                    "hook Job {NAMESPACE}/{name} (UID {uid}) still runs with {HOOK_FINALIZER}, although the last \
                     operation of {application} finished ({finished}): Argo CD's stale-hook race"
                )),
                HookJob::Leaked {
                    name,
                    uid,
                    condition,
                    index,
                } => {
                    if release(kubectl, &name, index) {
                        crate::log::line(format!(
                            "Argo CD hook-finalizer race workaround: removed {HOOK_FINALIZER} from Job \
                             {NAMESPACE}/{name} (UID {uid}, {condition}), which Argo CD left behind after the last \
                             operation of {application} finished ({finished}); gitops-engine judged the re-created \
                             hook by a stale cached copy of its predecessor"
                        ));
                    } else {
                        pending.push(format!(
                            "could not remove the leaked {HOOK_FINALIZER} from Job {NAMESPACE}/{name} (UID {uid}) yet"
                        ));
                    }
                }
            }
        }
        Ok(if pending.is_empty() {
            Poll::Ready(())
        } else {
            Poll::Pending(pending.join("; "))
        })
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const APPLICATION: &str = "dev-web";

    fn finished_application() -> Value {
        json!({"status": {"operationState": {"phase": "Succeeded", "finishedAt": "2026-11-09T15:52:28Z"}}})
    }

    fn job(finalizers: Value, tracking: &str, conditions: Value) -> Value {
        json!({
            "kind": "Job",
            "metadata": {
                "name": "web-smoke",
                "uid": "a9aba279",
                "finalizers": finalizers,
                "annotations": {TRACKING_ID: tracking},
            },
            "status": {"conditions": conditions},
        })
    }

    fn complete() -> Value {
        json!([{"type": "SuccessCriteriaMet", "status": "True"}, {"type": "Complete", "status": "True"}])
    }

    const TRACKED: &str = "dev-web:batch/Job:web/web-smoke";

    /// Catches the workaround missing the leak it exists for: a finished hook Job
    /// of the Application that kept the finalizer after the operation finished,
    /// also when the finalizer is not the first one, and with label tracking.
    #[test]
    fn test_a_finished_hook_job_that_kept_the_finalizer_after_the_operation_qualifies() {
        let leaked = classify(
            &job(json!(["example.com/other", HOOK_FINALIZER]), TRACKED, complete()),
            &finished_application(),
            APPLICATION,
        );
        assert_eq!(
            leaked,
            HookJob::Leaked {
                name: "web-smoke".to_owned(),
                uid: "a9aba279".to_owned(),
                condition: "Complete".to_owned(),
                index: 1,
            }
        );

        let failed = json!([{"type": "Failed", "status": "True"}]);
        assert!(matches!(
            classify(&job(json!([HOOK_FINALIZER]), TRACKED, failed), &finished_application(), APPLICATION),
            HookJob::Leaked { condition, index: 0, .. } if condition == "Failed"
        ));

        let mut labelled = job(json!([HOOK_FINALIZER]), "", complete());
        labelled["metadata"]["annotations"] = json!({});
        labelled["metadata"]["labels"] = json!({INSTANCE_LABEL: APPLICATION});
        assert!(matches!(
            classify(&labelled, &finished_application(), APPLICATION),
            HookJob::Leaked { .. }
        ));
    }

    /// Catches the workaround taking over Argo CD's own work or touching what is
    /// not its own: nothing qualifies while an operation is queued or running,
    /// without the finalizer, for another Application, or for another kind.
    #[test]
    fn test_nothing_qualifies_before_the_operation_finished_or_outside_the_application() {
        let leaked = job(json!([HOOK_FINALIZER]), TRACKED, complete());

        let mut running = finished_application();
        running["status"]["operationState"]["phase"] = json!("Running");
        assert_eq!(classify(&leaked, &running, APPLICATION), HookJob::Untouched);
        let mut queued = finished_application();
        queued["operation"] = json!({"sync": {"revision": "abc"}});
        assert_eq!(classify(&leaked, &queued, APPLICATION), HookJob::Untouched);
        assert_eq!(classify(&leaked, &json!({}), APPLICATION), HookJob::Untouched);

        let released = job(json!(["example.com/other"]), TRACKED, complete());
        assert_eq!(
            classify(&released, &finished_application(), APPLICATION),
            HookJob::Untouched
        );
        let elsewhere = job(json!([HOOK_FINALIZER]), "dev-web-2:batch/Job:web/web-smoke", complete());
        assert_eq!(
            classify(&elsewhere, &finished_application(), APPLICATION),
            HookJob::Untouched
        );
        let mut pod = leaked.clone();
        pod["kind"] = json!("Pod");
        assert_eq!(classify(&pod, &finished_application(), APPLICATION), HookJob::Untouched);
    }

    /// Catches the workaround releasing a Job that still runs: its finalizer is
    /// the race's signature, and the Job is waited for, not released.
    #[test]
    fn test_a_running_hook_job_with_the_finalizer_after_the_operation_is_waited_for() {
        let running = job(json!([HOOK_FINALIZER]), TRACKED, json!([]));
        assert_eq!(
            classify(&running, &finished_application(), APPLICATION),
            HookJob::Running {
                name: "web-smoke".to_owned(),
                uid: "a9aba279".to_owned()
            }
        );
        let suspended = job(
            json!([HOOK_FINALIZER]),
            TRACKED,
            json!([{"type": "Complete", "status": "False"}]),
        );
        assert!(matches!(
            classify(&suspended, &finished_application(), APPLICATION),
            HookJob::Running { .. }
        ));
    }
}
