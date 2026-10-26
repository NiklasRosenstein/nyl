//! `nyl verify`: which units it verifies, what it records, and its exit.
//!
//! Contract: [Commands](../../../../design/orchestration-core.md#commands)
//! (`verify` writes one observed commit with the latest observations and
//! driver attestations), [Drivers](../../../../design/orchestration-core.md#drivers)
//! (`verify`), [Attestations](../../../../design/orchestration-core.md#attestations),
//! and [Inspection](../../../../design/orchestration-core.md#inspection)
//! (exit categories).
//!
//! - Only units with a current receipt are verified; the others report
//!   `no-receipt` (`NYL-VERIFY-NO-RECEIPT`). A kind that cannot verify is
//!   reported as `unsupported` (`NYL-VERIFY-UNSUPPORTED`), never skipped.
//! - Verification never writes a receipt: it records the latest observation
//!   per type and the driver's attestations, and writes nothing when they
//!   did not change. A document equal to the stored one but for `at` keeps
//!   the stored one ([`Records::settle`]).
//! - Exit: 0 every verified unit clean and its attestations pass; 2 a unit
//!   could not be verified (no current receipt, unsupported, or unknown
//!   drift); 3 drift or a failing attestation; 1 an error. Most severe wins.

use std::collections::{BTreeMap, BTreeSet};

use nyl_core::orchestration::codes::{Code, Finding};
use nyl_core::orchestration::driver::Verification;
use nyl_core::orchestration::ids::{EnvironmentName, UnitName};
use nyl_core::orchestration::ports::EnvironmentSnapshot;
use nyl_core::orchestration::state::{
    ArtifactUnit, Attestation, AttestationKind, AttestationResult, DesiredUnit, Drift, Observation, ObservationKind,
    ObservationType, Receipt, StateV1,
};
use nyl_core::orchestration::time::Timestamp;
use serde_json::{json, Value};

use crate::rules::freshness::receipt_is_current;
use crate::run_report::{ExitCategory, UnitReport, UnitVerification, VerifyResult};

/// Whether `verify` verifies a unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// It has a current receipt.
    Verify,
    /// It has no current receipt: reported, exit 2.
    NoReceipt,
}

/// The units `verify` covers, with `--unit` limiting them. Names that are
/// not in the snapshot are left out; the shell reports them as
/// `NYL-UNIT-UNKNOWN` before it verifies anything.
pub fn targets(snapshot: &EnvironmentSnapshot, named: Option<&BTreeSet<UnitName>>) -> BTreeMap<UnitName, Target> {
    snapshot
        .desired
        .iter()
        .filter(|(name, _)| named.is_none_or(|named| named.contains(*name)))
        .map(|(name, desired)| {
            let target = if receipt_is_current(desired, snapshot.observed.get(name)) {
                Target::Verify
            } else {
                Target::NoReceipt
            };
            (name.clone(), target)
        })
        .collect()
}

/// The documents one verification records.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Records {
    pub observations: BTreeMap<ObservationType, Observation>,
    pub attestations: BTreeMap<String, Attestation>,
}

/// The observation and attestation documents of `verification` of `unit`'s
/// current `receipt`, at `now`.
///
/// The drift observation is always recorded; its `details` are the
/// driver's observations other than health ones, when it reports any. A
/// driver observation `{type: health, result: {drift: …}, details: …}`
/// becomes the `health` observation. Each attestation outcome becomes one
/// attestation of the receipt with `source: {driver: verify}`.
pub fn records(
    environment: &EnvironmentName,
    unit: &DesiredUnit,
    receipt: &Receipt,
    verification: &Verification,
    now: Timestamp,
) -> Records {
    let observation = |observation_type, result: Drift, details: Value| Observation {
        api_version: StateV1,
        kind: ObservationKind,
        unit: unit.unit.clone(),
        observation_type,
        execution_key: receipt.execution_key.clone(),
        at: now,
        result,
        details,
    };
    let mut observations = BTreeMap::new();
    let mut other = Vec::new();
    for value in &verification.observations {
        let health = (value.get("type").and_then(Value::as_str) == Some("health"))
            .then(|| value.get("result").cloned())
            .flatten()
            .and_then(|result| serde_json::from_value::<Drift>(result).ok());
        match health {
            Some(result) => {
                let details = value.get("details").cloned().unwrap_or(Value::Null);
                observations.insert(
                    ObservationType::Health,
                    observation(ObservationType::Health, result, details),
                );
            }
            None => other.push(value.clone()),
        }
    }
    let details = if other.is_empty() {
        Value::Null
    } else {
        Value::Array(other)
    };
    observations.insert(
        ObservationType::Drift,
        observation(ObservationType::Drift, verification.drift.clone(), details),
    );
    let subject = unit.unit.uid.clone().map(|uid| ArtifactUnit {
        environment: environment.clone(),
        name: unit.unit.name.clone(),
        uid,
    });
    let attestations = verification
        .attestations
        .iter()
        .map(|outcome| {
            (
                outcome.name.clone(),
                Attestation {
                    api_version: StateV1,
                    kind: AttestationKind,
                    name: outcome.name.clone(),
                    unit: subject.clone(),
                    execution_key: receipt.execution_key.to_string(),
                    result: outcome.result,
                    at: now,
                    source: json!({"driver": "verify"}),
                    reason: outcome.reason.clone(),
                    details: Value::Null,
                },
            )
        })
        .collect();
    Records {
        observations,
        attestations,
    }
}

impl Records {
    /// The complete documents to store, given the stored ones: a new
    /// document equal to the stored one but for `at` keeps the stored one,
    /// so a verification that found nothing new writes nothing; stored
    /// documents of the same receipt that this verification did not report
    /// stay (the newest per name and receipt wins); stored documents of an
    /// earlier receipt are dropped, since they no longer describe it.
    #[must_use]
    pub fn settle(
        mut self,
        stored_observations: Option<&BTreeMap<ObservationType, Observation>>,
        stored_attestations: Option<&BTreeMap<String, Attestation>>,
        receipt: &Receipt,
    ) -> Self {
        for (observation_type, stored) in stored_observations.into_iter().flatten() {
            let keep = match self.observations.get(observation_type) {
                Some(new) => same_observation(new, stored),
                None => stored.execution_key == receipt.execution_key,
            };
            if keep {
                self.observations.insert(*observation_type, stored.clone());
            }
        }
        for (name, stored) in stored_attestations.into_iter().flatten() {
            let keep = match self.attestations.get(name) {
                Some(new) => same_attestation(new, stored),
                None => stored.execution_key == receipt.execution_key.as_str(),
            };
            if keep {
                self.attestations.insert(name.clone(), stored.clone());
            }
        }
        self
    }

    /// The verdict and attestation results the report shows.
    pub fn verification(&self) -> UnitVerification {
        let result = match self
            .observations
            .get(&ObservationType::Drift)
            .map(|drift| &drift.result)
        {
            Some(Drift::Clean) => VerifyResult::Clean,
            Some(Drift::Drifted { summary }) => VerifyResult::Drifted {
                summary: summary.clone(),
            },
            Some(Drift::Unknown { reason }) => VerifyResult::Unknown { reason: reason.clone() },
            None => VerifyResult::Unknown {
                reason: "the driver reported no drift result".to_owned(),
            },
        };
        UnitVerification {
            result,
            attestations: self
                .attestations
                .iter()
                .map(|(name, attestation)| (name.clone(), attestation.result))
                .collect(),
        }
    }
}

fn same_observation(left: &Observation, right: &Observation) -> bool {
    Observation {
        at: right.at,
        ..left.clone()
    } == *right
}

fn same_attestation(left: &Attestation, right: &Attestation) -> bool {
    Attestation {
        at: right.at,
        ..left.clone()
    } == *right
}

/// The exit category one unit of a `VerifyReport` contributes.
pub fn unit_exit(unit: &UnitReport) -> ExitCategory {
    let Some(verification) = &unit.verification else {
        return ExitCategory::Ok;
    };
    let failing = verification
        .attestations
        .values()
        .any(|result| *result == AttestationResult::Fail);
    match &verification.result {
        VerifyResult::Drifted { .. } => ExitCategory::Failed,
        _ if failing => ExitCategory::Failed,
        VerifyResult::Clean => ExitCategory::Ok,
        VerifyResult::NoReceipt | VerifyResult::Unsupported | VerifyResult::Unknown { .. } => ExitCategory::Incomplete,
    }
}

/// The warnings a unit's verification adds to its report, each with its fix.
pub fn warnings(environment: &EnvironmentName, unit: &UnitName, verification: &UnitVerification) -> Vec<Finding> {
    let mut warnings = Vec::new();
    match &verification.result {
        VerifyResult::Clean => {}
        VerifyResult::NoReceipt => warnings.push(Finding::new(
            Code::VerifyNoReceipt,
            format!("{unit} in {environment} has no current receipt to verify"),
            format!("run `nyl reconcile -e {environment}` so the unit has a receipt for its current desired document"),
        )),
        VerifyResult::Unsupported => warnings.push(Finding::new(
            Code::VerifyUnsupported,
            format!("the kind of {unit} cannot verify its effects"),
            "verify the unit with its own tool, or give it a verify step where its kind supports one",
        )),
        VerifyResult::Unknown { reason } => warnings.push(Finding::new(
            Code::VerifyUnknown,
            format!("drift of {unit} in {environment} is unknown: {reason}"),
            "fix the cause, such as a missing tool or credentials, and run `nyl verify` again",
        )),
        VerifyResult::Drifted { summary } => warnings.push(Finding::new(
            Code::VerifyDrift,
            format!("{unit} in {environment} drifted from its receipt: {summary}"),
            format!(
                "investigate the change made outside Nyl; `nyl plan -e {environment} --unit {unit}` shows what the unit would apply (nyl reconcile does not execute a unit with a current receipt again yet)"
            ),
        )),
    }
    for (name, result) in &verification.attestations {
        if *result == AttestationResult::Fail {
            warnings.push(Finding::new(
                Code::AttestationFailed,
                format!("attestation {name} of {unit} in {environment} failed"),
                "investigate the failing check; references that require the attestation stay unsatisfied until a newer one passes",
            ));
        }
    }
    warnings
}

#[cfg(test)]
mod tests {
    use nyl_core::orchestration::driver::AttestationOutcome;

    use super::*;
    use crate::rules::fixtures::{desired_numbered, env, hex_key, name, observed_numbered};

    fn at(minute: u32) -> Timestamp {
        format!("2026-01-01T00:{minute:02}:00Z").parse().unwrap()
    }

    fn snapshot() -> EnvironmentSnapshot {
        let mut snapshot = EnvironmentSnapshot::default();
        for (unit, desired_key, observed_key) in [
            ("network", 'a', Some('a')),
            ("database", 'b', Some('a')),
            ("web", 'c', None),
        ] {
            snapshot
                .desired
                .insert(name(unit), desired_numbered(unit, 1, Some(hex_key(desired_key))));
            snapshot
                .observed
                .insert(name(unit), observed_numbered(unit, 1, observed_key.map(hex_key)));
        }
        snapshot
    }

    /// Catches `verify` verifying a stale receipt (drift against what no
    /// longer should run) or skipping a unit instead of reporting it.
    #[test]
    fn test_targets_split_units_by_current_receipt_and_named_units() {
        let snapshot = snapshot();
        assert_eq!(
            targets(&snapshot, None),
            BTreeMap::from([
                (name("database"), Target::NoReceipt),
                (name("network"), Target::Verify),
                (name("web"), Target::NoReceipt),
            ])
        );
        let named = BTreeSet::from([name("network"), name("missing")]);
        assert_eq!(
            targets(&snapshot, Some(&named)),
            BTreeMap::from([(name("network"), Target::Verify)])
        );
    }

    fn verification(drift: Drift, attestation: Option<AttestationResult>) -> Verification {
        Verification {
            drift,
            observations: vec![
                json!({"type": "health", "result": {"drift": "clean"}, "details": {"apps": 1}}),
                json!({"application": "web", "sync": "Synced"}),
            ],
            attestations: attestation
                .map(|result| AttestationOutcome {
                    name: "healthy".to_owned(),
                    result,
                    reason: None,
                })
                .into_iter()
                .collect(),
        }
    }

    /// Catches a second `verify` of an unchanged unit writing a commit only
    /// because time moved on, and a changed result not being written.
    #[test]
    fn test_records_keep_stored_documents_that_differ_only_in_time() {
        let unit = desired_numbered("network", 1, Some(hex_key('a')));
        let observed = observed_numbered("network", 1, Some(hex_key('a')));
        let receipt = observed.receipt.as_ref().unwrap();
        let first = records(
            &env(),
            &unit,
            receipt,
            &verification(Drift::Clean, Some(AttestationResult::Pass)),
            at(1),
        );
        assert_eq!(
            first.observations.keys().copied().collect::<Vec<_>>(),
            vec![ObservationType::Drift, ObservationType::Health]
        );
        assert_eq!(
            first.observations[&ObservationType::Drift].details,
            json!([{"application": "web", "sync": "Synced"}])
        );
        assert_eq!(first.attestations["healthy"].source, json!({"driver": "verify"}));

        let again = records(
            &env(),
            &unit,
            receipt,
            &verification(Drift::Clean, Some(AttestationResult::Pass)),
            at(2),
        )
        .settle(Some(&first.observations), Some(&first.attestations), receipt);
        assert_eq!(again, first, "nothing changed but the time");

        let drifted = records(
            &env(),
            &unit,
            receipt,
            &verification(
                Drift::Drifted {
                    summary: "1 change".to_owned(),
                },
                Some(AttestationResult::Pass),
            ),
            at(3),
        )
        .settle(Some(&first.observations), Some(&first.attestations), receipt);
        assert_eq!(drifted.observations[&ObservationType::Drift].at, at(3));
        assert_eq!(drifted.observations[&ObservationType::Health].at, at(1));
        assert_eq!(drifted.attestations, first.attestations);
    }

    /// Catches an attestation of an earlier receipt surviving into the new
    /// receipt's set, where it would satisfy a requirement it never checked.
    #[test]
    fn test_settle_drops_documents_of_an_earlier_receipt() {
        let unit = desired_numbered("network", 1, Some(hex_key('a')));
        let old = observed_numbered("network", 1, Some(hex_key('f')));
        let earlier = records(
            &env(),
            &unit,
            old.receipt.as_ref().unwrap(),
            &verification(Drift::Clean, Some(AttestationResult::Pass)),
            at(1),
        );
        let current = observed_numbered("network", 1, Some(hex_key('a')));
        let receipt = current.receipt.as_ref().unwrap();
        let mut plain = verification(Drift::Clean, None);
        plain.observations.clear();
        let settled = records(&env(), &unit, receipt, &plain, at(2)).settle(
            Some(&earlier.observations),
            Some(&earlier.attestations),
            receipt,
        );
        assert!(settled.attestations.is_empty());
        assert_eq!(
            settled.observations.keys().copied().collect::<Vec<_>>(),
            vec![ObservationType::Drift]
        );
    }

    fn report(result: VerifyResult, attestation: Option<AttestationResult>) -> UnitReport {
        UnitReport {
            verification: Some(UnitVerification {
                result,
                attestations: attestation
                    .map(|result| ("healthy".to_owned(), result))
                    .into_iter()
                    .collect(),
            }),
            ..UnitReport::new("OpenTofu")
        }
    }

    /// Catches CI treating drift or an unverifiable unit as a clean
    /// environment, or a failing attestation hiding behind a clean drift.
    #[test]
    fn test_unit_exit_table() {
        let drifted = || VerifyResult::Drifted {
            summary: "x".to_owned(),
        };
        let cases = [
            (VerifyResult::Clean, None, ExitCategory::Ok),
            (VerifyResult::Clean, Some(AttestationResult::Pass), ExitCategory::Ok),
            (VerifyResult::Clean, Some(AttestationResult::Fail), ExitCategory::Failed),
            (VerifyResult::NoReceipt, None, ExitCategory::Incomplete),
            (VerifyResult::Unsupported, None, ExitCategory::Incomplete),
            (
                VerifyResult::Unknown { reason: "x".to_owned() },
                None,
                ExitCategory::Incomplete,
            ),
            (
                VerifyResult::Unknown { reason: "x".to_owned() },
                Some(AttestationResult::Fail),
                ExitCategory::Failed,
            ),
            (drifted(), None, ExitCategory::Failed),
        ];
        for (result, attestation, expected) in cases {
            let unit = report(result.clone(), attestation);
            assert_eq!(unit_exit(&unit), expected, "{result:?} {attestation:?}");
            let codes: Vec<Code> = warnings(&env(), &name("network"), unit.verification.as_ref().unwrap())
                .iter()
                .map(|finding| finding.code)
                .collect();
            let expected_codes: Vec<Code> = match result {
                VerifyResult::Clean => vec![],
                VerifyResult::NoReceipt => vec![Code::VerifyNoReceipt],
                VerifyResult::Unsupported => vec![Code::VerifyUnsupported],
                VerifyResult::Unknown { .. } => vec![Code::VerifyUnknown],
                VerifyResult::Drifted { .. } => vec![Code::VerifyDrift],
            }
            .into_iter()
            .chain((attestation == Some(AttestationResult::Fail)).then_some(Code::AttestationFailed))
            .collect();
            assert_eq!(codes, expected_codes, "{result:?} {attestation:?}");
        }
    }
}
