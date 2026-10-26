//! The clock the command line constructs.
//!
//! Release builds always use the system clock. Built with the `test-clock`
//! Cargo feature, the binary reads the current time from `NYL_TEST_NOW` and
//! turns waits, such as a `delay` teardown wait, into clock advances, so
//! end-to-end tests of the real binary control time per child process.

use std::sync::{Arc, Mutex};

use nyl_core::orchestration::ports::Clock;
use nyl_core::orchestration::time::{Duration, Timestamp};

/// The system clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        chrono::Utc::now()
    }

    fn wait(&self, duration: Duration) {
        std::thread::sleep(std::time::Duration::from_secs(duration.as_secs()));
    }
}

/// A clock that starts at a given time and moves only by waits.
#[derive(Debug)]
pub struct AdvancingClock {
    now: Mutex<Timestamp>,
}

impl AdvancingClock {
    /// Parse an RFC 3339 time, as `NYL_TEST_NOW` holds it.
    pub fn parse(value: &str) -> Result<Self, String> {
        chrono::DateTime::parse_from_rfc3339(value)
            .map(|time| Self {
                now: Mutex::new(time.to_utc()),
            })
            .map_err(|error| format!("NYL_TEST_NOW {value:?} is not an RFC 3339 time: {error}"))
    }
}

impl Clock for AdvancingClock {
    fn now(&self) -> Timestamp {
        *self.now.lock().expect("clock lock")
    }

    fn wait(&self, duration: Duration) {
        let mut now = self.now.lock().expect("clock lock");
        *now = duration.after(*now);
    }
}

/// The clock of this process: the system clock, or with the `test-clock`
/// feature the time `NYL_TEST_NOW` names, when it is set.
pub fn process_clock() -> Result<Arc<dyn Clock>, String> {
    #[cfg(feature = "test-clock")]
    if let Ok(value) = std::env::var("NYL_TEST_NOW") {
        return AdvancingClock::parse(&value).map(|clock| Arc::new(clock) as Arc<dyn Clock>);
    }
    Ok(Arc::new(SystemClock))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_advancing_clock_turns_waits_into_advances() {
        let clock = AdvancingClock::parse("2026-09-24T10:00:00Z").unwrap();
        let start = clock.now();
        clock.wait(Duration::from_mins(10));
        assert_eq!(clock.now() - start, chrono::Duration::minutes(10));
        assert!(AdvancingClock::parse("yesterday").is_err());
    }
}
