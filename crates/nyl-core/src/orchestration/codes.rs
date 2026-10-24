//! Stable codes for orchestration errors and warnings, each with its fix.
//!
//! The same problem reads the same in every place it appears: tables,
//! `--output json`, transition commit summaries, and status. Scripts match
//! on the code, never on the message.

use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A stable code. Its string form never changes once released.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Code {
    /// Another run holds the environment's lease.
    LeaseHeld,
    /// The run's lease was taken over or deleted; it pushed nothing.
    LeaseLost,
    /// The configured location has no `state.yaml`.
    StateMissing,
    /// The state store cannot be reached or read, or the location cannot be resolved.
    StateUnavailable,
    /// The configured location's `state.yaml` names another environment.
    StateEnvironmentMismatch,
    /// The state moved to another location, which `state.yaml` names.
    StateMoved,
    /// `state.yaml` records another location than the configured one.
    StateLocationMismatch,
    /// A state ref is a full `refs/…` name that forges cannot protect.
    StateUnprotected,
    /// A state ref was rewritten outside Nyl.
    StateNotFastForward,
    /// State refs or coordination refs of environments cannot coexist in Git.
    StateRefConflict,
    /// `state forget` names a unit that is neither retained nor pending teardown.
    StateForgetRefused,
    /// A unit name the environment's state does not know.
    UnitUnknown,
    /// More than one EnvironmentGroup with shared fields selects an environment.
    EnvironmentGroupOverlap,
    /// An environment name no Environment declares.
    EnvironmentUnknown,
}

impl Code {
    pub const ALL: [Self; 14] = [
        Self::LeaseHeld,
        Self::LeaseLost,
        Self::StateMissing,
        Self::StateUnavailable,
        Self::StateEnvironmentMismatch,
        Self::StateMoved,
        Self::StateLocationMismatch,
        Self::StateUnprotected,
        Self::StateNotFastForward,
        Self::StateRefConflict,
        Self::StateForgetRefused,
        Self::UnitUnknown,
        Self::EnvironmentGroupOverlap,
        Self::EnvironmentUnknown,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::LeaseHeld => "NYL-LEASE-HELD",
            Self::LeaseLost => "NYL-LEASE-LOST",
            Self::StateMissing => "NYL-STATE-MISSING",
            Self::StateUnavailable => "NYL-STATE-UNAVAILABLE",
            Self::StateEnvironmentMismatch => "NYL-STATE-ENVIRONMENT-MISMATCH",
            Self::StateMoved => "NYL-STATE-MOVED",
            Self::StateLocationMismatch => "NYL-STATE-LOCATION-MISMATCH",
            Self::StateUnprotected => "NYL-STATE-UNPROTECTED",
            Self::StateNotFastForward => "NYL-STATE-NOT-FAST-FORWARD",
            Self::StateRefConflict => "NYL-STATE-REF-CONFLICT",
            Self::StateForgetRefused => "NYL-STATE-FORGET-REFUSED",
            Self::UnitUnknown => "NYL-UNIT-UNKNOWN",
            Self::EnvironmentGroupOverlap => "NYL-ENVIRONMENT-GROUP-OVERLAP",
            Self::EnvironmentUnknown => "NYL-ENVIRONMENT-UNKNOWN",
        }
    }
}

impl fmt::Display for Code {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl Serialize for Code {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Code {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::ALL
            .into_iter()
            .find(|code| code.as_str() == text)
            .ok_or_else(|| serde::de::Error::custom(format!("unknown code {text:?}")))
    }
}

impl JsonSchema for Code {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Code".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "enum": Self::ALL.map(Code::as_str)
        })
    }
}

/// An error or warning: its code, what happened, and how to fix it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Finding {
    pub code: Code,
    pub message: String,
    pub fix: String,
}

impl Finding {
    pub fn new(code: Code, message: impl Into<String>, fix: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            fix: fix.into(),
        }
    }
}

impl fmt::Display for Finding {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {} ({})", self.code, self.message, self.fix)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_codes_round_trip_and_never_collide() {
        let mut seen = std::collections::BTreeSet::new();
        for code in Code::ALL {
            assert!(seen.insert(code.as_str()), "{code} is used twice");
            assert!(code.as_str().starts_with("NYL-"));
            let json = serde_json::to_value(code).unwrap();
            assert_eq!(serde_json::from_value::<Code>(json).unwrap(), code);
        }
    }
}
