//! What a consumer requires of its producer beyond a current receipt.
//!
//! Contract: [Units](../../../../design/orchestration-core.md#units)
//! (`dependsOn` entries and `fromUnit` references with `evidence` and
//! `attestations`), [References](../../../../design/orchestration-core.md#references),
//! and [Attestations](../../../../design/orchestration-core.md#attestations).
//!
//! One module owns the rule for every consumer, whichever field states it:
//! a `dependsOn` entry, an output, artifact, or observation `fromUnit`, in
//! this environment or another declared one. Nothing here knows a unit kind
//! or an attestation by name; the names come from what the producer
//! declares.
//!
//! - **Declared.** A producer declares the attestations its driver reports
//!   for its resolved spec ([`DesiredUnit::attestations`]) and the names in
//!   its `spec.attestations`, including those with `from: external`.
//! - **Required.** `attestations: [a, b]` requires those names, and every one
//!   must be declared (naming another is a resolution error, so a typo never
//!   waits forever). Without names, `evidence: attested` requires every
//!   declared attestation, and a producer that declares none is satisfied by
//!   its current receipt; `published` requires none.
//! - **External.** A name declared only with `from: external` can be
//!   recorded by nothing until `nyl attest --name … --result …` ships (M6),
//!   so a requirement that would include one, by name or through `evidence:
//!   attested`, is a resolution error too ([`RequirementError::External`],
//!   [`Code::AttestationExternalUnsupported`]) instead of a consumer that
//!   waits forever. A name the driver also reports, or that another entry
//!   declares without `from: external`, is not external-only.
//! - **Standing.** An attestation counts only for the receipt it names: the
//!   recorded `executionKey` equals the current receipt's and, when the record
//!   names an incarnation, its uid equals the producer's. A required name
//!   with no such record is `pending` (never attested, or attested for a
//!   receipt that was replaced), one recorded as `fail` is `failing`, and
//!   either keeps the consumer `blocked`, exit 2, until a newer attestation of
//!   the current receipt passes. Age plays no part: a dependent is satisfied
//!   by a recorded pass for the current receipt, and `maxAttestationAge`
//!   belongs to promotion paths.
//! - **Reasons.** A consumer's blocker is [`Blocker::Attestation`], which
//!   names the producer (with its environment across environments), the
//!   attestation, and whether it is pending or failing; its text reads
//!   `attestation <name> of <producer> is pending` or `… is failing`. Reports
//!   show the stable reasons [`ATTESTATION_PENDING`] and
//!   [`ATTESTATION_FAILING`] with the producer, and [`warnings`] gives the
//!   warnings ([`Code::AttestationPending`], [`Code::AttestationFailing`]),
//!   all from the typed fields.

use std::collections::{BTreeMap, BTreeSet};

use nyl_core::orchestration::blocked::{AttestationWait, Blocker};
use nyl_core::orchestration::codes::{Code, Finding};
use nyl_core::orchestration::ids::UnitName;
use nyl_core::orchestration::state::{
    Attestation, AttestationResult, BlockedReason, DesiredUnit, ObservedUnit, Receipt,
};
use nyl_core::resources::units::Evidence;
use serde_json::Value;

/// The stable `blockedBy` reason of a consumer waiting for an attestation
/// that is not recorded for its producer's current receipt.
pub const ATTESTATION_PENDING: &str = "attestation-pending";
/// The stable `blockedBy` reason of a consumer waiting for an attestation
/// that is recorded as failing for its producer's current receipt.
pub const ATTESTATION_FAILING: &str = "attestation-failing";

/// The stable reason reports show for an attestation that does not pass.
pub fn stable(wait: AttestationWait) -> &'static str {
    match wait {
        AttestationWait::Pending => ATTESTATION_PENDING,
        AttestationWait::Failing => ATTESTATION_FAILING,
    }
}

/// The attestation names a producer declares: those its driver reports and
/// those its `spec.attestations` names.
pub fn declared(producer: &DesiredUnit) -> BTreeSet<String> {
    let mut names: BTreeSet<String> = producer.attestations.iter().cloned().collect();
    if let Some(Value::Array(entries)) = producer.spec.get("attestations") {
        names.extend(
            entries
                .iter()
                .filter_map(|entry| entry.get("name").and_then(Value::as_str))
                .map(str::to_owned),
        );
    }
    names
}

/// Why a requirement cannot be met by anything: a resolution error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequirementError {
    /// A name the producer does not declare.
    Undeclared(String),
    /// A name only an external source supplies, which Nyl cannot record yet.
    External(String),
}

/// The fix a resolution error of this kind carries; a `nyl validate` finding
/// names the producer's attestations in its fix instead.
impl RequirementError {
    pub fn fix(&self) -> &'static str {
        match self {
            Self::Undeclared(_) => {
                "require an attestation the producer declares, or declare it in the producer's `spec.attestations`"
            }
            Self::External(_) => {
                "external attestations cannot be recorded yet (`nyl attest --name … --result …` arrives with promotion, M6): require only the names the producer's driver observes"
            }
        }
    }

    /// The stable code the error is reported with.
    pub fn code(&self) -> Code {
        match self {
            Self::Undeclared(_) => Code::AttestationUndeclared,
            Self::External(_) => Code::AttestationExternalUnsupported,
        }
    }
}

impl std::fmt::Display for RequirementError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Undeclared(message) | Self::External(message) => formatter.write_str(message),
        }
    }
}

/// The declared names that only an external source supplies: declared in
/// `spec.attestations` with `from: external`, and not also reported by the
/// driver or declared there without it.
pub fn external_only(producer: &DesiredUnit) -> BTreeSet<String> {
    let mut external = BTreeSet::new();
    let mut observable: BTreeSet<String> = producer.attestations.iter().cloned().collect();
    if let Some(Value::Array(entries)) = producer.spec.get("attestations") {
        for entry in entries {
            let Some(name) = entry.get("name").and_then(Value::as_str) else {
                continue;
            };
            if entry.get("from").and_then(Value::as_str) == Some("external") {
                external.insert(name.to_owned());
            } else {
                observable.insert(name.to_owned());
            }
        }
    }
    external.retain(|name| !observable.contains(name));
    external
}

/// The names a requirement on `producer` can be met by today: declared and
/// not external-only.
pub fn recordable(producer: &DesiredUnit) -> BTreeSet<String> {
    let external = external_only(producer);
    declared(producer)
        .into_iter()
        .filter(|name| !external.contains(name))
        .collect()
}

fn external_error(names: &[String], producer: &DesiredUnit) -> RequirementError {
    let unit = &producer.unit.name;
    let recordable = recordable(producer);
    let others = if recordable.is_empty() {
        format!("{unit} declares no attestation its driver observes")
    } else {
        format!(
            "{unit}'s driver observes {}",
            recordable.into_iter().collect::<Vec<_>>().join(", ")
        )
    };
    RequirementError::External(format!(
        "requires attestation {} of unit {unit}, which only an external source supplies; Nyl cannot record external attestations yet, so the consumer would wait forever ({others})",
        names
            .iter()
            .map(|name| format!("{name:?}"))
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

/// The attestation names a consumer requires of `producer`, or why the
/// requirement is a resolution error: a name the producer does not declare,
/// or one only an external source supplies.
pub fn required(evidence: Evidence, named: &[String], producer: &DesiredUnit) -> Result<Vec<String>, RequirementError> {
    let declared = declared(producer);
    let external = external_only(producer);
    let selected: Vec<String> = if named.is_empty() {
        match evidence {
            Evidence::Published => Vec::new(),
            Evidence::Attested => declared.iter().cloned().collect(),
        }
    } else {
        let mut selected = Vec::new();
        for name in named {
            if !declared.contains(name) {
                let unit = &producer.unit.name;
                return Err(RequirementError::Undeclared(if declared.is_empty() {
                    format!("requires attestation {name:?} of unit {unit}, which declares none")
                } else {
                    format!(
                        "requires attestation {name:?} of unit {unit}, which declares {}",
                        declared.iter().cloned().collect::<Vec<_>>().join(", ")
                    )
                }));
            }
            if !selected.contains(name) {
                selected.push(name.clone());
            }
        }
        selected
    };
    let blocking: Vec<String> = selected
        .iter()
        .filter(|name| external.contains(*name))
        .cloned()
        .collect();
    if blocking.is_empty() {
        Ok(selected)
    } else {
        Err(external_error(&blocking, producer))
    }
}

/// How one required attestation stands for the producer's current receipt,
/// `None` when it passes.
fn waiting(
    name: &str,
    producer: &DesiredUnit,
    receipt: &Receipt,
    recorded: Option<&BTreeMap<String, Attestation>>,
) -> Option<AttestationWait> {
    let attestation = recorded
        .and_then(|recorded| recorded.get(name))
        .filter(|attestation| attestation.execution_key == receipt.execution_key.as_str())
        .filter(|attestation| {
            attestation
                .unit
                .as_ref()
                .is_none_or(|subject| Some(&subject.uid) == producer.unit.uid.as_ref())
        });
    match attestation.map(|attestation| attestation.result) {
        Some(AttestationResult::Pass) => None,
        Some(AttestationResult::Fail) => Some(AttestationWait::Failing),
        None => Some(AttestationWait::Pending),
    }
}

/// The required attestations of `producer`'s current receipt that do not
/// pass, in the order they are required.
pub fn unmet(
    required: &[String],
    producer: &DesiredUnit,
    observed: &ObservedUnit,
    recorded: Option<&BTreeMap<String, Attestation>>,
) -> Vec<(String, AttestationWait)> {
    let Some(receipt) = &observed.receipt else {
        return Vec::new();
    };
    required
        .iter()
        .filter_map(|name| waiting(name, producer, receipt, recorded).map(|waiting| (name.clone(), waiting)))
        .collect()
}

/// The warnings of a blocked unit that waits for attestations: each names
/// the attestation and what to do about it. The producer reads
/// `<environment>/<unit>` across environments.
pub fn warnings(unit: &UnitName, environment: &str, reasons: &[BlockedReason]) -> Vec<Finding> {
    reasons
        .iter()
        .filter_map(|blocked| match &blocked.blocker {
            Blocker::Attestation { producer, name, state } => Some((producer, name, *state)),
            _ => None,
        })
        .map(|(producer, name, state)| {
            match state {
                AttestationWait::Pending => Finding::new(
                    Code::AttestationPending,
                    format!(
                        "unit {unit} waits for attestation {name} of {producer}: none is recorded for its current receipt"
                    ),
                    format!(
                        "record it, such as with `nyl attest -e {environment}` for a kind that attests itself, then run nyl reconcile again"
                    ),
                ),
                AttestationWait::Failing => Finding::new(
                    Code::AttestationFailing,
                    format!(
                        "unit {unit} waits for attestation {name} of {producer}, which is recorded as failing for its current receipt"
                    ),
                    format!(
                        "fix what {producer} reported, then run `nyl attest -e {environment}` and nyl reconcile again; a newer passing attestation unblocks {unit}"
                    ),
                ),
            }
        })
        .collect()
}

/// The attestation sets that a new observed state leaves standing: a unit
/// whose receipt was replaced or removed drops the attestations of the
/// receipt it replaced, in the same commit, because they no longer describe
/// it (owner of this decision as for drift records:
/// `rules::drift::drift_changes`). `None` removes a unit's whole set.
pub fn attestation_changes(
    stored: &BTreeMap<UnitName, BTreeMap<String, Attestation>>,
    observed: &BTreeMap<UnitName, ObservedUnit>,
) -> BTreeMap<UnitName, Option<BTreeMap<String, Attestation>>> {
    let mut changes = BTreeMap::new();
    for (unit, set) in stored {
        let receipt = observed.get(unit).and_then(|observed| observed.receipt.as_ref());
        let kept: BTreeMap<String, Attestation> = set
            .iter()
            .filter(|(_, attestation)| {
                receipt.is_some_and(|receipt| attestation.execution_key == receipt.execution_key.as_str())
            })
            .map(|(name, attestation)| (name.clone(), attestation.clone()))
            .collect();
        if kept.len() != set.len() {
            changes.insert(unit.clone(), (!kept.is_empty()).then_some(kept));
        }
    }
    changes
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::rules::fixtures::{desired, key, name, observed, receipt};
    use nyl_core::orchestration::blocked::ProducerRef;
    use nyl_core::orchestration::state::{AttestationKind, StateV1};

    fn producer(attestations: &[&str], spec: Value) -> DesiredUnit {
        let mut unit = desired("kubernetes", "Command", spec, "k");
        unit.attestations = attestations.iter().map(|name| (*name).to_owned()).collect();
        unit
    }

    fn attestation(name: &str, seed: &str, result: AttestationResult) -> Attestation {
        Attestation {
            api_version: StateV1,
            kind: AttestationKind,
            name: name.to_owned(),
            unit: None,
            execution_key: key(seed).to_string(),
            result,
            at: "2026-09-24T10:00:00Z".parse().unwrap(),
            source: json!({"driver": "attest"}),
            reason: None,
            details: Value::Null,
        }
    }

    fn recorded(entries: &[(&str, &str, AttestationResult)]) -> BTreeMap<String, Attestation> {
        entries
            .iter()
            .map(|(name, seed, result)| ((*name).to_owned(), attestation(name, seed, *result)))
            .collect()
    }

    /// Catches a requirement that names an attestation the producer cannot
    /// make, which would wait forever, and one that names no unit's driver
    /// names: declared names are the driver's plus `spec.attestations`.
    #[test]
    fn test_required_names_must_be_declared_by_the_producer() {
        let unit = producer(
            &["healthy"],
            json!({"attestations": [{"name": "qa", "from": "external"}, {"name": "smoke"}]}),
        );
        assert_eq!(
            declared(&unit),
            BTreeSet::from(["healthy".to_owned(), "qa".to_owned(), "smoke".to_owned()])
        );
        assert_eq!(
            required(Evidence::Published, &["smoke".to_owned(), "healthy".to_owned()], &unit),
            Ok(vec!["smoke".to_owned(), "healthy".to_owned()])
        );
        let error = required(Evidence::Published, &["nope".to_owned()], &unit).unwrap_err();
        assert_eq!(error.code(), Code::AttestationUndeclared);
        assert!(
            error.to_string().contains("\"nope\"") && error.to_string().contains("healthy, qa, smoke"),
            "{error}"
        );
        let none = producer(&[], json!({}));
        assert!(required(Evidence::Attested, &["healthy".to_owned()], &none)
            .unwrap_err()
            .to_string()
            .contains("declares none"));
    }

    /// Catches a consumer that waits forever for an attestation nothing can
    /// record yet: a name declared only `from: external` is rejected, by name
    /// and through `evidence: attested`, with the names that can be required
    /// instead, while a name the driver also reports is not external-only.
    #[test]
    fn test_requiring_an_external_only_attestation_is_rejected() {
        let unit = producer(
            &["healthy"],
            json!({"attestations": [{"name": "qa", "from": "external"}, {"name": "healthy", "from": "external"}]}),
        );
        assert_eq!(external_only(&unit), BTreeSet::from(["qa".to_owned()]));
        assert_eq!(recordable(&unit), BTreeSet::from(["healthy".to_owned()]));
        for (evidence, named) in [
            (Evidence::Published, vec!["qa".to_owned()]),
            (Evidence::Published, vec!["healthy".to_owned(), "qa".to_owned()]),
            (Evidence::Attested, Vec::new()),
        ] {
            let error = required(evidence, &named, &unit).unwrap_err();
            assert_eq!(error.code(), Code::AttestationExternalUnsupported, "{named:?}");
            assert!(
                error.to_string().contains("\"qa\"") && error.to_string().contains("observes healthy"),
                "{error}"
            );
        }
        assert_eq!(
            required(Evidence::Published, &["healthy".to_owned()], &unit),
            Ok(vec!["healthy".to_owned()])
        );
    }

    /// Catches `evidence: attested` demanding more than the producer
    /// declares, or `published` demanding anything: `attested` is every
    /// declared name unless names narrow it, and a unit declaring none is
    /// satisfied by its receipt.
    #[test]
    fn test_required_follows_the_evidence_level() {
        let unit = producer(&["accepted", "healthy"], json!({}));
        assert_eq!(required(Evidence::Published, &[], &unit), Ok(Vec::new()));
        assert_eq!(
            required(Evidence::Attested, &[], &unit),
            Ok(vec!["accepted".to_owned(), "healthy".to_owned()])
        );
        assert_eq!(
            required(Evidence::Attested, &["healthy".to_owned()], &unit),
            Ok(vec!["healthy".to_owned()])
        );
        assert_eq!(
            required(Evidence::Attested, &[], &producer(&[], json!({}))),
            Ok(Vec::new())
        );
    }

    /// Catches an attestation of another receipt satisfying a consumer, a
    /// failing one being treated as missing, or a pass not counting: only a
    /// record naming the current receipt counts, and its result decides.
    #[test]
    fn test_an_attestation_counts_only_for_the_current_receipt() {
        let unit = producer(&["healthy", "accepted"], json!({}));
        let current = observed("kubernetes", "Command", Some(receipt("k", json!({}), Vec::new())));
        let required = ["accepted".to_owned(), "healthy".to_owned()];
        let standing = |set: &BTreeMap<String, Attestation>| unmet(&required, &unit, &current, Some(set));
        assert_eq!(
            unmet(&required, &unit, &current, None),
            [
                ("accepted".to_owned(), AttestationWait::Pending),
                ("healthy".to_owned(), AttestationWait::Pending)
            ]
        );
        assert_eq!(
            standing(&recorded(&[
                ("accepted", "k", AttestationResult::Pass),
                ("healthy", "k", AttestationResult::Fail)
            ])),
            [("healthy".to_owned(), AttestationWait::Failing)]
        );
        assert_eq!(
            standing(&recorded(&[
                ("accepted", "k", AttestationResult::Pass),
                ("healthy", "older", AttestationResult::Pass)
            ])),
            [("healthy".to_owned(), AttestationWait::Pending)],
            "a pass for a replaced receipt is not evidence for the new one"
        );
        assert!(standing(&recorded(&[
            ("accepted", "k", AttestationResult::Pass),
            ("healthy", "k", AttestationResult::Pass)
        ]))
        .is_empty());
    }

    /// Catches an attestation of a recreated incarnation satisfying a
    /// consumer of the new one when the receipt's key is the same.
    #[test]
    fn test_an_attestation_naming_another_incarnation_is_pending() {
        let unit = producer(&["healthy"], json!({}));
        let current = observed("kubernetes", "Command", Some(receipt("k", json!({}), Vec::new())));
        let mut set = recorded(&[("healthy", "k", AttestationResult::Pass)]);
        let subject = nyl_core::orchestration::state::ArtifactUnit {
            environment: crate::rules::fixtures::env(),
            name: name("kubernetes"),
            uid: crate::rules::fixtures::uid("other"),
        };
        set.get_mut("healthy").unwrap().unit = Some(subject);
        assert_eq!(
            unmet(&["healthy".to_owned()], &unit, &current, Some(&set)),
            [("healthy".to_owned(), AttestationWait::Pending)]
        );
    }

    /// Catches warnings losing what the consumer waits for: each carries its
    /// stable code from the typed blocker, names the producer as its text
    /// does (with the environment across environments), and ignores other
    /// blockers.
    #[test]
    fn test_warnings_carry_stable_codes_and_name_the_producer() {
        let at = |label: &str| ProducerRef::parse(label).unwrap();
        let attestation = |label: &str, name: &str, state| Blocker::Attestation {
            producer: at(label),
            name: name.to_owned(),
            state,
        };
        let reasons = [
            BlockedReason::new(
                "/dependsOn/0",
                attestation("kubernetes", "healthy", AttestationWait::Pending),
            ),
            BlockedReason::new(
                "/dependsOn/1",
                attestation("infra/kubernetes", "accepted", AttestationWait::Failing),
            ),
            BlockedReason::new("/dependsOn/2", Blocker::no_current_receipt(at("network"))),
        ];
        let findings = warnings(&name("smoke"), "dev", &reasons);
        assert_eq!(
            findings.iter().map(|finding| finding.code).collect::<Vec<_>>(),
            [Code::AttestationPending, Code::AttestationFailing]
        );
        assert!(findings[0].fix.contains("nyl attest -e dev"), "{}", findings[0].fix);
        assert!(
            findings[1].message.contains("attestation accepted of infra/kubernetes"),
            "{}",
            findings[1].message
        );
        assert_eq!(stable(AttestationWait::Pending), ATTESTATION_PENDING);
        assert_eq!(stable(AttestationWait::Failing), ATTESTATION_FAILING);
    }

    /// Catches stale attestations surviving a new receipt, or a unit's
    /// current ones being rewritten: only the replaced receipt's go.
    #[test]
    fn test_a_replaced_receipt_drops_its_attestations_in_the_same_commit() {
        let stored = BTreeMap::from([(
            name("kubernetes"),
            BTreeMap::from([
                (
                    "accepted".to_owned(),
                    attestation("accepted", "old", AttestationResult::Pass),
                ),
                (
                    "healthy".to_owned(),
                    attestation("healthy", "new", AttestationResult::Pass),
                ),
            ]),
        )]);
        let observed_new = BTreeMap::from([(
            name("kubernetes"),
            observed("kubernetes", "Command", Some(receipt("new", json!({}), Vec::new()))),
        )]);
        let changes = attestation_changes(&stored, &observed_new);
        let kept = changes[&name("kubernetes")].as_ref().unwrap();
        assert_eq!(kept.keys().collect::<Vec<_>>(), ["healthy"]);
        let observed_same = BTreeMap::from([(
            name("kubernetes"),
            observed("kubernetes", "Command", Some(receipt("old", json!({}), Vec::new()))),
        )]);
        assert_eq!(
            attestation_changes(&stored, &observed_same)[&name("kubernetes")]
                .as_ref()
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            ["accepted"]
        );
        assert_eq!(
            attestation_changes(&stored, &BTreeMap::new()),
            BTreeMap::from([(name("kubernetes"), None)])
        );
        let all_current = BTreeMap::from([(
            name("kubernetes"),
            BTreeMap::from([(
                "healthy".to_owned(),
                attestation("healthy", "new", AttestationResult::Pass),
            )]),
        )]);
        assert!(attestation_changes(&all_current, &observed_new).is_empty());
    }
}
