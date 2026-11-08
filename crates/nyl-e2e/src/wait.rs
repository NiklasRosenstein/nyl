//! Polling with backoff. Every "wait until the cluster is ready" in the suite
//! is a [`Waiter`]: the check runs at once, then after a growing pause, until
//! it succeeds or the timeout passes. A timeout reports what was being waited
//! for and the last status the check saw, which is usually the whole
//! diagnosis (`ImagePullBackOff`, `OutOfSync`, `field-not-set`).

use std::time::{Duration, Instant};

use crate::error::{Error, Result};
use crate::log;

/// The result of one check.
#[derive(Debug, PartialEq, Eq)]
pub enum Poll<T> {
    /// The condition holds.
    Ready(T),
    /// It does not hold yet; the text says what the check saw.
    Pending(String),
}

impl<T> Poll<T> {
    pub fn pending(reason: impl Into<String>) -> Self {
        Self::Pending(reason.into())
    }
}

/// Time as the waiter sees it, so tests run without sleeping.
pub trait Clock {
    /// The time since an arbitrary start.
    fn now(&self) -> Duration;
    fn sleep(&self, duration: Duration);
}

/// The wall clock.
pub struct SystemClock {
    start: Instant,
}

impl SystemClock {
    pub fn new() -> Self {
        Self { start: Instant::now() }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now(&self) -> Duration {
        self.start.elapsed()
    }

    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

/// How often a still-pending wait repeats its status on the console.
const LOG_EVERY: Duration = Duration::from_secs(30);

/// A bounded wait for a condition.
pub struct Waiter {
    what: String,
    timeout: Duration,
    first_pause: Duration,
    longest_pause: Duration,
}

impl Waiter {
    /// Wait for `what` for at most `timeout`, pausing 1s after the first
    /// check and doubling up to 10s.
    pub fn new(what: impl Into<String>, timeout: Duration) -> Self {
        Self {
            what: what.into(),
            timeout,
            first_pause: Duration::from_secs(1),
            longest_pause: Duration::from_secs(10),
        }
    }

    /// Pause `first` after the first check and double up to `longest`.
    pub fn backoff(mut self, first: Duration, longest: Duration) -> Self {
        self.first_pause = first;
        self.longest_pause = longest.max(first);
        self
    }

    /// Run `check` until it is [`Poll::Ready`]. An `Err` from the check is
    /// fatal and ends the wait at once: a check reports a transient problem,
    /// such as an API server that is not up yet, as [`Poll::Pending`].
    pub fn until<T>(&self, check: impl FnMut() -> Result<Poll<T>>) -> Result<T> {
        self.until_with(&SystemClock::new(), check)
    }

    /// [`until`](Self::until) on a given clock.
    pub fn until_with<T>(&self, clock: &dyn Clock, mut check: impl FnMut() -> Result<Poll<T>>) -> Result<T> {
        let start = clock.now();
        let deadline = start + self.timeout;
        let mut pause = self.first_pause;
        let mut attempts = 0_u32;
        let mut last_logged: Option<(Duration, String)> = None;
        loop {
            attempts += 1;
            let reason = match check().map_err(|error| error.context(format!("while waiting for {}", self.what)))? {
                Poll::Ready(value) => {
                    if attempts > 1 {
                        log::line(format!(
                            "done waiting for {} after {}s ({attempts} checks)",
                            self.what,
                            (clock.now() - start).as_secs()
                        ));
                    }
                    return Ok(value);
                }
                Poll::Pending(reason) => reason,
            };
            let now = clock.now();
            if now >= deadline {
                return Err(Error::new(format!(
                    "timed out after {}s waiting for {} ({attempts} checks); last status: {reason}",
                    self.timeout.as_secs(),
                    self.what
                )));
            }
            let changed = last_logged.as_ref().is_none_or(|(_, logged)| *logged != reason);
            let due = last_logged
                .as_ref()
                .is_none_or(|(at, _)| now.saturating_sub(*at) >= LOG_EVERY);
            if changed || due {
                log::line(format!(
                    "waiting for {} ({}s of {}s): {reason}",
                    self.what,
                    (now - start).as_secs(),
                    self.timeout.as_secs()
                ));
                last_logged = Some((now, reason));
            }
            // Never sleep past the deadline: the last check runs at it.
            clock.sleep(pause.min(deadline - now));
            pause = (pause * 2).min(self.longest_pause);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    /// A clock that only moves when the waiter sleeps.
    struct FakeClock {
        now: Cell<Duration>,
        sleeps: std::cell::RefCell<Vec<Duration>>,
    }

    impl FakeClock {
        fn new() -> Self {
            Self {
                now: Cell::new(Duration::ZERO),
                sleeps: Default::default(),
            }
        }
    }

    impl Clock for FakeClock {
        fn now(&self) -> Duration {
            self.now.get()
        }

        fn sleep(&self, duration: Duration) {
            self.sleeps.borrow_mut().push(duration);
            self.now.set(self.now.get() + duration);
        }
    }

    fn secs(seconds: u64) -> Duration {
        Duration::from_secs(seconds)
    }

    #[test]
    fn test_a_condition_that_holds_returns_without_sleeping() {
        let clock = FakeClock::new();
        let value = Waiter::new("the API", secs(60))
            .until_with(&clock, || Ok(Poll::Ready(7)))
            .unwrap();
        assert_eq!(value, 7);
        assert!(clock.sleeps.borrow().is_empty());
    }

    #[test]
    fn test_pauses_double_up_to_the_longest_and_the_value_comes_back() {
        let clock = FakeClock::new();
        let mut checks = 0;
        let value = Waiter::new("the Deployment", secs(600))
            .backoff(secs(1), secs(5))
            .until_with(&clock, || {
                checks += 1;
                Ok(if checks < 6 {
                    Poll::pending(format!("{checks} replicas"))
                } else {
                    Poll::Ready("ready")
                })
            })
            .unwrap();
        assert_eq!(value, "ready");
        assert_eq!(
            *clock.sleeps.borrow(),
            vec![secs(1), secs(2), secs(4), secs(5), secs(5)]
        );
    }

    #[test]
    fn test_a_timeout_names_what_was_awaited_and_the_last_status() {
        let clock = FakeClock::new();
        let mut checks = 0;
        let error = Waiter::new("Application dev-web to be Healthy", secs(20))
            .backoff(secs(5), secs(5))
            .until_with(&clock, || -> Result<Poll<()>> {
                checks += 1;
                Ok(Poll::pending(format!("OutOfSync (check {checks})")))
            })
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("timed out after 20s waiting for Application dev-web to be Healthy"),
            "{error}"
        );
        // The last check runs at the deadline, and its status is the one reported.
        assert!(
            error.contains("(5 checks); last status: OutOfSync (check 5)"),
            "{error}"
        );
        assert_eq!(clock.now(), secs(20));
    }

    #[test]
    fn test_a_pause_never_overshoots_the_deadline() {
        let clock = FakeClock::new();
        let _ = Waiter::new("x", secs(7))
            .backoff(secs(4), secs(4))
            .until_with(&clock, || -> Result<Poll<()>> { Ok(Poll::pending("no")) });
        assert_eq!(*clock.sleeps.borrow(), vec![secs(4), secs(3)]);
        assert_eq!(clock.now(), secs(7));
    }

    #[test]
    fn test_an_error_from_the_check_ends_the_wait_at_once_with_context() {
        let clock = FakeClock::new();
        let mut checks = 0;
        let error = Waiter::new("the registry", secs(60))
            .until_with(&clock, || -> Result<Poll<()>> {
                checks += 1;
                Err(Error::new("docker is not running"))
            })
            .unwrap_err()
            .to_string();
        assert_eq!(checks, 1);
        assert_eq!(error, "while waiting for the registry: docker is not running");
    }
}
