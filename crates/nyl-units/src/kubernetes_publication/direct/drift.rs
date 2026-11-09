//! Drift of a direct target's cluster: whether the objects the unit applied
//! still hold what it applied.
//!
//! Contract: [Direct reconciler](../../../../../design/orchestration-core.md#direct-reconciler)
//! (drift). Nothing reconciles a direct target between runs, so `kubectl edit`,
//! a deleted object, or another tool's change goes unnoticed until the cluster
//! is compared with what the unit last applied.
//!
//! - **Desired state.** Never the declaration: drift is the cluster moving away
//!   from what Nyl applied, not the declaration moving on. Per Release the
//!   manifest of its newest `Deployed` revision applied for this unit's
//!   publication (repository, branch, prefix; the reader's rule,
//!   [`applied_for`]) stands for it. A newer revision of another publication,
//!   or of a manual apply, is not this unit's and is skipped. The Releases, the
//!   destination, and where the cluster is reached are the facts the unit
//!   recorded at resolution, as `nyl attest` reads them.
//! - **Comparison.** Per object a live read and a server-side apply dry run as
//!   Nyl's field manager, the machinery `nyl diff` uses
//!   ([`DiffEngine::are_equivalent_as`]): the dry run changes only fields the
//!   manifest declares, so a field only another manager sets (an HPA's
//!   replicas, a controller's annotation) is not drift. The objects come from
//!   the manifest, never from keys that stay tracked only because their prune
//!   awaits confirmation or because another Release has yet to apply them.
//!   An object two Releases render is compared once, by the revision applied
//!   last.
//! - **Missing is drift**, except a Job whose `ttlSecondsAfterFinished` makes
//!   its disappearance after completion expected ([`job_expires`], the
//!   reader's rule). A Release without a revision applied for this publication
//!   is drift too.
//! - **Unreadable is never clean.** An unreachable cluster, a read or dry run
//!   the credentials may not make, an object the API rejects, or a deadline
//!   that passes is `unknown`, unless drift was already found, which is
//!   reported with what stayed unverified.
//! - **Cost.** One live read and one dry run per object, eight at a time, one
//!   Release's state read after another, all within the unit's deadline.
//!   Objects not compared by then are unverified.
//!
//! Objects that carry Nyl's labels but that no current revision tracks
//! (orphans) are not reported.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::time::Duration;

use nyl_core::orchestration::driver::ExecutionContext;
use nyl_core::orchestration::state::{DesiredUnit, Drift};
use nyl_render::gitops::observation::{DirectRelease, ExpectedDeployment, ReconcilerLocation};
use nyl_render::kubernetes::field_manager::NYL_FIELD_MANAGER;
use nyl_render::kubernetes::lifecycle::ObjectScopes;
use nyl_render::kubernetes::{
    run_concurrently, DiffEngine, KubeClient, ObjectIdentity, ReleasePublication, ReleaseState, ReleaseStatus,
    ResourceKey,
};
use nyl_render::{KubeErrorKind, NylError};
use serde_json::Value;

use super::health::job_expires;
use super::reader::{api_error, applied_for};
use super::{ClusterAccess, ClusterHandle};
use crate::kubernetes_publication::observe::facts_of;
use crate::kubernetes_publication::reconciler::{block_on_with as block_on, ReadError, KUBECONFIG_ENV};

/// Objects compared at the same time, as an apply writes them.
const CONCURRENCY: usize = 8;

/// The objects a summary names before it counts the rest.
const LISTED: usize = 8;

const DEADLINE: &str = "the unit's deadline passed before the cluster was compared; raise the unit's `timeout`";

/// The drift of the cluster of a unit whose target is direct, or `None` for a
/// unit whose recorded reconciler is another one, which has no cluster of
/// its own to compare.
pub(crate) fn detect(
    ctx: &ExecutionContext<'_>,
    unit: &DesiredUnit,
    target: &str,
    access: &ClusterAccess,
) -> Option<Drift> {
    let spec = unit.resolved_spec.as_ref().unwrap_or(&unit.spec);
    if spec.pointer("/reconciler/kind").and_then(Value::as_str) != Some("direct") {
        return None;
    }
    let facts = match facts_of(unit) {
        Ok(facts) => facts,
        Err(reason) => return Some(Drift::Unknown { reason }),
    };
    let ReconcilerLocation::Direct(location) = &facts.location else {
        return None;
    };
    let Some(repository) = facts
        .expected
        .deployments
        .first()
        .map(|deployment| deployment.source.repository.clone())
    else {
        // A target without Releases applies nothing, so nothing can differ.
        return Some(Drift::Clean);
    };
    let destination = ReleasePublication {
        repository,
        branch: facts.expected.publication.branch.clone(),
        path_prefix: facts.expected.publication.path_prefix.clone(),
        commit: None,
    };
    let now = ctx.now.max(ctx.clock.now());
    let remaining = (ctx.deadline - now).to_std().unwrap_or_default();
    let kubeconfig = ctx.env.get(KUBECONFIG_ENV).map(String::as_str);
    let report = block_on(|| async {
        let end = tokio::time::Instant::now() + remaining;
        let cluster = match tokio::time::timeout_at(
            end,
            access.open(location, kubeconfig, remaining.max(Duration::from_secs(1))),
        )
        .await
        {
            Ok(Ok(cluster)) => cluster,
            Ok(Err(error)) => return Report::unverified(error.message),
            Err(_) => return Report::unverified(DEADLINE.to_owned()),
        };
        compare(&cluster, &facts.expected.deployments, &destination, end).await
    });
    Some(match report {
        Ok(report) => report.into_drift(target),
        Err(error) => Drift::Unknown { reason: error.message },
    })
}

/// What the cluster check found.
struct Report {
    /// What differs, in Release and manifest order.
    differences: Vec<Difference>,
    /// What could not be compared, and why.
    unverified: Vec<String>,
}

/// One Release's finding: an object that changed or is missing, or the
/// Release itself.
struct Difference {
    /// `<namespace>/<name>` of the Release.
    release: String,
    what: String,
}

impl Report {
    fn unverified(reason: String) -> Self {
        Self {
            differences: Vec::new(),
            unverified: vec![reason],
        }
    }

    fn into_drift(self, target: &str) -> Drift {
        if self.differences.is_empty() {
            return if self.unverified.is_empty() {
                Drift::Clean
            } else {
                Drift::Unknown {
                    reason: format!(
                        "cannot compare the cluster with what the unit applied: {}",
                        bounded(&self.unverified, "problem")
                    ),
                }
            };
        }
        let mut releases: Vec<(&str, Vec<&str>)> = Vec::new();
        for difference in self.differences.iter().take(LISTED) {
            match releases.last_mut() {
                Some((release, whats)) if *release == difference.release => whats.push(&difference.what),
                _ => releases.push((&difference.release, vec![&difference.what])),
            }
        }
        let mut summary = format!(
            "the cluster no longer holds what the unit applied: {}",
            releases
                .iter()
                .map(|(release, whats)| format!("Release {release}: {}", whats.join(", ")))
                .collect::<Vec<_>>()
                .join("; ")
        );
        if self.differences.len() > LISTED {
            let _ = write!(summary, " (and {} more)", self.differences.len() - LISTED);
        }
        let _ = write!(summary, "; `nyl diff --target {target}` shows each change");
        if !self.unverified.is_empty() {
            let _ = write!(summary, "; not compared: {}", bounded(&self.unverified, "problem"));
        }
        Drift::Drifted { summary }
    }
}

/// The first few of `items`, and how many more there are.
fn bounded(items: &[String], noun: &str) -> String {
    let listed = items.iter().take(LISTED).cloned().collect::<Vec<_>>().join("; ");
    match items.len().checked_sub(LISTED) {
        Some(more) if more > 0 => format!("{listed} (and {more} more {noun}s)"),
        _ => listed,
    }
}

/// One object of a Release's desired manifest.
struct Object {
    release: String,
    applied: Option<chrono::DateTime<chrono::Utc>>,
    manifest: Value,
    key: ResourceKey,
}

/// What the cluster holds of one object.
enum Found {
    Same,
    Changed,
    Missing,
    /// A Job a TTL removed after it finished.
    Expired,
    Failed(String),
}

async fn compare(
    cluster: &ClusterHandle,
    deployments: &[ExpectedDeployment],
    destination: &ReleasePublication,
    end: tokio::time::Instant,
) -> Report {
    let mut report = Report {
        differences: Vec::new(),
        unverified: Vec::new(),
    };
    let mut objects: Vec<Object> = Vec::new();
    for deployment in deployments {
        let release: DirectRelease = match serde_json::from_value(deployment.locator.clone()) {
            Ok(release) => release,
            Err(error) => {
                report.unverified.push(format!(
                    "deployment {} records no Release locator: {error}",
                    deployment.id
                ));
                continue;
            }
        };
        let label = format!("{}/{}", release.namespace, release.name);
        match tokio::time::timeout_at(end, desired_revision(cluster, &release, destination)).await {
            Err(_) => report.unverified.push(DEADLINE.to_owned()),
            Ok(Err(error)) => report.unverified.push(error.message),
            Ok(Ok(None)) => report.differences.push(Difference {
                release: label,
                what: "no revision applied for this publication is recorded".to_owned(),
            }),
            Ok(Ok(Some(state))) => match objects_of(&label, &state) {
                Ok(found) => objects.extend(found),
                Err(problem) => report.unverified.push(problem),
            },
        }
    }
    let Ok(objects) = tokio::time::timeout_at(end, one_owner_each(cluster.client.as_ref(), objects)).await else {
        report.unverified.push(DEADLINE.to_owned());
        return report;
    };
    let mut found: Vec<Option<Found>> = objects.iter().map(|_| None).collect();
    let listed = &objects;
    run_concurrently(
        0..objects.len(),
        CONCURRENCY,
        |index| async move {
            let client = cluster.client.as_ref();
            tokio::time::timeout_at(end, compare_object(client, &listed[index]))
                .await
                .unwrap_or_else(|_| Found::Failed(DEADLINE.to_owned()))
        },
        |index, result| found[index] = Some(result),
    )
    .await;
    for (object, found) in objects.iter().zip(found.into_iter().flatten()) {
        let label = |what: &str| Difference {
            release: object.release.clone(),
            what: format!("{} {what}", object.key),
        };
        match found {
            Found::Same | Found::Expired => {}
            Found::Changed => report.differences.push(label("changed")),
            Found::Missing => report.differences.push(label("is missing")),
            Found::Failed(problem) => {
                if !report.unverified.contains(&problem) {
                    report.unverified.push(problem);
                }
            }
        }
    }
    report
}

/// The newest `Deployed` revision of `release` applied for `destination`: what
/// the unit last applied to it. `None` when no revision qualifies.
async fn desired_revision(
    cluster: &ClusterHandle,
    release: &DirectRelease,
    destination: &ReleasePublication,
) -> Result<Option<ReleaseState>, ReadError> {
    let what = format!("list the release state of {}/{}", release.namespace, release.name);
    let mut numbers = cluster
        .storage
        .list_revisions(&release.name, &release.namespace)
        .await
        .map_err(|error| api_error(&error, &what))?;
    numbers.sort_unstable_by(|a, b| b.cmp(a));
    for number in numbers {
        let state = cluster
            .storage
            .get_release(&release.name, &release.namespace, number)
            .await
            .map_err(|error| api_error(&error, &what))?;
        if let Some(state) =
            state.filter(|state| state.status == ReleaseStatus::Deployed && applied_for(state, destination))
        {
            return Ok(Some(state));
        }
    }
    Ok(None)
}

/// The documents of a revision's manifest, each with its key.
fn objects_of(release: &str, state: &ReleaseState) -> Result<Vec<Object>, String> {
    let documents = nyl_render::yaml::parse_yaml_documents_k8s_compatible(&state.manifest).map_err(|error| {
        format!(
            "the manifest of revision {} of Release {release} cannot be read: {error}",
            state.revision
        )
    })?;
    documents
        .into_iter()
        .map(|manifest| {
            let key = ResourceKey::from_json_value(&manifest).map_err(|error| {
                format!(
                    "the manifest of revision {} of Release {release} holds an object without an identity: {error}",
                    state.revision
                )
            })?;
            Ok(Object {
                release: release.to_owned(),
                applied: state.applied_at,
                manifest,
                key,
            })
        })
        .collect()
}

/// An object that two Releases render is compared once, by the revision applied
/// last, which is the one the cluster holds.
async fn one_owner_each(client: &dyn KubeClient, objects: Vec<Object>) -> Vec<Object> {
    let keys: Vec<ResourceKey> = objects.iter().map(|object| object.key.clone()).collect();
    let mut scopes = ObjectScopes::default();
    scopes.load(client, &keys).await;
    let mut position: HashMap<ObjectIdentity, usize> = HashMap::new();
    let mut owned: Vec<Object> = Vec::new();
    for object in objects {
        let identity = scopes.identity(&object.key);
        match position.get(&identity) {
            Some(&index) if object.applied > owned[index].applied => owned[index] = object,
            Some(_) => {}
            None => {
                position.insert(identity, owned.len());
                owned.push(object);
            }
        }
    }
    owned
}

async fn compare_object(client: &dyn KubeClient, object: &Object) -> Found {
    let key = &object.key;
    let live = match client.get_resource(&key.gvk, key.namespace.as_deref(), &key.name).await {
        Ok(live) => live,
        // A kind the cluster no longer serves holds no object.
        Err(error) if error.is_api_resource_not_found_error() => return Found::Missing,
        Err(error) => return Found::Failed(failure(&error, &format!("read {key}"))),
    };
    let Some(live) = live else {
        let is_job = key.gvk.group == "batch" && key.gvk.kind == "Job";
        return if is_job && job_expires(&object.manifest) {
            Found::Expired
        } else {
            Found::Missing
        };
    };
    let live = match serde_json::to_value(&live) {
        Ok(live) => live,
        Err(error) => return Found::Failed(format!("cannot read {key} as it is: {error}")),
    };
    match DiffEngine::are_equivalent_as(&object.manifest, &live, client, NYL_FIELD_MANAGER).await {
        Ok(true) => Found::Same,
        Ok(false) => Found::Changed,
        Err(error) => Found::Failed(failure(
            &error,
            &format!("compare {key} by a server-side apply dry run"),
        )),
    }
}

/// A failed read or dry run, with the fix attached.
fn failure(error: &NylError, what: &str) -> String {
    let text = error.to_string();
    let first = text.lines().next().unwrap_or_default();
    match error.kube_kind() {
        Some(KubeErrorKind::Forbidden) => format!(
            "the context may not {what} (HTTP 403): grant its user `get` on the objects of the Releases and `patch` \
             for server-side apply dry runs. {first}"
        ),
        Some(KubeErrorKind::Unauthorized) => format!(
            "the Kubernetes API rejected the kubeconfig's credentials (HTTP 401): they are invalid or have expired; \
             log in again or refresh the admitted kubeconfig. {first}"
        ),
        Some(KubeErrorKind::Rejected) => format!(
            "the Kubernetes API rejected the object when asked to {what} (HTTP 422); it may be rejected by an \
             admission webhook or hold a field that cannot change. {first}"
        ),
        _ => format!("cannot {what}: {first}"),
    }
}

/// What a unit's drift is, from what its publication branch and its cluster
/// each found: drift when either found some, unknown when one could not tell
/// and the other found none, clean when both did.
pub(crate) fn combine(findings: [(&str, Option<Drift>); 2]) -> Drift {
    let mut drifted = Vec::new();
    let mut unknown = Vec::new();
    for (what, finding) in findings {
        match finding {
            Some(Drift::Drifted { summary }) => drifted.push(summary),
            Some(Drift::Unknown { reason }) => unknown.push((what, reason)),
            Some(Drift::Clean) | None => {}
        }
    }
    if !drifted.is_empty() {
        let mut summary = drifted.join("; ");
        for (what, reason) in unknown {
            let _ = write!(summary, "; the {what} could not be compared: {reason}");
        }
        return Drift::Drifted { summary };
    }
    match unknown.as_slice() {
        [] => Drift::Clean,
        [(_, reason)] => Drift::Unknown { reason: reason.clone() },
        _ => Drift::Unknown {
            reason: unknown
                .iter()
                .map(|(_, reason)| reason.as_str())
                .collect::<Vec<_>>()
                .join("; "),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn difference(release: &str, object: usize) -> Difference {
        Difference {
            release: release.to_owned(),
            what: format!("ConfigMap {release}/{object} changed"),
        }
    }

    /// Catches a summary that grows with the number of changed objects: it
    /// names the first eight, grouped under their Release, and counts the rest.
    #[test]
    fn test_a_summary_names_the_first_objects_and_counts_the_rest() {
        let report = Report {
            differences: (0..10).map(|object| difference("ns/a", object)).collect(),
            unverified: Vec::new(),
        };

        let Drift::Drifted { summary } = report.into_drift("dev") else {
            panic!("differences are drift");
        };

        assert_eq!(summary.matches("changed").count(), LISTED, "{summary}");
        assert_eq!(summary.matches("Release ns/a:").count(), 1, "{summary}");
        assert!(summary.contains("(and 2 more)"), "{summary}");
    }

    /// Catches unverified objects being lost when drift is found: they stay in the
    /// summary, and without drift they make the check unknown, never clean.
    #[test]
    fn test_unverified_objects_never_make_a_clean_report() {
        let unverified = vec!["cannot read Deployment web/web".to_owned()];

        let unknown = Report {
            differences: Vec::new(),
            unverified: unverified.clone(),
        }
        .into_drift("dev");
        let Drift::Unknown { reason } = unknown else {
            panic!("an unverified object is unknown");
        };
        assert!(reason.contains("cannot read Deployment web/web"), "{reason}");

        let drifted = Report {
            differences: vec![difference("ns/a", 0)],
            unverified,
        }
        .into_drift("dev");
        let Drift::Drifted { summary } = drifted else {
            panic!("a difference is drift");
        };
        assert!(
            summary.contains("not compared: cannot read Deployment web/web"),
            "{summary}"
        );

        let clean = Report {
            differences: Vec::new(),
            unverified: Vec::new(),
        };
        assert_eq!(clean.into_drift("dev"), Drift::Clean);
    }

    /// Catches the branch and the cluster hiding one another: drift in either is
    /// drift, a half that could not be compared never reads clean, and a clean
    /// half beside an unknown one is unknown.
    #[test]
    fn test_the_branch_and_the_cluster_combine_to_the_worse_finding() {
        let drifted = |text: &str| Drift::Drifted {
            summary: text.to_owned(),
        };
        let unknown = |text: &str| Drift::Unknown {
            reason: text.to_owned(),
        };
        let branch = "publication branch";

        assert_eq!(combine([(branch, Some(Drift::Clean)), ("cluster", None)]), Drift::Clean);
        assert_eq!(
            combine([(branch, Some(Drift::Clean)), ("cluster", Some(drifted("gone")))]),
            drifted("gone")
        );
        assert_eq!(
            combine([(branch, Some(unknown("no remote"))), ("cluster", Some(Drift::Clean))]),
            unknown("no remote")
        );
        assert_eq!(
            combine([(branch, Some(drifted("edited"))), ("cluster", Some(unknown("403")))]),
            drifted("edited; the cluster could not be compared: 403")
        );
    }
}
