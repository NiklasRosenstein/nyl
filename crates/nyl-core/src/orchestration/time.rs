//! Time as data.
//!
//! Decisions never read the clock: the shell passes the current
//! [`Timestamp`] in, from a [`Clock`](super::ports::Clock). Durations in
//! resources, such as a unit's `timeout`, are written like `30m`, `1h30m`,
//! or `7d`.

use std::fmt;
use std::str::FromStr;

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};

use crate::{CoreError, Result};

/// A point in time, in UTC, serialized as RFC 3339.
pub type Timestamp = chrono::DateTime<chrono::Utc>;

/// A non-negative length of time in whole seconds, written as a sequence of
/// `<integer><unit>` parts with the units `d`, `h`, `m`, and `s`, largest
/// first, such as `90s`, `1h30m`, or `7d`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Duration {
    seconds: u64,
}

impl Duration {
    pub const fn from_secs(seconds: u64) -> Self {
        Self { seconds }
    }

    pub const fn from_mins(minutes: u64) -> Self {
        Self::from_secs(minutes * 60)
    }

    pub const fn as_secs(self) -> u64 {
        self.seconds
    }

    /// The same length as a chrono duration, for timestamp arithmetic,
    /// saturating at chrono's largest duration.
    pub fn to_chrono(self) -> chrono::Duration {
        i64::try_from(self.seconds)
            .ok()
            .and_then(chrono::TimeDelta::try_seconds)
            .unwrap_or(chrono::TimeDelta::MAX)
    }

    /// `at` plus this duration, saturating at the latest representable
    /// time, so no deadline computation can panic.
    pub fn after(self, at: Timestamp) -> Timestamp {
        at.checked_add_signed(self.to_chrono())
            .unwrap_or(chrono::DateTime::<chrono::Utc>::MAX_UTC)
    }

    /// Saturating sum.
    #[must_use]
    pub const fn saturating_add(self, other: Self) -> Self {
        Self::from_secs(self.seconds.saturating_add(other.seconds))
    }
}

const UNITS: [(char, u64); 4] = [('d', 86_400), ('h', 3_600), ('m', 60), ('s', 1)];

impl FromStr for Duration {
    type Err = CoreError;

    fn from_str(value: &str) -> Result<Self> {
        let invalid = || {
            CoreError::config(format!(
                "duration {value:?} must be parts like 90s, 30m, 1h30m, or 7d: an integer and one of d, h, m, s, largest unit first"
            ))
        };
        if value.is_empty() {
            return Err(invalid());
        }
        let mut seconds: u64 = 0;
        let mut digits = String::new();
        let mut last_unit = None;
        for character in value.chars() {
            if character.is_ascii_digit() {
                digits.push(character);
                continue;
            }
            let position = UNITS
                .iter()
                .position(|(unit, _)| *unit == character)
                .ok_or_else(invalid)?;
            if digits.is_empty() || last_unit.is_some_and(|last| position <= last) {
                return Err(invalid());
            }
            let amount: u64 = digits.parse().map_err(|_| invalid())?;
            seconds = amount
                .checked_mul(UNITS[position].1)
                .and_then(|part| seconds.checked_add(part))
                .ok_or_else(invalid)?;
            digits.clear();
            last_unit = Some(position);
        }
        if !digits.is_empty() {
            return Err(invalid());
        }
        Ok(Self { seconds })
    }
}

impl fmt::Display for Duration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.seconds == 0 {
            return formatter.write_str("0s");
        }
        let mut remaining = self.seconds;
        for (unit, size) in UNITS {
            if remaining >= size {
                write!(formatter, "{}{unit}", remaining / size)?;
                remaining %= size;
            }
        }
        Ok(())
    }
}

impl Serialize for Duration {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Duration {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for Duration {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Duration".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "pattern": "^(?=.)([0-9]+d)?([0-9]+h)?([0-9]+m)?([0-9]+s)?$",
            "description": "A duration such as `90s`, `30m`, `1h30m`, or `7d`.",
            "examples": ["30m"]
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_duration_parses_the_contract_forms() {
        assert_eq!("30m".parse::<Duration>().unwrap(), Duration::from_mins(30));
        assert_eq!("1h30m".parse::<Duration>().unwrap(), Duration::from_mins(90));
        assert_eq!("7d".parse::<Duration>().unwrap().as_secs(), 7 * 86_400);
        assert_eq!(Duration::from_secs(3_661).to_string(), "1h1m1s");
        for invalid in ["", "30", "m", "30m1h", "1h1h", "1.5h", "-1m"] {
            assert!(invalid.parse::<Duration>().is_err(), "{invalid}");
        }
    }

    /// Catches a huge authored duration panicking deadline arithmetic.
    #[test]
    fn test_duration_after_saturates_instead_of_panicking() {
        let now = chrono::DateTime::<chrono::Utc>::UNIX_EPOCH;
        assert_eq!(
            Duration::from_secs(u64::MAX).after(now),
            chrono::DateTime::<chrono::Utc>::MAX_UTC
        );
        let huge: Duration = "100000000d".parse().unwrap();
        assert_eq!(huge.after(now), chrono::DateTime::<chrono::Utc>::MAX_UTC);
        assert_eq!(Duration::from_mins(1).after(now), now + chrono::Duration::minutes(1));
    }

    proptest::proptest! {
        #[test]
        fn test_duration_text_round_trips(seconds in 0u64..10_000_000) {
            let duration = Duration::from_secs(seconds);
            proptest::prop_assert_eq!(duration.to_string().parse::<Duration>().unwrap(), duration);
        }
    }
}
