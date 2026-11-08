//! Migrations of state documents to their current format version.
//!
//! Contract: [State layout](../../../../design/orchestration-core.md#state-layout),
//! where the version in `apiVersion` is the format version of each kind and a
//! format change adds a version with a migration from the previous one.
//!
//! A document of a kind with an older version is read through its migration
//! and written back in the current version by the next transition that
//! touches it; nothing rewrites state in bulk. Golden files under
//! `crates/nyl-core/tests/golden/state/` keep one document of every version
//! so each migration stays covered.
//!
//! | Kind | From | To | Change |
//! | --- | --- | --- | --- |
//! | `DesiredUnit`, `ObservedUnit` | `gitops.nyl/v1` | `gitops.nyl/v2` | A blocked reason stores a typed [`Blocker`] instead of its text. |
//!
//! [`blocker_from_v1_text`] is the only reader of blocked-reason text: it
//! recovers what the text said from the wording Nyl wrote before the format
//! stored it typed, and keeps what it does not recognize as
//! [`Blocker::Other`], whose message is the text unchanged.

use serde_json::{json, Map, Value};

use super::blocked::{AttestationWait, Blocker, ProducerRef};
use crate::constants::{API_VERSION_GITOPS, STATE_FORMAT_V2};

/// Type the blocked reason that a `gitops.nyl/v1` document stored as `text`.
///
/// Every blocker it returns renders back to `text` ([`Blocker::message`]),
/// so a migrated document shows what it showed before.
pub fn blocker_from_v1_text(text: &str) -> Blocker {
    parse_attestation(text)
        .or_else(|| parse_observation(text))
        .or_else(|| parse_no_current_receipt(text))
        .or_else(|| parse_multi_environment(text))
        .unwrap_or_else(|| Blocker::other(text))
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

/// Migrate a `DesiredUnit` document, as a JSON value, to the current
/// version: a `gitops.nyl/v1` document's blocked reasons become typed.
/// Any other version is left for the reader to accept or reject.
pub(super) fn desired_unit(document: &mut Value) -> Result<(), String> {
    let Some(fields) = document.as_object_mut() else {
        return Ok(());
    };
    if fields.get("apiVersion").and_then(Value::as_str) != Some(API_VERSION_GITOPS) {
        return Ok(());
    }
    if let Some(reasons) = fields
        .get_mut("readiness")
        .and_then(|readiness| readiness.get_mut("reasons"))
        .and_then(Value::as_array_mut)
    {
        reasons
            .iter_mut()
            .filter_map(Value::as_object_mut)
            .for_each(typed_reason);
    }
    fields.insert("apiVersion".to_owned(), json!(STATE_FORMAT_V2));
    Ok(())
}

/// Migrate an `ObservedUnit` document to the current version. Its own fields
/// did not change; a `gitops.nyl/v1` document's embedded desired document
/// migrates when it is read. A `gitops.nyl/v2` document embeds a
/// `gitops.nyl/v2` one, because the transition that writes the observed
/// document writes its desired copy in the same version; one that embeds an
/// older desired document is refused rather than read as current.
pub(super) fn observed_unit(document: &mut Value) -> Result<(), String> {
    let Some(fields) = document.as_object_mut() else {
        return Ok(());
    };
    match fields.get("apiVersion").and_then(Value::as_str) {
        Some(API_VERSION_GITOPS) => {
            fields.insert("apiVersion".to_owned(), json!(STATE_FORMAT_V2));
        }
        Some(STATE_FORMAT_V2) => {
            let embedded = fields.get("desired").and_then(|desired| desired.get("apiVersion"));
            if embedded.and_then(Value::as_str) == Some(API_VERSION_GITOPS) {
                return Err(format!(
                    "a {STATE_FORMAT_V2} ObservedUnit embeds a {API_VERSION_GITOPS} desired document; it embeds a {STATE_FORMAT_V2} one"
                ));
            }
        }
        _ => {}
    }
    Ok(())
}

/// Replace a `reason` text by the typed `blocker`. A reason that is not
/// text is left as it is, for the reader to reject.
fn typed_reason(reason: &mut Map<String, Value>) {
    let Some(text) = reason.get("reason").and_then(Value::as_str) else {
        return;
    };
    let blocker = serde_json::to_value(blocker_from_v1_text(text)).expect("a blocker serializes");
    reason.remove("reason");
    reason.insert("blocker".to_owned(), blocker);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches a migration that types text it should not, or loses what the
    /// text said: every text Nyl wrote before format `gitops.nyl/v2`,
    /// including cross-environment producers, reads back as the typed
    /// blocker whose message is that text.
    #[test]
    fn test_blocker_from_v1_text_types_every_wording_nyl_wrote() {
        let producer = |label: &str| ProducerRef::parse(label).unwrap();
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
            assert_eq!(blocker_from_v1_text(text), blocker, "{text}");
            assert_eq!(blocker.message(), text);
        }
    }

    /// Catches a migration that guesses: text that is not a blocker Nyl
    /// wrote (planned changes, fenced incarnations, a producer that is no
    /// valid reference, an attestation sentence that does not end as one)
    /// stays `Other` with its text, so it renders as before.
    #[test]
    fn test_blocker_from_v1_text_keeps_other_text_unchanged() {
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
            assert_eq!(blocker_from_v1_text(text), Blocker::other(text), "{text:?}");
            assert_eq!(blocker_from_v1_text(text).message(), text);
        }
    }

    /// Catches the version bump losing the rest of the document, or
    /// rewriting a version the migration does not own.
    #[test]
    fn test_desired_unit_migration_types_reasons_and_bumps_only_v1() {
        let mut document = json!({
            "apiVersion": "gitops.nyl/v1", "kind": "DesiredUnit",
            "readiness": {"state": "blocked", "reasons": [
                {"pointer": "/values/host", "reason": "network has no current receipt"},
                {"reason": "its retained tombstone of kind Command is dropped first"}
            ]}
        });
        desired_unit(&mut document).unwrap();
        assert_eq!(
            document,
            json!({
                "apiVersion": "gitops.nyl/v2", "kind": "DesiredUnit",
                "readiness": {"state": "blocked", "reasons": [
                    {"pointer": "/values/host", "blocker": {"type": "no-current-receipt", "producer": {"unit": "network"}}},
                    {"blocker": {"type": "other", "message": "its retained tombstone of kind Command is dropped first"}}
                ]}
            })
        );
        let mut current = document.clone();
        desired_unit(&mut current).unwrap();
        assert_eq!(current, document);
        let mut newer = json!({"apiVersion": "gitops.nyl/v3", "readiness": {"reasons": [{"reason": "x"}]}});
        desired_unit(&mut newer).unwrap();
        assert_eq!(newer["apiVersion"], "gitops.nyl/v3");
        assert_eq!(newer["readiness"]["reasons"][0]["reason"], "x");
    }

    /// Catches a v2 observed document passing as current while it embeds a
    /// desired document of the older format: nothing writes one, so it is
    /// refused; a v1 observed document takes its embedded v1 desired along.
    #[test]
    fn test_observed_unit_migration_refuses_an_older_desired_copy_in_a_current_document() {
        let embedded_v1 = json!({"apiVersion": "gitops.nyl/v2", "desired": {"apiVersion": "gitops.nyl/v1"}});
        let mut current = embedded_v1.clone();
        current["apiVersion"] = json!("gitops.nyl/v2");
        assert!(observed_unit(&mut current).unwrap_err().contains("embeds"));
        let mut old = embedded_v1;
        old["apiVersion"] = json!("gitops.nyl/v1");
        observed_unit(&mut old).unwrap();
        assert_eq!(old["apiVersion"], "gitops.nyl/v2");
    }
}
