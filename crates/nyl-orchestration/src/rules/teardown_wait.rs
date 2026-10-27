//! When a publication's teardown wait after phase 1 ends.
//!
//! Contract: [Kubernetes publication unit](../../../../design/orchestration-core.md#kubernetes-publication-unit),
//! Teardown: `teardownWait` strategies `observe` (Argo CD no longer reports
//! the target's workload Applications within the unit's `timeout`, else
//! uncertain with `applications-remain` or `observation-failed`; default for
//! `mode: observe`), `delay` (`duration` after the phase 1 commit, recorded
//! as not observed), and `manual` (an operator's `--confirm-removed`; the
//! default for `mode: publish`; never keeps a run waiting). A confirmation
//! bound to the run and phase 1 commit, or `--allow-incomplete`, ends any
//! wait once phase 1 is published. A waiting run checks its signal ref about
//! every [`SIGNAL_POLL`].

use nyl_core::orchestration::driver::WaitCompletion;
use nyl_core::orchestration::ids::RunId;
use nyl_core::orchestration::ports::ApplicationsObservation;
use nyl_core::orchestration::state::{Signal, SignalAction, TeardownWaiting};
use nyl_core::orchestration::time::{Duration, Timestamp};
use serde_json::Value;

/// How often a waiting run reads its signal ref.
pub const SIGNAL_POLL: Duration = Duration::from_secs(15);

/// A publication's `teardownWait`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    Observe,
    Delay { duration: Duration },
    Manual,
}

/// The strategy a stored publication spec declares, with the defaults by
/// `mode` (`observe` for `mode: observe`, `manual` for `mode: publish`).
///
/// Validation rejects an unknown strategy and a `delay` without a valid
/// `duration` before a document is stored; a stored document that still
/// carries one reads as `manual`, the only strategy that never ends a wait
/// without an operator.
pub fn strategy(spec: &Value) -> Strategy {
    let declared = spec.pointer("/teardownWait/strategy").and_then(Value::as_str);
    match declared {
        Some("observe") => Strategy::Observe,
        Some("delay") => spec
            .pointer("/teardownWait/duration")
            .and_then(Value::as_str)
            .and_then(|duration| duration.parse::<Duration>().ok())
            .map_or(Strategy::Manual, |duration| Strategy::Delay { duration }),
        Some(_) => Strategy::Manual,
        None => match spec.get("mode").and_then(Value::as_str) {
            Some("observe") => Strategy::Observe,
            _ => Strategy::Manual,
        },
    }
}

/// When a wait ends by itself: `since + duration` for `delay`, `None` for
/// `observe` and `manual`. The run path sets the phase deadline from this,
/// not from `Outcome::Waiting.until`: only the recorded `since` (the
/// phase 1 commit's time, which the run records) is authoritative, so a
/// driver may leave `until` empty.
pub fn ends(strategy: Strategy, waiting: &TeardownWaiting) -> Option<Timestamp> {
    match strategy {
        Strategy::Delay { duration } => Some(duration.after(waiting.since)),
        Strategy::Observe | Strategy::Manual => None,
    }
}

/// Whether `signal` confirms the removal `waiting` waits for in `run`: a
/// `confirm-removed` signal bound to the run waiting now (whose signal ref
/// it was delivered to) and to the recorded phase 1 commit, so a leftover
/// signal never ends another teardown's wait. The waiting run is the one
/// whose signal ref carries the confirmation, which is not always the run
/// that published phase 1 (`waiting.run`): a later run continues a wait
/// recorded by an earlier one. The caller matches the unit, which the
/// waiting record does not name.
pub fn confirms(signal: &Signal, run: &RunId, waiting: &TeardownWaiting) -> bool {
    signal.action == SignalAction::ConfirmRemoved && &signal.run == run && signal.phase1_commit == waiting.phase1_commit
}

/// What deciding a wait reads.
pub struct WaitInputs<'a> {
    pub strategy: Strategy,
    /// The run waiting now, whose signal ref confirmations arrive on.
    pub run: &'a RunId,
    pub waiting: &'a TeardownWaiting,
    pub now: Timestamp,
    /// The unit's teardown deadline (its `timeout` from the teardown's start).
    pub deadline: Timestamp,
    /// The observer's latest answer, for `observe`.
    pub observation: Option<&'a ApplicationsObservation>,
    /// A confirmation sent to the waiting run for this unit, if any arrived.
    pub confirmation: Option<&'a Signal>,
    /// `--allow-incomplete`.
    pub allow_incomplete: bool,
}

/// How a wait continues.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitDecision {
    /// The wait is over: run phase 2.
    Complete(WaitCompletion),
    /// Keep waiting in this run until about `until`, reading signals meanwhile.
    Poll { until: Timestamp },
    /// Stop waiting in this run: the unit stays `tearing-down`, exit 2.
    Stop { reason: String },
    /// The wait failed to establish removal: the teardown is uncertain.
    Uncertain { reason: String },
}

/// Decide how a wait continues.
///
/// - A confirmation bound to the waiting run and phase 1 commit
///   ([`confirms`]) completes any wait as confirmed; `--allow-incomplete`
///   completes it without any claim. A `hold` signal is not a confirmation.
/// - `manual` stops at once: a later `--confirm-removed` applies directly.
/// - `delay` completes as not observed at `since + duration`; before that
///   it polls, never past the deadline, and stops at the deadline (the wait
///   continues in a later run).
/// - `observe` completes once the observer reports no workload
///   Application, polls before the deadline, and is uncertain past it
///   (`applications-remain`) or when the observation fails
///   (`observation-failed`).
pub fn decide(inputs: &WaitInputs<'_>) -> WaitDecision {
    if let Some(signal) = inputs
        .confirmation
        .filter(|signal| confirms(signal, inputs.run, inputs.waiting))
    {
        return WaitDecision::Complete(WaitCompletion::Confirmed {
            by: signal.by.clone(),
            reason: signal.reason.clone(),
            from: signal.from.clone(),
        });
    }
    if inputs.allow_incomplete {
        return WaitDecision::Complete(WaitCompletion::Incomplete);
    }
    let poll = |end: Option<Timestamp>| {
        let until = SIGNAL_POLL.after(inputs.now).min(inputs.deadline);
        WaitDecision::Poll {
            until: end.map_or(until, |end| until.min(end)),
        }
    };
    match inputs.strategy {
        Strategy::Manual => WaitDecision::Stop {
            reason: "teardownWait is manual: an operator confirms the removal with --confirm-removed".to_owned(),
        },
        Strategy::Delay { duration } => {
            let end = duration.after(inputs.waiting.since);
            if inputs.now >= end {
                WaitDecision::Complete(WaitCompletion::Delayed)
            } else if inputs.now >= inputs.deadline {
                WaitDecision::Stop {
                    reason: format!(
                        "the teardownWait delay ends at {}, after this run's deadline",
                        end.to_rfc3339()
                    ),
                }
            } else {
                poll(Some(end))
            }
        }
        Strategy::Observe => match inputs.observation {
            None => WaitDecision::Uncertain {
                reason: "observation-failed: no Argo CD observation was made".to_owned(),
            },
            Some(ApplicationsObservation::Failed(error)) => WaitDecision::Uncertain {
                reason: format!("observation-failed: {error}"),
            },
            Some(ApplicationsObservation::Remaining(applications)) if applications.is_empty() => {
                WaitDecision::Complete(WaitCompletion::Observed)
            }
            Some(ApplicationsObservation::Remaining(applications)) => {
                if inputs.now >= inputs.deadline {
                    WaitDecision::Uncertain {
                        reason: format!("applications-remain: {}", applications.join(", ")),
                    }
                } else {
                    poll(None)
                }
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use nyl_core::orchestration::state::{ApprovalIdentity, SignalKind, StateV1};
    use serde_json::json;

    use super::*;
    use crate::rules::fixtures::{commit, name, run, COMMIT, OTHER_COMMIT, RUN_1, RUN_2};

    fn time(seconds: i64) -> Timestamp {
        chrono::DateTime::parse_from_rfc3339("2026-09-24T10:00:00Z")
            .unwrap()
            .to_utc()
            + chrono::Duration::seconds(seconds)
    }

    fn waiting() -> TeardownWaiting {
        TeardownWaiting {
            phase: "applications-removed".to_owned(),
            reason: "phase 1 published".to_owned(),
            since: time(0),
            until: None,
            phase1_commit: Some(commit(COMMIT)),
            run: run(RUN_1),
        }
    }

    fn signal(action: SignalAction, run_id: &str, phase1: Option<&str>) -> Signal {
        Signal {
            api_version: StateV1,
            kind: SignalKind,
            run: run(run_id),
            action,
            unit: name("kubernetes"),
            phase1_commit: phase1.map(commit),
            by: "alice".to_owned(),
            identity: ApprovalIdentity::Asserted,
            reason: Some("checked by hand".to_owned()),
            at: time(1),
            from: "https://ci.example.com/runs/1".to_owned(),
        }
    }

    fn inputs<'a>(
        strategy: Strategy,
        waiting: &'a TeardownWaiting,
        now: i64,
        observation: Option<&'a ApplicationsObservation>,
    ) -> WaitInputs<'a> {
        WaitInputs {
            strategy,
            run: &waiting.run,
            waiting,
            now: time(now),
            deadline: time(600),
            observation,
            confirmation: None,
            allow_incomplete: false,
        }
    }

    const DELAY: Strategy = Strategy::Delay {
        duration: Duration::from_secs(300),
    };

    /// Catches a publication waiting for the wrong thing: `mode: observe`
    /// must observe and `mode: publish` must wait for an operator unless
    /// `teardownWait` says otherwise.
    #[test]
    fn test_strategy_reads_teardown_wait_with_defaults_by_mode() {
        assert_eq!(strategy(&json!({"mode": "observe"})), Strategy::Observe);
        assert_eq!(strategy(&json!({"mode": "publish"})), Strategy::Manual);
        assert_eq!(strategy(&json!({})), Strategy::Manual);
        assert_eq!(
            strategy(&json!({"mode": "observe", "teardownWait": {"strategy": "delay", "duration": "10m"}})),
            Strategy::Delay {
                duration: Duration::from_mins(10)
            }
        );
        assert_eq!(
            strategy(&json!({"mode": "publish", "teardownWait": {"strategy": "observe"}})),
            Strategy::Observe
        );
        // A stored delay without a usable duration never completes by itself.
        assert_eq!(
            strategy(&json!({"teardownWait": {"strategy": "delay"}})),
            Strategy::Manual
        );
    }

    /// Catches a delay measured from anything but the recorded phase 1 time.
    #[test]
    fn test_only_a_delay_ends_by_itself_from_the_recorded_since() {
        let waiting = waiting();
        assert_eq!(ends(DELAY, &waiting), Some(time(300)));
        assert_eq!(ends(Strategy::Observe, &waiting), None);
        assert_eq!(ends(Strategy::Manual, &waiting), None);
    }

    /// Catches a confirmation that the operator bound to this run and phase 1
    /// commit being ignored, whatever the strategy.
    #[test]
    fn test_a_bound_confirmation_completes_any_wait() {
        let waiting = waiting();
        let confirmation = signal(SignalAction::ConfirmRemoved, RUN_1, Some(COMMIT));
        let remaining = ApplicationsObservation::Remaining(vec!["web".to_owned()]);
        for strategy in [Strategy::Manual, DELAY, Strategy::Observe] {
            let decision = decide(&WaitInputs {
                confirmation: Some(&confirmation),
                ..inputs(strategy, &waiting, 10, Some(&remaining))
            });
            assert_eq!(
                decision,
                WaitDecision::Complete(WaitCompletion::Confirmed {
                    by: "alice".to_owned(),
                    reason: Some("checked by hand".to_owned()),
                    from: "https://ci.example.com/runs/1".to_owned(),
                })
            );
        }
    }

    /// Catches a leftover or misdirected signal ending a wait: a hold, a
    /// confirmation for another run, or one for another phase 1 commit.
    #[test]
    fn test_unbound_signals_and_holds_never_complete_a_wait() {
        let waiting = waiting();
        for other in [
            signal(SignalAction::Hold, RUN_1, None),
            signal(SignalAction::Hold, RUN_1, Some(COMMIT)),
            signal(SignalAction::ConfirmRemoved, RUN_2, Some(COMMIT)),
            signal(SignalAction::ConfirmRemoved, RUN_1, Some(OTHER_COMMIT)),
            signal(SignalAction::ConfirmRemoved, RUN_1, None),
        ] {
            let decision = decide(&WaitInputs {
                confirmation: Some(&other),
                ..inputs(Strategy::Manual, &waiting, 10, None)
            });
            assert!(matches!(decision, WaitDecision::Stop { .. }), "{other:?}: {decision:?}");
        }
    }

    /// Catches `--allow-incomplete` failing to end a wait it is meant to skip.
    #[test]
    fn test_allow_incomplete_completes_without_a_claim() {
        let waiting = waiting();
        let decision = decide(&WaitInputs {
            allow_incomplete: true,
            ..inputs(Strategy::Manual, &waiting, 10, None)
        });
        assert_eq!(decision, WaitDecision::Complete(WaitCompletion::Incomplete));
    }

    /// Catches a manual wait keeping a run waiting.
    #[test]
    fn test_manual_stops_at_once() {
        let waiting = waiting();
        assert!(matches!(
            decide(&inputs(Strategy::Manual, &waiting, 0, None)),
            WaitDecision::Stop { .. }
        ));
    }

    /// Catches a delay ending early, never ending, or waiting past the deadline.
    #[test]
    fn test_delay_polls_then_completes_as_not_observed() {
        let waiting = waiting();
        assert_eq!(
            decide(&inputs(DELAY, &waiting, 10, None)),
            WaitDecision::Poll { until: time(25) }
        );
        assert_eq!(
            decide(&inputs(DELAY, &waiting, 290, None)),
            WaitDecision::Poll { until: time(300) }
        );
        assert_eq!(
            decide(&inputs(DELAY, &waiting, 300, None)),
            WaitDecision::Complete(WaitCompletion::Delayed)
        );
        let long = Strategy::Delay {
            duration: Duration::from_mins(20),
        };
        assert_eq!(
            decide(&inputs(long, &waiting, 595, None)),
            WaitDecision::Poll { until: time(600) }
        );
        assert!(matches!(
            decide(&inputs(long, &waiting, 600, None)),
            WaitDecision::Stop { .. }
        ));
    }

    /// Catches observe completing while Applications remain, or waiting
    /// forever when the observer cannot see them.
    #[test]
    fn test_observe_completes_on_removal_and_is_uncertain_otherwise() {
        let waiting = waiting();
        let gone = ApplicationsObservation::Remaining(Vec::new());
        assert_eq!(
            decide(&inputs(Strategy::Observe, &waiting, 10, Some(&gone))),
            WaitDecision::Complete(WaitCompletion::Observed)
        );
        let remaining = ApplicationsObservation::Remaining(vec!["web".to_owned(), "api".to_owned()]);
        assert_eq!(
            decide(&inputs(Strategy::Observe, &waiting, 10, Some(&remaining))),
            WaitDecision::Poll { until: time(25) }
        );
        assert_eq!(
            decide(&inputs(Strategy::Observe, &waiting, 600, Some(&remaining))),
            WaitDecision::Uncertain {
                reason: "applications-remain: web, api".to_owned()
            }
        );
        let failed = ApplicationsObservation::Failed("no credentials for the Argo CD cluster".to_owned());
        assert_eq!(
            decide(&inputs(Strategy::Observe, &waiting, 10, Some(&failed))),
            WaitDecision::Uncertain {
                reason: "observation-failed: no credentials for the Argo CD cluster".to_owned()
            }
        );
    }

    /// Catches a wait continued by a later run (its record names the run
    /// that published phase 1) ignoring a confirmation delivered to it.
    #[test]
    fn test_a_later_run_continuing_the_wait_accepts_its_confirmation() {
        let waiting = waiting();
        let later = run(RUN_2);
        let confirmation = signal(SignalAction::ConfirmRemoved, RUN_2, Some(COMMIT));
        let decision = decide(&WaitInputs {
            run: &later,
            confirmation: Some(&confirmation),
            ..inputs(Strategy::Manual, &waiting, 10, None)
        });
        assert!(matches!(
            decision,
            WaitDecision::Complete(WaitCompletion::Confirmed { .. })
        ));
    }
}
