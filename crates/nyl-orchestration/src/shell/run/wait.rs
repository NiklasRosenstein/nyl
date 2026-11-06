//! A run waiting on a publication's teardown after phase 1.
//!
//! Contract: [Kubernetes publication unit](../../../../../design/orchestration-core.md#kubernetes-publication-unit),
//! Teardown and the signal path, and [Runs and leases](../../../../../design/orchestration-core.md#runs-and-leases)
//! item 2 (a teardown wait's deadline is the wait's end). Decisions come
//! from [`crate::rules::teardown_wait`]; this module performs the waiting:
//! it reads the run's signal ref about every 15 seconds through the
//! injected clock, asks the unit's driver ([`Driver::observe_removal`]) for
//! `observe`, shows the remaining time and accepts Enter through the
//! [`OperatorPrompt`](nyl_core::orchestration::ports::OperatorPrompt) when
//! there is one, and stops on cancellation. A `manual` wait returns at once.

use nyl_core::orchestration::driver::{
    Driver, RemovalContext, RemovalObservation, Supported, WaitCompletion, WaitStrategy,
};
use nyl_core::orchestration::ids::{EnvironmentName, RunId, UnitAddress};
use nyl_core::orchestration::state::{DesiredUnit, StateLocation, TeardownWaiting};
use nyl_core::orchestration::time::{Duration, Timestamp};

use super::{execution_credentials, Cancellation, RunContext};
use crate::rules::teardown_wait::{self, confirms, WaitDecision, WaitInputs};
use crate::shell::signal::received;

/// One wait of one unit.
pub(crate) struct WaitRequest<'a> {
    pub location: &'a StateLocation,
    pub environment: &'a EnvironmentName,
    /// The waiting run, whose signal ref confirmations arrive on.
    pub run: &'a RunId,
    /// The unit's stored desired document (its `teardownWait` and target).
    pub unit: &'a DesiredUnit,
    pub waiting: &'a TeardownWaiting,
    /// The end of the unit's teardown deadline; the caller has set the
    /// lease's deadline to cover it.
    pub deadline: Timestamp,
    /// `--allow-incomplete`.
    pub allow_incomplete: bool,
    pub cancellation: &'a Cancellation,
}

/// How a wait ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WaitEnd {
    /// Run phase 2 with this completion.
    Complete(WaitCompletion),
    /// The wait continues beyond this run: the unit stays `tearing-down` (exit 2).
    Stop { reason: String },
    /// Removal could not be established: the teardown is uncertain.
    Uncertain { reason: String },
    /// A hold pauses the teardown before phase 2.
    Held,
    /// SIGINT or SIGTERM.
    Cancelled,
}

/// Wait for `request`'s unit until its wait ends, this run stops waiting,
/// or the run is cancelled.
///
/// Each round reads the run's signals (a `confirm-removed` for this unit),
/// asks the unit's driver what remains for `observe` (a driver that cannot
/// observe reads as `observation-failed`), and asks [`teardown_wait::decide`]. While the rule
/// says to poll, the run shows the remaining time and accepts Enter when it
/// has a terminal ([`RunContext::prompt`]), and otherwise waits on the
/// injected clock until the next poll, which is never past the deadline.
/// Cancellation is checked before and after each wait.
pub(crate) fn wait(context: &RunContext<'_>, request: &WaitRequest<'_>) -> WaitEnd {
    let base = context.base;
    let strategy = strategy(context, request.unit);
    let unit = &request.unit.unit.name;
    loop {
        if request.cancellation.is_cancelled() {
            return WaitEnd::Cancelled;
        }
        let now = base.clock.now();
        let signals = received(base, request.location, request.environment, request.run);
        if signals
            .iter()
            .any(|signal| &signal.unit == unit && signal.action == nyl_core::orchestration::state::SignalAction::Hold)
        {
            return WaitEnd::Held;
        }
        let confirmation = signals
            .iter()
            .find(|signal| &signal.unit == unit && confirms(signal, request.run, request.waiting));
        let observation = (strategy == WaitStrategy::Observe).then(|| observe(context, request, now));
        let decision = teardown_wait::decide(&WaitInputs {
            strategy,
            run: request.run,
            waiting: request.waiting,
            now,
            deadline: request.deadline,
            observation: observation.as_ref(),
            confirmation,
            allow_incomplete: request.allow_incomplete,
        });
        let until = match decision {
            WaitDecision::Complete(completion) => return WaitEnd::Complete(completion),
            WaitDecision::Stop { reason } => return WaitEnd::Stop { reason },
            WaitDecision::Uncertain { reason } => return WaitEnd::Uncertain { reason },
            WaitDecision::Poll { until } => until,
        };
        if let Some(prompt) = context.prompt {
            let end = teardown_wait::ends(strategy, request.waiting)
                .unwrap_or(request.deadline)
                .min(request.deadline);
            let message = format!(
                "{}/{unit} waits for its workload Applications to be removed ({}, {} left); press Enter to confirm they are gone",
                request.environment,
                request.waiting.reason,
                seconds_between(now, end),
            );
            if prompt.confirm(&message, seconds_between(now, until)) {
                return WaitEnd::Complete(WaitCompletion::Confirmed {
                    by: base.requested_by.clone(),
                    reason: None,
                    from: "terminal".to_owned(),
                });
            }
        }
        if request.cancellation.is_cancelled() {
            return WaitEnd::Cancelled;
        }
        // A prompt that already waited out the poll leaves nothing to wait.
        let rest = seconds_between(base.clock.now(), until);
        if rest > Duration::default() {
            base.clock.wait(rest);
        }
    }
}

/// How `unit` waits after phase 1, as its driver reads it from the stored
/// document; a kind without a driver waits for an operator.
pub(crate) fn strategy(context: &RunContext<'_>, unit: &DesiredUnit) -> WaitStrategy {
    let spec = unit.resolved_spec.as_ref().unwrap_or(&unit.spec);
    context
        .drivers
        .get(&unit.unit.api_version, &unit.unit.kind)
        .map_or(WaitStrategy::Manual, |driver| driver.teardown_wait(spec))
}

/// What the unit's driver reports remains of what phase 1 removes. A kind
/// without a driver, a driver that cannot observe, missing credentials, and
/// a failed reading are all `observation-failed`, with the reason unchanged.
fn observe(context: &RunContext<'_>, request: &WaitRequest<'_>, now: Timestamp) -> RemovalObservation {
    let unit = &request.unit.unit;
    let Some(driver) = context.drivers.get(&unit.api_version, &unit.kind) else {
        return RemovalObservation::Failed(format!("no driver serves kind {}", unit.kind));
    };
    observe_with(context, driver, request, now)
}

fn observe_with(
    context: &RunContext<'_>,
    driver: &dyn Driver,
    request: &WaitRequest<'_>,
    now: Timestamp,
) -> RemovalObservation {
    let (env, secrets) = match execution_credentials(context, driver, &request.unit.spec) {
        Ok(credentials) => credentials,
        Err(message) => return RemovalObservation::Failed(message),
    };
    let ctx = RemovalContext {
        unit: UnitAddress::new(request.environment.clone(), request.unit.unit.name.clone()),
        env,
        secrets,
        now,
        deadline: request.deadline,
    };
    match driver.observe_removal(&ctx, request.unit) {
        Ok(Supported::Supported(observation)) => observation,
        Ok(Supported::Unsupported) => RemovalObservation::Failed(format!(
            "kind {} cannot observe the removal of what it published",
            request.unit.unit.kind
        )),
        Err(error) => RemovalObservation::Failed(error.to_string()),
    }
}

/// Whole seconds from `from` to `to`, rounded up; zero when `to` is not later.
fn seconds_between(from: Timestamp, to: Timestamp) -> Duration {
    let delta = to - from;
    let seconds = delta.num_seconds() + i64::from(delta.subsec_nanos() > 0);
    Duration::from_secs(u64::try_from(seconds).unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::Path;
    use std::sync::Mutex;

    use nyl_core::orchestration::driver::{DriverResult, DriverSet, ToolExit, ToolInvocation, ToolRunner};
    use nyl_core::orchestration::ids::{CommitId, UnitName};
    use nyl_core::orchestration::ports::{
        Clock, OperatorPrompt, RenderedSource, SourceRenderer, SourceRepository, SourceResolver, SourceResult,
    };
    use nyl_core::orchestration::state::{DesiredUnitKind, Lifecycle, Readiness, SignalAction, StateV1, UnitIdentity};
    use serde_json::json;

    use super::*;
    use crate::shell::signal::doubles::{context, dev, location, signal, time, SignalStore};
    use crate::testing::FakeClock;

    struct Unused;

    impl SourceResolver for Unused {
        fn resolve_revision(&self, _: &SourceRepository, _: &str) -> SourceResult<CommitId> {
            unimplemented!("waits resolve no sources")
        }
        fn reachable_from(&self, _: &SourceRepository, _: &CommitId, _: &[String]) -> SourceResult<Option<String>> {
            unimplemented!("waits resolve no sources")
        }
        fn worktree(&self, _: &SourceRepository, _: &CommitId, _: &Path) -> SourceResult<()> {
            unimplemented!("waits resolve no sources")
        }
        fn reset_worktree(&self, _: &SourceRepository, _: &CommitId, _: &Path) -> SourceResult<()> {
            unimplemented!("waits resolve no sources")
        }
        fn default_branch(&self, _: &SourceRepository) -> SourceResult<String> {
            unimplemented!("waits resolve no sources")
        }
    }

    impl SourceRenderer for Unused {
        fn render(&self, _: &Path, _: &EnvironmentName, _: &CommitId) -> SourceResult<RenderedSource> {
            unimplemented!("waits render nothing")
        }
    }

    impl ToolRunner for Unused {
        fn run(&self, _: ToolInvocation) -> DriverResult<ToolExit> {
            unimplemented!("waits run no tools")
        }
    }

    /// A test clock that records each wait and runs `hook` with the number
    /// of waits so far, so a test acts while the run waits.
    struct HookedClock {
        clock: FakeClock,
        waits: Mutex<Vec<Duration>>,
        hook: Box<dyn Fn(usize) + Send + Sync>,
    }

    impl HookedClock {
        fn new(hook: impl Fn(usize) + Send + Sync + 'static) -> Self {
            Self {
                clock: FakeClock::new(time(0)),
                waits: Mutex::new(Vec::new()),
                hook: Box::new(hook),
            }
        }

        fn waits(&self) -> Vec<Duration> {
            self.waits.lock().unwrap().clone()
        }
    }

    impl Clock for HookedClock {
        fn now(&self) -> Timestamp {
            self.clock.now()
        }

        fn wait(&self, duration: Duration) {
            self.clock.wait(duration);
            let count = {
                let mut waits = self.waits.lock().unwrap();
                waits.push(duration);
                waits.len()
            };
            (self.hook)(count);
        }
    }

    struct Enter(Mutex<Vec<(String, Duration)>>);

    impl OperatorPrompt for Enter {
        fn confirm(&self, message: &str, timeout: Duration) -> bool {
            self.0.lock().unwrap().push((message.to_owned(), timeout));
            true
        }
    }

    fn phase1() -> CommitId {
        CommitId::new("3e7b9c0a1d2e3f4a5b6c7d8e9f0a1b2c3d4e5f6a").unwrap()
    }

    fn run_id() -> RunId {
        RunId::new("0192f0c4-8d0a-7c3e-9a51-3c1f0e7d2b90").unwrap()
    }

    fn unit(teardown_wait: &serde_json::Value) -> DesiredUnit {
        let spec = json!({"target": "dev", "teardownWait": teardown_wait});
        DesiredUnit {
            api_version: StateV1,
            kind: DesiredUnitKind,
            unit: UnitIdentity {
                environment: dev(),
                name: UnitName::new("kubernetes").unwrap(),
                uid: None,
                api_version: "units.gitops.nyl/v1".to_owned(),
                kind: "KubernetesPublication".to_owned(),
            },
            lifecycle: Lifecycle::Active,
            source_commit: None,
            source_revision: None,
            spec: spec.clone(),
            resolved_spec: Some(spec),
            provenance: BTreeMap::new(),
            execution_key: None,
            fingerprint: None,
            attestations: Vec::new(),
            readiness: Readiness::Ready,
        }
    }

    fn waiting() -> TeardownWaiting {
        TeardownWaiting {
            phase: "applications-removed".to_owned(),
            reason: "phase 1 removed the workload Applications".to_owned(),
            since: time(0),
            until: None,
            phase1_commit: Some(phase1()),
            run: run_id(),
        }
    }

    /// Run `wait` for `unit` with a deadline 10 minutes after phase 1.
    fn wait_for(
        store: &SignalStore,
        clock: &HookedClock,
        unit: &DesiredUnit,
        prompt: Option<&dyn OperatorPrompt>,
        cancellation: &Cancellation,
    ) -> WaitEnd {
        let base = context(store, clock);
        let mut drivers = DriverSet::new();
        drivers.register(std::sync::Arc::new(crate::rules::fixtures::TestDriver {
            kind: "KubernetesPublication",
        }));
        let process_env = BTreeMap::new();
        let run_context = RunContext {
            base: &base,
            sources: &Unused,
            renderer: &Unused,
            drivers: &drivers,
            tools: &Unused,
            workspace: Path::new("unused"),
            process_env: &process_env,
            secrets: None,
            commit_identity: None,
            progress: None,
            prompt,
        };
        let waiting = waiting();
        let location = location();
        let environment = dev();
        let run = run_id();
        wait(
            &run_context,
            &WaitRequest {
                location: &location,
                environment: &environment,
                run: &run,
                unit,
                waiting: &waiting,
                deadline: time(600),
                allow_incomplete: false,
                cancellation,
            },
        )
    }

    /// Catches a confirmation that arrives during a `delay` being missed
    /// until the delay runs out.
    #[test]
    fn test_a_confirmation_during_a_delay_ends_the_wait_early() {
        let store = std::sync::Arc::new(SignalStore::default());
        let sender = store.clone();
        let clock = HookedClock::new(move |waits| {
            if waits == 2 {
                sender.push(signal(SignalAction::Hold, &run_id(), "unrelated", None, 20));
                sender.push(signal(
                    SignalAction::ConfirmRemoved,
                    &run_id(),
                    "web",
                    Some(&phase1()),
                    21,
                ));
                sender.push(signal(
                    SignalAction::ConfirmRemoved,
                    &run_id(),
                    "kubernetes",
                    Some(&phase1()),
                    22,
                ));
            }
        });
        let end = wait_for(
            &store,
            &clock,
            &unit(&json!({"strategy": "delay", "duration": "5m"})),
            None,
            &Cancellation::new(),
        );
        assert_eq!(
            end,
            WaitEnd::Complete(WaitCompletion::Confirmed {
                by: "alice".to_owned(),
                reason: None,
                from: "https://ci.example.com/runs/1".to_owned(),
            })
        );
        assert_eq!(clock.waits(), vec![Duration::from_secs(15); 2]);
    }

    /// Catches a delay that ends early or waits past its end.
    #[test]
    fn test_a_delay_without_signals_ends_as_not_observed_at_its_end() {
        let store = SignalStore::default();
        let clock = HookedClock::new(|_| {});
        let end = wait_for(
            &store,
            &clock,
            &unit(&json!({"strategy": "delay", "duration": "1m"})),
            None,
            &Cancellation::new(),
        );
        assert_eq!(end, WaitEnd::Complete(WaitCompletion::Delayed));
        assert_eq!(clock.now(), time(60));
    }

    /// Catches a SIGINT or SIGTERM leaving the run waiting.
    #[test]
    fn test_cancellation_stops_the_wait() {
        let store = SignalStore::default();
        let cancellation = Cancellation::new();
        let cancel = cancellation.clone();
        let clock = HookedClock::new(move |_| cancel.cancel(crate::shell::run::SIGINT));
        let end = wait_for(
            &store,
            &clock,
            &unit(&json!({"strategy": "delay", "duration": "5m"})),
            None,
            &cancellation,
        );
        assert_eq!(end, WaitEnd::Cancelled);
        assert_eq!(clock.waits().len(), 1);
    }

    /// Catches Enter on the waiting run's terminal not counting as a
    /// confirmation by the person running it.
    #[test]
    fn test_enter_on_the_terminal_confirms_from_the_terminal() {
        let store = SignalStore::default();
        let clock = HookedClock::new(|_| {});
        let prompt = Enter(Mutex::new(Vec::new()));
        let end = wait_for(
            &store,
            &clock,
            &unit(&json!({"strategy": "delay", "duration": "5m"})),
            Some(&prompt),
            &Cancellation::new(),
        );
        assert_eq!(
            end,
            WaitEnd::Complete(WaitCompletion::Confirmed {
                by: "Bob <bob@example.invalid>".to_owned(),
                reason: None,
                from: "terminal".to_owned(),
            })
        );
        let prompts = prompt.0.lock().unwrap();
        assert_eq!(prompts.len(), 1);
        assert!(prompts[0].0.contains("5m left"), "{}", prompts[0].0);
        assert_eq!(prompts[0].1, Duration::from_secs(15));
    }

    /// A unit whose test driver reports `remaining` still existing.
    fn observing(remaining: &[&str]) -> DesiredUnit {
        let mut unit = unit(&json!({"strategy": "observe"}));
        for spec in [&mut unit.spec, unit.resolved_spec.as_mut().unwrap()] {
            spec["remaining"] = json!(remaining);
        }
        unit
    }

    /// Catches an `observe` wait running past the unit's deadline, or
    /// ending as anything but uncertain while the driver still reports
    /// remnants.
    #[test]
    fn test_observe_never_waits_past_the_deadline() {
        let store = SignalStore::default();
        let clock = HookedClock::new(|_| {});
        let end = wait_for(&store, &clock, &observing(&["web"]), None, &Cancellation::new());
        assert_eq!(
            end,
            WaitEnd::Uncertain {
                reason: "applications-remain: web".to_owned()
            }
        );
        assert_eq!(clock.now(), time(600));
    }

    /// Catches an `observe` wait that does not complete as observed once the
    /// driver reports nothing remaining, which would hold the dependencies'
    /// teardown for the whole timeout.
    #[test]
    fn test_observe_completes_when_the_driver_reports_nothing_remaining() {
        let store = SignalStore::default();
        let clock = HookedClock::new(|_| {});
        let end = wait_for(&store, &clock, &observing(&[]), None, &Cancellation::new());
        assert_eq!(end, WaitEnd::Complete(WaitCompletion::Observed));
        assert!(clock.waits().is_empty());
    }

    /// Catches a failed reading or an unsupported driver waiting out the
    /// timeout instead of reporting at once why removal cannot be observed,
    /// with the driver's reason unchanged.
    #[test]
    fn test_observe_that_cannot_read_is_uncertain_at_once() {
        let store = SignalStore::default();
        let reason = |end: WaitEnd| match end {
            WaitEnd::Uncertain { reason } => reason,
            other => panic!("{other:?}"),
        };
        let clock = HookedClock::new(|_| {});
        let mut failing = observing(&[]);
        failing.resolved_spec.as_mut().unwrap()["removalFails"] = json!("Argo CD rejected the token (401)");
        assert_eq!(
            reason(wait_for(&store, &clock, &failing, None, &Cancellation::new())),
            "observation-failed: Argo CD rejected the token (401)"
        );
        let mut unsupported = observing(&[]);
        unsupported.resolved_spec.as_mut().unwrap()["removalUnsupported"] = json!(true);
        assert!(
            reason(wait_for(&store, &clock, &unsupported, None, &Cancellation::new()))
                .starts_with("observation-failed: kind KubernetesPublication cannot observe"),
        );
        assert!(clock.waits().is_empty());
    }

    /// Catches a kind without a driver ending a wait by itself: nothing can
    /// say what it removed, so it waits for an operator.
    #[test]
    fn test_a_kind_without_a_driver_waits_for_an_operator() {
        let store = SignalStore::default();
        let clock = HookedClock::new(|_| {});
        let mut other = observing(&[]);
        other.unit.kind = "Elsewhere".to_owned();
        let end = wait_for(&store, &clock, &other, None, &Cancellation::new());
        assert!(matches!(end, WaitEnd::Stop { .. }), "{end:?}");
    }

    /// Catches a manual wait keeping the run waiting.
    #[test]
    fn test_manual_returns_at_once() {
        let store = SignalStore::default();
        let clock = HookedClock::new(|_| {});
        let end = wait_for(
            &store,
            &clock,
            &unit(&json!({"strategy": "manual"})),
            None,
            &Cancellation::new(),
        );
        assert!(matches!(end, WaitEnd::Stop { .. }), "{end:?}");
        assert!(clock.waits().is_empty());
    }
    /// A hold delivered during a delay pauses it before phase 2, even if
    /// the same poll would otherwise complete the delay or confirm removal.
    #[test]
    fn test_a_hold_during_a_wait_pauses_before_phase_two() {
        let store = std::sync::Arc::new(SignalStore::default());
        let sender = store.clone();
        let clock = HookedClock::new(move |_| {
            sender.push(signal(SignalAction::Hold, &run_id(), "kubernetes", None, 15));
            sender.push(signal(
                SignalAction::ConfirmRemoved,
                &run_id(),
                "kubernetes",
                Some(&phase1()),
                15,
            ));
        });
        assert_eq!(
            wait_for(
                &store,
                &clock,
                &unit(&json!({"strategy": "delay", "duration": "15s"})),
                None,
                &Cancellation::new()
            ),
            WaitEnd::Held
        );
        assert_eq!(clock.waits(), vec![Duration::from_secs(15)]);
    }
}
