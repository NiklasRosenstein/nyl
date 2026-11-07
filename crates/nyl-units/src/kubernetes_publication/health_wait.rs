//! Waiting inside a reconcile until the unit's target is healthy: the opt-in
//! `observe: {waitHealthy: {timeout}}`.
//!
//! Contract: [Kubernetes publication unit](../../../../design/orchestration-core.md#kubernetes-publication-unit)
//! (`observe`, waiting for health). After a successful publish (and, for a
//! direct target, the apply), the driver polls the unit's
//! [`ReconcilerReader`](super::reconciler::ReconcilerReader) and runs the
//! shared [`evaluate`] until `healthy` passes, with no reader-specific code, so
//! it works for a direct target and for both Argo CD access methods. Then it
//! reports the same `accepted` and `healthy` attestations `nyl attest` would
//! record, which the engine records with the receipt.
//!
//! - [`wait_until_healthy`] owns the rule, over a poll function and the
//!   injected clock ([`Clock`]): it backs off between polls, 2 seconds doubling
//!   to 30, and never waits past the timeout or the unit's deadline. Nothing
//!   in it reads the system clock or sleeps. A clock that does not advance
//!   still ends the wait, after a bounded number of polls.
//! - A reading that is unreadable or whose absences are not provable is never
//!   a pass: it is `Unread`, which retries until the timeout, except for a
//!   credential or declaration the wait cannot fix.
//! - A target the reader cannot see yet (its catalog Application or release
//!   state is not applied) is not healthy yet, not an error: the first deploy
//!   publishes before anyone applies the catalog. The wait keeps polling and,
//!   on the timeout, names the reason and that the catalog must be applied.
//! - A `Degraded` reading ends the wait early only when the reader says it is
//!   terminal (a rollout past its progress deadline, a failed Job); Argo CD's
//!   `Degraded` is never terminal, because controllers retry.
//! - On the timeout the unit fails, retryable, with
//!   `NYL-KUBERNETES-HEALTH-TIMEOUT` naming the worst deployment and its last
//!   health. A failure, not a warning, because the author asked for the wait;
//!   the tree is published and applied, so the next execution publishes
//!   nothing, applies again, and waits again.

use nyl_core::orchestration::driver::{AttestationOutcome, ExecutionContext, Outcome};
use nyl_core::orchestration::ports::Clock;
use nyl_core::orchestration::state::{DesiredUnit, FailureCategory};
use nyl_core::orchestration::time::{Duration, Timestamp};
use nyl_render::gitops::observation::{Expected, GitPublicationHistory, ReconcilerFacts};
use serde_json::Value;

use super::direct::ClusterAccess;
use super::observe::{budget, facts_of};
use super::reconciler::evaluate::{ensure_provable, evaluate, Conclusion, EvaluateError, Evaluation, State};
use super::reconciler::{reader_with, Health, ReadError, ReadErrorKind, ReadRequest, Reading};

/// The code of a wait that ran out of time.
pub const HEALTH_TIMEOUT: &str = "NYL-KUBERNETES-HEALTH-TIMEOUT";
/// The code of a wait that ended early on a health that cannot recover.
pub const HEALTH_DEGRADED: &str = "NYL-KUBERNETES-HEALTH-DEGRADED";

/// The first pause between polls, and the longest.
const FIRST_PAUSE: u64 = 2;
const LONGEST_PAUSE: u64 = 30;
/// The most polls one wait makes. A clock that never advances would otherwise
/// poll forever; a real wait of a day makes under 3000.
const MAX_POLLS: usize = 4096;

/// One reading of the reconciler, judged.
pub enum Poll {
    /// Every deployment runs the publication and is `Healthy`.
    Healthy(Vec<AttestationOutcome>),
    /// Not healthy yet. `worst` names the deployment and its last health;
    /// `terminal` says it will not recover without a change.
    NotYet { worst: String, terminal: bool },
    /// The reconciler could not be read, or its reading proves nothing.
    Unread(ReadError),
}

/// How a wait ended.
pub enum Waited {
    Healthy(Vec<AttestationOutcome>),
    Failed(Outcome),
}

/// Poll until healthy, the timeout, or the deadline passes.
pub fn wait_until_healthy(
    clock: &dyn Clock,
    started: Timestamp,
    deadline: Timestamp,
    timeout: Duration,
    mut poll: impl FnMut() -> Poll,
) -> Waited {
    let end = timeout.after(started).min(deadline);
    let mut pause = FIRST_PAUSE;
    let mut polls = 0usize;
    loop {
        polls += 1;
        let last = match poll() {
            Poll::Healthy(attestations) => return Waited::Healthy(attestations),
            Poll::NotYet { worst, terminal: true } => {
                return Waited::Failed(Outcome::Failed {
                    category: FailureCategory::ToolError,
                    retryable: true,
                    message: format!(
                        "{HEALTH_DEGRADED}: the target is degraded and will not recover without a change: {worst}. \
                         The tree is published and applied; fix what runs and reconcile again"
                    ),
                })
            }
            Poll::NotYet { worst, terminal: false } => worst,
            Poll::Unread(error)
                if matches!(
                    error.kind,
                    ReadErrorKind::Credentials
                        | ReadErrorKind::Declaration
                        | ReadErrorKind::Forbidden
                        | ReadErrorKind::Rejected
                ) =>
            {
                return Waited::Failed(Outcome::Failed {
                    category: FailureCategory::Credentials,
                    retryable: true,
                    message: format!(
                        "the target cannot be waited on, because the reconciler cannot be read: {}",
                        error.message
                    ),
                })
            }
            Poll::Unread(error) => format!("the reconciler could not be read: {}", error.message),
        };
        let now = clock.now();
        if now >= end || polls >= MAX_POLLS {
            let allowed = (end - started).num_seconds().max(0);
            return Waited::Failed(Outcome::Failed {
                category: FailureCategory::Timeout,
                retryable: true,
                message: format!(
                    "{HEALTH_TIMEOUT}: the target was not healthy within {allowed}s: {last}. The tree is published and \
                     applied, so the next execution publishes nothing, applies again, and waits again; raise \
                     observe.waitHealthy.timeout if the rollout needs longer"
                ),
            });
        }
        let remaining = u64::try_from((end - now).num_seconds()).unwrap_or(0).max(1);
        clock.wait(Duration::from_secs(pause.min(remaining)));
        pause = (pause * 2).min(LONGEST_PAUSE);
    }
}

/// `observe.waitHealthy.timeout`, when the unit waits for health.
pub(super) fn timeout_of(spec: &Value) -> Option<Duration> {
    spec.pointer("/observe/waitHealthy/timeout")
        .and_then(Value::as_str)
        .and_then(|timeout| timeout.parse().ok())
}

/// The deployment that falls shortest of the publication, named with its last
/// health, for the message a failed wait gives.
fn worst(evaluation: &Evaluation) -> (String, bool) {
    let rank = |finding: &super::reconciler::evaluate::Finding| match (finding.state, finding.health) {
        (State::Accepted, Some(Health::Healthy)) => 0,
        (State::Accepted, Some(Health::Unknown) | None) => 1,
        (State::Accepted, Some(Health::Progressing)) => 2,
        (State::Accepted, Some(Health::Degraded)) => 3,
        _ => 4,
    };
    let finding = evaluation
        .findings
        .iter()
        .max_by_key(|finding| rank(finding))
        .expect("an evaluation that is not healthy has a finding");
    let health = finding.health.map_or("not reporting health", Health::name);
    let reported = finding
        .reported
        .as_deref()
        .map(|reported| format!(", {reported}"))
        .unwrap_or_default();
    let mut text = format!(
        "{} is {} ({health}{reported})",
        finding.deployment.label(),
        finding.state.name()
    );
    if let Some(reason) = &finding.reason {
        text = format!("{text}: {reason}");
    }
    (text, false)
}

/// Whether `reading` shows the target's anchors and deployments at all yet.
///
/// A catalog Application, an AppProject, or release state that the reader
/// cannot see is the normal state of a target on its first deploy: the tree is
/// published, and whoever applies it (the operator for Argo CD, or the apply
/// that comes before this wait for a direct target) has not made it visible.
/// Inside the wait that is "not healthy yet", and it keeps polling until the
/// timeout, whose message then names the reason and the way out. It is never a
/// pass. What cannot be fixed by waiting still ends the wait at once: a read
/// that fails ([`Poll::Unread`] with the reader's own kind, 401 and 403
/// included) and a declaration the reader cannot read.
fn visible_yet(expected: &Expected, reading: &Reading) -> Result<(), Poll> {
    match ensure_provable(expected, expected.deployments.iter(), reading, Conclusion::Presence) {
        Ok(()) => Ok(()),
        // Only what is not there to see yet waits. A credential that is denied (a
        // `Hidden` anchor or an explicit RBAC refusal, `Forbidden`) or rejected fails
        // at once, as does a read that failed.
        Err(error) if error.kind == ReadErrorKind::NotVisible => Err(Poll::NotYet {
            worst: format!(
                "the reconciler does not show the target yet, which is normal until its catalog or release state is \
                 applied (apply it, and the wait passes): {}",
                error.message
            ),
            terminal: false,
        }),
        Err(error) => Err(Poll::Unread(error)),
    }
}

/// One reading of the unit's reconciler, judged against `commit`, the
/// publication this execution published, exactly as `nyl attest` judges it.
pub(super) fn poll_reconciler(
    ctx: &ExecutionContext<'_>,
    facts: &ReconcilerFacts,
    commit: &str,
    access: &ClusterAccess,
) -> Poll {
    let reader = match reader_with(
        &facts.location,
        &facts.expected.publication,
        &ctx.env,
        budget(ctx.clock.now(), ctx.deadline),
        access,
        None,
    ) {
        Ok(reader) => reader,
        Err(error) => return Poll::Unread(error),
    };
    let reading = match reader.read(&ReadRequest {
        deployments: &facts.expected.deployments,
        anchors: &facts.expected.anchors,
        concluding_absence: false,
    }) {
        Ok(reading) => reading,
        Err(error) => return Poll::Unread(error),
    };
    if let Err(poll) = visible_yet(&facts.expected, &reading) {
        return poll;
    }
    let variable = |name: &str| ctx.env.get(name).cloned();
    let history = match GitPublicationHistory::fetch(&facts.expected.publication, &ctx.project_dir, &variable) {
        Ok(history) => history,
        Err(error) => {
            return Poll::Unread(ReadError::new(
                ReadErrorKind::Unreachable,
                format!("cannot read the publication branch to match what the reconciler runs: {error}"),
            ))
        }
    };
    match evaluate(&facts.expected, commit, &reading, &history) {
        Ok(evaluation) if evaluation.healthy() => Poll::Healthy(evaluation.outcomes(&facts.location.summary())),
        Ok(evaluation) => {
            let (worst, _) = worst(&evaluation);
            let terminal = evaluation.findings.iter().any(|finding| {
                finding.state == State::Accepted
                    && finding.health == Some(Health::Degraded)
                    && reading
                        .deployments
                        .get(&finding.deployment.id)
                        .is_some_and(|observed| observed.terminal)
            });
            Poll::NotYet { worst, terminal }
        }
        Err(EvaluateError::Unprovable(error)) => Poll::Unread(error),
        Err(EvaluateError::History(error)) => Poll::Unread(ReadError::new(
            ReadErrorKind::Unreachable,
            format!("cannot match what the reconciler runs to the publication branch: {error}"),
        )),
    }
}

/// Wait for the unit's target to be healthy after it published `commit`, with
/// the unit's recorded reconciler facts, within the unit's deadline.
pub(super) fn wait_for_target(
    ctx: &ExecutionContext<'_>,
    unit: &DesiredUnit,
    commit: &str,
    timeout: Duration,
    access: &ClusterAccess,
) -> Waited {
    let facts = match facts_of(unit) {
        Ok(facts) => facts,
        Err(reason) => {
            return Waited::Failed(Outcome::Failed {
                category: FailureCategory::ToolError,
                retryable: false,
                message: format!("the unit cannot wait for health: {reason}"),
            })
        }
    };
    wait_until_healthy(ctx.clock, ctx.clock.now(), ctx.deadline, timeout, || {
        poll_reconciler(ctx, &facts, commit, access)
    })
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;

    use chrono::TimeZone;

    use super::*;

    /// A clock that advances when it waits and records each pause.
    struct ScriptedClock {
        now: Cell<Timestamp>,
        pauses: RefCell<Vec<u64>>,
    }

    // The tests are single-threaded; the trait asks for Send + Sync.
    unsafe impl Sync for ScriptedClock {}

    impl Clock for ScriptedClock {
        fn now(&self) -> Timestamp {
            self.now.get()
        }

        fn wait(&self, duration: Duration) {
            self.pauses.borrow_mut().push(duration.as_secs());
            self.now.set(duration.after(self.now.get()));
        }
    }

    fn start() -> Timestamp {
        chrono::Utc.timestamp_opt(1_000_000, 0).unwrap()
    }

    fn run(timeout_secs: u64, deadline_secs: u64, mut polls: VecDeque<Poll>) -> (Waited, Vec<u64>, usize) {
        let clock = ScriptedClock {
            now: Cell::new(start()),
            pauses: RefCell::default(),
        };
        let mut count = 0;
        let waited = wait_until_healthy(
            &clock,
            start(),
            Duration::from_secs(deadline_secs).after(start()),
            Duration::from_secs(timeout_secs),
            || {
                count += 1;
                polls.pop_front().expect("the wait polled more than the script has")
            },
        );
        let pauses = clock.pauses.borrow().clone();
        (waited, pauses, count)
    }

    fn not_yet(text: &str) -> Poll {
        Poll::NotYet {
            worst: text.to_owned(),
            terminal: false,
        }
    }

    fn failed(waited: Waited) -> (FailureCategory, bool, String) {
        match waited {
            Waited::Failed(Outcome::Failed {
                category,
                retryable,
                message,
            }) => (category, retryable, message),
            _ => panic!("the wait should have failed"),
        }
    }

    /// Catches a wait that polls at a fixed rate (hammering the reconciler) or
    /// never settles: pauses back off, doubling from 2 seconds to a 30 second cap,
    /// and the first healthy reading ends the wait with its attestations.
    #[test]
    fn test_polls_back_off_and_the_first_healthy_reading_ends_the_wait() {
        let mut polls: VecDeque<Poll> = (0..7).map(|_| not_yet("web is Progressing")).collect();
        polls.push_back(Poll::Healthy(Vec::new()));
        let (waited, pauses, count) = run(3600, 3600, polls);
        assert!(matches!(waited, Waited::Healthy(_)));
        assert_eq!(pauses, vec![2, 4, 8, 16, 30, 30, 30]);
        assert_eq!(count, 8);
    }

    /// Catches a wait that outlives its timeout, and one that gives up
    /// without a last look: the final poll happens at the end, and the failure
    /// names the worst deployment and its last health under a stable code.
    #[test]
    fn test_the_timeout_fails_retryably_naming_the_worst_deployment() {
        let polls: VecDeque<Poll> = (0..6)
            .map(|_| not_yet("Deployment web/web is accepted (Progressing, 1 of 2 replicas are available)"))
            .collect();
        let (waited, pauses, _) = run(20, 3600, polls);
        let (category, retryable, message) = failed(waited);
        assert_eq!(category, FailureCategory::Timeout);
        assert!(retryable);
        assert!(message.starts_with("NYL-KUBERNETES-HEALTH-TIMEOUT:"), "{message}");
        assert!(
            message.contains("Deployment web/web is accepted (Progressing"),
            "{message}"
        );
        assert_eq!(
            pauses.iter().sum::<u64>(),
            20,
            "the waits end exactly at the timeout: {pauses:?}"
        );
    }

    fn expected() -> Expected {
        use nyl_render::gitops::observation::{
            Anchor, DeploymentRole, ExpectedDeployment, ObservedPublication, Source,
        };
        Expected {
            publication: ObservedPublication {
                url: "https://example.invalid/deploy.git".to_owned(),
                branch: "deploy".to_owned(),
                path_prefix: "dev".to_owned(),
            },
            deployments: vec![ExpectedDeployment {
                id: "argocd/web".to_owned(),
                display: String::new(),
                role: DeploymentRole::Workload,
                path: "dev/web".to_owned(),
                release: None,
                source: Source {
                    repository: "https://example.invalid/deploy.git".to_owned(),
                    path: "dev/web".to_owned(),
                    options: Value::Null,
                },
                locator: Value::Null,
            }],
            anchors: vec![Anchor {
                id: "argocd/dev-catalog".to_owned(),
                locator: Value::Null,
            }],
        }
    }

    fn reading(anchor: super::super::reconciler::Visibility) -> Reading {
        Reading {
            deployments: std::collections::BTreeMap::new(),
            anchors: std::collections::BTreeMap::from([("argocd/dev-catalog".to_owned(), anchor)]),
            absent: std::collections::BTreeMap::from([(
                "argocd/web".to_owned(),
                super::super::reconciler::Visibility::Visible,
            )]),
            proof: super::super::reconciler::Proof::Anchors,
            unprovable: None,
        }
    }

    /// Catches the first deploy failing at once: the catalog Application does
    /// not exist until whoever applies the tree has applied it, after this very
    /// publish. A reading that lacks the catalog is "not healthy yet" and the
    /// wait keeps polling; once the catalog shows, the wait passes. A hidden
    /// anchor is never a pass by itself, and what waiting cannot fix (an
    /// unprovable declaration) still ends the wait at once.
    #[test]
    fn test_a_catalog_that_is_not_visible_yet_is_waited_for_not_failed() {
        use super::super::reconciler::Visibility;
        let hidden = || {
            reading(Visibility::NotYet(
                "the catalog Application argocd/dev-catalog is not in the reading (if the catalog was never applied, \
                 apply it first)"
                    .to_owned(),
            ))
        };
        let expected = expected();
        let poll_of = |reading: &Reading| match visible_yet(&expected, reading) {
            Ok(()) => Poll::Healthy(Vec::new()),
            Err(poll) => poll,
        };
        let clock = ScriptedClock {
            now: Cell::new(start()),
            pauses: RefCell::default(),
        };
        let mut polls = 0;
        let waited = wait_until_healthy(
            &clock,
            start(),
            Duration::from_secs(3600).after(start()),
            Duration::from_secs(600),
            || {
                polls += 1;
                poll_of(&if polls <= 3 {
                    hidden()
                } else {
                    reading(Visibility::Visible)
                })
            },
        );
        assert!(matches!(waited, Waited::Healthy(_)));
        assert_eq!(polls, 4);
        assert_eq!(*clock.pauses.borrow(), vec![2, 4, 8]);

        // Never visible: the timeout names the reason and the way out.
        let clock = ScriptedClock {
            now: Cell::new(start()),
            pauses: RefCell::default(),
        };
        let waited = wait_until_healthy(
            &clock,
            start(),
            Duration::from_secs(3600).after(start()),
            Duration::from_secs(60),
            || poll_of(&hidden()),
        );
        let (category, retryable, message) = failed(waited);
        assert_eq!(category, FailureCategory::Timeout);
        assert!(retryable);
        assert!(message.starts_with("NYL-KUBERNETES-HEALTH-TIMEOUT:"), "{message}");
        assert!(
            message.contains("catalog Application argocd/dev-catalog is not in the reading"),
            "{message}"
        );
        assert!(message.contains("apply it"), "{message}");

        // An explicit denial is no bootstrap state either: the credential may not read,
        // and waiting does not change that, so the wait ends at once.
        let denied = reading(Visibility::Hidden(
            "the credential may not get Application argocd/web".to_owned(),
        ));
        let mut polls = 0;
        let waited = wait_until_healthy(
            &ScriptedClock {
                now: Cell::new(start()),
                pauses: RefCell::default(),
            },
            start(),
            Duration::from_secs(3600).after(start()),
            Duration::from_secs(600),
            || {
                polls += 1;
                poll_of(&denied)
            },
        );
        assert_eq!(polls, 1);
        let (category, _, message) = failed(waited);
        assert_eq!(category, FailureCategory::Credentials);
        assert!(message.contains("may not get Application"), "{message}");

        // A declaration the reader cannot read is no bootstrap state.
        let mut unprovable = reading(Visibility::Visible);
        unprovable.unprovable = Some("the Cluster declares no context".to_owned());
        let Err(Poll::Unread(error)) = visible_yet(&expected, &unprovable) else {
            panic!("an unprovable declaration is unread");
        };
        assert_eq!(error.kind, ReadErrorKind::Declaration);
    }

    /// Catches a clock that never advances turning the wait into an endless
    /// poll loop: the poll count is bounded, and the wait ends as a timeout.
    #[test]
    fn test_a_clock_that_never_advances_still_ends_the_wait() {
        struct Stuck;
        impl Clock for Stuck {
            fn now(&self) -> Timestamp {
                start()
            }
            fn wait(&self, _: Duration) {}
        }
        let hour = Duration::from_secs(3600);
        let waited = wait_until_healthy(&Stuck, start(), hour.after(start()), hour, || {
            not_yet("web is Progressing")
        });
        assert_eq!(failed(waited).0, FailureCategory::Timeout);
    }

    /// Catches a wait that runs past the unit's own deadline when its timeout
    /// is longer: the deadline bounds it.
    #[test]
    fn test_the_units_deadline_bounds_the_wait() {
        let polls: VecDeque<Poll> = (0..10).map(|_| not_yet("web is Progressing")).collect();
        let (waited, pauses, _) = run(3600, 10, polls);
        assert_eq!(failed(waited).0, FailureCategory::Timeout);
        assert_eq!(pauses.iter().sum::<u64>(), 10);
    }

    /// Catches a terminal Degraded waiting out the timeout, and a transient one
    /// (Argo CD's) failing early.
    #[test]
    fn test_only_a_terminal_degraded_reading_fails_early() {
        let polls = VecDeque::from([Poll::NotYet {
            worst: "Deployment web/web: its rollout exceeded its progress deadline".to_owned(),
            terminal: true,
        }]);
        let (waited, pauses, count) = run(3600, 3600, polls);
        let (_, retryable, message) = failed(waited);
        assert!(retryable);
        assert!(message.starts_with("NYL-KUBERNETES-HEALTH-DEGRADED:"), "{message}");
        assert!(pauses.is_empty());
        assert_eq!(count, 1);
        let transient: VecDeque<Poll> = (0..3).map(|_| not_yet("app is Degraded")).collect();
        let mut transient = transient;
        transient.push_back(Poll::Healthy(Vec::new()));
        assert!(matches!(run(3600, 3600, transient).0, Waited::Healthy(_)));
    }

    /// Catches an unreadable reconciler counting as a pass, and a failure the
    /// wait cannot fix (a missing credential) being retried until the timeout.
    #[test]
    fn test_an_unread_reconciler_is_never_a_pass() {
        let unreachable = ReadError::new(ReadErrorKind::Unreachable, "connection refused");
        let polls = VecDeque::from([
            Poll::Unread(unreachable.clone()),
            Poll::Unread(unreachable.clone()),
            Poll::Unread(unreachable),
        ]);
        let (waited, _, _) = run(6, 3600, polls);
        let (category, _, message) = failed(waited);
        assert_eq!(category, FailureCategory::Timeout);
        assert!(
            message.contains("the reconciler could not be read: connection refused"),
            "{message}"
        );
        let credentials = VecDeque::from([Poll::Unread(ReadError::new(
            ReadErrorKind::Credentials,
            "KUBECONFIG is not admitted",
        ))]);
        let (waited, pauses, count) = run(3600, 3600, credentials);
        let (category, _, message) = failed(waited);
        assert_eq!(category, FailureCategory::Credentials);
        assert!(message.contains("KUBECONFIG is not admitted"));
        assert!(pauses.is_empty());
        assert_eq!(count, 1);
    }

    /// Catches a configured wait being ignored, and a malformed one counting.
    #[test]
    fn test_the_timeout_is_read_from_the_spec() {
        let spec = serde_json::json!({"observe": {"waitHealthy": {"timeout": "5m"}}});
        assert_eq!(timeout_of(&spec), Some(Duration::from_mins(5)));
        assert_eq!(timeout_of(&serde_json::json!({"observe": {}})), None);
    }
}
