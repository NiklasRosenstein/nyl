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
//! Releases before this change stored the text instead, under `reason`, in
//! the same `gitops.nyl/v1` document. [`BlockedReason`] still reads that form:
//! [`Blocker::from_stored_text`] types the wording Nyl wrote, once, and keeps
//! any other text as [`Blocker::Other`]. It is the only reader of blocked-reason
//! text, and the next transition that writes the document writes the typed form.

use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};

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
    /// A multi-environment run waits for the producer's new receipt, because
    /// the plan that precedes it would change the producer.
    PlannedChange { producer: ProducerRef },
    /// A multi-environment run waits for a producer whose environment failed
    /// or is itself waiting in this run.
    ProducerUnavailable { producer: ProducerRef },
    /// Anything else, such as a fenced incarnation, which only its text
    /// describes.
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
            | Self::Observation { producer, .. }
            | Self::PlannedChange { producer }
            | Self::ProducerUnavailable { producer } => Some(producer),
            Self::Other { .. } => None,
        }
    }

    /// Type the text that releases before the typed form stored as a blocked
    /// reason. Every blocker it returns renders back to `text`
    /// ([`Self::message`]), so a document shows what it showed before; text
    /// it does not recognize is [`Self::Other`] with the text unchanged.
    pub fn from_stored_text(text: &str) -> Self {
        parse_attestation(text)
            .or_else(|| parse_observation(text))
            .or_else(|| parse_no_current_receipt(text))
            .or_else(|| parse_multi_environment(text))
            .unwrap_or_else(|| Self::other(text))
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
            Self::PlannedChange { producer } => {
                format!("{producer} has a planned change; waiting for its new receipt")
            }
            Self::ProducerUnavailable { producer } => {
                format!("{producer} waits because its producer environment failed or is waiting")
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

/// `attestation <name> of <producer> is pending|failing`.
fn parse_attestation(text: &str) -> Option<Blocker> {
    let rest = text.strip_prefix("attestation ")?;
    let (rest, state) = if let Some(rest) = rest.strip_suffix(" is pending") {
        (rest, AttestationWait::Pending)
    } else {
        (rest.strip_suffix(" is failing")?, AttestationWait::Failing)
    };
    let (name, producer) = rest.rsplit_once(" of ")?;
    Some(Blocker::Attestation {
        producer: ProducerRef::parse(producer)?,
        name: name.to_owned(),
        state,
    })
}

/// `observation <name> of <producer> is pending: <why>`.
fn parse_observation(text: &str) -> Option<Blocker> {
    let rest = text.strip_prefix("observation ")?;
    let (name, rest) = rest.split_once(" of ")?;
    let (producer, why) = rest.split_once(" is pending: ")?;
    if name.contains(' ') {
        return None;
    }
    Some(Blocker::Observation {
        producer: ProducerRef::parse(producer)?,
        name: name.to_owned(),
        why: why.to_owned(),
    })
}

/// `<producer> has no current receipt`.
fn parse_no_current_receipt(text: &str) -> Option<Blocker> {
    let producer = text.strip_suffix(" has no current receipt")?;
    Some(Blocker::no_current_receipt(ProducerRef::parse(producer)?))
}

/// `<environment>/<unit> has a planned change; waiting for its new receipt`
/// and `<environment>/<unit> waits because its producer environment failed or
/// is waiting`: the waits of a multi-environment run, which always name a
/// producer of another environment.
fn parse_multi_environment(text: &str) -> Option<Blocker> {
    let producer = |label: &str| ProducerRef::parse(label).filter(|producer| producer.environment.is_some());
    if let Some(label) = text.strip_suffix(" has a planned change; waiting for its new receipt") {
        return Some(Blocker::PlannedChange {
            producer: producer(label)?,
        });
    }
    let label = text.strip_suffix(" waits because its producer environment failed or is waiting")?;
    Some(Blocker::ProducerUnavailable {
        producer: producer(label)?,
    })
}

/// One blocked location of a desired document and what it waits for.
///
/// It is stored as `{pointer, blocker}`. A document written by a release
/// before the typed form stores `{pointer, reason}` with the text instead,
/// and reads as the blocker [`Blocker::from_stored_text`] makes of it; that
/// form is read, never written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BlockedReason {
    /// Location of the unresolved reference, or empty for a `dependsOn` entry.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub pointer: String,
    pub blocker: Blocker,
}

impl<'de> Deserialize<'de> for BlockedReason {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Stored {
            #[serde(default)]
            pointer: String,
            #[serde(default)]
            blocker: Option<Blocker>,
            /// The text form of releases before the typed one.
            #[serde(default)]
            reason: Option<String>,
        }
        let stored = Stored::deserialize(deserializer)?;
        let blocker = match (stored.blocker, stored.reason) {
            (Some(blocker), None) => blocker,
            (None, Some(text)) => Blocker::from_stored_text(&text),
            (Some(_), Some(_)) => {
                return Err(serde::de::Error::custom(
                    "a blocked reason sets both `blocker` and the older text form `reason`; keep `blocker`",
                ))
            }
            (None, None) => return Err(serde::de::Error::missing_field("blocker")),
        };
        Ok(Self {
            pointer: stored.pointer,
            blocker,
        })
    }
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
                Blocker::PlannedChange {
                    producer: producer("infra/network"),
                },
                "infra/network has a planned change; waiting for its new receipt",
            ),
            (
                Blocker::ProducerUnavailable {
                    producer: producer("infra/network"),
                },
                "infra/network waits because its producer environment failed or is waiting",
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
    /// the reader of stored text relies on.
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

    /// Catches reading the text form of earlier releases wrongly: every text
    /// Nyl wrote before the typed form, including cross-environment
    /// producers, reads back as the typed blocker whose message is that text.
    #[test]
    fn test_from_stored_text_types_every_wording_nyl_wrote() {
        let cases = [
            (
                "network has no current receipt",
                Blocker::no_current_receipt(producer("network")),
            ),
            (
                "infra/network has no current receipt",
                Blocker::no_current_receipt(producer("infra/network")),
            ),
            (
                "attestation healthy of network is pending",
                Blocker::Attestation {
                    producer: producer("network"),
                    name: "healthy".to_owned(),
                    state: AttestationWait::Pending,
                },
            ),
            (
                "attestation healthy of infra/network is failing",
                Blocker::Attestation {
                    producer: producer("infra/network"),
                    name: "healthy".to_owned(),
                    state: AttestationWait::Failing,
                },
            ),
            (
                "infra/network has a planned change; waiting for its new receipt",
                Blocker::PlannedChange {
                    producer: producer("infra/network"),
                },
            ),
            (
                "infra/network waits because its producer environment failed or is waiting",
                Blocker::ProducerUnavailable {
                    producer: producer("infra/network"),
                },
            ),
            (
                "observation ip of infra/kubernetes is pending: waiting for a load balancer of the cluster",
                Blocker::Observation {
                    producer: producer("infra/kubernetes"),
                    name: "ip".to_owned(),
                    why: "waiting for a load balancer of the cluster".to_owned(),
                },
            ),
        ];
        for (text, blocker) in cases {
            assert_eq!(Blocker::from_stored_text(text), blocker, "{text}");
            assert_eq!(blocker.message(), text);
        }
    }

    /// Catches reading that guesses: text that is not a blocker Nyl wrote (a
    /// fenced incarnation, a producer that is no valid reference, an
    /// attestation sentence that does not end as one) stays `Other` with its
    /// text, so it renders as before.
    #[test]
    fn test_from_stored_text_keeps_other_text_unchanged() {
        for text in [
            "network has a planned change; waiting for its new receipt",
            "network waits because its producer environment failed or is waiting",
            "Infra/network has a planned change; waiting for its new receipt",
            "the deleting incarnation of cache (kind Command) is removed first",
            "its retained tombstone of kind Command is dropped first",
            "two words has no current receipt",
            "Infra/network has no current receipt",
            "attestation healthy of two words is pending",
            "attestation healthy of network is unknown",
            "observation ip of network is pending",
            "observation two words of network is pending: why",
            "",
        ] {
            assert_eq!(Blocker::from_stored_text(text), Blocker::other(text), "{text:?}");
            assert_eq!(Blocker::from_stored_text(text).message(), text);
        }
    }

    /// Catches a document that reads both forms ambiguously: the text form of
    /// earlier releases reads as the typed blocker, the typed form reads as
    /// itself, and a reason that sets both or neither is refused.
    #[test]
    fn test_a_blocked_reason_reads_the_typed_form_and_the_text_of_earlier_releases() {
        let typed = BlockedReason::new("/values/host", Blocker::no_current_receipt(producer("infra/network")));
        let from_text: BlockedReason = serde_json::from_value(
            serde_json::json!({"pointer": "/values/host", "reason": "infra/network has no current receipt"}),
        )
        .unwrap();
        assert_eq!(from_text, typed);
        // Written typed, whatever form was read.
        assert_eq!(
            serde_json::to_value(&from_text).unwrap(),
            serde_json::json!({
                "pointer": "/values/host",
                "blocker": {"type": "no-current-receipt", "producer": {"environment": "infra", "unit": "network"}}
            })
        );
        let both = serde_json::json!({"blocker": {"type": "other", "message": "x"}, "reason": "x"});
        assert!(serde_json::from_value::<BlockedReason>(both).is_err());
        assert!(serde_json::from_value::<BlockedReason>(serde_json::json!({"pointer": "/x"})).is_err());
    }
}
