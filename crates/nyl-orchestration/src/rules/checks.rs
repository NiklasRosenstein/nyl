//! `nyl detect-drift` and `nyl attest`: the two checks of current receipts,
//! which units each covers, what it records, and its exit.
//!
//! Contract: [Drift](../../../../design/orchestration-core.md#drift),
//! [Attestations](../../../../design/orchestration-core.md#attestations),
//! [Commands](../../../../design/orchestration-core.md#commands), and
//! [Inspection](../../../../design/orchestration-core.md#inspection)
//! (exit categories).
//!
//! - Only units with a current receipt are checked; the others report
//!   `no-receipt`. `detect-drift` covers every unit and reports a kind that
//!   cannot detect drift as `unsupported`, never skipped. `attest` covers
//!   the units whose driver attests for their spec; a unit named with
//!   `--unit` whose driver does not is `unsupported`.
//! - A check never writes a receipt. `detect-drift` records the unit's
//!   drift record; `attest` records the driver's attestations of the
//!   receipt (source `{driver: attest}`). A document equal to the stored one
//!   but for `at` keeps the stored one, so a check that found nothing new
//!   writes nothing.
//! - `nyl observe` is the third check, of observed values: it covers the
//!   units whose kind reads observed values for their spec and that declare
//!   observations (the decisions about the record are
//!   [`super::observed_values`]'s); a unit named with `--unit` whose kind
//!   cannot is `unsupported`.
//! - Exit: 0 every checked unit clean, its attestations pass, or every
//!   observation has a value; 2 a unit could not be checked (no current
//!   receipt, unsupported, or unknown) or an observation is `pending` or
//!   `failed`; 3 drift or a failing attestation; 1 an error. Most severe wins.

use std::collections::{BTreeMap, BTreeSet};

use nyl_core::orchestration::codes::{Code, Finding};
use nyl_core::orchestration::driver::{
    AttestationOutcome, DriftDetection, DriverOperation, DriverSet, ObservedValue, ObservedValues, RecoveryPolicy,
};
use nyl_core::orchestration::ids::{EnvironmentName, UnitName};
use nyl_core::orchestration::ports::EnvironmentSnapshot;
use nyl_core::orchestration::state::{
    ArtifactUnit, Attestation, AttestationKind, AttestationResult, DesiredUnit, Drift, DriftRecord, DriftRecordKind,
    Operation, Receipt, StateV1,
};
use nyl_core::orchestration::time::{Duration, Timestamp};
use nyl_core::resources::environment::DriftRepair;
use nyl_core::resources::units::ObservationDeclaration;
use serde_json::json;

use crate::rules::freshness::receipt_is_current;
use crate::run_report::{CheckResult, ExitCategory, ObservationStatus, ReportKind, UnitCheck, UnitReport};

/// The check an invocation runs against current receipts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Check {
    /// `nyl detect-drift`: whether each receipt still holds.
    DetectDrift,
    /// `nyl attest`: the driver attestations of each receipt.
    Attest,
    /// `nyl observe`: the observed values of each receipt.
    Observe,
}

impl Check {
    /// The command, as findings and fixes name it.
    pub fn command(self) -> &'static str {
        match self {
            Self::DetectDrift => "detect-drift",
            Self::Attest => "attest",
            Self::Observe => "observe",
        }
    }

    /// The `Nyl-Operation` of its transition commit and lease.
    pub fn operation(self) -> Operation {
        match self {
            Self::DetectDrift => Operation::DetectDrift,
            Self::Attest => Operation::Attest,
            Self::Observe => Operation::Observe,
        }
    }

    /// The operation a driver call serves.
    pub fn driver_operation(self) -> DriverOperation {
        match self {
            Self::DetectDrift => DriverOperation::DetectDrift,
            Self::Attest => DriverOperation::Attest,
            Self::Observe => DriverOperation::Observe,
        }
    }

    pub fn report_kind(self) -> ReportKind {
        match self {
            Self::DetectDrift => ReportKind::DetectDriftReport,
            Self::Attest => ReportKind::AttestReport,
            Self::Observe => ReportKind::ObserveReport,
        }
    }

    /// The results a transition commit's headline counts, in order.
    pub fn headline(self) -> &'static [&'static str] {
        match self {
            Self::DetectDrift => &["clean", "drifted", "unknown", "unsupported", "no-receipt"],
            Self::Attest => &["attested", "unknown", "unsupported", "no-receipt"],
            Self::Observe => &["observed", "unknown", "unsupported", "no-receipt"],
        }
    }
}

/// Whether a check covers a unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// It has a current receipt, and its kind supports the check.
    Check,
    /// It has no current receipt: reported, exit 2.
    NoReceipt,
    /// `attest` or `observe --unit` named it, but its driver does not attest or observe for its spec: reported, exit 2.
    Unsupported,
}

/// The units a check covers, with `--unit` limiting them. Names that are
/// not in the snapshot are left out; the shell reports them as
/// `NYL-UNIT-UNKNOWN` before it checks anything. `attest` leaves out the
/// units whose driver does not attest for their spec unless `--unit`
/// names them.
pub fn targets(
    check: Check,
    snapshot: &EnvironmentSnapshot,
    named: Option<&BTreeSet<UnitName>>,
    drivers: &DriverSet,
) -> BTreeMap<UnitName, Target> {
    snapshot
        .desired
        .iter()
        .filter(|(name, _)| named.is_none_or(|named| named.contains(*name)))
        .filter_map(|(name, desired)| {
            let covers = match check {
                Check::DetectDrift => true,
                Check::Attest => attests(drivers, desired),
                Check::Observe => observes(drivers, desired),
            };
            if !covers {
                return named.is_some().then(|| (name.clone(), Target::Unsupported));
            }
            let target = if receipt_is_current(desired, snapshot.observed.get(name)) {
                Target::Check
            } else {
                Target::NoReceipt
            };
            Some((name.clone(), target))
        })
        .collect()
}

/// Whether `desired`'s driver attests for its spec.
fn attests(drivers: &DriverSet, desired: &DesiredUnit) -> bool {
    let spec = desired.resolved_spec.as_ref().unwrap_or(&desired.spec);
    drivers
        .get(&desired.unit.api_version, &desired.unit.kind)
        .is_some_and(|driver| driver.capabilities(spec).attest)
}

/// Whether `desired`'s driver reads observed values for its spec, which
/// declares some.
fn observes(drivers: &DriverSet, desired: &DesiredUnit) -> bool {
    let spec = desired.resolved_spec.as_ref().unwrap_or(&desired.spec);
    super::observed_values::declares(desired)
        && drivers
            .get(&desired.unit.api_version, &desired.unit.kind)
            .is_some_and(|driver| driver.capabilities(spec).observed_values)
}

/// The drift record of `detection` of `unit`'s current `receipt` at `now`.
pub fn drift_record(unit: &DesiredUnit, receipt: &Receipt, detection: &DriftDetection, now: Timestamp) -> DriftRecord {
    DriftRecord {
        api_version: StateV1,
        kind: DriftRecordKind,
        unit: unit.unit.clone(),
        execution_key: receipt.execution_key.clone(),
        at: now,
        result: detection.drift.clone(),
        details: detection.details.clone(),
    }
}

/// The record to store, given the stored one: a new record equal to the
/// stored one but for `at` keeps the stored one.
#[must_use]
pub fn settle_drift(new: DriftRecord, stored: Option<&DriftRecord>) -> DriftRecord {
    match stored {
        Some(stored)
            if DriftRecord {
                at: stored.at,
                ..new.clone()
            } == *stored =>
        {
            stored.clone()
        }
        _ => new,
    }
}

/// The attestation documents of `outcomes` for `unit`'s current `receipt`
/// at `now`, each with `source: {driver: <source>}` (`attest` for `nyl
/// attest`, `reconcile` for the outcome of an execution) and the driver's
/// details.
pub fn attestations(
    environment: &EnvironmentName,
    unit: &DesiredUnit,
    receipt: &Receipt,
    outcomes: &[AttestationOutcome],
    source: &str,
    now: Timestamp,
) -> BTreeMap<String, Attestation> {
    let subject = unit.unit.uid.clone().map(|uid| ArtifactUnit {
        environment: environment.clone(),
        name: unit.unit.name.clone(),
        uid,
    });
    outcomes
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
                    source: json!({"driver": source}),
                    reason: outcome.reason.clone(),
                    details: outcome.details.clone(),
                },
            )
        })
        .collect()
}

/// The complete attestation set to store, given the stored one: a new
/// document equal to the stored one but for `at` keeps the stored one while
/// the stored one is younger than `refresh`, so a repeated `attest` writes
/// nothing within the interval; once it is older, the new `at` replaces it,
/// so a system that is re-verified keeps a fresh attestation (and satisfies a
/// `maxAttestationAge` longer than `refresh`) while one that cannot be
/// reached grows stale; stored documents of the same receipt that this check did not report stay
/// (the newest per name and receipt wins); stored documents of an earlier
/// receipt are dropped, since they no longer describe it.
#[must_use]
pub fn settle_attestations(
    mut new: BTreeMap<String, Attestation>,
    stored: Option<&BTreeMap<String, Attestation>>,
    receipt: &Receipt,
    now: Timestamp,
    refresh: Duration,
) -> BTreeMap<String, Attestation> {
    for (name, stored) in stored.into_iter().flatten() {
        let keep = match new.get(name) {
            Some(new) => {
                Attestation {
                    at: stored.at,
                    ..new.clone()
                } == *stored
                    && now < refresh.after(stored.at)
            }
            None => stored.execution_key == receipt.execution_key.as_str(),
        };
        if keep {
            new.insert(name.clone(), stored.clone());
        }
    }
    new
}

/// What `detect-drift` reports for a drift result.
pub fn drift_check(drift: &Drift) -> UnitCheck {
    match drift {
        Drift::Clean => CheckResult::Clean,
        Drift::Drifted { summary } => CheckResult::Drifted {
            summary: summary.clone(),
        },
        Drift::Unknown { reason } => CheckResult::Unknown { reason: reason.clone() },
    }
    .into()
}

/// What `attest` reports for a unit's attestation set.
pub fn attest_check(attestations: &BTreeMap<String, Attestation>) -> UnitCheck {
    UnitCheck {
        result: CheckResult::Attested,
        attestations: attestations
            .iter()
            .map(|(name, attestation)| (name.clone(), attestation.result))
            .collect(),
        observations: BTreeMap::new(),
    }
}

/// What `nyl observe` reports for a unit: each declared observation as this
/// read found it.
pub fn observe_check(readings: &ObservedValues, declared: &BTreeMap<String, ObservationDeclaration>) -> UnitCheck {
    UnitCheck {
        result: CheckResult::Observed,
        attestations: BTreeMap::new(),
        observations: declared
            .keys()
            .map(|name| {
                let status = match readings.values.get(name) {
                    Some(ObservedValue::Value { value }) => ObservationStatus::Value { value: value.clone() },
                    Some(ObservedValue::Pending { reason }) => ObservationStatus::Pending {
                        reason: reason.clone(),
                    },
                    Some(ObservedValue::Failed { reason }) => ObservationStatus::Failed {
                        reason: reason.clone(),
                    },
                    None => ObservationStatus::Failed {
                        reason: format!("the driver reported nothing for observation {name}"),
                    },
                };
                (name.clone(), status)
            })
            .collect(),
    }
}

/// The exit category one unit of a check report contributes.
pub fn unit_exit(unit: &UnitReport) -> ExitCategory {
    let Some(check) = &unit.check else {
        return ExitCategory::Ok;
    };
    let failing = check
        .attestations
        .values()
        .any(|result| *result == AttestationResult::Fail);
    match &check.result {
        CheckResult::Drifted { .. } => ExitCategory::Failed,
        _ if failing => ExitCategory::Failed,
        CheckResult::Observed
            if check
                .observations
                .values()
                .any(|status| !matches!(status, ObservationStatus::Value { .. })) =>
        {
            ExitCategory::Incomplete
        }
        CheckResult::Clean | CheckResult::Attested | CheckResult::Observed => ExitCategory::Ok,
        CheckResult::NoReceipt | CheckResult::Unsupported | CheckResult::Unknown { .. } => ExitCategory::Incomplete,
    }
}

/// The warnings a unit's check adds to its report, each with its fix.
///
/// The fix of `NYL-DRIFT-DETECTED` says what the next reconcile does with
/// the drifted unit ([Drift](../../../../design/orchestration-core.md#drift)),
/// from its execution recovery `policy` (`None` when no driver serves its
/// kind) and the environment's stored `driftRepair`: under `converge`, a
/// `converge` unit is repaired by the next `nyl reconcile`, and any other
/// awaits approval (`--approve`); under `manual`, only `--reapply`
/// repairs it.
pub fn warnings(
    check: Check,
    environment: &EnvironmentName,
    unit: &UnitName,
    result: &UnitCheck,
    policy: Option<RecoveryPolicy>,
    drift_repair: DriftRepair,
) -> Vec<Finding> {
    let command = check.command();
    let mut warnings = Vec::new();
    let codes = match check {
        Check::DetectDrift => (Code::DriftNoReceipt, Code::DriftUnsupported, Code::DriftUnknown),
        Check::Attest => (Code::AttestNoReceipt, Code::AttestUnsupported, Code::AttestUnknown),
        Check::Observe => (Code::ObserveNoReceipt, Code::ObserveUnsupported, Code::DriverError),
    };
    match &result.result {
        CheckResult::Clean | CheckResult::Attested | CheckResult::Observed => {}
        CheckResult::NoReceipt => warnings.push(Finding::new(
            codes.0,
            format!("{unit} in {environment} has no current receipt to check"),
            format!("run `nyl reconcile -e {environment}` so the unit has a receipt for its current desired document"),
        )),
        CheckResult::Unsupported => warnings.push(match check {
            Check::DetectDrift => Finding::new(
                codes.1,
                format!("the kind of {unit} cannot detect drift"),
                "check the unit with its own tool, or give it a drift detection step where its kind supports one",
            ),
            Check::Attest => Finding::new(
                codes.1,
                format!("the kind of {unit} produces no attestations for its spec"),
                "attestations only someone else can produce are declared with `attestations: [{name, from: external}]`",
            ),
            Check::Observe => Finding::new(
                codes.1,
                format!("{unit} declares no observed values its kind can read"),
                "declare `observations` on a unit whose kind reads observed values, such as a KubernetesPublication in `mode: observe`",
            ),
        }),
        CheckResult::Unknown { reason } => warnings.push(Finding::new(
            codes.2,
            match check {
                Check::DetectDrift => format!("drift of {unit} in {environment} is unknown: {reason}"),
                Check::Attest => format!("attestations of {unit} in {environment} are unknown: {reason}"),
                Check::Observe => format!("observed values of {unit} in {environment} are unknown: {reason}"),
            },
            format!("fix the cause, such as a missing tool or credentials, and run `nyl {command}` again"),
        )),
        CheckResult::Drifted { summary } => warnings.push(Finding::new(
            Code::DriftDetected,
            format!("{unit} in {environment} drifted from its receipt: {summary}"),
            drift_fix(environment, unit, policy, drift_repair),
        )),
    }
    for (name, status) in &result.observations {
        match status {
            ObservationStatus::Value { .. } => {}
            ObservationStatus::Pending { reason } => warnings.push(Finding::new(
                Code::ObservationPending,
                format!("observed value {name} of {unit} in {environment} has none yet: {reason}"),
                format!("run `nyl observe -e {environment}` again once the system holds it; consumers of it stay blocked"),
            )),
            ObservationStatus::Failed { reason } => warnings.push(Finding::new(
                Code::ObservationFailed,
                format!("observed value {name} of {unit} in {environment} could not be read: {reason}; the last recorded value for its current receipt stays usable"),
                "fix the cause, such as missing credentials, and run `nyl observe` again",
            )),
        }
    }
    for (name, result) in &result.attestations {
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

/// What the next reconcile does with a drifted unit, as the fix of
/// `NYL-DRIFT-DETECTED`.
fn drift_fix(
    environment: &EnvironmentName,
    unit: &UnitName,
    policy: Option<RecoveryPolicy>,
    drift_repair: DriftRepair,
) -> String {
    let converges = policy == Some(RecoveryPolicy::Converge);
    match drift_repair {
        DriftRepair::Converge if converges => {
            format!("the next `nyl reconcile -e {environment}` repairs it by executing {unit} again")
        }
        DriftRepair::Converge => format!(
            "it awaits approval, since {unit} is not safe to re-run: run `nyl reconcile -e {environment} --approve {unit}`"
        ),
        DriftRepair::Manual if converges => format!(
            "repair it with `nyl reconcile -e {environment} --unit {unit} --reapply --reason <text>` (driftRepair: manual)"
        ),
        DriftRepair::Manual => format!(
            "repair it with `nyl reconcile -e {environment} --unit {unit} --reapply --reason <text> --approve {unit}` (driftRepair: manual; {unit} is not safe to re-run without an approval)"
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::Value;

    use super::*;
    use crate::rules::fixtures::{desired_numbered, env, hex_key, name, observed_numbered, TestDriver};

    fn at(minute: u32) -> Timestamp {
        format!("2026-01-01T00:{minute:02}:00Z").parse().unwrap()
    }

    fn drivers() -> DriverSet {
        let mut drivers = DriverSet::new();
        for kind in ["Command", "OpenTofu"] {
            drivers.register(Arc::new(TestDriver { kind }));
        }
        drivers
    }

    /// `network` current, `database` stale, `web` never executed; only
    /// `network` and `database` attest.
    fn snapshot() -> EnvironmentSnapshot {
        let mut snapshot = EnvironmentSnapshot::default();
        for (unit, desired_key, observed_key, attests) in [
            ("network", 'a', Some('a'), true),
            ("database", 'b', Some('a'), true),
            ("web", 'c', None, false),
            ("cache", 'd', Some('d'), false),
        ] {
            let mut desired = desired_numbered(unit, 1, Some(hex_key(desired_key)));
            desired.spec = json!({"attests": attests});
            desired.resolved_spec = None;
            snapshot.desired.insert(name(unit), desired);
            snapshot
                .observed
                .insert(name(unit), observed_numbered(unit, 1, observed_key.map(hex_key)));
        }
        snapshot
    }

    /// Catches a check of a stale receipt (drift against what no longer
    /// should run), a unit skipped instead of reported, or `attest`
    /// reporting every unit that cannot attest as incomplete.
    #[test]
    fn test_targets_split_units_by_current_receipt_check_and_named_units() {
        let snapshot = snapshot();
        let drivers = drivers();
        assert_eq!(
            targets(Check::DetectDrift, &snapshot, None, &drivers),
            BTreeMap::from([
                (name("cache"), Target::Check),
                (name("database"), Target::NoReceipt),
                (name("network"), Target::Check),
                (name("web"), Target::NoReceipt),
            ])
        );
        assert_eq!(
            targets(Check::Attest, &snapshot, None, &drivers),
            BTreeMap::from([(name("database"), Target::NoReceipt), (name("network"), Target::Check)])
        );
        let named = BTreeSet::from([name("network"), name("cache"), name("missing")]);
        assert_eq!(
            targets(Check::Attest, &snapshot, Some(&named), &drivers),
            BTreeMap::from([(name("cache"), Target::Unsupported), (name("network"), Target::Check)])
        );
    }

    /// Catches `observe` covering units that read nothing for their spec:
    /// only a unit that declares observations and whose kind reads them is
    /// checked, and `--unit` reports the others as unsupported.
    #[test]
    fn test_targets_for_observe_cover_units_that_declare_observations_their_kind_reads() {
        let mut snapshot = snapshot();
        let declaring = json!({"observes": true, "observations": {"ip": {"type": "string", "from": {}}}});
        for unit in ["network", "database", "web"] {
            let desired = snapshot.desired.get_mut(&name(unit)).unwrap();
            desired.spec = declaring.clone();
        }
        // `cache` reads observed values but declares none.
        snapshot.desired.get_mut(&name("cache")).unwrap().spec = json!({"observes": true});
        let drivers = drivers();
        assert_eq!(
            targets(Check::Observe, &snapshot, None, &drivers),
            BTreeMap::from([
                (name("database"), Target::NoReceipt),
                (name("network"), Target::Check),
                (name("web"), Target::NoReceipt),
            ])
        );
        let named = BTreeSet::from([name("network"), name("cache")]);
        assert_eq!(
            targets(Check::Observe, &snapshot, Some(&named), &drivers),
            BTreeMap::from([(name("cache"), Target::Unsupported), (name("network"), Target::Check)])
        );
    }

    /// Catches `observe` exiting 0 while a value is pending or failed, which
    /// a scheduled job would read as healthy, and a pending or failed
    /// observation without its own code and fix.
    #[test]
    fn test_observe_exits_and_warns_by_each_observation() {
        let declared: BTreeMap<String, ObservationDeclaration> = serde_json::from_value(json!({
            "ip": {"type": "string", "from": {}}, "port": {"type": "integer", "from": {}}
        }))
        .unwrap();
        let readings = |port: ObservedValue| ObservedValues {
            values: BTreeMap::from([
                ("ip".to_owned(), ObservedValue::Value { value: json!("203.0.113.10") }),
                ("port".to_owned(), port),
            ]),
        };
        let check = |port| {
            let unit = UnitReport {
                check: Some(observe_check(&readings(port), &declared)),
                ..UnitReport::new("Command")
            };
            let findings = warnings(
                Check::Observe,
                &env(),
                &name("lb"),
                unit.check.as_ref().unwrap(),
                None,
                DriftRepair::Converge,
            );
            (unit_exit(&unit), findings.iter().map(|finding| finding.code).collect::<Vec<_>>())
        };
        assert_eq!(check(ObservedValue::Value { value: json!(443) }), (ExitCategory::Ok, vec![]));
        assert_eq!(
            check(ObservedValue::Pending { reason: "no port".to_owned() }),
            (ExitCategory::Incomplete, vec![Code::ObservationPending])
        );
        assert_eq!(
            check(ObservedValue::Failed { reason: "no credentials".to_owned() }),
            (ExitCategory::Incomplete, vec![Code::ObservationFailed])
        );
        // A name the driver left out is failed.
        let missing = observe_check(
            &ObservedValues {
                values: BTreeMap::new(),
            },
            &declared,
        );
        assert!(missing
            .observations
            .values()
            .all(|status| matches!(status, ObservationStatus::Failed { .. })));
    }

    /// Catches a second check of an unchanged unit writing a commit only
    /// because time moved on, and a changed result not being written.
    #[test]
    fn test_records_keep_stored_documents_that_differ_only_in_time() {
        let unit = desired_numbered("network", 1, Some(hex_key('a')));
        let observed = observed_numbered("network", 1, Some(hex_key('a')));
        let receipt = observed.receipt.as_ref().unwrap();
        let detection = |drift| DriftDetection {
            drift,
            details: json!({"resources": 1}),
        };
        let first = drift_record(&unit, receipt, &detection(Drift::Clean), at(1));
        assert_eq!(first.details, json!({"resources": 1}));
        let again = settle_drift(
            drift_record(&unit, receipt, &detection(Drift::Clean), at(2)),
            Some(&first),
        );
        assert_eq!(again, first, "nothing changed but the time");
        let drifted = Drift::Drifted {
            summary: "1 change".to_owned(),
        };
        let changed = settle_drift(drift_record(&unit, receipt, &detection(drifted), at(3)), Some(&first));
        assert_eq!(changed.at, at(3));

        let outcome = |result| AttestationOutcome {
            name: "healthy".to_owned(),
            result,
            reason: None,
            details: json!({"applications": 1}),
        };
        let attested = attestations(
            &env(),
            &unit,
            receipt,
            &[outcome(AttestationResult::Pass)],
            "attest",
            at(1),
        );
        assert_eq!(attested["healthy"].source, json!({"driver": "attest"}));
        assert_eq!(attested["healthy"].details, json!({"applications": 1}));
        let again = settle_attestations(
            attestations(
                &env(),
                &unit,
                receipt,
                &[outcome(AttestationResult::Pass)],
                "attest",
                at(2),
            ),
            Some(&attested),
            receipt,
            at(2),
            Duration::from_mins(15),
        );
        assert_eq!(again, attested);

        // Once the stored attestation is older than the refresh interval, an
        // unchanged result is written again with its new time.
        let later = at(1) + chrono::Duration::minutes(16);
        let refreshed = settle_attestations(
            attestations(
                &env(),
                &unit,
                receipt,
                &[outcome(AttestationResult::Pass)],
                "attest",
                later,
            ),
            Some(&attested),
            receipt,
            later,
            Duration::from_mins(15),
        );
        assert_eq!(refreshed["healthy"].at, later);
        assert_eq!(refreshed["healthy"].details, attested["healthy"].details);
    }

    /// Catches an attestation of an earlier receipt surviving into the new
    /// receipt's set, where it would satisfy a requirement it never checked.
    #[test]
    fn test_settle_drops_attestations_of_an_earlier_receipt() {
        let unit = desired_numbered("network", 1, Some(hex_key('a')));
        let old = observed_numbered("network", 1, Some(hex_key('f')));
        let outcome = AttestationOutcome {
            name: "healthy".to_owned(),
            result: AttestationResult::Pass,
            reason: None,
            details: Value::Null,
        };
        let earlier = attestations(
            &env(),
            &unit,
            old.receipt.as_ref().unwrap(),
            &[outcome],
            "attest",
            at(1),
        );
        let current = observed_numbered("network", 1, Some(hex_key('a')));
        let settled = settle_attestations(
            BTreeMap::new(),
            Some(&earlier),
            current.receipt.as_ref().unwrap(),
            at(1),
            Duration::from_mins(15),
        );
        assert!(settled.is_empty());
    }

    fn report(result: CheckResult, attestation: Option<AttestationResult>) -> UnitReport {
        UnitReport {
            check: Some(UnitCheck {
                result,
                attestations: attestation
                    .map(|result| ("healthy".to_owned(), result))
                    .into_iter()
                    .collect(),
                observations: BTreeMap::new(),
            }),
            ..UnitReport::new("OpenTofu")
        }
    }

    /// Catches CI treating drift, a failing attestation, or a unit that
    /// could not be checked as a clean environment.
    #[test]
    fn test_unit_exit_table() {
        let drifted = || CheckResult::Drifted {
            summary: "x".to_owned(),
        };
        let unknown = || CheckResult::Unknown { reason: "x".to_owned() };
        let cases = [
            (Check::DetectDrift, CheckResult::Clean, None, ExitCategory::Ok),
            (
                Check::DetectDrift,
                CheckResult::NoReceipt,
                None,
                ExitCategory::Incomplete,
            ),
            (
                Check::DetectDrift,
                CheckResult::Unsupported,
                None,
                ExitCategory::Incomplete,
            ),
            (Check::DetectDrift, unknown(), None, ExitCategory::Incomplete),
            (Check::DetectDrift, drifted(), None, ExitCategory::Failed),
            (
                Check::Attest,
                CheckResult::Attested,
                Some(AttestationResult::Pass),
                ExitCategory::Ok,
            ),
            (
                Check::Attest,
                CheckResult::Attested,
                Some(AttestationResult::Fail),
                ExitCategory::Failed,
            ),
            (Check::Attest, CheckResult::NoReceipt, None, ExitCategory::Incomplete),
            (Check::Attest, CheckResult::Unsupported, None, ExitCategory::Incomplete),
            (Check::Attest, unknown(), None, ExitCategory::Incomplete),
        ];
        for (check, result, attestation, expected) in cases {
            let unit = report(result.clone(), attestation);
            assert_eq!(unit_exit(&unit), expected, "{check:?} {result:?} {attestation:?}");
            let codes: Vec<Code> = warnings(
                check,
                &env(),
                &name("network"),
                unit.check.as_ref().unwrap(),
                Some(RecoveryPolicy::Converge),
                DriftRepair::Converge,
            )
            .iter()
            .map(|finding| finding.code)
            .collect();
            let expected_codes: Vec<Code> = match (check, &result) {
                (_, CheckResult::Clean | CheckResult::Attested | CheckResult::Observed) => vec![],
                (Check::DetectDrift, CheckResult::NoReceipt) => vec![Code::DriftNoReceipt],
                (Check::DetectDrift, CheckResult::Unsupported) => vec![Code::DriftUnsupported],
                (Check::DetectDrift, CheckResult::Unknown { .. }) => vec![Code::DriftUnknown],
                (Check::Attest, CheckResult::NoReceipt) => vec![Code::AttestNoReceipt],
                (Check::Attest, CheckResult::Unsupported) => vec![Code::AttestUnsupported],
                (Check::Attest, CheckResult::Unknown { .. }) => vec![Code::AttestUnknown],
                (Check::Observe, CheckResult::NoReceipt) => vec![Code::ObserveNoReceipt],
                (Check::Observe, CheckResult::Unsupported) => vec![Code::ObserveUnsupported],
                (Check::Observe, CheckResult::Unknown { .. }) => vec![Code::DriverError],
                (_, CheckResult::Drifted { .. }) => vec![Code::DriftDetected],
            }
            .into_iter()
            .chain((attestation == Some(AttestationResult::Fail)).then_some(Code::AttestationFailed))
            .collect();
            assert_eq!(codes, expected_codes, "{check:?} {result:?} {attestation:?}");
        }
    }

    /// Catches the drift fix sending an operator to the wrong command: a
    /// plain reconcile for a unit that awaits approval or under `manual`
    /// (it would do nothing), or `--approve` for a unit the next reconcile
    /// repairs anyway.
    #[test]
    fn test_drift_fix_names_what_the_next_reconcile_does() {
        let drifted: UnitCheck = CheckResult::Drifted {
            summary: "1 change".to_owned(),
        }
        .into();
        let fix = |policy, repair| {
            let findings = warnings(Check::DetectDrift, &env(), &name("network"), &drifted, policy, repair);
            assert_eq!(findings.len(), 1);
            assert_eq!(findings[0].code, Code::DriftDetected);
            findings[0].fix.clone()
        };
        let environment = env();
        assert_eq!(
            fix(Some(RecoveryPolicy::Converge), DriftRepair::Converge),
            format!("the next `nyl reconcile -e {environment}` repairs it by executing network again")
        );
        for policy in [Some(RecoveryPolicy::Manual), Some(RecoveryPolicy::Inspect), None] {
            assert_eq!(
                fix(policy, DriftRepair::Converge),
                format!(
                    "it awaits approval, since network is not safe to re-run: run `nyl reconcile -e {environment} --approve network`"
                )
            );
        }
        assert_eq!(
            fix(Some(RecoveryPolicy::Converge), DriftRepair::Manual),
            format!(
                "repair it with `nyl reconcile -e {environment} --unit network --reapply --reason <text>` (driftRepair: manual)"
            )
        );
        assert!(
            fix(Some(RecoveryPolicy::Manual), DriftRepair::Manual).starts_with(&format!(
            "repair it with `nyl reconcile -e {environment} --unit network --reapply --reason <text> --approve network`"
        ))
        );
    }
}
