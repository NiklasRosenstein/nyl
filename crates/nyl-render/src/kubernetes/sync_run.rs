//! One sync of a Release's documents, the way Argo CD syncs them: phases, waves,
//! hooks, and the waits between them.
//!
//! Contract: [Direct reconciler](../../../../design/orchestration-core.md#direct-reconciler),
//! applying. [`super::lifecycle`] drives it, so `nyl apply`, `nyl release rollback`,
//! and the direct reconciler sync alike; [`super::hooks`] decides what each document
//! is and when it runs.
//!
//! - **Steps.** A sync is a sequence of `(phase, wave)` steps: `PreSync` waves,
//!   then `Sync` waves, then `PostSync` waves, each ascending. A step applies its
//!   documents kind by kind ([`ResourceOrdering::apply_batches`]); the documents of
//!   one kind apply concurrently and a kind finishes before the next starts. A
//!   Release without hooks or waves is one step, applied exactly as it always was.
//!   A Namespace or CustomResourceDefinition applies in the step of its first
//!   dependent when that comes earlier ([`super::hooks::resource_slots`]).
//! - **Waits.** A step does not finish before its hooks completed: a Job or Pod
//!   succeeded, another kind exists. In a sync of more than one step it also waits
//!   until the workloads the step applied are healthy ([`health`]), as Argo CD does,
//!   except in the last step when that holds nothing but ordinary resources: the sync
//!   is done once they are applied. That is why `PostSync` hooks start only after the
//!   `Sync` phase is healthy.
//! - **Finalizer.** A hook is created with Argo CD's hook finalizer, so a Job a TTL
//!   controller removes the moment it finishes is still there to be read. The sync
//!   removes the finalizer once it has seen how the hook ended, and again on every path
//!   that ends the hook: failure, timeout, and deletion by a policy or a purge.
//! - **Failure.** A failed document, a failed hook, or a wait that ran out stops the
//!   sync after the step it happened in; the steps after it never start. `SyncFail`
//!   hooks then run, and objects with the `HookFailed` policy are deleted.
//! - **Cleanup.** Before a hook with the `BeforeHookCreation` policy (the default) is
//!   created, an existing object is deleted and waited for. After a successful sync,
//!   objects with the `HookSucceeded` policy are deleted.
//!
//! `PostSync` runs apart ([`SyncExecution::run_post_sync`]) because the lifecycle prunes
//! between the `Sync` phase and `PostSync`, where Argo CD prunes within the `Sync` phase.

use std::collections::HashSet;
use std::time::Duration;

use kube::api::DynamicObject;
use serde_json::Value;
use tokio::time::Instant;

use super::health::{assessed, rollout, Health};
use super::hooks::{self, DeletePolicy, HookPhase, HookTask, ReleasePlan, HOOK_FINALIZER};
use super::lifecycle::{apply_manifest, ApplyEvent, ApplyEventSink, ApplyExecutionResult, RegisteredKinds};
use super::{
    field_manager::FieldOwnership, run_concurrently, ApplyOutcome, DeleteOptions, DeletePropagation, GroupVersionKind,
    KubeClient, ResourceKey, ResourceOrdering,
};
use crate::{NylError, Result};

/// How long a sync waits for hooks and workloads, and how often it looks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WaitSettings {
    /// The longest one wait lasts: for a step's hooks to complete, for its
    /// workloads to become healthy, for a deleted hook to be gone. `None` waits until
    /// the caller interrupts (Argo CD has no limit of its own).
    pub timeout: Option<Duration>,
    /// The moment after which no wait goes on, whatever `timeout` says: the end of a
    /// caller's budget for the whole sync, less what it needs to report the failure.
    /// The failure then takes the normal path (`SyncFail` hooks, the policies, the
    /// recorded revision) instead of an interrupt cutting the sync short.
    pub deadline: Option<Instant>,
    /// How far past `deadline` the waits of the failure path (`SyncFail` hooks) may go:
    /// the margin the deadline keeps back, which is what they run in.
    pub grace: Duration,
    /// The pause between two reads of the cluster; later reads of one wait pause longer,
    /// up to five times as long.
    pub poll_interval: Duration,
}

impl WaitSettings {
    /// What `nyl apply` waits by default: ten minutes per wait, reading every two seconds.
    pub const DEFAULT_TIMEOUT: Duration = Duration::from_mins(10);

    /// No limit, for a caller that bounds the whole sync itself.
    pub const fn unbounded() -> Self {
        Self {
            timeout: None,
            deadline: None,
            grace: Duration::ZERO,
            poll_interval: Duration::from_secs(2),
        }
    }

    /// No limit of its own, but no wait goes on after `deadline`, except those of the
    /// failure path, which may use `grace` more.
    #[must_use]
    pub const fn until(deadline: Instant, grace: Duration) -> Self {
        Self {
            deadline: Some(deadline),
            grace,
            ..Self::unbounded()
        }
    }

    /// The waits of the failure path, which has the grace the deadline kept back.
    fn failing(self) -> Self {
        Self {
            deadline: self.deadline.map(|deadline| deadline + self.grace),
            ..self
        }
    }
}

impl Default for WaitSettings {
    fn default() -> Self {
        Self {
            timeout: Some(Self::DEFAULT_TIMEOUT),
            deadline: None,
            grace: Duration::ZERO,
            poll_interval: Duration::from_secs(2),
        }
    }
}

/// What became of one run of a hook.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookReport {
    pub key: ResourceKey,
    pub phase: HookPhase,
    pub wave: i64,
    /// Why it failed; `None` when it completed.
    pub failure: Option<String>,
}

/// What bounds one wait.
#[derive(Clone, Copy)]
enum Bound {
    Timeout(Duration),
    Deadline,
}

/// A deadline for one wait: its own timeout or the sync's deadline, whichever comes first.
#[derive(Clone, Copy)]
struct Deadline(Option<(Instant, Bound)>);

impl Deadline {
    fn new(wait: &WaitSettings) -> Self {
        let own = wait
            .timeout
            .map(|timeout| (Instant::now() + timeout, Bound::Timeout(timeout)));
        let overall = wait.deadline.map(|deadline| (deadline, Bound::Deadline));
        Self(match (own, overall) {
            (Some(own), Some(overall)) => Some(if overall.0 < own.0 { overall } else { own }),
            (own, overall) => own.or(overall),
        })
    }

    fn passed(&self) -> bool {
        self.0.is_some_and(|(end, _)| Instant::now() >= end)
    }

    /// Why the wait ended, for the failure it causes.
    fn explain(&self) -> String {
        match self.0 {
            Some((_, Bound::Timeout(timeout))) => format!(
                "it did not finish within {}s; raise the wait limit with `--hook-timeout`",
                timeout.as_secs()
            ),
            Some((_, Bound::Deadline)) => {
                "it did not finish in the time left for the sync, which keeps a margin to record the failure".to_owned()
            }
            None => "it did not finish".to_owned(),
        }
    }
}

/// The pauses between the reads of one wait: the poll interval, growing by half
/// each time up to five times as long, so a long wait reads less often than a short one.
struct Pace {
    next: Duration,
    cap: Duration,
}

impl Pace {
    fn new(wait: &WaitSettings) -> Self {
        Self {
            next: wait.poll_interval,
            cap: wait.poll_interval.saturating_mul(5),
        }
    }

    async fn pause(&mut self) {
        tokio::time::sleep(self.next).await;
        self.next = (self.next + self.next / 2).min(self.cap);
    }
}

/// A document of a step.
#[derive(Clone, Copy)]
enum Task {
    Resource(usize),
    Hook(usize),
}

/// What a task's apply returned.
enum Done {
    Resource(Result<ApplyOutcome>, Option<Result<bool>>),
    Hook(Result<ApplyOutcome>),
}

/// Where a hook object stands.
enum Progress {
    Done,
    Running(String),
    Failed(String),
}

/// A hook object's progress from its live state: a Job, Pod, or Argo Workflow is done
/// when it succeeded and failed when it did; a workload when it is healthy; any other
/// kind once it exists, as an Argo CD resource without a health check is.
fn hook_progress(gvk: &GroupVersionKind, live: &Value) -> Progress {
    if !assessed(&gvk.group, &gvk.kind) {
        return Progress::Done;
    }
    let Some(status) = rollout(&gvk.kind, live) else {
        return Progress::Done;
    };
    let why = status.why.unwrap_or_default();
    match status.health {
        Health::Healthy => Progress::Done,
        // A held workload is waited for, as Argo CD waits for a suspended one.
        Health::Progressing | Health::Suspended => Progress::Running(why),
        Health::Degraded | Health::Unknown => Progress::Failed(why),
    }
}

async fn read(client: &dyn KubeClient, key: &ResourceKey) -> Result<Option<Value>> {
    let live = client
        .get_resource(&key.gvk, key.namespace.as_deref(), &key.name)
        .await?;
    live.map(|object| serde_json::to_value(&object).map_err(NylError::from))
        .transpose()
}

/// Whether the live object still carries the hook finalizer.
fn has_finalizer(live: &Value) -> bool {
    live.pointer("/metadata/finalizers")
        .and_then(Value::as_array)
        .is_some_and(|finalizers| finalizers.iter().any(|name| name == HOOK_FINALIZER))
}

/// Let go of a hook object: remove the finalizer this sync added, so the object can
/// be deleted by a policy, a purge, or the TTL controller that was waiting for it.
/// Best-effort: an object that cannot be reached keeps it until the next sync deletes
/// or purges the hook, which removes it first.
pub(super) async fn release_hook(client: &dyn KubeClient, key: &ResourceKey) {
    if let Err(error) = client
        .remove_finalizer(&key.gvk, key.namespace.as_deref(), &key.name, HOOK_FINALIZER)
        .await
    {
        tracing::warn!(
            hook = %key,
            "Could not remove the hook finalizer {HOOK_FINALIZER}; the next sync or purge of the hook removes it: {error}"
        );
    }
}

/// Delete a hook object the way Argo CD does, in the foreground, and, with
/// `wait`, until it is gone. An object that does not exist counts as deleted, also
/// one that goes between the read and the delete.
async fn delete_hook(client: &dyn KubeClient, key: &ResourceKey, until_gone: Option<&WaitSettings>) -> Result<()> {
    let Some(live) = read(client, key).await? else {
        return Ok(());
    };
    if has_finalizer(&live) {
        // A foreground delete would otherwise wait on our own finalizer for ever.
        client
            .remove_finalizer(&key.gvk, key.namespace.as_deref(), &key.name, HOOK_FINALIZER)
            .await?;
    }
    let options = DeleteOptions {
        propagation: Some(DeletePropagation::Foreground),
        grace_period_seconds: None,
    };
    if let Err(error) = client
        .delete_resource_with(&key.gvk, key.namespace.as_deref(), &key.name, &options)
        .await
    {
        // Gone between the read and the delete is what was asked for.
        return match read(client, key).await {
            Ok(None) => Ok(()),
            _ => Err(error),
        };
    }
    let Some(wait) = until_gone else {
        return Ok(());
    };
    let deadline = Deadline::new(wait);
    let mut pace = Pace::new(wait);
    while read(client, key).await?.is_some() {
        if deadline.passed() {
            return Err(NylError::Kubernetes(format!(
                "the previous {key} is still being deleted: {}",
                deadline.explain()
            )));
        }
        pace.pause().await;
    }
    Ok(())
}

/// The manifest of a hook as it is created: with the hook finalizer.
fn with_finalizer(manifest: &Value) -> Value {
    let mut manifest = manifest.clone();
    let Some(metadata) = manifest.get_mut("metadata").and_then(Value::as_object_mut) else {
        return manifest;
    };
    let finalizers = metadata.entry("finalizers").or_insert_with(|| Value::Array(Vec::new()));
    if let Some(finalizers) = finalizers.as_array_mut() {
        if !finalizers.iter().any(|name| name == HOOK_FINALIZER) {
            finalizers.push(Value::String(HOOK_FINALIZER.to_owned()));
        }
    }
    manifest
}

/// Create one run of a hook: delete the object first when its policy says so,
/// then apply it.
async fn create_hook(
    client: &dyn KubeClient,
    task: &HookTask,
    ownership: &FieldOwnership,
    wait: &WaitSettings,
) -> Result<ApplyOutcome> {
    if task.has_policy(DeletePolicy::BeforeHookCreation) {
        delete_hook(client, &task.key, Some(wait)).await?;
    }
    let resource: DynamicObject = serde_json::from_value(with_finalizer(&task.manifest))?;
    client.apply_resource(&resource, ownership.manager(), false).await
}

/// Wait until every hook of `indices` completed or failed, each reported once.
/// An object deleted while it ran failed; a wait that ran out fails what still runs.
async fn await_hooks(
    client: &dyn KubeClient,
    hooks: &[HookTask],
    indices: Vec<usize>,
    wait: &WaitSettings,
) -> Vec<(usize, std::result::Result<(), String>)> {
    let deadline = Deadline::new(wait);
    let mut pace = Pace::new(wait);
    let mut finished = Vec::new();
    let mut running = indices;
    loop {
        let mut still = Vec::new();
        for index in running {
            let key = &hooks[index].key;
            let progress = match read(client, key).await {
                Ok(Some(live)) => hook_progress(&key.gvk, &live),
                Ok(None) => Progress::Failed("it was deleted before it completed".to_owned()),
                // A failed read is not a verdict; the deadline bounds the wait.
                Err(error) => Progress::Running(format!("it cannot be read: {error}")),
            };
            match progress {
                Progress::Done => finished.push((index, Ok(()))),
                Progress::Failed(why) => finished.push((index, Err(why))),
                Progress::Running(why) if deadline.passed() => {
                    finished.push((index, Err(format!("{why}; {}", deadline.explain()))));
                }
                Progress::Running(_) => still.push(index),
            }
        }
        if still.is_empty() {
            return finished;
        }
        running = still;
        pace.pause().await;
    }
}

/// Wait until the workloads among `keys` are healthy; returns those that are not,
/// with why. A workload that is gone, such as a Job a TTL removed, is not waited for;
/// one that cannot be read is, until the deadline, like a hook.
async fn await_healthy(
    client: &dyn KubeClient,
    keys: Vec<ResourceKey>,
    wait: &WaitSettings,
) -> Vec<(ResourceKey, String)> {
    let deadline = Deadline::new(wait);
    let mut pace = Pace::new(wait);
    let mut unhealthy = Vec::new();
    let mut waiting: Vec<ResourceKey> = keys
        .into_iter()
        .filter(|key| assessed(&key.gvk.group, &key.gvk.kind))
        .collect();
    loop {
        let mut still = Vec::new();
        for key in waiting {
            let live = match read(client, &key).await {
                Ok(Some(live)) => live,
                // Gone: nothing is left to judge.
                Ok(None) => continue,
                // A failed read is not a verdict; the deadline bounds the wait.
                Err(error) => {
                    if deadline.passed() {
                        let why = format!("it cannot be read: {error}; {}", deadline.explain());
                        unhealthy.push((key, why));
                    } else {
                        still.push(key);
                    }
                    continue;
                }
            };
            let Some(status) = rollout(&key.gvk.kind, &live) else {
                continue;
            };
            let why = status.why.unwrap_or_default();
            match status.health {
                Health::Healthy => {}
                Health::Degraded => unhealthy.push((key, why)),
                Health::Progressing | Health::Suspended | Health::Unknown if deadline.passed() => {
                    unhealthy.push((key, format!("{why}; {}", deadline.explain())));
                }
                Health::Progressing | Health::Suspended | Health::Unknown => still.push(key),
            }
        }
        if still.is_empty() {
            return unhealthy;
        }
        waiting = still;
        pace.pause().await;
    }
}

/// A sync in progress. Create it with [`Self::new`], run it to the end of the `Sync`
/// phase with [`Self::run_to_sync`], then with [`Self::run_post_sync`], and read
/// [`Self::result`].
pub(super) struct SyncExecution<'a> {
    plan: ReleasePlan,
    keys: Vec<ResourceKey>,
    /// The step each resource applies in ([`hooks::resource_slots`]).
    slots: Vec<(HookPhase, i64)>,
    results: Vec<Option<Result<ApplyOutcome>>>,
    /// Every `(phase, wave)` the sync runs, in order, `SyncFail` apart.
    steps: Vec<(HookPhase, i64)>,
    ownership: &'a FieldOwnership,
    concurrency: usize,
    wait: WaitSettings,
    registered: RegisteredKinds,
    released_count: usize,
    release_failed_count: usize,
    reports: Vec<HookReport>,
    /// Hooks (by index into the plan) created in this sync.
    created: Vec<usize>,
    unhealthy: Vec<(ResourceKey, String)>,
    /// Objects of the manifests that this sync leaves alone and records as applied.
    left: Vec<ResourceKey>,
    failed: bool,
}

impl<'a> SyncExecution<'a> {
    /// Plan the sync of `manifests`, reporting the hooks that are left out.
    pub(super) fn new(
        manifests: &[Value],
        ownership: &'a FieldOwnership,
        concurrency: usize,
        wait: WaitSettings,
        on_event: &mut ApplyEventSink<'_>,
    ) -> Result<Self> {
        let plan = ReleasePlan::of(manifests)?;
        if let Some(finding) = hooks::delete_hook_finding(&plan.ignored) {
            on_event(ApplyEvent::DeleteHooksIgnored { finding: &finding });
        }
        let keys = plan
            .resources
            .iter()
            .map(ResourceKey::from_json_value)
            .collect::<Result<Vec<_>>>()?;
        let slots = hooks::resource_slots(&plan.resources, &plan.hooks);
        let mut steps: Vec<(HookPhase, i64)> = slots
            .iter()
            .copied()
            .chain(
                plan.hooks
                    .iter()
                    .filter(|hook| hook.phase != HookPhase::SyncFail)
                    .map(|hook| (hook.phase, hook.wave)),
            )
            .collect();
        steps.sort_unstable();
        steps.dedup();
        Ok(Self {
            results: (0..keys.len()).map(|_| None).collect(),
            plan,
            keys,
            slots,
            steps,
            ownership,
            concurrency,
            wait,
            registered: RegisteredKinds::default(),
            released_count: 0,
            release_failed_count: 0,
            reports: Vec::new(),
            created: Vec::new(),
            unhealthy: Vec::new(),
            left: Vec::new(),
            failed: false,
        })
    }

    /// Pretend that releasing a field manager failed for `count` objects.
    #[cfg(test)]
    pub(super) fn set_release_failed_count(&mut self, count: usize) {
        self.release_failed_count = count;
    }

    /// Whether the sync failed so far.
    pub(super) fn failed(&self) -> bool {
        self.failed
    }

    /// Run `PreSync` and `Sync`. On failure, the failure steps run too.
    pub(super) async fn run_to_sync(
        &mut self,
        client: &dyn KubeClient,
        on_event: &mut ApplyEventSink<'_>,
    ) -> Result<()> {
        tracing::info!(
            "Applying {} resources and {} hook run(s) in {} step(s) (up to {} at a time)",
            self.keys.len(),
            self.plan.hooks.len(),
            self.steps.len().max(1),
            self.concurrency
        );
        for (phase, wave) in self.steps.clone() {
            if self.failed {
                break;
            }
            if phase == HookPhase::PostSync {
                break;
            }
            self.run_step(client, phase, wave, on_event).await?;
        }
        if self.failed {
            self.run_failure(client, on_event).await?;
        }
        Ok(())
    }

    /// Run `PostSync`, unless the sync failed; then clean up hooks by their policies.
    pub(super) async fn run_post_sync(
        &mut self,
        client: &dyn KubeClient,
        on_event: &mut ApplyEventSink<'_>,
    ) -> Result<()> {
        if self.failed {
            return Ok(());
        }
        for (phase, wave) in self.steps.clone() {
            if phase != HookPhase::PostSync {
                continue;
            }
            if self.failed {
                break;
            }
            self.run_step(client, phase, wave, on_event).await?;
        }
        if self.failed {
            return self.run_failure(client, on_event).await;
        }
        self.delete_created(client, DeletePolicy::HookSucceeded, on_event).await;
        Ok(())
    }

    /// After a failure: `SyncFail` hooks run, then `HookFailed` objects go.
    async fn run_failure(&mut self, client: &dyn KubeClient, on_event: &mut ApplyEventSink<'_>) -> Result<()> {
        let mut waves: Vec<i64> = self.plan.hooks_in(HookPhase::SyncFail).map(|hook| hook.wave).collect();
        waves.dedup();
        for wave in waves {
            self.run_step(client, HookPhase::SyncFail, wave, on_event).await?;
        }
        self.delete_created(client, DeletePolicy::HookFailed, on_event).await;
        Ok(())
    }

    /// Delete the objects this sync created whose delete policies include `policy`.
    /// `HookSucceeded` leaves `SyncFail` hooks, which Argo CD never deletes that way.
    async fn delete_created(&self, client: &dyn KubeClient, policy: DeletePolicy, on_event: &mut ApplyEventSink<'_>) {
        let mut seen: HashSet<&ResourceKey> = HashSet::new();
        for &index in &self.created {
            let task = &self.plan.hooks[index];
            if !task.has_policy(policy)
                || (policy == DeletePolicy::HookSucceeded && task.phase == HookPhase::SyncFail)
                || !seen.insert(&task.key)
            {
                continue;
            }
            let result = delete_hook(client, &task.key, None).await;
            on_event(ApplyEvent::HookDeleted {
                key: &task.key,
                policy,
                result: &result,
            });
        }
    }

    fn later_step(&self, phase: HookPhase, wave: i64) -> bool {
        self.steps.iter().any(|step| *step > (phase, wave))
    }

    /// Apply one step, then wait as the step requires.
    #[allow(clippy::too_many_lines)] // One step in order: apply by kind, report, wait for hooks, wait for workloads.
    async fn run_step(
        &mut self,
        client: &dyn KubeClient,
        phase: HookPhase,
        wave: i64,
        on_event: &mut ApplyEventSink<'_>,
    ) -> Result<()> {
        let mut tasks: Vec<Task> = Vec::new();
        tasks.extend(
            (0..self.keys.len())
                .filter(|index| self.slots[*index] == (phase, wave))
                .map(Task::Resource),
        );
        let step_hooks: Vec<usize> = self
            .plan
            .hooks
            .iter()
            .enumerate()
            .filter(|(_, hook)| hook.phase == phase && hook.wave == wave)
            .map(|(index, _)| index)
            .collect();
        tasks.extend(step_hooks.iter().copied().map(Task::Hook));
        let plan = &self.plan;
        let keys = &self.keys;
        let ownership = self.ownership;
        let concurrency = self.concurrency;
        // The failure path runs in the margin the deadline keeps back.
        let wait = if phase == HookPhase::SyncFail {
            self.wait.failing()
        } else {
            self.wait
        };
        let results = &mut self.results;
        let registered = &mut self.registered;
        let released_count = &mut self.released_count;
        let release_failed_count = &mut self.release_failed_count;
        let reports = &mut self.reports;
        let created = &mut self.created;
        let failed = &mut self.failed;
        let gvk_of = |task: &Task| match *task {
            Task::Resource(index) => &keys[index].gvk,
            Task::Hook(index) => &plan.hooks[index].key.gvk,
        };
        if !step_hooks.is_empty() && tasks.len() > step_hooks.len() {
            // Hooks join the kind order of the resources of their wave; without hooks
            // the order given stands, whatever it is.
            tasks.sort_by(|a, b| ResourceOrdering::compare_kinds(gvk_of(a), gvk_of(b)));
        }
        let batches = ResourceOrdering::apply_batches(tasks.iter().map(gvk_of));
        let mut started: Vec<usize> = Vec::new();
        let mut hook_failures: Vec<(usize, String)> = Vec::new();

        for batch in batches {
            // Kinds of a newly registered group are not resolvable until discovery is
            // refreshed, and a fresh CRD or APIService may not be served the instant its
            // apply returns.
            if tasks[batch.clone()].iter().any(|task| registered.needs(gvk_of(task))) {
                client
                    .refresh_discovery_until_available(
                        &registered.needed_among(tasks[batch.start..].iter().map(gvk_of)),
                    )
                    .await?;
                registered.clear();
            }
            run_concurrently(
                tasks[batch].iter().copied(),
                concurrency,
                |task| async move {
                    match task {
                        Task::Resource(index) => {
                            let (result, release) = apply_manifest(client, &plan.resources[index], ownership).await;
                            Done::Resource(result, release)
                        }
                        Task::Hook(index) => {
                            Done::Hook(create_hook(client, &plan.hooks[index], ownership, &wait).await)
                        }
                    }
                },
                |task, done| match (task, done) {
                    (Task::Resource(index), Done::Resource(result, release)) => {
                        on_event(ApplyEvent::Applied {
                            key: &keys[index],
                            result: &result,
                        });
                        if let (Some(release), Some(manager)) = (release, ownership.released_manager()) {
                            match release {
                                Ok(false) => {}
                                Ok(true) => {
                                    *released_count += 1;
                                    on_event(ApplyEvent::OwnershipReleased {
                                        key: &keys[index],
                                        manager,
                                        result: &Ok(()),
                                    });
                                }
                                Err(err) => {
                                    *release_failed_count += 1;
                                    on_event(ApplyEvent::OwnershipReleased {
                                        key: &keys[index],
                                        manager,
                                        result: &Err(err),
                                    });
                                }
                            }
                        }
                        if result.is_ok() {
                            registered.record(&plan.resources[index]);
                        } else {
                            *failed = true;
                        }
                        results[index] = Some(result);
                    }
                    (Task::Hook(index), Done::Hook(result)) => match result {
                        Ok(_) => {
                            on_event(ApplyEvent::HookStarted {
                                key: &plan.hooks[index].key,
                                phase,
                                wave,
                            });
                            started.push(index);
                            created.push(index);
                        }
                        Err(error) => hook_failures.push((index, error.to_string())),
                    },
                    _ => unreachable!("a task's result is of its own kind"),
                },
            )
            .await;
        }

        // Hooks that could not be created failed; the ones created are waited for.
        let mut finished: Vec<(usize, std::result::Result<(), String>)> = hook_failures
            .into_iter()
            .map(|(index, error)| (index, Err(error)))
            .collect();
        finished.extend(await_hooks(client, &plan.hooks, started.clone(), &wait).await);
        finished.sort_by_key(|(index, _)| *index);
        // However each hook ended, the sync has seen it: the finalizer goes before it is reported.
        for index in started {
            release_hook(client, &plan.hooks[index].key).await;
        }
        for (index, outcome) in finished {
            let task = &plan.hooks[index];
            on_event(ApplyEvent::HookFinished {
                key: &task.key,
                phase,
                wave,
                result: &outcome,
            });
            if outcome.is_err() {
                *failed = true;
            }
            reports.push(HookReport {
                key: task.key.clone(),
                phase,
                wave,
                failure: outcome.err(),
            });
        }

        // The workloads of the step are healthy before a later step starts, and in a
        // sync of several steps before the last one, when it holds hooks, is done.
        // The last step of ordinary resources alone is done once they are applied.
        let waits_for_health = self.later_step(phase, wave) || (self.steps.len() > 1 && !step_hooks.is_empty());
        if !self.failed && waits_for_health {
            let applied: Vec<ResourceKey> = (0..self.keys.len())
                .filter(|index| self.slots[*index] == (phase, wave) && matches!(self.results[*index], Some(Ok(_))))
                .map(|index| self.keys[index].clone())
                .collect();
            let unhealthy = await_healthy(client, applied, &self.wait).await;
            if !unhealthy.is_empty() {
                self.failed = true;
                self.unhealthy.extend(unhealthy);
            }
        }
        Ok(())
    }

    /// Keys the apply leaves alone and still records as applied, such as a Job a TTL removed.
    pub(super) fn leave_recorded(&mut self, left: Vec<ResourceKey>) {
        self.left = left;
    }

    /// What the sync did so far.
    pub(super) fn result(&self, released_manager: Option<&str>) -> ApplyExecutionResult {
        let mut outcomes = Vec::new();
        let mut failed_count = 0;
        let mut resource_keys = Vec::new();
        for (key, result) in self.keys.iter().zip(&self.results) {
            match result {
                Some(Ok(outcome)) => {
                    outcomes.push(outcome.clone());
                    resource_keys.push(key.clone());
                }
                Some(Err(_)) => failed_count += 1,
                // Never reached: an earlier step failed, which fails the sync.
                None => {}
            }
        }
        resource_keys.extend(self.left.iter().cloned());
        let hook_failed_count = self.reports.iter().filter(|report| report.failure.is_some()).count();
        ApplyExecutionResult {
            outcomes,
            failed_count,
            resource_keys,
            released_manager: released_manager.map(str::to_string),
            released_count: self.released_count,
            release_failed_count: self.release_failed_count,
            hooks: self.reports.clone(),
            hook_failed_count,
            unhealthy: self.unhealthy.clone(),
            ignored_hooks: self.plan.ignored.clone(),
            untracked: self.plan.untracked_keys(),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn kind(api_version: &str, kind: &str) -> GroupVersionKind {
        GroupVersionKind::from_api_version_kind(api_version, kind).unwrap()
    }

    /// Catches a hook that is held, or an Argo Workflow, being judged by the wrong rule:
    /// a paused Deployment is waited for rather than failed, and a Workflow follows its
    /// phase, so a hook Workflow is neither passed while it runs nor failed before it ends.
    #[test]
    fn test_hook_progress_waits_for_a_held_workload_and_follows_a_workflow() {
        let progress = |gvk: &GroupVersionKind, live: Value| match hook_progress(gvk, &live) {
            Progress::Done => "done",
            Progress::Running(_) => "running",
            Progress::Failed(_) => "failed",
        };
        let deployment = kind("apps/v1", "Deployment");
        let workflow = kind("argoproj.io/v1alpha1", "Workflow");

        assert_eq!(progress(&deployment, json!({"spec": {"paused": true}})), "running");
        assert_eq!(progress(&workflow, json!({"status": {"phase": "Running"}})), "running");
        assert_eq!(progress(&workflow, json!({"status": {"phase": "Succeeded"}})), "done");
        assert_eq!(progress(&workflow, json!({"status": {"phase": "Error"}})), "failed");
        assert_eq!(
            progress(&kind("v1", "ConfigMap"), json!({})),
            "done",
            "no health check, done once it exists"
        );
    }

    /// Catches a hook created without the finalizer, which a TTL controller could delete
    /// before the sync read it, one that drops finalizers another party set, and one that
    /// adds it twice.
    #[test]
    fn test_a_hook_is_created_with_the_finalizer_once_and_beside_the_others() {
        let bare = json!({"metadata": {"name": "migrate"}});
        assert_eq!(with_finalizer(&bare)["metadata"]["finalizers"], json!([HOOK_FINALIZER]));

        let held = json!({"metadata": {"name": "migrate", "finalizers": ["example.com/other"]}});
        let once = with_finalizer(&held);
        assert_eq!(
            once["metadata"]["finalizers"],
            json!(["example.com/other", HOOK_FINALIZER])
        );
        assert_eq!(with_finalizer(&once), once, "adding it again changes nothing");
        assert!(has_finalizer(&once) && !has_finalizer(&bare));
    }

    /// Catches a wait that reads as often at its end as at its start, and one whose
    /// pauses grow without bound.
    #[tokio::test(start_paused = true)]
    async fn test_the_pauses_of_a_wait_grow_to_five_times_the_poll_interval() {
        let mut pace = Pace::new(&WaitSettings::unbounded());
        let mut pauses = Vec::new();
        for _ in 0..6 {
            pauses.push(pace.next);
            pace.pause().await;
        }

        assert_eq!(
            pauses,
            [2000, 3000, 4500, 6750, 10_000, 10_000].map(Duration::from_millis),
            "growing by half, capped at five times the two seconds"
        );
    }

    /// Catches the wait's own timeout being named when the sync's deadline cut it, which
    /// would send the user to a flag that does not help, and the reverse.
    #[tokio::test(start_paused = true)]
    async fn test_a_wait_ends_at_the_earlier_of_its_timeout_and_the_sync_deadline() {
        let soon = Instant::now() + Duration::from_secs(10);
        let by_deadline = Deadline::new(&WaitSettings {
            timeout: Some(Duration::from_secs(30)),
            ..WaitSettings::until(soon, Duration::ZERO)
        });
        let by_timeout = Deadline::new(&WaitSettings {
            timeout: Some(Duration::from_secs(5)),
            ..WaitSettings::until(soon, Duration::ZERO)
        });

        assert!(by_deadline.explain().contains("time left for the sync"));
        assert!(by_timeout.explain().contains("--hook-timeout"));
        tokio::time::advance(Duration::from_secs(6)).await;
        assert!(by_timeout.passed() && !by_deadline.passed());
        tokio::time::advance(Duration::from_secs(5)).await;
        assert!(by_deadline.passed());
        assert!(!Deadline::new(&WaitSettings::unbounded()).passed(), "no bound, no end");
    }
}
