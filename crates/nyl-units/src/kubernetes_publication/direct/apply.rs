//! The publication driver's operations on a direct target's cluster: the
//! preflight before Git is touched, applying the Releases of the pushed tree
//! in waves, purging the Releases that left it, and the two teardown phases.
//!
//! Contract: [Direct reconciler](../../../../../design/orchestration-core.md#direct-reconciler)
//! (publication and order, applying, fencing, teardown). Everything that
//! applies, prunes, records, or purges is `nyl_render::kubernetes::lifecycle`,
//! the code `nyl apply` runs; this module orders it, fences it by owner, and
//! maps its results to the unit's outcome. The credentials are the unit's
//! admitted `KUBECONFIG` and the Cluster's declared context alone.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::time::Duration;

use nyl_core::orchestration::codes::{Code, Finding};
use nyl_core::orchestration::driver::{
    AttestationOutcome, DriverError, DriverResult, ExecutionContext, IncarnationStatus, Outcome,
};
use nyl_core::orchestration::ids::{EnvironmentName, Uid, UnitName};
use nyl_core::orchestration::state::FailureCategory;
use nyl_render::gitops::direct::release_documents;
use nyl_render::gitops::observation::{
    DirectLocation, GitPublicationHistory, ObservedPublication, PublishedDirectRelease, MATCH_WALK_LIMIT,
};
use nyl_render::gitops::publication::{release_history_limit, RenderedTarget};
use nyl_render::gitops::reconcile::RenderIndexOwner;
use nyl_render::kubernetes::fence::{admit, may_purge, OwnerState, ReleaseClaim, Requester};
use nyl_render::kubernetes::field_manager::FieldOwnership;
use nyl_render::kubernetes::lifecycle::{
    ensure_namespace_exists, prepare_desired_manifests, purge_resources, ApplyEvent, ApplySettings, Interruption,
    ReleaseRecorder,
};
use nyl_render::kubernetes::prune::PruneDecision;
use nyl_render::kubernetes::{ReleasePublication, ReleaseState, ResourceKey, ResourceOrdering};
use nyl_render::{KubeErrorKind, NylError};
use serde_json::Value;

use super::reader::{api_error, is_state_namespace};
use super::{ClusterAccess, ClusterHandle};
use crate::kubernetes_publication::reconciler::{block_on_with as block_on, ReadError, ReadErrorKind, KUBECONFIG_ENV};

/// Resources applied or pruned at the same time, as `nyl apply` does by default.
const CONCURRENCY: usize = 8;

/// What a step of the driver decided.
pub(crate) enum Step {
    /// Carry on; the warnings and attestations go with the receipt.
    Continue {
        warnings: Vec<Finding>,
        attestations: Vec<AttestationOutcome>,
    },
    /// The call ends with this outcome.
    Stop(Outcome),
}

/// Why a cluster operation did not complete.
#[derive(Debug)]
enum Failure {
    Read(ReadError),
    Nyl(NylError),
}

impl From<ReadError> for Failure {
    fn from(error: ReadError) -> Self {
        Self::Read(error)
    }
}

impl From<NylError> for Failure {
    fn from(error: NylError) -> Self {
        Self::Nyl(error)
    }
}

/// How a failure maps to an outcome.
enum Why {
    Fenced(String),
    Connection(String),
    Timeout(String),
    Rejected(String),
    Failed(String),
    Credentials(String),
}

fn why(failure: &Failure) -> Why {
    match failure {
        Failure::Read(error) => match error.kind {
            ReadErrorKind::Credentials
            | ReadErrorKind::Declaration
            | ReadErrorKind::Rejected
            | ReadErrorKind::Forbidden => Why::Credentials(error.message.clone()),
            ReadErrorKind::Unreachable | ReadErrorKind::Malformed => Why::Failed(error.message.clone()),
        },
        Failure::Nyl(error) => {
            let text = error.to_string();
            match error.kube_kind() {
                _ if error.is_owner_fenced() => Why::Fenced(text),
                Some(KubeErrorKind::Connection) => Why::Connection(text),
                Some(KubeErrorKind::Rejected) => Why::Rejected(text),
                _ if matches!(error, NylError::Interrupted(_)) => Why::Timeout(text),
                _ => Why::Failed(text),
            }
        }
    }
}

/// The outcome of failures that stopped the call, as the contract's table maps them.
fn outcome_of(context: &str, failures: &[Why]) -> Outcome {
    let messages = failures
        .iter()
        .map(|failure| match failure {
            Why::Fenced(m)
            | Why::Connection(m)
            | Why::Timeout(m)
            | Why::Rejected(m)
            | Why::Failed(m)
            | Why::Credentials(m) => m.as_str(),
        })
        .collect::<Vec<_>>()
        .join("; ");
    let message = format!("{context}: {messages}");
    if failures.iter().any(|failure| matches!(failure, Why::Connection(_))) {
        return Outcome::Uncertain { reason: message };
    }
    let all = |predicate: fn(&Why) -> bool| failures.iter().all(predicate);
    let (category, retryable) = if all(|f| matches!(f, Why::Fenced(_))) {
        (FailureCategory::Ownership, true)
    } else if all(|f| matches!(f, Why::Credentials(_))) {
        (FailureCategory::Credentials, true)
    } else if failures.iter().any(|f| matches!(f, Why::Timeout(_))) {
        (FailureCategory::Timeout, true)
    } else {
        (FailureCategory::ToolError, !all(|f| matches!(f, Why::Rejected(_))))
    };
    Outcome::Failed {
        category,
        retryable,
        message,
    }
}

/// Which wave a Release applies in, and, reversed, is removed in: Releases that
/// register kinds (CustomResourceDefinitions, APIServices) come first.
fn wave_of(documents: &[Value]) -> u8 {
    let registers = documents.iter().any(|document| {
        ResourceKey::from_json_value(document).is_ok_and(|key| ResourceOrdering::registers_api_group(&key.gvk))
    });
    u8::from(!registers)
}

fn identity(key: &ResourceKey) -> (String, String, Option<String>, String) {
    (
        key.gvk.group.clone(),
        key.gvk.kind.clone(),
        key.namespace.clone(),
        key.name.clone(),
    )
}

/// The driver's view of one direct target during a call.
pub(crate) struct Direct<'a, 'c> {
    pub ctx: &'a ExecutionContext<'c>,
    pub access: &'a ClusterAccess,
    pub location: DirectLocation,
    pub owner: RenderIndexOwner,
    /// The destination of the target's tree, without a commit.
    pub destination: ReleasePublication,
    pub observed: ObservedPublication,
}

impl<'a, 'c> Direct<'a, 'c> {
    pub(crate) fn new(
        ctx: &'a ExecutionContext<'c>,
        access: &'a ClusterAccess,
        location: &DirectLocation,
        owner: RenderIndexOwner,
        rendered: &RenderedTarget,
    ) -> Self {
        Self {
            ctx,
            access,
            location: location.clone(),
            owner,
            destination: ReleasePublication {
                repository: rendered.destination.repository.clone(),
                branch: rendered.destination.branch.clone(),
                path_prefix: rendered.destination.path_prefix.clone(),
                commit: None,
            },
            observed: ObservedPublication {
                url: rendered.destination.push_url.clone(),
                branch: rendered.destination.branch.clone(),
                path_prefix: rendered.destination.path_prefix.clone(),
            },
        }
    }

    fn budget(&self) -> Duration {
        (self.ctx.deadline - self.ctx.now).to_std().unwrap_or_default()
    }

    fn kubeconfig(&self) -> Option<&str> {
        self.ctx.env.get(KUBECONFIG_ENV).map(String::as_str)
    }

    /// Whether another incarnation still exists, from the shell's directory.
    fn probe(&self) -> impl Fn(&RenderIndexOwner) -> OwnerState + Sync + '_ {
        move |owner| {
            let RenderIndexOwner::Unit { environment, unit, uid } = owner else {
                return OwnerState::Exists;
            };
            let (Ok(environment), Ok(unit), Ok(uid)) =
                (EnvironmentName::new(environment), UnitName::new(unit), Uid::new(uid))
            else {
                return OwnerState::Unknown(format!("the release names {owner}, which is not a valid unit identity"));
            };
            match self.ctx.incarnations.status(&environment, &unit, &uid) {
                IncarnationStatus::Exists => OwnerState::Exists,
                IncarnationStatus::Gone => OwnerState::Gone,
                IncarnationStatus::Unknown(reason) => OwnerState::Unknown(reason),
            }
        }
    }

    fn claim<'p>(
        &'p self,
        publication: &'p ReleasePublication,
        probe: &'p (dyn Fn(&RenderIndexOwner) -> OwnerState + Sync),
    ) -> ReleaseClaim<'p> {
        ReleaseClaim {
            requester: Requester::Unit {
                owner: &self.owner,
                publication,
                probe,
            },
            publication: None,
        }
    }

    fn missing_context(&self) -> Option<Outcome> {
        self.location.context.is_none().then(|| Outcome::Failed {
            category: FailureCategory::Credentials,
            retryable: true,
            message: format!(
                "NYL-KUBERNETES-CLUSTER-CONTEXT-MISSING: Cluster {} declares no spec.live.context, which a direct target is \
                 applied and read through (Nyl never uses the current context); declare it on the Cluster",
                self.location.cluster
            ),
        })
    }

    async fn open(&self) -> Result<ClusterHandle, ReadError> {
        self.access
            .open(
                &self.location,
                self.kubeconfig(),
                self.budget().max(Duration::from_secs(1)),
            )
            .await
    }

    /// Before Git is touched: connect, and check the fence of every Release,
    /// all read-only. `Some` ends the call.
    pub(crate) fn preflight(&self, releases: &[PublishedDirectRelease]) -> DriverResult<Option<Outcome>> {
        if let Some(outcome) = self.missing_context() {
            return Ok(Some(outcome));
        }
        let probe = self.probe();
        let claim = self.claim(&self.destination, &probe);
        let result = block_on(|| async {
            let cluster = self.open().await?;
            for release in releases {
                let (namespace, name) = release.identity();
                admit(cluster.storage.as_ref(), namespace, name, &claim).await?;
            }
            Ok::<_, Failure>(())
        })
        .map_err(|error| DriverError::before_effects(error.message))?;
        Ok(result
            .err()
            .map(|failure| outcome_of("the cluster cannot be applied to", &[why(&failure)])))
    }

    /// Apply every Release of the pushed `rendered` tree, published at
    /// `commit`, in waves; then, only when every Release reached `Deployed`,
    /// purge the Releases that left the tree.
    #[allow(clippy::too_many_lines)] // One ordered pass: read, apply by waves, purge, and map the outcome.
    pub(crate) fn apply(
        &self,
        rendered: &RenderedTarget,
        commit: &str,
        releases: &[PublishedDirectRelease],
    ) -> DriverResult<Step> {
        let history_limit = release_history_limit(&self.ctx.project_dir).map_err(|error| {
            DriverError::after_effects(format!("cannot read the project's release history limit: {error}"))
        })?;
        let mut documents = Vec::new();
        for release in releases {
            let docs = release_documents(&rendered.files, Path::new(&release.directory)).map_err(|error| {
                DriverError::after_effects(format!(
                    "cannot read the pushed tree of Release {}: {error}",
                    release.key
                ))
            })?;
            documents.push(docs);
        }
        let probe = self.probe();
        let publication = ReleasePublication {
            commit: Some(commit.to_owned()),
            ..self.destination.clone()
        };
        let budget = self.budget();
        let tree_namespaces: BTreeSet<String> = releases
            .iter()
            .zip(&documents)
            .flat_map(|(release, docs)| {
                std::iter::once(release.release.namespace.clone())
                    .chain(std::iter::once(release.release.destination_namespace.clone()))
                    .chain(docs.iter().filter_map(|doc| {
                        doc.pointer("/metadata/namespace")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    }))
            })
            .collect();
        let result = block_on(|| async {
            let cluster = self.open().await?;
            let claim = self.claim(&publication, &probe);
            let mut failures = Vec::new();
            let mut awaiting = Vec::new();
            let mut tracked: HashSet<(String, String, Option<String>, String)> = HashSet::new();
            let mut order = (0..releases.len()).collect::<Vec<_>>();
            order.sort_by_key(|index| (wave_of(&documents[*index]), releases[*index].key.clone()));
            let mut current_wave = None;
            for index in order {
                let wave = wave_of(&documents[index]);
                if current_wave.is_some_and(|current| current != wave) && !failures.is_empty() {
                    // Later waves may need what the failed Release registers.
                    break;
                }
                current_wave = Some(wave);
                let release = &releases[index];
                let outcome = self
                    .apply_one(
                        &cluster,
                        release,
                        documents[index].clone(),
                        history_limit,
                        budget,
                        &claim,
                    )
                    .await;
                match outcome {
                    Ok(done) => {
                        tracked.extend(done.tracked.iter().map(identity));
                        awaiting.extend(done.awaiting);
                        failures.extend(done.failures);
                    }
                    Err(failure) => failures.push(why(&Failure::Nyl(failure))),
                }
            }
            if !failures.is_empty() {
                return Ok::<_, Failure>((failures, awaiting, Vec::new()));
            }
            let warnings = self
                .purge_vanished(&cluster, rendered, releases, &tree_namespaces, &tracked, &probe)
                .await?;
            Ok((failures, awaiting, warnings))
        })
        .map_err(|error| DriverError::after_effects(error.message))?;
        let (failures, awaiting, left_behind) = match result {
            Ok(done) => done,
            Err(failure) => (vec![why(&failure)], Vec::new(), Vec::new()),
        };
        if !failures.is_empty() {
            return Ok(Step::Stop(outcome_of(
                &format!("the tree was published at {commit}, but applying it to the cluster failed (Git is ahead of the cluster; the next execution applies again)"),
                &failures,
            )));
        }
        let mut warnings = left_behind;
        if !awaiting.is_empty() {
            warnings.push(Finding::new(
                Code::KubernetesPruneAwaitingConfirmation,
                format!(
                    "{} object(s) no Release declares any more stay in the cluster, because their Prune=confirm sync option awaits a confirmation the driver never gives: {}",
                    awaiting.len(),
                    awaiting.join(", ")
                ),
                "declare `namespace.prunePolicy: Automatic` in the ApplicationGroup to prune them, or delete them by hand",
            ));
        }
        Ok(Step::Continue {
            warnings,
            attestations: Vec::new(),
        })
    }

    async fn apply_one(
        &self,
        cluster: &ClusterHandle,
        release: &PublishedDirectRelease,
        documents: Vec<Value>,
        history_limit: u32,
        budget: Duration,
        claim: &ReleaseClaim<'_>,
    ) -> Result<Applied, NylError> {
        let (namespace, name) = release.identity();
        let mut duplicates = HashMap::new();
        let (mut manifests, mut scopes) = prepare_desired_manifests(
            cluster.client.as_ref(),
            documents,
            &mut duplicates,
            Some(&release.release.destination_namespace),
        )
        .await?;
        ResourceOrdering::sort_by_priority(&mut manifests)?;
        let ownership = FieldOwnership::Nyl;
        ensure_namespace_exists(cluster.client.as_ref(), namespace, &ownership).await?;
        let recorder = ReleaseRecorder::reserve(
            cluster.storage.as_ref(),
            name,
            namespace,
            &manifests,
            false,
            cluster.client.as_ref(),
            &mut scopes,
            claim,
        )
        .await?;
        let mut applied = Applied::default();
        let run = {
            let mut sink = |event: ApplyEvent<'_>| match event {
                ApplyEvent::Applied {
                    key,
                    result: Err(error),
                } => {
                    applied.failure_texts.push((
                        key.to_string(),
                        error.to_string(),
                        error.kube_kind() == Some(KubeErrorKind::Rejected),
                    ));
                }
                ApplyEvent::PruneKept {
                    key,
                    decision: PruneDecision::AwaitConfirmation,
                    ..
                } => applied.awaiting.push(key.to_string()),
                _ => {}
            };
            recorder
                .run(
                    cluster.client.as_ref(),
                    &manifests,
                    &mut scopes,
                    ApplySettings {
                        concurrency: CONCURRENCY,
                        history_limit,
                        confirm_prune: false,
                        ownership: &ownership,
                    },
                    Interruption {
                        interrupt: tokio::time::sleep(budget),
                        escalate: std::future::pending(),
                    },
                    &mut sink,
                )
                .await?
        };
        applied.tracked.clone_from(&run.release.resource_keys);
        if let Some(error) = run.error("Apply") {
            let detail = applied
                .failure_texts
                .iter()
                .map(|(key, error, _)| format!("{key}: {}", error.lines().next().unwrap_or_default()))
                .collect::<Vec<_>>()
                .join("; ");
            let text = format!(
                "Release {namespace}/{name}: {error}{}",
                if detail.is_empty() {
                    String::new()
                } else {
                    format!(" ({detail})")
                }
            );
            let rejected =
                !applied.failure_texts.is_empty() && applied.failure_texts.iter().all(|(_, _, rejected)| *rejected);
            applied.failures.push(if rejected {
                Why::Rejected(text)
            } else {
                Why::Failed(text)
            });
        }
        Ok(applied)
    }

    /// Purge the Releases that left the tree, by state identity, once every
    /// Release of this run is `Deployed`.
    async fn purge_vanished(
        &self,
        cluster: &ClusterHandle,
        rendered: &RenderedTarget,
        releases: &[PublishedDirectRelease],
        tree_namespaces: &BTreeSet<String>,
        tracked: &HashSet<(String, String, Option<String>, String)>,
        probe: &(dyn Fn(&RenderIndexOwner) -> OwnerState + Sync),
    ) -> Result<Vec<Finding>, Failure> {
        let variable = |name: &str| self.ctx.env.get(name).cloned();
        let history =
            GitPublicationHistory::fetch(&self.observed, &self.ctx.project_dir, &variable).map_err(|error| {
                NylError::config(format!(
                    "cannot read the publication branch to find the Releases that left the tree: {error}"
                ))
            })?;
        let receipt = self
            .ctx
            .artifacts
            .get("tree")
            .and_then(|tree| tree.spec.get("commit"))
            .and_then(Value::as_str);
        let previous = history.direct_releases_since(receipt, MATCH_WALK_LIMIT)?;
        let current: HashSet<(&str, &str)> = releases.iter().map(PublishedDirectRelease::identity).collect();
        let vanished = previous
            .iter()
            .filter(|release| !current.contains(&release.identity()))
            .map(|release| (release.release.namespace.clone(), release.release.name.clone()))
            .collect::<Vec<_>>();
        let _ = rendered;
        let protect = Protect {
            keys: tracked.clone(),
            namespaces: tree_namespaces.clone(),
        };
        let mut warnings = Vec::new();
        let mut failures = Vec::new();
        for (namespace, name) in vanished {
            match self
                .purge_release(cluster, &namespace, &name, &protect, true, probe)
                .await?
            {
                Purge::Done => {}
                Purge::Foreign(owner) => warnings.push(Finding::new(
                    Code::KubernetesReleaseLeftBehind,
                    format!(
                        "Release {namespace}/{name} left the tree but is not this unit's to remove ({}), so it stays",
                        owner.map_or_else(
                            || "it records no owner".to_owned(),
                            |owner| format!("it is owned by {owner}")
                        )
                    ),
                    "remove it with `nyl release delete --target <target> --take-over`, or leave it",
                )),
                Purge::Failed(message) => failures.push(message),
            }
        }
        if failures.is_empty() {
            Ok(warnings)
        } else {
            Err(Failure::Nyl(NylError::Kubernetes(format!(
                "purging the Releases that left the tree failed: {}",
                failures.join("; ")
            ))))
        }
    }

    /// Delete one release's resources by their `Delete` option, in reverse
    /// order with waits, keeping what `protect` names and the Namespaces that
    /// hold release state; with `remove_state`, then delete its state and those
    /// Namespaces. Only for a release this unit may purge.
    async fn purge_release(
        &self,
        cluster: &ClusterHandle,
        namespace: &str,
        name: &str,
        protect: &Protect,
        remove_state: bool,
        probe: &(dyn Fn(&RenderIndexOwner) -> OwnerState + Sync),
    ) -> Result<Purge, Failure> {
        let what = format!("read the release state of {namespace}/{name}");
        let revisions = cluster
            .storage
            .list_revisions(name, namespace)
            .await
            .map_err(|error| api_error(&error, &what))?;
        if revisions.is_empty() {
            return Ok(Purge::Done);
        }
        let mut states: Vec<ReleaseState> = Vec::new();
        for revision in &revisions {
            if let Some(state) = cluster
                .storage
                .get_release(name, namespace, *revision)
                .await
                .map_err(|error| api_error(&error, &what))?
            {
                states.push(state);
            }
        }
        let latest = states
            .iter()
            .max_by_key(|state| state.revision)
            .expect("a release has a revision");
        let owner = latest.meta.owner.clone();
        let adoptable = owner.is_none()
            && latest
                .meta
                .publication
                .as_ref()
                .is_some_and(|publication| publication.same_destination(&self.destination));
        if !adoptable && !may_purge(owner.as_ref(), &self.owner, probe) {
            return Ok(Purge::Foreign(owner));
        }
        let mut seen = HashSet::new();
        let keys: Vec<ResourceKey> = states
            .iter()
            .flat_map(|state| state.resource_keys.iter().cloned())
            .filter(|key| seen.insert(identity(key)))
            .collect();
        let state_namespaces: BTreeSet<String> = BTreeSet::from([namespace.to_owned()]);
        let excluded = |key: &ResourceKey| protect.protects(key) || is_state_namespace(key, &state_namespaces);
        let purged = purge_resources(
            cluster.client.as_ref(),
            &keys,
            &excluded,
            CONCURRENCY,
            false,
            &mut |_| {},
        )
        .await;
        if !purged.failed.is_empty() {
            return Ok(Purge::Failed(format!(
                "release {namespace}/{name}: {} resource(s) failed to delete",
                purged.failed.len()
            )));
        }
        if remove_state {
            cluster
                .storage
                .delete_all_revisions(name, namespace)
                .await
                .map_err(|error| api_error(&error, &format!("delete the release state of {namespace}/{name}")))?;
            let only_state_namespaces =
                |key: &ResourceKey| protect.protects(key) || !is_state_namespace(key, &state_namespaces);
            let purged = purge_resources(
                cluster.client.as_ref(),
                &keys,
                &only_state_namespaces,
                CONCURRENCY,
                false,
                &mut |_| {},
            )
            .await;
            if !purged.failed.is_empty() {
                return Ok(Purge::Failed(format!(
                    "namespace {namespace} of release {name} failed to delete"
                )));
            }
        }
        Ok(Purge::Done)
    }

    /// The Releases a teardown removes: those of the rendered tree and those
    /// the history since the last receipt published.
    fn teardown_releases(&self, rendered: &RenderedTarget) -> DriverResult<Vec<(String, String)>> {
        let current = nyl_render::gitops::observation::direct_deployments(rendered)
            .map_err(|error| DriverError::before_effects(format!("cannot read the Releases of the target: {error}")))?;
        let variable = |name: &str| self.ctx.env.get(name).cloned();
        let history =
            GitPublicationHistory::fetch(&self.observed, &self.ctx.project_dir, &variable).map_err(|error| {
                DriverError::before_effects(format!(
                    "cannot read the publication branch to find the Releases to remove: {error}"
                ))
            })?;
        let receipt = self
            .ctx
            .artifacts
            .get("tree")
            .and_then(|tree| tree.spec.get("commit"))
            .and_then(Value::as_str);
        let previous = history
            .direct_releases_since(receipt, MATCH_WALK_LIMIT)
            .map_err(|error| DriverError::before_effects(error.to_string()))?;
        let mut all = std::collections::BTreeSet::new();
        for release in current.into_iter().chain(previous) {
            all.insert((release.release.namespace.clone(), release.release.name.clone()));
        }
        Ok(all.into_iter().collect())
    }

    /// Teardown, phase 1 (after the tree is published without its Releases):
    /// purge everything but the Namespaces that hold release state, which stay
    /// with the state until phase 2.
    pub(crate) fn teardown_delete(&self, rendered: &RenderedTarget) -> DriverResult<Option<Outcome>> {
        self.teardown(rendered, false)
    }

    /// Teardown, phase 2: delete the release state, then the Namespaces that held it.
    pub(crate) fn teardown_remove_state(&self, rendered: &RenderedTarget) -> DriverResult<Option<Outcome>> {
        self.teardown(rendered, true)
    }

    fn teardown(&self, rendered: &RenderedTarget, remove_state: bool) -> DriverResult<Option<Outcome>> {
        if let Some(outcome) = self.missing_context() {
            return Ok(Some(outcome));
        }
        let releases = self.teardown_releases(rendered)?;
        let probe = self.probe();
        let result = block_on(|| async {
            let cluster = self.open().await?;
            // Classify first, so what a foreign release tracks is never deleted
            // by another's purge.
            let mut foreign_keys = HashSet::new();
            let mut purgeable = Vec::new();
            for (namespace, name) in &releases {
                let states = cluster
                    .storage
                    .get_latest_release(name, namespace)
                    .await
                    .map_err(|error| api_error(&error, "read the release state"))?;
                let Some(latest) = states else { continue };
                let adoptable = latest.meta.owner.is_none()
                    && latest
                        .meta
                        .publication
                        .as_ref()
                        .is_some_and(|publication| publication.same_destination(&self.destination));
                if adoptable || may_purge(latest.meta.owner.as_ref(), &self.owner, &probe) {
                    let wave = nyl_render::yaml::parse_yaml_documents_k8s_compatible(&latest.manifest)
                        .map_or(1, |docs| wave_of(&docs));
                    purgeable.push((wave, namespace.clone(), name.clone()));
                } else {
                    foreign_keys.extend(latest.resource_keys.iter().map(identity));
                }
            }
            // Reverse apply order: the last wave and the last Release first.
            purgeable.sort();
            purgeable.reverse();
            let protect = Protect {
                keys: foreign_keys,
                namespaces: BTreeSet::new(),
            };
            let mut failures = Vec::new();
            for (_, namespace, name) in purgeable {
                if let Purge::Failed(message) = self
                    .purge_release(&cluster, &namespace, &name, &protect, remove_state, &probe)
                    .await?
                {
                    failures.push(message);
                }
            }
            Ok::<_, Failure>(failures)
        })
        .map_err(|error| DriverError::after_effects(error.message))?;
        Ok(match result {
            Ok(failures) if failures.is_empty() => None,
            Ok(failures) => Some(Outcome::Failed {
                category: FailureCategory::ToolError,
                retryable: true,
                message: format!("the teardown could not delete everything: {}", failures.join("; ")),
            }),
            Err(failure) => Some(outcome_of("the teardown could not reach the cluster", &[why(&failure)])),
        })
    }
}

/// What applying one Release found.
#[derive(Default)]
struct Applied {
    tracked: Vec<ResourceKey>,
    awaiting: Vec<String>,
    failures: Vec<Why>,
    /// Each failed resource, its error, and whether the API rejected the object itself.
    failure_texts: Vec<(String, String, bool)>,
}

enum Purge {
    Done,
    /// Not this unit's: the recorded owner, if any.
    Foreign(Option<RenderIndexOwner>),
    Failed(String),
}

/// What a purge must not delete: objects another release tracks, and the
/// Namespaces a current release uses.
struct Protect {
    keys: HashSet<(String, String, Option<String>, String)>,
    namespaces: BTreeSet<String>,
}

impl Protect {
    fn protects(&self, key: &ResourceKey) -> bool {
        self.keys.contains(&identity(key))
            || (key.gvk.group.is_empty() && key.gvk.kind == "Namespace" && self.namespaces.contains(&key.name))
    }
}
