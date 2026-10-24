//! Test doubles of the core ports: a clock tests set and advance, and
//! identifiers that count up, so scenario runs are reproducible.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use nyl_core::orchestration::ids::{RunId, Uid};
use nyl_core::orchestration::ports::{Clock, IdGenerator};
use nyl_core::orchestration::time::{Duration, Timestamp};

/// A clock that only moves when told to. A wait advances it instead of
/// sleeping, as the `test-clock` build does in the binary.
#[derive(Debug)]
pub struct FakeClock {
    now: Mutex<Timestamp>,
}

impl FakeClock {
    pub fn new(now: Timestamp) -> Self {
        Self { now: Mutex::new(now) }
    }

    pub fn set(&self, now: Timestamp) {
        *self.now.lock().expect("clock lock") = now;
    }

    pub fn advance(&self, duration: Duration) {
        let mut now = self.now.lock().expect("clock lock");
        *now += duration.to_chrono();
    }
}

impl Clock for FakeClock {
    fn now(&self) -> Timestamp {
        *self.now.lock().expect("clock lock")
    }

    fn wait(&self, duration: Duration) {
        self.advance(duration);
    }
}

/// Identifiers in UUIDv7 form whose last group counts up: uids and run IDs
/// from separate counters, so they never collide.
#[derive(Debug, Default)]
pub struct SequentialIds {
    uids: AtomicU64,
    runs: AtomicU64,
}

impl SequentialIds {
    pub fn new() -> Self {
        Self::default()
    }
}

impl IdGenerator for SequentialIds {
    fn uid(&self) -> Uid {
        let next = self.uids.fetch_add(1, Ordering::SeqCst) + 1;
        Uid::new(format!("00000000-0000-7000-8000-{next:012x}")).expect("UUID form")
    }

    fn run_id(&self) -> RunId {
        let next = self.runs.fetch_add(1, Ordering::SeqCst) + 1;
        RunId::new(format!("00000000-0000-7000-9000-{next:012x}")).expect("UUID form")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fake_clock_waits_by_advancing() {
        let start = chrono::DateTime::parse_from_rfc3339("2026-09-24T10:00:00Z")
            .unwrap()
            .to_utc();
        let clock = FakeClock::new(start);
        clock.wait(Duration::from_mins(71));
        assert_eq!(clock.now(), start + chrono::Duration::minutes(71));
    }

    #[test]
    fn test_sequential_ids_never_repeat_across_kinds() {
        let ids = SequentialIds::new();
        let uid = ids.uid();
        let run = ids.run_id();
        assert_ne!(uid.as_str(), run.as_str());
        assert_ne!(ids.uid(), uid);
    }
}
