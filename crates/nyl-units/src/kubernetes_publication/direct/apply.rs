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
    collect_live_state, ensure_namespace_exists, prepare_desired_manifests, purge_resources, ApplyEvent, ApplySettings,
    Interruption, ObjectScopes, ReleaseRecorder,
};
use nyl_render::kubernetes::prune::PruneDecision;
use nyl_render::kubernetes::{ObjectIdentity, ReleasePublication, ResourceKey, ResourceOrdering};
use nyl_render::{KubeErrorKind, NylError};
use serde_json::Value;

use super::reader::api_error;
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

fn identity(key: &ResourceKey) -> ObjectKey {
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

    /// Now, by the injected clock, never earlier than the call's start: nothing
    /// here reads the system clock.
    fn now(&self) -> chrono::DateTime<chrono::Utc> {
        self.ctx.now.max(self.ctx.clock.now())
    }

    /// What is left of the unit's deadline, after whatever came before (the publish).
    fn budget(&self) -> Duration {
        (self.ctx.deadline - self.now()).to_std().unwrap_or_default()
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
    /// purge the Releases that left the tree. One absolute deadline bounds the
    /// whole pass: no Release gets a fresh budget, and nothing continues after
    /// the deadline passed.
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
        let remaining = self.budget();
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
            let end = tokio::time::Instant::now() + remaining;
            let claim = self.claim(&publication, &probe);
            let mut order = (0..releases.len()).collect::<Vec<_>>();
            order.sort_by_key(|index| (wave_of(&documents[*index]), releases[*index].key.clone()));
            // What every current Release renders is known before any of them
            // prunes, so an object that moved from one Release to another is
            // not deleted by the one that gave it up.
            let mut prepared = Vec::new();
            let mut protected: HashSet<ObjectIdentity> = HashSet::new();
            for &index in &order {
                let mut duplicates = HashMap::new();
                let (mut manifests, scopes) = prepare_desired_manifests(
                    cluster.client.as_ref(),
                    documents[index].clone(),
                    &mut duplicates,
                    Some(&releases[index].release.destination_namespace),
                )
                .await?;
                ResourceOrdering::sort_by_priority(&mut manifests)?;
                for manifest in &manifests {
                    protected.insert(scopes.identity(&ResourceKey::from_json_value(manifest)?));
                }
                prepared.push((manifests, scopes));
            }
            let mut failures = Vec::new();
            let mut awaiting = Vec::new();
            let mut tracked: HashSet<ObjectKey> = HashSet::new();
            let mut current_wave = None;
            for (&index, (manifests, scopes)) in order.iter().zip(prepared) {
                let wave = wave_of(&documents[index]);
                if current_wave.is_some_and(|current| current != wave) && !failures.is_empty() {
                    // Later waves may need what the failed Release registers.
                    break;
                }
                current_wave = Some(wave);
                let release = &releases[index];
                if tokio::time::Instant::now() >= end {
                    failures.push(Why::Timeout(format!(
                        "the unit's deadline passed before Release {} was applied",
                        release.key
                    )));
                    break;
                }
                match self
                    .apply_one(
                        &cluster,
                        release,
                        (manifests, scopes),
                        history_limit,
                        end,
                        &claim,
                        &protected,
                    )
                    .await
                {
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
                .purge_vanished(&cluster, releases, &tree_namespaces, &tracked, &probe)
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
                &format!(
                    "the tree was published at {commit}, but applying it to the cluster failed (Git is ahead of the \
                     cluster; no Release that left the tree was purged; the next execution applies again)"
                ),
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

    #[allow(clippy::too_many_arguments)] // The inputs of one Release's apply are independent facts.
    async fn apply_one(
        &self,
        cluster: &ClusterHandle,
        release: &PublishedDirectRelease,
        (manifests, mut scopes): (Vec<Value>, ObjectScopes),
        history_limit: u32,
        end: tokio::time::Instant,
        claim: &ReleaseClaim<'_>,
        protected: &HashSet<ObjectIdentity>,
    ) -> Result<Applied, NylError> {
        let (namespace, name) = release.identity();
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
                        protected: Some(protected),
                    },
                    Interruption {
                        interrupt: tokio::time::sleep_until(end),
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
    /// Release of this run is `Deployed`. A Release this unit does not own (no
    /// owner, or another unit's) is left and reported, never purged.
    async fn purge_vanished(
        &self,
        cluster: &ClusterHandle,
        releases: &[PublishedDirectRelease],
        tree_namespaces: &BTreeSet<String>,
        tracked: &HashSet<ObjectKey>,
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
        let protect = Protect {
            keys: tracked.clone(),
            namespaces: tree_namespaces.clone(),
        };
        let mut warnings = Vec::new();
        let mut failures = Vec::new();
        for (namespace, name) in vanished {
            let defer = BTreeSet::from([namespace.clone()]);
            let purge = self
                .purge_release(cluster, &namespace, &name, &protect, &defer, true, false, probe)
                .await?;
            match purge {
                Purge::Done { deferred } => {
                    if !deferred.is_empty() {
                        let purged = purge_resources(
                            cluster.client.as_ref(),
                            &deferred,
                            &|key| protect.protects(key),
                            CONCURRENCY,
                            false,
                            &mut |_| {},
                        )
                        .await;
                        if !purged.failed.is_empty() {
                            failures.push(format!("namespace {namespace} of release {name} failed to delete"));
                        }
                    }
                }
                Purge::Foreign(owner) => warnings.push(Finding::new(
                    Code::KubernetesReleaseLeftBehind,
                    format!(
                        "Release {namespace}/{name} left the tree but is not this unit's to remove ({}), so it stays",
                        owner.map_or_else(
                            || "no unit owns it".to_owned(),
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
    /// order with waits, keeping what `protect` names and the Namespaces in
    /// `defer`, which hold release state; with `remove_state`, then delete its
    /// state and return the deferred Namespace objects it tracked, for the
    /// caller to delete once no release state remains in them. Only for a
    /// release this unit may purge: one it owns, or one an earlier incarnation of
    /// the same unit owned, or, with `adoptable`, an unowned one applied for this
    /// publication.
    #[allow(clippy::too_many_arguments)] // The inputs of one release's purge are independent facts.
    async fn purge_release(
        &self,
        cluster: &ClusterHandle,
        namespace: &str,
        name: &str,
        protect: &Protect,
        defer: &BTreeSet<String>,
        remove_state: bool,
        adoptable: bool,
        probe: &(dyn Fn(&RenderIndexOwner) -> OwnerState + Sync),
    ) -> Result<Purge, Failure> {
        let what = format!("read the release state of {namespace}/{name}");
        let revisions = cluster
            .storage
            .list_revisions(name, namespace)
            .await
            .map_err(|error| api_error(&error, &what))?;
        let Some(&latest_revision) = revisions.iter().max() else {
            return Ok(Purge::Done { deferred: Vec::new() });
        };
        let Some(latest) = cluster
            .storage
            .get_release(name, namespace, latest_revision)
            .await
            .map_err(|error| api_error(&error, &what))?
        else {
            return Ok(Purge::Done { deferred: Vec::new() });
        };
        let owner = latest.meta.owner.clone();
        let unowned_here = owner.is_none()
            && latest
                .meta
                .publication
                .as_ref()
                .is_some_and(|publication| publication.same_destination(&self.destination));
        if !(may_purge(owner.as_ref(), &self.owner, probe) || adoptable && unowned_here) {
            return Ok(Purge::Foreign(owner));
        }
        // What the release still has in the cluster: the most recent `Deployed`
        // revision's keys and what failed revisions after it applied, as a prune
        // reads it, so an object a retained revision left untracked is never deleted.
        let (_, live) = collect_live_state(
            cluster.storage.as_ref(),
            name,
            namespace,
            &revisions,
            latest_revision + 1,
        )
        .await
        .map_err(|error| api_error(&error, &what))?;
        let mut seen = HashSet::new();
        let mut keys: Vec<ResourceKey> = live.into_iter().filter(|key| seen.insert(identity(key))).collect();
        keys.sort_by_key(identity);
        let excluded = |key: &ResourceKey| protect.protects(key) || is_namespace(key, defer);
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
        if !remove_state {
            return Ok(Purge::Done { deferred: Vec::new() });
        }
        cluster
            .storage
            .delete_all_revisions(name, namespace)
            .await
            .map_err(|error| api_error(&error, &format!("delete the release state of {namespace}/{name}")))?;
        let deferred = keys
            .into_iter()
            .filter(|key| is_namespace(key, defer))
            .collect::<Vec<_>>();
        Ok(Purge::Done { deferred })
    }

    /// What a teardown removes, read before anything is published or deleted: the
    /// Releases of the rendered tree and those the history since the last receipt
    /// published, in the reverse of the apply order (wave, then key).
    pub(crate) fn teardown_plan(&self, rendered: &RenderedTarget) -> DriverResult<TeardownPlan> {
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
        let mut all = std::collections::BTreeMap::new();
        for release in current.into_iter().chain(previous) {
            all.entry((release.release.namespace.clone(), release.release.name.clone()))
                .or_insert(release.key);
        }
        Ok(TeardownPlan {
            releases: all
                .into_iter()
                .map(|((namespace, name), key)| (key, namespace, name))
                .collect(),
        })
    }

    /// Teardown, phase 1 (after the tree is published without its Releases):
    /// purge everything but the Namespaces that hold release state, which stay
    /// with the state until phase 2.
    pub(crate) fn teardown_delete(&self, plan: &TeardownPlan) -> DriverResult<TeardownResult> {
        self.teardown(plan, false)
    }

    /// Sort a teardown's Releases into those this unit may purge and those it
    /// may not, whose tracked objects and Namespaces then stay.
    async fn classify_teardown(
        &self,
        cluster: &ClusterHandle,
        plan: &TeardownPlan,
        probe: &(dyn Fn(&RenderIndexOwner) -> OwnerState + Sync),
    ) -> Result<Classified, Failure> {
        let mut foreign_keys = HashSet::new();
        let mut foreign_namespaces = BTreeSet::new();
        let mut purgeable = Vec::new();
        let mut skipped = Vec::new();
        for (key, namespace, name) in &plan.releases {
            let revisions = cluster
                .storage
                .list_revisions(name, namespace)
                .await
                .map_err(|error| api_error(&error, "read the release state"))?;
            let Some(&latest_revision) = revisions.iter().max() else {
                continue;
            };
            let Some(latest) = cluster
                .storage
                .get_release(name, namespace, latest_revision)
                .await
                .map_err(|error| api_error(&error, "read the release state"))?
            else {
                continue;
            };
            let unowned_here = latest.meta.owner.is_none()
                && latest
                    .meta
                    .publication
                    .as_ref()
                    .is_some_and(|publication| publication.same_destination(&self.destination));
            if unowned_here || may_purge(latest.meta.owner.as_ref(), &self.owner, probe) {
                let wave = nyl_render::yaml::parse_yaml_documents_k8s_compatible(&latest.manifest)
                    .map_or(1, |docs| wave_of(&docs));
                purgeable.push((wave, key.clone(), namespace.clone(), name.clone()));
            } else {
                let (_, live) = collect_live_state(
                    cluster.storage.as_ref(),
                    name,
                    namespace,
                    &revisions,
                    latest_revision + 1,
                )
                .await
                .map_err(|error| api_error(&error, "read the release state"))?;
                foreign_keys.extend(live.iter().map(identity));
                foreign_namespaces.insert(namespace.clone());
                skipped.push(format!(
                    "{namespace}/{name} ({})",
                    latest.meta.owner.as_ref().map_or_else(
                        || "applied for another publication".to_owned(),
                        |owner| format!("owned by {owner}")
                    )
                ));
            }
        }
        Ok(Classified {
            foreign_keys,
            foreign_namespaces,
            purgeable,
            skipped,
        })
    }

    /// Teardown, phase 2: delete the release state, then the Namespaces that held it.
    pub(crate) fn teardown_remove_state(&self, plan: &TeardownPlan) -> DriverResult<TeardownResult> {
        self.teardown(plan, true)
    }

    fn teardown(&self, plan: &TeardownPlan, remove_state: bool) -> DriverResult<TeardownResult> {
        if let Some(outcome) = self.missing_context() {
            return Ok(TeardownResult {
                outcome: Some(outcome),
                skipped: Vec::new(),
            });
        }
        let probe = self.probe();
        let result = block_on(|| async {
            let cluster = self.open().await?;
            // Classify first, so what a release this unit may not purge tracks is never
            // deleted by another's purge, and no Namespace holding any release's state,
            // theirs included, is deleted before all state is gone.
            let all_namespaces: BTreeSet<String> = plan
                .releases
                .iter()
                .map(|(_, namespace, _)| namespace.clone())
                .collect();
            let Classified {
                foreign_keys,
                foreign_namespaces,
                mut purgeable,
                skipped,
            } = self.classify_teardown(&cluster, plan, &probe).await?;
            // The exact reverse of the apply order: the last wave and the last key first.
            purgeable.sort();
            purgeable.reverse();
            let protect = Protect {
                keys: foreign_keys,
                namespaces: foreign_namespaces,
            };
            let defer = if remove_state {
                BTreeSet::new()
            } else {
                all_namespaces.clone()
            };
            // Phase 2 defers the same Namespaces past each release's own state, to
            // delete them once every release's state is gone.
            let defer_until_all_state = if remove_state { all_namespaces } else { BTreeSet::new() };
            let mut failures = Vec::new();
            let mut deferred = Vec::new();
            for (_, _, namespace, name) in purgeable {
                let defer = if remove_state { &defer_until_all_state } else { &defer };
                match self
                    .purge_release(&cluster, &namespace, &name, &protect, defer, remove_state, true, &probe)
                    .await?
                {
                    Purge::Done { deferred: more } => deferred.extend(more),
                    Purge::Failed(message) => failures.push(message),
                    Purge::Foreign(_) => {}
                }
            }
            if failures.is_empty() && !deferred.is_empty() {
                let purged = purge_resources(
                    cluster.client.as_ref(),
                    &deferred,
                    &|key| protect.protects(key),
                    CONCURRENCY,
                    false,
                    &mut |_| {},
                )
                .await;
                if !purged.failed.is_empty() {
                    failures.push(format!("{} Namespace(s) failed to delete", purged.failed.len()));
                }
            }
            Ok::<_, Failure>((failures, skipped))
        })
        .map_err(|error| DriverError::after_effects(error.message))?;
        Ok(match result {
            Ok((failures, skipped)) if failures.is_empty() => TeardownResult { outcome: None, skipped },
            Ok((failures, skipped)) => TeardownResult {
                outcome: Some(Outcome::Failed {
                    category: FailureCategory::ToolError,
                    retryable: true,
                    message: format!("the teardown could not delete everything: {}", failures.join("; ")),
                }),
                skipped,
            },
            Err(failure) => TeardownResult {
                outcome: Some(outcome_of("the teardown could not reach the cluster", &[why(&failure)])),
                skipped: Vec::new(),
            },
        })
    }
}

/// What a teardown removes: `(key, release namespace, release name)` of each Release.
pub(crate) struct TeardownPlan {
    releases: Vec<(String, String, String)>,
}

/// A teardown's Releases, sorted by whether this unit may purge them.
struct Classified {
    /// The objects the Releases it may not purge track.
    foreign_keys: HashSet<ObjectKey>,
    /// The Namespaces holding their state.
    foreign_namespaces: BTreeSet<String>,
    /// (wave, key, namespace, name) of the Releases it purges.
    purgeable: Vec<(u8, String, String, String)>,
    /// The Releases it leaves, described for the warning.
    skipped: Vec<String>,
}

/// What one teardown phase did.
pub(crate) struct TeardownResult {
    /// The outcome that ends the call, `None` when the phase completed.
    pub outcome: Option<Outcome>,
    /// The Releases it left alone because this unit may not remove them.
    pub skipped: Vec<String>,
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
    /// Purged; the Namespace objects it left for the caller to delete once no release state is in them.
    Done {
        deferred: Vec<ResourceKey>,
    },
    /// Not this unit's: the recorded owner, if any.
    Foreign(Option<RenderIndexOwner>),
    Failed(String),
}

/// An object's group, kind, namespace, and name.
type ObjectKey = (String, String, Option<String>, String);

/// What a purge must not delete: objects another release tracks, and the
/// Namespaces that hold state or that a current release uses.
struct Protect {
    keys: HashSet<ObjectKey>,
    namespaces: BTreeSet<String>,
}

impl Protect {
    fn protects(&self, key: &ResourceKey) -> bool {
        self.keys.contains(&identity(key)) || is_namespace(key, &self.namespaces)
    }
}

/// Whether `key` is a Namespace named in `names`.
fn is_namespace(key: &ResourceKey, names: &BTreeSet<String>) -> bool {
    key.gvk.group.is_empty() && key.gvk.kind == "Namespace" && names.contains(&key.name)
}
