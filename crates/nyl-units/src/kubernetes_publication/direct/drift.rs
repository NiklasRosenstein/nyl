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
//!   or of a manual apply, is not this unit's and is not used as the desired
//!   state. The Releases, the destination, and where the cluster is reached are
//!   the facts the unit recorded at resolution, as `nyl attest` reads them. Whether
//!   the cluster is compared at all follows the target as it renders: a direct
//!   unit that recorded no facts is `unknown`.
//! - **Comparison.** Per object a live read and a server-side apply dry run as
//!   Nyl's field manager, the machinery `nyl diff` uses
//!   ([`DiffEngine::compare_as`]): the dry run changes only fields the
//!   manifest declares, so a field only another manager sets (a controller's
//!   annotation) is not drift. The dry run's answer carries the version it was
//!   applied to; when the object moved since the live read, both are done
//!   again once. The objects come from the manifest, never from keys that stay
//!   tracked without a document (their prune awaits confirmation, failed, or
//!   another Release has yet to apply them): those are not compared. An object
//!   two Releases render is compared once, against the Release that applies
//!   last ([`wave_of`], then the Release key), as the cluster holds it.
//! - **Missing is drift**, except a Job whose `ttlSecondsAfterFinished` makes
//!   its disappearance after completion expected ([`job_expires`], the
//!   reader's rule). A Release without a revision applied for this publication
//!   is drift too. A kind the cluster answers with an unknown API resource is
//!   told apart: missing when the kind is gone ([`KubeClient::is_kind_removed`]),
//!   drift ("its apiVersion is no longer served") when it is served at another
//!   version, unverified otherwise (an aggregated API that is down).
//! - **Unreadable is never clean.** An unreachable cluster, a read or dry run
//!   the credentials may not make, an object the API rejects (an admission
//!   webhook whose `sideEffects` are `Unknown` or `Some` refuses dry runs), or a
//!   deadline that passes is `unknown`, unless drift was already found, which is
//!   reported with what stayed unverified.
//! - **Cost.** One live read and one dry run per object, eight at a time, one
//!   Release's state read after another, all within the unit's deadline less a
//!   [`margin`], so the check can still report what it found. Objects not
//!   compared by then are unverified.
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

use super::apply::wave_of;
use super::health::job_expires;
use super::reader::{api_error, applied_for};
use super::{destination_of, ClusterAccess, ClusterHandle};
use crate::kubernetes_publication::observe::facts_of;
use crate::kubernetes_publication::reconciler::{block_on_with as block_on, ReadError, KUBECONFIG_ENV};

/// Objects compared at the same time, as an apply writes them.
const CONCURRENCY: usize = 8;

/// The objects a summary names before it counts the rest.
const LISTED: usize = 8;

const DEADLINE: &str = "the unit's deadline passed before the cluster was compared; raise the unit's `timeout`";

/// How long before the unit's deadline the check stops: a tenth of what is
/// left, at most ten seconds. The shell discards what a check returns once the
/// deadline passed, so a check that used the whole budget could never report
/// what it had found.
fn margin(remaining: Duration) -> Duration {
    (remaining / 10).min(Duration::from_secs(10))
}

/// The drift of the cluster of a unit whose target is direct (the caller decides
/// that from the target as it renders). The unit's recorded facts say where the
/// cluster is and which Releases it applied; a unit that records none for a
/// direct reconciler cannot be compared, which is `unknown`, never clean.
pub(crate) fn detect(ctx: &ExecutionContext<'_>, unit: &DesiredUnit, target: &str, access: &ClusterAccess) -> Drift {
    let facts = match facts_of(unit) {
        Ok(facts) => facts,
        Err(reason) => return Drift::Unknown { reason },
    };
    let ReconcilerLocation::Direct(location) = &facts.location else {
        return Drift::Unknown {
            reason: "the unit was resolved for an Argo CD target, which the target no longer declares; reconcile it \
                     so it records the direct reconciler"
                .to_owned(),
        };
    };
    let Some(repository) = facts
        .expected
        .deployments
        .first()
        .map(|deployment| deployment.source.repository.clone())
    else {
        // A target without Releases applies nothing, so nothing can differ.
        return Drift::Clean;
    };
    let destination = destination_of(repository, &facts.expected.publication);
    let now = ctx.now.max(ctx.clock.now());
    let remaining = (ctx.deadline - now).to_std().unwrap_or_default();
    let usable = remaining.saturating_sub(margin(remaining));
    let kubeconfig = ctx.env.get(KUBECONFIG_ENV).map(String::as_str);
    let report = block_on(|| async {
        let end = tokio::time::Instant::now() + usable;
        let cluster = match tokio::time::timeout_at(
            end,
            access.open(location, kubeconfig, usable.max(Duration::from_secs(1))),
        )
        .await
        {
            Ok(Ok(cluster)) => cluster,
            Ok(Err(error)) => return Report::unverified(error.message),
            Err(_) => return Report::unverified(DEADLINE.to_owned()),
        };
        compare(&cluster, &facts.expected.deployments, &destination, end).await
    });
    match report {
        Ok(report) => report.into_drift(target),
        Err(error) => Drift::Unknown { reason: error.message },
    }
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
        let target = if target.is_empty() { "<target>" } else { target };
        let _ = write!(
            summary,
            "; run `nyl diff --target {target}` on the Release's file to see each change in full"
        );
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
    /// Where its Release applies among the target's: the wave, then the Release
    /// key, which is the order [`super::apply`] applies them in. The Release
    /// that applies last owns an object that two render.
    order: (u8, String),
    manifest: Value,
    key: ResourceKey,
}

/// What the cluster holds of one object.
enum Found {
    Same,
    Changed,
    Missing,
    /// The object's kind is served, but not at the manifest's version.
    Unserved,
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
            Ok(Ok(Some(state))) => match objects_of(&label, &deployment.id, &state) {
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
            Found::Unserved => report
                .differences
                .push(label("is missing: its apiVersion is no longer served")),
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

/// The documents of a revision's manifest, each with its key. `id` is the
/// Release's key in the target, which orders it among the Releases.
fn objects_of(release: &str, id: &str, state: &ReleaseState) -> Result<Vec<Object>, String> {
    let documents = nyl_render::yaml::parse_yaml_documents_k8s_compatible(&state.manifest).map_err(|error| {
        format!(
            "the manifest of revision {} of Release {release} cannot be read: {error}",
            state.revision
        )
    })?;
    let order = (wave_of(&documents), id.to_owned());
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
                order: order.clone(),
                manifest,
                key,
            })
        })
        .collect()
}

/// An object that two Releases render is compared once, by the Release that
/// applies last, whose revision the cluster holds. Not by when the revisions were
/// applied: an apply that changed nothing keeps its revision, and its timestamps.
async fn one_owner_each(client: &dyn KubeClient, objects: Vec<Object>) -> Vec<Object> {
    let keys: Vec<ResourceKey> = objects.iter().map(|object| object.key.clone()).collect();
    let mut scopes = ObjectScopes::default();
    scopes.load(client, &keys).await;
    let mut position: HashMap<ObjectIdentity, usize> = HashMap::new();
    let mut owned: Vec<Object> = Vec::new();
    for object in objects {
        let identity = scopes.identity(&object.key);
        match position.get(&identity) {
            Some(&index) if object.order > owned[index].order => owned[index] = object,
            Some(_) => {}
            None => {
                position.insert(identity, owned.len());
                owned.push(object);
            }
        }
    }
    owned
}

/// What the cluster holds of `object`. The live object and the dry run are two
/// requests, and the object may change between them (an autoscaler scaling it):
/// the dry run's answer carries the version it was applied to, and when that is
/// not the version read, the object is read and compared once more. Changing
/// again is not something to judge by, so the object is unverified.
async fn compare_object(client: &dyn KubeClient, object: &Object) -> Found {
    let key = &object.key;
    for attempt in 0..2 {
        let live = match client.get_resource(&key.gvk, key.namespace.as_deref(), &key.name).await {
            Ok(live) => live,
            Err(error) if error.is_api_resource_not_found_error() => return unserved(client, key).await,
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
        let live_version = live.metadata.resource_version.clone();
        let live = match serde_json::to_value(&live) {
            Ok(live) => live,
            Err(error) => return Found::Failed(format!("cannot read {key} as it is: {error}")),
        };
        let comparison = match DiffEngine::compare_as(&object.manifest, &live, client, NYL_FIELD_MANAGER).await {
            Ok(comparison) => comparison,
            Err(error) => {
                return Found::Failed(failure(
                    &error,
                    &format!("compare {key} by a server-side apply dry run"),
                ))
            }
        };
        let moved = matches!((&comparison.resource_version, &live_version), (Some(dry), Some(live)) if dry != live);
        if !moved {
            return if comparison.equivalent {
                Found::Same
            } else {
                Found::Changed
            };
        }
        if attempt == 1 {
            return Found::Failed(format!(
                "{key} changed while it was compared, twice; detect drift again"
            ));
        }
    }
    unreachable!("the second attempt returns")
}

/// What it means that the cluster answers a read of `key` with an unknown API
/// resource. A version of the kind that is no longer served, while another is,
/// holds no object at that version: drift. A kind that is gone altogether (no
/// version, no CRD, no APIService) holds none either: missing. Anything else,
/// such as an aggregated API that is down, cannot be told from an object that
/// exists behind it: unverified.
async fn unserved(client: &dyn KubeClient, key: &ResourceKey) -> Found {
    let gvk = &key.gvk;
    match client.get_api_versions().await {
        Ok(served) if serves_another_version(&served, gvk) => return Found::Unserved,
        Ok(_) => {}
        Err(error) => return Found::Failed(failure(&error, &format!("list the API versions to read {key}"))),
    }
    match client.is_kind_removed(gvk).await {
        Ok(true) => Found::Missing,
        Ok(false) => Found::Failed(format!(
            "cannot tell whether {key} exists: the cluster does not serve {}, which may be an aggregated API that is \
             unavailable; check the APIService, or run detect-drift again once it is",
            api_version_of(gvk)
        )),
        Err(error) => Found::Failed(failure(&error, &format!("check whether the kind of {key} was removed"))),
    }
}

fn api_version_of(gvk: &nyl_render::kubernetes::GroupVersionKind) -> String {
    if gvk.group.is_empty() {
        format!("{} {}", gvk.version, gvk.kind)
    } else {
        format!("{}/{} {}", gvk.group, gvk.version, gvk.kind)
    }
}

/// Whether `served` (`<group>/<version>/<Kind>`, or `<version>/<Kind>` for the
/// core group, as [`KubeClient::get_api_versions`] lists them) holds the kind of
/// `gvk` at another version.
fn serves_another_version(served: &[String], gvk: &nyl_render::kubernetes::GroupVersionKind) -> bool {
    served.iter().any(|entry| {
        let parts: Vec<&str> = entry.split('/').collect();
        match parts.as_slice() {
            [version, kind] => gvk.group.is_empty() && *kind == gvk.kind && *version != gvk.version,
            [group, version, kind] => *group == gvk.group && *kind == gvk.kind && *version != gvk.version,
            _ => false,
        }
    })
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

    /// A client that records the field manager of each dry run it answers.
    struct Recording {
        inner: nyl_render::kubernetes::MockKubeClient,
        managers: std::sync::Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl KubeClient for Recording {
        async fn get_resource(
            &self,
            gvk: &nyl_render::kubernetes::GroupVersionKind,
            namespace: Option<&str>,
            name: &str,
        ) -> nyl_render::Result<Option<kube::api::DynamicObject>> {
            self.inner.get_resource(gvk, namespace, name).await
        }

        async fn apply_resource(
            &self,
            resource: &kube::api::DynamicObject,
            manager: &str,
            dry_run: bool,
        ) -> nyl_render::Result<nyl_render::kubernetes::ApplyOutcome> {
            self.inner.apply_resource(resource, manager, dry_run).await
        }

        async fn get_server_version(&self) -> nyl_render::Result<String> {
            self.inner.get_server_version().await
        }

        async fn get_api_versions(&self) -> nyl_render::Result<Vec<String>> {
            self.inner.get_api_versions().await
        }

        async fn is_namespaced(&self, gvk: &nyl_render::kubernetes::GroupVersionKind) -> nyl_render::Result<bool> {
            self.inner.is_namespaced(gvk).await
        }

        fn default_namespace(&self) -> &str {
            self.inner.default_namespace()
        }

        async fn delete_resource_with(
            &self,
            gvk: &nyl_render::kubernetes::GroupVersionKind,
            namespace: Option<&str>,
            name: &str,
            options: &nyl_render::kubernetes::DeleteOptions,
        ) -> nyl_render::Result<()> {
            self.inner.delete_resource_with(gvk, namespace, name, options).await
        }

        async fn get_normalized_resource(
            &self,
            resource: &kube::api::DynamicObject,
            manager: &str,
        ) -> nyl_render::Result<kube::api::DynamicObject> {
            self.managers.lock().unwrap().push(manager.to_owned());
            self.inner.get_normalized_resource(resource, manager).await
        }

        async fn release_field_manager(
            &self,
            resource: &kube::api::DynamicObject,
            manager: &str,
        ) -> nyl_render::Result<bool> {
            self.inner.release_field_manager(resource, manager).await
        }
    }

    /// Catches the comparison being made as another field manager than Nyl's,
    /// which would judge by fields Nyl does not own: the dry run of an object
    /// that exists is applied as `nyl`.
    #[tokio::test]
    async fn test_the_dry_run_is_made_as_nyls_field_manager() {
        let manifest = serde_json::json!({
            "apiVersion": "v1", "kind": "ConfigMap",
            "metadata": {"name": "cfg", "namespace": "web"}, "data": {"a": "b"}
        });
        let key = ResourceKey::from_json_value(&manifest).unwrap();
        let client = Recording {
            inner: nyl_render::kubernetes::MockKubeClient::new(),
            managers: std::sync::Mutex::default(),
        };
        client
            .inner
            .add_resource(key.clone(), serde_json::from_value(manifest.clone()).unwrap());
        let object = Object {
            release: "web/web".to_owned(),
            order: (1, "web/web".to_owned()),
            manifest,
            key,
        };

        assert!(matches!(compare_object(&client, &object).await, Found::Same));

        assert_eq!(*client.managers.lock().unwrap(), [NYL_FIELD_MANAGER]);
    }

    /// Catches a check that spends the whole budget: the shell discards what a
    /// check returns after the deadline, so the check stops a tenth of what is
    /// left before it, at most ten seconds.
    #[test]
    fn test_the_check_stops_before_the_deadline() {
        assert_eq!(margin(Duration::from_secs(600)), Duration::from_secs(10));
        assert_eq!(margin(Duration::from_secs(30)), Duration::from_secs(3));
        assert_eq!(margin(Duration::ZERO), Duration::ZERO);
    }

    /// Catches another version of a kind being mistaken for the kind, and a kind
    /// being served at another version being missed, in the list the cluster
    /// reports its API versions in (`<group>/<version>/<Kind>`, and
    /// `<version>/<Kind>` for the core group).
    #[test]
    fn test_another_served_version_of_the_kind_is_recognized() {
        let served = |entries: &[&str]| entries.iter().map(|entry| (*entry).to_owned()).collect::<Vec<_>>();
        let gvk = |group: &str, version: &str, kind: &str| nyl_render::kubernetes::GroupVersionKind {
            group: group.to_owned(),
            version: version.to_owned(),
            kind: kind.to_owned(),
        };
        let list = served(&[
            "v1",
            "v1/ConfigMap",
            "apps/v1",
            "apps/v1/Deployment",
            "batch/v1/Job",
            "v2/Widget",
        ]);

        assert!(serves_another_version(&list, &gvk("apps", "v1beta1", "Deployment")));
        assert!(serves_another_version(&list, &gvk("", "v1beta1", "ConfigMap")));
        assert!(!serves_another_version(&list, &gvk("apps", "v1", "Deployment")));
        assert!(!serves_another_version(&list, &gvk("apps", "v1", "Gone")));
        assert!(!serves_another_version(
            &list,
            &gvk("other.example.com", "v2", "Widget")
        ));
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
