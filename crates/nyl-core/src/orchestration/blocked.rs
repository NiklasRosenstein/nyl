//! Why a desired document is blocked, as data.
//!
//! Contract: [Desired unit](../../../../design/orchestration-core.md#desired-unit)
//! (`readiness`), [References](../../../../design/orchestration-core.md#references),
//! [Attestations](../../../../design/orchestration-core.md#attestations), and
//! [Observed values](../../../../design/orchestration-core.md#observed-values).
//!
//! A blocked document stores each blocker as a [`Blocker`], a data-carrying
//! enum, so every decision that depends on what blocks a unit (the report's
//! stable reasons, the attestation and observation warnings, and whether a
//! blocked consumer can resolve again) reads the typed fields and never the
//! text. The human text is rendered from the enum in one place,
//! [`Blocker::message`], which is also how a table, a `message` field, or a
//! blocked result shows a blocker.
//!
//! State documents before format `gitops.nyl/v2` stored the text instead;
//! [`super::migration`] parses it once, when such a document is read.

use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::ids::{EnvironmentName, UnitName};

/// The producer a blocker names: a unit of the consumer's own environment, or
/// of another one.
///
/// Its [`Display`](fmt::Display) form is how every blocked reason names the
/// producer, `<unit>` or `<environment>/<unit>`, so a unit of another
/// environment is never spelled two ways.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProducerRef {
    /// The producer's environment; absent for a unit of the consumer's own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<EnvironmentName>,
    pub unit: UnitName,
}

impl ProducerRef {
    /// A producer in `environment`, or in the consumer's own environment when
    /// `environment` is `None`.
    pub fn new(environment: Option<EnvironmentName>, unit: UnitName) -> Self {
        Self { environment, unit }
    }

    /// A unit of the consumer's own environment.
    pub fn local(unit: UnitName) -> Self {
        Self::new(None, unit)
    }

    /// Read `<unit>` or `<environment>/<unit>`, the form [`Display`](fmt::Display)
    /// writes; `None` when either name is invalid.
    pub fn parse(label: &str) -> Option<Self> {
        match label.split_once('/') {
            Some((environment, unit)) => Some(Self::new(
                Some(EnvironmentName::new(environment).ok()?),
                UnitName::new(unit).ok()?,
            )),
            None => Some(Self::local(UnitName::new(label).ok()?)),
        }
    }
}

impl fmt::Display for ProducerRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.environment {
            Some(environment) => write!(formatter, "{environment}/{}", self.unit),
            None => write!(formatter, "{}", self.unit),
        }
    }
}

/// Whether a required attestation that does not pass has not been recorded
/// for the producer's current receipt, or has been recorded as failing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum AttestationWait {
    Pending,
    Failing,
}

impl AttestationWait {
    /// The word [`Blocker::message`] ends an attestation reason with.
    pub fn word(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Failing => "failing",
        }
    }
}

/// What one blocked reference or `dependsOn` entry waits for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Blocker {
    /// The producer has no current receipt yet.
    NoCurrentReceipt { producer: ProducerRef },
    /// The producer lacks a required attestation for its current receipt.
    Attestation {
        producer: ProducerRef,
        /// The attestation's name.
        name: String,
        state: AttestationWait,
    },
    /// The producer's observed value has no usable value yet.
    Observation {
        producer: ProducerRef,
        /// The observed value's name.
        name: String,
        /// Why the value is pending, in the driver's words.
        why: String,
    },
    /// Anything else, such as a fenced incarnation or a producer environment
    /// that failed in this run, which only its text describes.
    Other { message: String },
}

impl Blocker {
    pub fn no_current_receipt(producer: ProducerRef) -> Self {
        Self::NoCurrentReceipt { producer }
    }

    pub fn other(message: impl Into<String>) -> Self {
        Self::Other {
            message: message.into(),
        }
    }

    /// The producer this blocker waits for, when it waits for one.
    pub fn producer(&self) -> Option<&ProducerRef> {
        match self {
            Self::NoCurrentReceipt { producer }
            | Self::Attestation { producer, .. }
            | Self::Observation { producer, .. } => Some(producer),
            Self::Other { .. } => None,
        }
    }

    /// The human text of the blocker, the one place it is written.
    pub fn message(&self) -> String {
        match self {
            Self::NoCurrentReceipt { producer } => format!("{producer} has no current receipt"),
            Self::Attestation { producer, name, state } => {
                format!("attestation {name} of {producer} is {}", state.word())
            }
            Self::Observation { producer, name, why } => {
                format!("observation {name} of {producer} is pending: {why}")
            }
            Self::Other { message } => message.clone(),
        }
    }
}

impl fmt::Display for Blocker {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message())
    }
}

/// One blocked location of a desired document and what it waits for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BlockedReason {
    /// Location of the unresolved reference, or empty for a `dependsOn` entry.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub pointer: String,
    pub blocker: Blocker,
}

impl BlockedReason {
    pub fn new(pointer: impl Into<String>, blocker: Blocker) -> Self {
        Self {
            pointer: pointer.into(),
            blocker,
        }
    }

    /// The human text of what blocks this location ([`Blocker::message`]).
    pub fn message(&self) -> String {
        self.blocker.message()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn producer(label: &str) -> ProducerRef {
        ProducerRef::parse(label).unwrap()
    }

    /// Catches a blocker rendering differently from the text states have
    /// always shown (tables, `message` fields, blocked results), and a
    /// cross-environment producer spelled any way but `<environment>/<unit>`.
    #[test]
    fn test_message_renders_every_variant_as_the_text_it_always_had() {
        let cases = [
            (
                Blocker::no_current_receipt(producer("network")),
                "network has no current receipt",
            ),
            (
                Blocker::no_current_receipt(producer("infra/network")),
                "infra/network has no current receipt",
            ),
            (
                Blocker::Attestation {
                    producer: producer("network"),
                    name: "healthy".to_owned(),
                    state: AttestationWait::Pending,
                },
                "attestation healthy of network is pending",
            ),
            (
                Blocker::Attestation {
                    producer: producer("infra/network"),
                    name: "healthy".to_owned(),
                    state: AttestationWait::Failing,
                },
                "attestation healthy of infra/network is failing",
            ),
            (
                Blocker::Observation {
                    producer: producer("kubernetes"),
                    name: "ip".to_owned(),
                    why: "no load balancer address yet".to_owned(),
                },
                "observation ip of kubernetes is pending: no load balancer address yet",
            ),
            (
                Blocker::Observation {
                    producer: producer("infra/kubernetes"),
                    name: "ip".to_owned(),
                    why: "not read yet".to_owned(),
                },
                "observation ip of infra/kubernetes is pending: not read yet",
            ),
            (
                Blocker::other("the deleting incarnation of cache (kind Command) is removed first"),
                "the deleting incarnation of cache (kind Command) is removed first",
            ),
        ];
        for (blocker, text) in cases {
            assert_eq!(blocker.message(), text);
            assert_eq!(blocker.to_string(), text);
            assert_eq!(BlockedReason::new("/values/x", blocker).message(), text);
        }
    }

    /// Catches a producer that cannot be read back from its own label, which
    /// the migration relies on to type the old text.
    #[test]
    fn test_producer_parses_the_label_it_displays() {
        for label in ["network", "infra/network"] {
            assert_eq!(producer(label).to_string(), label);
        }
        for invalid in ["", "a/b/c", "Infra/network", "infra/", "/network", "net work"] {
            assert_eq!(ProducerRef::parse(invalid), None, "{invalid:?}");
        }
    }

    /// Catches a blocker stored in a shape a reader or a schema cannot tell
    /// apart: the state document's form of every variant.
    #[test]
    fn test_blockers_serialize_with_their_type_and_typed_producer() {
        let reason = BlockedReason::new(
            "/dependsOn/0",
            Blocker::Attestation {
                producer: producer("infra/network"),
                name: "healthy".to_owned(),
                state: AttestationWait::Pending,
            },
        );
        let value = serde_json::to_value(&reason).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "pointer": "/dependsOn/0",
                "blocker": {
                    "type": "attestation",
                    "producer": {"environment": "infra", "unit": "network"},
                    "name": "healthy",
                    "state": "pending"
                }
            })
        );
        assert_eq!(serde_json::from_value::<BlockedReason>(value).unwrap(), reason);
    }
}
