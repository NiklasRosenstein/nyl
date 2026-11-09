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
//! - **Waits.** A step does not finish before its hooks completed: a Job or Pod
//!   succeeded, another kind exists. When a later step follows, it also waits until
//!   the workloads the step applied are healthy ([`health`]), as Argo CD does when
//!   a sync has more than one wave or phase. That is why `PostSync` hooks start only
//!   after the `Sync` phase is healthy.
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
use super::hooks::{DeletePolicy, HookPhase, HookTask, ReleasePlan};
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
    /// The pause between two reads of the cluster.
    pub poll_interval: Duration,
}

impl WaitSettings {
    /// What `nyl apply` waits by default: ten minutes per wait, reading every two seconds.
    pub const DEFAULT_TIMEOUT: Duration = Duration::from_mins(10);

    /// No limit, for a caller that bounds the whole sync itself.
    pub const fn unbounded() -> Self {
        Self {
            timeout: None,
            poll_interval: Duration::from_secs(2),
        }
    }
}

impl Default for WaitSettings {
    fn default() -> Self {
        Self {
            timeout: Some(Self::DEFAULT_TIMEOUT),
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

/// A deadline for one wait.
#[derive(Clone, Copy)]
struct Deadline(Option<Instant>);

impl Deadline {
    fn new(wait: &WaitSettings) -> Self {
        Self(wait.timeout.map(|timeout| Instant::now() + timeout))
    }

    fn passed(&self) -> bool {
        self.0.is_some_and(|end| Instant::now() >= end)
    }
}

fn timed_out(wait: &WaitSettings) -> String {
    format!(
        "it did not finish within {}s; raise the wait limit with `--hook-timeout`",
        wait.timeout.unwrap_or_default().as_secs()
    )
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

/// A hook object's progress from its live state: a Job or Pod is done when it
/// succeeded and failed when it did; a workload when it is healthy; any other
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
        Health::Progressing => Progress::Running(why),
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

/// Delete a hook object the way Argo CD does, in the foreground, and, with
/// `wait`, until it is gone. An object that does not exist counts as deleted.
async fn delete_hook(client: &dyn KubeClient, key: &ResourceKey, until_gone: Option<&WaitSettings>) -> Result<()> {
    if read(client, key).await?.is_none() {
        return Ok(());
    }
    let options = DeleteOptions {
        propagation: Some(DeletePropagation::Foreground),
        grace_period_seconds: None,
    };
    client
        .delete_resource_with(&key.gvk, key.namespace.as_deref(), &key.name, &options)
        .await?;
    let Some(wait) = until_gone else {
        return Ok(());
    };
    let deadline = Deadline::new(wait);
    while read(client, key).await?.is_some() {
        if deadline.passed() {
            return Err(NylError::Kubernetes(format!(
                "the previous {key} is still being deleted: {}",
                timed_out(wait)
            )));
        }
        tokio::time::sleep(wait.poll_interval).await;
    }
    Ok(())
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
    let resource: DynamicObject = serde_json::from_value(task.manifest.clone())?;
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
                    finished.push((index, Err(format!("{why}; {}", timed_out(wait)))));
                }
                Progress::Running(_) => still.push(index),
            }
        }
        if still.is_empty() {
            return finished;
        }
        running = still;
        tokio::time::sleep(wait.poll_interval).await;
    }
}

/// Wait until the workloads among `keys` are healthy; returns those that are not,
/// with why. A workload that is gone, such as a Job a TTL removed, is not waited for.
async fn await_healthy(
    client: &dyn KubeClient,
    keys: Vec<ResourceKey>,
    wait: &WaitSettings,
) -> Vec<(ResourceKey, String)> {
    let deadline = Deadline::new(wait);
    let mut unhealthy = Vec::new();
    let mut waiting: Vec<ResourceKey> = keys
        .into_iter()
        .filter(|key| assessed(&key.gvk.group, &key.gvk.kind))
        .collect();
    loop {
        let mut still = Vec::new();
        for key in waiting {
            let Ok(Some(live)) = read(client, &key).await else {
                // Gone, or unreadable: it cannot be judged, and is no ground to hold the sync.
                continue;
            };
            let Some(status) = rollout(&key.gvk.kind, &live) else {
                continue;
            };
            let why = status.why.unwrap_or_default();
            match status.health {
                Health::Healthy => {}
                Health::Degraded => unhealthy.push((key, why)),
                Health::Progressing | Health::Unknown if deadline.passed() => {
                    unhealthy.push((key, format!("{why}; {}", timed_out(wait))));
                }
                Health::Progressing | Health::Unknown => still.push(key),
            }
        }
        if still.is_empty() {
            return unhealthy;
        }
        waiting = still;
        tokio::time::sleep(wait.poll_interval).await;
    }
}

/// A sync in progress. Create it with [`Self::new`], run it to the end of the `Sync`
/// phase with [`Self::run_to_sync`], then with [`Self::run_post_sync`], and read
/// [`Self::result`].
pub(super) struct SyncExecution<'a> {
    plan: ReleasePlan,
    keys: Vec<ResourceKey>,
    waves: Vec<i64>,
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
        if !plan.ignored.is_empty() {
            on_event(ApplyEvent::HooksIgnored { hooks: &plan.ignored });
        }
        let keys = plan
            .resources
            .iter()
            .map(ResourceKey::from_json_value)
            .collect::<Result<Vec<_>>>()?;
        let waves: Vec<i64> = plan.resources.iter().map(super::hooks::wave).collect();
        let mut steps: Vec<(HookPhase, i64)> = waves
            .iter()
            .map(|wave| (HookPhase::Sync, *wave))
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
            waves,
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
        if phase == HookPhase::Sync {
            tasks.extend(
                (0..self.keys.len())
                    .filter(|index| self.waves[*index] == wave)
                    .map(Task::Resource),
            );
        }
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
        let wait = self.wait;
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
        finished.extend(await_hooks(client, &plan.hooks, started, &wait).await);
        finished.sort_by_key(|(index, _)| *index);
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

        // The workloads of the step are healthy before a later step starts.
        if !self.failed && phase == HookPhase::Sync && self.later_step(phase, wave) {
            let applied: Vec<ResourceKey> = (0..self.keys.len())
                .filter(|index| self.waves[*index] == wave && matches!(self.results[*index], Some(Ok(_))))
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
