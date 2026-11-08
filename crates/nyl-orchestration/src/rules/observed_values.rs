//! Observed values: what the driver reads of a unit's system, what Nyl
//! records of it, and which values consumers may resolve.
//!
//! Contract: [Observed values](../../../../design/orchestration-core.md#observed-values)
//! and [References](../../../../design/orchestration-core.md#references).
//!
//! An observed value is an observation of the unit's *current receipt*, never
//! part of it. Nothing here knows a unit kind, a native tool, or what a
//! driver's `from` source means; the engine owns the declared `type` and
//! `maxAge`, and the driver reports `Value`, `Pending`, or `Failed` for each
//! declared name.
//!
//! - **Declared.** [`declarations`] reads `observations` from a stored spec.
//! - **Recorded.** [`settle`] folds one reading into the stored record of the
//!   unit's receipt. A value is checked against its declared type (a value of
//!   the wrong type is a failed read). A `pending` state replaces a value; a
//!   failed read keeps what was recorded, so the last value for the current
//!   receipt stays usable. A read that changes nothing keeps the stored entry,
//!   and so the stored record, until the entry's last successful read is
//!   older than the refresh interval (or than half the observation's
//!   `maxAge`, when that is shorter), which keeps `maxAge` satisfiable
//!   without a commit per run.
//! - **Lifetime.** A record names the receipt it was read for. [`changes`]
//!   drops the record of a replaced or removed receipt in the commit that
//!   replaces it, so a value read from what ran before never satisfies a
//!   reference.
//! - **Standing.** [`standing`] is what a consumer may resolve: a value
//!   recorded for the producer's current receipt that is not older than its
//!   `maxAge`, else `pending` with the reason.
//! - **Changed.** [`changed_consumers`] finds, from stored documents alone,
//!   the consumers whose resolved value no longer equals the recorded one,
//!   and the ones that wait for a value that has now arrived: they are `ready`
//!   for the next reconcile, which `plan` and `status` show and `nyl observe`
//!   names with `NYL-OBSERVATION-CHANGED`. It is neither drift nor evidence:
//!   the producer's receipt still holds.
//! - **Reasons.** A consumer's blocker is [`Blocker::Observation`], which
//!   names the producer (with its environment across environments), the
//!   observed value, and the driver's reason; its text reads `observation
//!   <name> of <producer> is pending: <why>`. Reports show the stable reason
//!   [`OBSERVATION_PENDING`], and [`warnings`] gives the warnings
//!   ([`Code::ObservationPending`]), all from the typed fields.

use std::collections::{BTreeMap, BTreeSet};

use nyl_core::bindings::unit_references;
use nyl_core::orchestration::blocked::{Blocker, ProducerRef};
use nyl_core::orchestration::codes::{Code, Finding};
use nyl_core::orchestration::driver::{ObservedValue, ObservedValues};
use nyl_core::orchestration::ids::{EnvironmentName, UnitName};
use nyl_core::orchestration::ports::EnvironmentSnapshot;
use nyl_core::orchestration::state::{
    BlockedReason, DesiredUnit, Lifecycle, ObservationRecord, ObservationRecordKind, ObservedUnit, Readiness, Receipt,
    RecordedObservation, StateV1,
};
use nyl_core::orchestration::time::{Duration, Timestamp};
use nyl_core::resources::units::ObservationDeclaration;
use serde_json::Value;

use super::freshness::receipt_is_current;

/// The stable `blockedBy` reason of a consumer waiting for an observed value
/// its producer's current receipt has none of yet.
pub const OBSERVATION_PENDING: &str = "observation-pending";

/// The observations a stored spec declares, by name; none when it declares
/// none. An error is a declaration that does not parse.
pub fn declarations(spec: &Value) -> Result<BTreeMap<String, ObservationDeclaration>, String> {
    match spec.get("observations") {
        None | Some(Value::Null) => Ok(BTreeMap::new()),
        Some(observations) => serde_json::from_value(observations.clone())
            .map_err(|error| format!("spec.observations is invalid: {error}")),
    }
}

/// The spec the engine reads a stored unit's declarations from.
pub fn spec_of(unit: &DesiredUnit) -> &Value {
    unit.resolved_spec.as_ref().unwrap_or(&unit.spec)
}

/// Whether a stored unit declares any observation.
pub fn declares(unit: &DesiredUnit) -> bool {
    declarations(spec_of(unit)).is_ok_and(|declared| !declared.is_empty())
}

/// How long a run that executed `unit` waits for its pending observations;
/// zero when the spec says nothing.
pub fn observation_wait(unit: &DesiredUnit) -> Duration {
    spec_of(unit)
        .get("observationWait")
        .and_then(Value::as_str)
        .and_then(|wait| wait.parse().ok())
        .unwrap_or_default()
}

/// Whether `record` was read for `unit`'s `receipt`: the same incarnation,
/// execution key, and run, so a receipt that replaced another with the same
/// key still starts with every observation pending.
fn belongs_to(record: &ObservationRecord, unit: &DesiredUnit, receipt: &Receipt) -> bool {
    record.execution_key == receipt.execution_key && record.run == receipt.run && record.unit.uid == unit.unit.uid
}

/// What a read left recorded.
#[derive(Debug, Clone, PartialEq)]
pub struct Settled {
    /// The record to store; `None` when nothing is recorded for the receipt.
    pub record: Option<ObservationRecord>,
    /// The observations whose recorded value changed: a value arrived, a
    /// value is another one, or a value became `pending`.
    pub changed: BTreeSet<String>,
    /// The observations whose read failed, with the driver's reason (or the
    /// type mismatch). Their last recorded value stays in the record.
    pub failed: BTreeMap<String, String>,
    /// The observations still without a value, with the reason.
    pub pending: BTreeMap<String, String>,
}

/// What [`settle`] reads.
pub struct SettleInputs<'a> {
    /// The unit whose current `receipt` was read.
    pub unit: &'a DesiredUnit,
    pub receipt: &'a Receipt,
    pub declared: &'a BTreeMap<String, ObservationDeclaration>,
    /// The stored record; used only when it belongs to this receipt.
    pub stored: Option<&'a ObservationRecord>,
    pub readings: &'a ObservedValues,
    pub now: Timestamp,
    /// How old a successful read may get before it is recorded again.
    pub refresh: Duration,
}

/// The record that results from one reading of `inputs.unit`'s current
/// receipt. Contract: [Observed values](../../../../design/orchestration-core.md#observed-values),
/// "Unavailable reads" and "Recording".
pub fn settle(inputs: &SettleInputs<'_>) -> Settled {
    let stored = inputs
        .stored
        .filter(|record| belongs_to(record, inputs.unit, inputs.receipt));
    let mut values = BTreeMap::new();
    let mut changed = BTreeSet::new();
    let mut failed = BTreeMap::new();
    let mut pending = BTreeMap::new();
    for (name, declaration) in inputs.declared {
        let before = stored.and_then(|record| record.values.get(name));
        let reading = inputs
            .readings
            .values
            .get(name)
            .cloned()
            .unwrap_or_else(|| ObservedValue::Failed {
                reason: format!("the driver reported nothing for observation {name}"),
            });
        let reading = match reading {
            ObservedValue::Value { value } if !declaration.observation_type.accepts(&value) => ObservedValue::Failed {
                reason: format!(
                    "the driver read {value} for observation {name}, which is not of its declared type {:?}",
                    declaration.observation_type
                ),
            },
            other => other,
        };
        let entry = match reading {
            ObservedValue::Value { value } => {
                // A value read again is recorded again once its last read is
                // older than the refresh interval, or than half its `maxAge`
                // when that is shorter, so `maxAge` stays satisfiable.
                let refresh = declaration.max_age.map_or(inputs.refresh, |age| {
                    inputs.refresh.min(Duration::from_secs(age.as_secs() / 2))
                });
                let unchanged = matches!(before, Some(RecordedObservation::Value { value: old, observed_at })
                    if *old == value && inputs.now < refresh.after(*observed_at));
                if unchanged {
                    before.cloned()
                } else {
                    let same_value = before.and_then(RecordedObservation::value) == Some(&value);
                    if !same_value {
                        changed.insert(name.clone());
                    }
                    Some(RecordedObservation::Value {
                        value,
                        observed_at: inputs.now,
                    })
                }
            }
            ObservedValue::Pending { reason } => {
                pending.insert(name.clone(), reason.clone());
                match before {
                    // The state is unchanged: a different reason alone writes nothing.
                    Some(RecordedObservation::Pending { .. }) => before.cloned(),
                    Some(RecordedObservation::Value { .. }) => {
                        changed.insert(name.clone());
                        Some(RecordedObservation::Pending { pending: reason })
                    }
                    None => Some(RecordedObservation::Pending { pending: reason }),
                }
            }
            ObservedValue::Failed { reason } => {
                failed.insert(name.clone(), reason);
                before.cloned()
            }
        };
        if let Some(entry) = entry {
            values.insert(name.clone(), entry);
        }
    }
    let record = if values.is_empty() {
        None
    } else if stored.is_some_and(|record| record.values == values) {
        stored.cloned()
    } else {
        Some(ObservationRecord {
            api_version: StateV1,
            kind: ObservationRecordKind,
            unit: inputs.unit.unit.clone(),
            execution_key: inputs.receipt.execution_key.clone(),
            run: inputs.receipt.run.clone(),
            at: inputs.now,
            values,
        })
    };
    Settled {
        record,
        changed,
        failed,
        pending,
    }
}

/// What a consumer may resolve of one observation.
#[derive(Debug, Clone, PartialEq)]
pub enum Standing {
    Value {
        value: Value,
        observed_at: Timestamp,
    },
    /// No usable value, with the reason a consumer's blocked reason shows.
    Pending {
        reason: String,
    },
}

/// The standing of observation `name` of `unit`, whose current receipt is
/// `receipt`, at `now`: the value recorded for that receipt unless it is older
/// than the declaration's `maxAge`.
pub fn standing(
    unit: &DesiredUnit,
    receipt: &Receipt,
    declaration: &ObservationDeclaration,
    name: &str,
    record: Option<&ObservationRecord>,
    now: Timestamp,
) -> Standing {
    let entry = record
        .filter(|record| belongs_to(record, unit, receipt))
        .and_then(|record| record.values.get(name));
    match entry {
        None => Standing::Pending {
            reason: "not observed yet".to_owned(),
        },
        Some(RecordedObservation::Pending { pending }) => Standing::Pending {
            reason: pending.clone(),
        },
        Some(RecordedObservation::Value { value, observed_at }) => match declaration.max_age {
            Some(max_age) if now > max_age.after(*observed_at) => Standing::Pending {
                reason: format!(
                    "its last successful read, at {}, is older than its maxAge of {max_age}",
                    observed_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
                ),
            },
            _ => Standing::Value {
                value: value.clone(),
                observed_at: *observed_at,
            },
        },
    }
}

/// The observation records that a new observed state leaves standing: a unit
/// whose receipt was replaced or removed drops the record of the receipt it
/// replaced, in the same commit, because it no longer describes it (owner of
/// this decision, as for drift records and attestations). `None` removes a
/// record.
pub fn changes(
    stored: &BTreeMap<UnitName, ObservationRecord>,
    observed: &BTreeMap<UnitName, ObservedUnit>,
) -> BTreeMap<UnitName, Option<ObservationRecord>> {
    stored
        .iter()
        .filter(|(unit, record)| {
            let receipt = observed
                .get(*unit)
                .and_then(|observed| observed.receipt.as_ref().map(|r| (observed, r)));
            !receipt.is_some_and(|(observed, receipt)| {
                receipt.execution_key == record.execution_key
                    && receipt.run == record.run
                    && observed.unit.uid == record.unit.uid
            })
        })
        .map(|(unit, _)| (unit.clone(), None))
        .collect()
}

/// The warnings of a blocked unit that waits for observed values: each names
/// the observation, the driver's reason, and what to do about it. The
/// producer reads `<environment>/<unit>` across environments.
pub fn warnings(unit: &UnitName, environment: &str, reasons: &[BlockedReason]) -> Vec<Finding> {
    reasons
        .iter()
        .filter_map(|blocked| match &blocked.blocker {
            Blocker::Observation { producer, name, why } => Some((producer, name, why)),
            _ => None,
        })
        .map(|(producer, name, why)| {
            Finding::new(
                Code::ObservationPending,
                format!("unit {unit} waits for observed value {name} of {producer}: {why}"),
                format!(
                    "once the system holds it, `nyl observe -e {environment}` records it and the next `nyl reconcile -e {environment}` resolves {unit}; `observationWait` on {producer} makes a run that executes it wait for the value"
                ),
            )
        })
        .collect()
}

/// A consumer whose inputs differ from what is recorded for an observation.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ChangedConsumer {
    pub consumer: UnitName,
    pub producer: ProducerRef,
    pub observation: String,
    /// The value was not there when the consumer last resolved (it blocked
    /// on it); otherwise it resolved another value.
    pub arrived: bool,
}

/// The consumers whose stored resolution of an observation differs from what
/// is recorded now, found from the stored documents alone (no rendering):
///
/// - each active consumer of this environment with a resolved spec, for each
///   of its `fromUnit: {unit, observation}` references whose producer has a
///   current receipt and a usable value that is not the value the consumer
///   resolved;
/// - each active consumer that is blocked only on observations (see
///   [`Blocker::Observation`]) that now all have a usable value: `arrived`.
///   A blocker names its producer with its environment ([`ProducerRef`]); a
///   producer of another environment is read from `foreign`, the snapshots of
///   the environments the caller loaded, and never from `desired`, so a
///   same-named unit of this environment cannot stand in for it.
///
/// A producer without a usable value, or of an environment not in
/// `foreign`, leaves its consumer to resolution, which blocks it. A consumer
/// that resolved a value of another environment's producer is not compared
/// here.
pub fn changed_consumers(
    desired: &BTreeMap<UnitName, DesiredUnit>,
    observed: &BTreeMap<UnitName, ObservedUnit>,
    records: &BTreeMap<UnitName, ObservationRecord>,
    foreign: &BTreeMap<EnvironmentName, EnvironmentSnapshot>,
    now: Timestamp,
) -> Vec<ChangedConsumer> {
    // What a producer's observation holds now: its value, if usable.
    let usable = |producer: &ProducerRef, observation: &str| -> Option<Value> {
        let (desired, observed, records) = match &producer.environment {
            None => (desired, observed, records),
            Some(environment) => {
                let snapshot = foreign.get(environment)?;
                (&snapshot.desired, &snapshot.observed, &snapshot.observations)
            }
        };
        let producer = &producer.unit;
        let producer_document = desired.get(producer)?;
        if !receipt_is_current(producer_document, observed.get(producer)) {
            return None;
        }
        let receipt = observed.get(producer)?.receipt.as_ref()?;
        let declaration = declarations(spec_of(producer_document)).ok()?.remove(observation)?;
        match standing(
            producer_document,
            receipt,
            &declaration,
            observation,
            records.get(producer),
            now,
        ) {
            Standing::Value { value, .. } => Some(value),
            Standing::Pending { .. } => None,
        }
    };
    let mut changed = Vec::new();
    for (consumer, document) in desired {
        if !matches!(document.lifecycle, Lifecycle::Active) {
            continue;
        }
        let Some(resolved) = document.resolved_spec.as_ref() else {
            // Blocked only on observations that now have values.
            if let Readiness::Blocked { reasons } = &document.readiness {
                let arrived: Vec<ChangedConsumer> = reasons
                    .iter()
                    .filter_map(|reason| match &reason.blocker {
                        Blocker::Observation { producer, name, .. } => {
                            usable(producer, name)?;
                            Some(ChangedConsumer {
                                consumer: consumer.clone(),
                                producer: producer.clone(),
                                observation: name.clone(),
                                arrived: true,
                            })
                        }
                        _ => None,
                    })
                    .collect();
                if !reasons.is_empty() && arrived.len() == reasons.len() {
                    changed.extend(arrived);
                }
            }
            continue;
        };
        let Ok(references) = unit_references("", &document.spec) else {
            continue;
        };
        for reference in references {
            let source = reference.source;
            let Some(observation) = source.observation.as_deref().filter(|_| source.environment.is_none()) else {
                continue;
            };
            let Ok(producer) = UnitName::new(&source.unit) else {
                continue;
            };
            let producer = ProducerRef::local(producer);
            let Some(value) = usable(&producer, observation) else {
                continue;
            };
            let selected = nyl_core::json_pointer::resolve(&value, source.pointer.as_deref().unwrap_or(""));
            if selected != resolved.pointer(&reference.pointer) {
                changed.push(ChangedConsumer {
                    consumer: consumer.clone(),
                    producer,
                    observation: observation.to_owned(),
                    arrived: false,
                });
            }
        }
    }
    changed.sort();
    changed.dedup();
    changed
}

/// The `NYL-OBSERVATION-CHANGED` warning of a consumer whose resolved value
/// no longer equals the recorded one: the next reconcile executes it.
pub fn changed_warnings(
    environment: &nyl_core::orchestration::ids::EnvironmentName,
    consumer: &UnitName,
    changed: &[&ChangedConsumer],
) -> Vec<Finding> {
    changed
        .iter()
        .map(|changed| {
            Finding::new(
                Code::ObservationChanged,
                if changed.arrived {
                    format!(
                        "observed value {} of {} has a value now, so {consumer} can resolve",
                        changed.observation, changed.producer
                    )
                } else {
                    format!(
                        "observed value {} of {} changed, so {consumer} resolves another value than it executed with",
                        changed.observation, changed.producer
                    )
                },
                format!("run `nyl reconcile -e {environment}`: it executes {consumer} with the new value, under its own approval"),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use nyl_core::orchestration::state::UnitIdentity;

    use super::super::fixtures::{desired_numbered, hex_key, name, observed_numbered};
    use super::*;

    fn at(minutes: i64) -> Timestamp {
        "2026-01-01T00:00:00Z".parse::<Timestamp>().unwrap() + chrono::Duration::minutes(minutes)
    }

    fn producer() -> DesiredUnit {
        let mut unit = desired_numbered("kubernetes", 1, Some(hex_key('a')));
        unit.spec = json!({"observations": {
            "ip": {"type": "string", "from": {"any": "thing"}},
            "port": {"type": "integer", "maxAge": "10m", "from": {}}
        }});
        unit.resolved_spec = Some(unit.spec.clone());
        unit
    }

    fn declared() -> BTreeMap<String, ObservationDeclaration> {
        declarations(&producer().spec).unwrap()
    }

    fn readings(values: &[(&str, ObservedValue)]) -> ObservedValues {
        ObservedValues {
            values: values
                .iter()
                .map(|(name, value)| ((*name).to_owned(), value.clone()))
                .collect(),
        }
    }

    fn value(value: Value) -> ObservedValue {
        ObservedValue::Value { value }
    }

    fn pending(reason: &str) -> ObservedValue {
        ObservedValue::Pending {
            reason: reason.to_owned(),
        }
    }

    fn failed(reason: &str) -> ObservedValue {
        ObservedValue::Failed {
            reason: reason.to_owned(),
        }
    }

    /// The producer's current receipt: key `a`, run [`RUN`](super::super::fixtures::RUN).
    fn receipt_a() -> Receipt {
        observed_numbered("kubernetes", 1, Some(hex_key('a'))).receipt.unwrap()
    }

    fn settled(stored: Option<&ObservationRecord>, readings: &ObservedValues, now: Timestamp) -> Settled {
        let unit = producer();
        let receipt = receipt_a();
        settle(&SettleInputs {
            unit: &unit,
            receipt: &receipt,
            declared: &declared(),
            stored,
            readings,
            now,
            refresh: Duration::from_mins(60),
        })
    }

    /// Catches a declaration the driver owns being read by the engine, or
    /// a unit without observations reading as declaring some.
    #[test]
    fn test_declarations_read_type_and_age_and_leave_from_alone() {
        let declared = declared();
        assert_eq!(declared.len(), 2);
        assert_eq!(declared["port"].max_age, Some(Duration::from_mins(10)));
        assert_eq!(declared["ip"].from, json!({"any": "thing"}));
        assert!(declares(&producer()));
        assert!(!declares(&desired_numbered("network", 1, Some(hex_key('a')))));
        assert!(declarations(&json!({"observations": {"ip": {"type": "string"}}})).is_err());
    }

    /// Catches a pending observation never reaching a value, and a value
    /// not being listed as changed when it arrives: consumers resolve from
    /// what is recorded, and the next reconcile only runs them when told.
    #[test]
    fn test_a_value_arriving_after_pending_is_recorded_and_changed() {
        let first = settled(
            None,
            &readings(&[("ip", pending("no address")), ("port", pending("no port"))]),
            at(1),
        );
        let record = first.record.clone().unwrap();
        assert_eq!(
            record.values["ip"],
            RecordedObservation::Pending {
                pending: "no address".to_owned()
            }
        );
        assert!(first.changed.is_empty(), "pending alone is not a change");
        assert_eq!(first.pending.len(), 2);

        let next = settled(
            Some(&record),
            &readings(&[("ip", value(json!("203.0.113.10"))), ("port", pending("no port"))]),
            at(2),
        );
        assert_eq!(next.changed, BTreeSet::from(["ip".to_owned()]));
        let record = next.record.unwrap();
        assert_eq!(
            record.values["ip"],
            RecordedObservation::Value {
                value: json!("203.0.113.10"),
                observed_at: at(2)
            }
        );
        assert_eq!(record.at, at(2));
    }

    /// Catches a read that changed nothing writing a commit only because
    /// time moved on, and a value older than the refresh interval never
    /// being read again, which would make `maxAge` unsatisfiable.
    #[test]
    fn test_an_unchanged_read_keeps_the_record_until_the_refresh_interval() {
        let reading = readings(&[("ip", value(json!("203.0.113.10"))), ("port", value(json!(443)))]);
        let mut record = settled(None, &reading, at(0)).record.unwrap();
        // `port` has a maxAge of 10m and is refreshed sooner (see below); keep it fresh here.
        record.values.insert(
            "port".to_owned(),
            RecordedObservation::Value {
                value: json!(443),
                observed_at: at(29),
            },
        );
        let again = settled(Some(&record), &reading, at(30));
        assert_eq!(
            again.record.as_ref().map(|record| record.values["ip"].clone()),
            Some(record.values["ip"].clone()),
            "within the interval the value keeps its time"
        );
        assert!(again.changed.is_empty());
        let refreshed = settled(Some(&record), &reading, at(61)).record.unwrap();
        assert_eq!(
            refreshed.values["ip"],
            RecordedObservation::Value {
                value: json!("203.0.113.10"),
                observed_at: at(61)
            }
        );
        assert!(
            settled(Some(&record), &reading, at(61)).changed.is_empty(),
            "re-reading the same value is no change"
        );
        // Nothing but time moved: the stored record is kept as it is.
        let steady = readings(&[("ip", value(json!("203.0.113.10"))), ("port", value(json!(443)))]);
        let stored = settled(None, &steady, at(0)).record.unwrap();
        assert_eq!(settled(Some(&stored), &steady, at(4)).record.as_ref(), Some(&stored));
    }

    /// Catches a `maxAge` shorter than the refresh interval that no read can
    /// satisfy, because an unchanged value kept its old time.
    #[test]
    fn test_a_short_max_age_is_refreshed_by_half_of_it() {
        let reading = readings(&[("ip", value(json!("203.0.113.10"))), ("port", value(json!(443)))]);
        let record = settled(None, &reading, at(0)).record.unwrap();
        // `port` has a maxAge of 10m: a read 6 minutes later records it again,
        // while `ip`, which never goes stale, keeps its time.
        let later = settled(Some(&record), &reading, at(6)).record.unwrap();
        assert_eq!(
            later.values["port"],
            RecordedObservation::Value {
                value: json!(443),
                observed_at: at(6)
            }
        );
        assert_eq!(later.values["ip"], record.values["ip"]);
        assert_eq!(settled(Some(&record), &reading, at(4)).record.as_ref(), Some(&record));
    }

    /// Catches a failed read erasing the last value, which would block
    /// consumers whenever the system cannot be reached, and a value of the
    /// wrong type being recorded.
    #[test]
    fn test_a_failed_read_keeps_the_last_value_and_a_wrong_type_fails() {
        let record = settled(
            None,
            &readings(&[("ip", value(json!("203.0.113.10"))), ("port", value(json!(443)))]),
            at(0),
        )
        .record
        .unwrap();
        let outcome = settled(
            Some(&record),
            &readings(&[("ip", failed("no credentials")), ("port", value(json!("https")))]),
            at(5),
        );
        assert_eq!(outcome.record.as_ref(), Some(&record));
        assert_eq!(outcome.failed["ip"], "no credentials");
        assert!(outcome.failed["port"].contains("declared type"), "{:?}", outcome.failed);
        assert!(outcome.changed.is_empty());
        let missing = settled(Some(&record), &readings(&[]), at(5));
        assert_eq!(missing.failed.len(), 2, "a name the driver left out failed");
    }

    /// Catches a value that went away still satisfying a reference: a
    /// pending read after a value replaces it.
    #[test]
    fn test_a_value_that_becomes_pending_replaces_the_value() {
        let record = settled(
            None,
            &readings(&[("ip", value(json!("203.0.113.10"))), ("port", value(json!(443)))]),
            at(0),
        )
        .record
        .unwrap();
        let outcome = settled(
            Some(&record),
            &readings(&[("ip", pending("address removed")), ("port", value(json!(443)))]),
            at(2),
        );
        assert_eq!(outcome.changed, BTreeSet::from(["ip".to_owned()]));
        assert_eq!(
            outcome.record.unwrap().values["ip"],
            RecordedObservation::Pending {
                pending: "address removed".to_owned()
            }
        );
    }

    /// Catches a value read for another receipt or another incarnation
    /// resolving for this one.
    #[test]
    fn test_standing_requires_the_receipt_and_honors_max_age() {
        let unit = producer();
        let key = receipt_a();
        let record = settled(
            None,
            &readings(&[("ip", value(json!("203.0.113.10"))), ("port", value(json!(443)))]),
            at(0),
        )
        .record
        .unwrap();
        let declared = declared();
        let standing_of = |name: &str, record: Option<&ObservationRecord>, key: &Receipt, now| {
            standing(&unit, key, &declared[name], name, record, now)
        };
        assert_eq!(
            standing_of("ip", Some(&record), &key, at(50)),
            Standing::Value {
                value: json!("203.0.113.10"),
                observed_at: at(0)
            }
        );
        let other_key = observed_numbered("kubernetes", 1, Some(hex_key('b'))).receipt.unwrap();
        assert!(matches!(
            standing_of("ip", Some(&record), &other_key, at(1)),
            Standing::Pending { .. }
        ));
        // The same key, produced by another run: a new receipt, so nothing is read for it yet.
        let mut reexecuted = receipt_a();
        reexecuted.run = super::super::fixtures::run(super::super::fixtures::RUN_2);
        assert!(matches!(
            standing_of("ip", Some(&record), &reexecuted, at(1)),
            Standing::Pending { .. }
        ));
        assert!(matches!(standing_of("ip", None, &key, at(1)), Standing::Pending { .. }));
        // `port` stops resolving 10 minutes after its last successful read.
        assert!(matches!(
            standing_of("port", Some(&record), &key, at(10)),
            Standing::Value { .. }
        ));
        let Standing::Pending { reason } = standing_of("port", Some(&record), &key, at(11)) else {
            panic!("older than maxAge");
        };
        assert!(reason.contains("maxAge of 10m"), "{reason}");
        let mut other = record.clone();
        other.unit = UnitIdentity {
            uid: Some(super::super::fixtures::uid_numbered(9)),
            ..other.unit
        };
        assert!(matches!(
            standing_of("ip", Some(&other), &key, at(1)),
            Standing::Pending { .. }
        ));
    }

    /// Catches an old receipt's values surviving the receipt that replaced
    /// it, which would let a value read from what ran before satisfy a
    /// reference.
    #[test]
    fn test_changes_drop_the_records_of_replaced_receipts() {
        let record = settled(
            None,
            &readings(&[("ip", value(json!("203.0.113.10"))), ("port", value(json!(443)))]),
            at(0),
        )
        .record
        .unwrap();
        let stored = BTreeMap::from([(name("kubernetes"), record)]);
        let current = BTreeMap::from([(
            name("kubernetes"),
            observed_numbered("kubernetes", 1, Some(hex_key('a'))),
        )]);
        assert!(changes(&stored, &current).is_empty());
        let replaced = BTreeMap::from([(
            name("kubernetes"),
            observed_numbered("kubernetes", 1, Some(hex_key('b'))),
        )]);
        assert_eq!(
            changes(&stored, &replaced),
            BTreeMap::from([(name("kubernetes"), None)])
        );
        // A re-execution with the same key is a new receipt too: its run differs.
        let mut reexecuted = observed_numbered("kubernetes", 1, Some(hex_key('a')));
        reexecuted.receipt.as_mut().unwrap().run = super::super::fixtures::run(super::super::fixtures::RUN_2);
        assert_eq!(
            changes(&stored, &BTreeMap::from([(name("kubernetes"), reexecuted)])),
            BTreeMap::from([(name("kubernetes"), None)])
        );
        assert_eq!(
            changes(&stored, &BTreeMap::new()),
            BTreeMap::from([(name("kubernetes"), None)])
        );
        let recreated = BTreeMap::from([(
            name("kubernetes"),
            observed_numbered("kubernetes", 2, Some(hex_key('a'))),
        )]);
        assert_eq!(
            changes(&stored, &recreated),
            BTreeMap::from([(name("kubernetes"), None)])
        );
    }

    fn waiting_for(producer: &str, observation: &str, why: &str) -> Blocker {
        Blocker::Observation {
            producer: ProducerRef::parse(producer).unwrap(),
            name: observation.to_owned(),
            why: why.to_owned(),
        }
    }

    /// Catches warnings losing what the consumer waits for: the observation,
    /// the driver's words (colons and all), the producer as its text names it
    /// (with the environment across environments), and the stable code; other
    /// blockers give none.
    #[test]
    fn test_warnings_name_the_observation_the_producer_and_the_drivers_words() {
        let why = "field-not-set: /status/loadBalancer: of the Service";
        let warnings = warnings(
            &name("dns"),
            "dev",
            &[
                BlockedReason::new("/values/address", waiting_for("kubernetes", "ip", why)),
                BlockedReason::new("/values/other", waiting_for("infra/kubernetes", "port", "none yet")),
                BlockedReason::new(
                    "/dependsOn/0",
                    Blocker::no_current_receipt(ProducerRef::parse("network").unwrap()),
                ),
            ],
        );
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings.iter().all(|finding| finding.code == Code::ObservationPending));
        assert_eq!(
            warnings[0].message,
            format!("unit dns waits for observed value ip of kubernetes: {why}")
        );
        assert!(warnings[0].fix.contains("nyl observe -e dev"), "{}", warnings[0].fix);
        assert!(
            warnings[1].message.contains("port of infra/kubernetes"),
            "{}",
            warnings[1].message
        );
        assert!(
            warnings[1].fix.contains("`observationWait` on infra/kubernetes"),
            "{}",
            warnings[1].fix
        );
    }

    fn consumer(resolved: Value) -> DesiredUnit {
        let mut unit = desired_numbered("dns", 2, Some(hex_key('c')));
        unit.spec = json!({"values": {"address": {"fromUnit": {"unit": "kubernetes", "observation": "ip", "pointer": "/host"}}}});
        unit.resolved_spec = Some(resolved);
        unit
    }

    /// Catches a consumer left `current` after the value it resolved
    /// changed, which `status` and `nyl observe` must show as ready, and a
    /// consumer reported although the pointer's selection is unchanged.
    #[test]
    fn test_changed_consumers_compare_the_selected_value_with_the_resolved_one() {
        let producer_document = producer();
        let mut declared_object = producer_document.clone();
        declared_object.spec = json!({"observations": {"ip": {"type": "object", "from": {}}}});
        declared_object.resolved_spec = Some(declared_object.spec.clone());
        let desired = |consumer: DesiredUnit| {
            BTreeMap::from([(name("kubernetes"), declared_object.clone()), (name("dns"), consumer)])
        };
        let observed = BTreeMap::from([(
            name("kubernetes"),
            observed_numbered("kubernetes", 1, Some(hex_key('a'))),
        )]);
        let unit = declared_object.clone();
        let receipt = receipt_a();
        let declaration = declarations(&unit.spec).unwrap();
        let record = settle(&SettleInputs {
            unit: &unit,
            receipt: &receipt,
            declared: &declaration,
            stored: None,
            readings: &readings(&[("ip", value(json!({"host": "203.0.113.20", "other": 1})))]),
            now: at(0),
            refresh: Duration::from_mins(60),
        })
        .record
        .unwrap();
        let records = BTreeMap::from([(name("kubernetes"), record)]);

        let stale = desired(consumer(json!({"values": {"address": "203.0.113.10"}})));
        assert_eq!(
            changed_consumers(&stale, &observed, &records, &BTreeMap::new(), at(1)),
            [ChangedConsumer {
                consumer: name("dns"),
                producer: ProducerRef::local(name("kubernetes")),
                observation: "ip".to_owned(),
                arrived: false
            }]
        );
        let same = desired(consumer(json!({"values": {"address": "203.0.113.20"}})));
        assert!(changed_consumers(&same, &observed, &records, &BTreeMap::new(), at(1)).is_empty());
        // A producer whose receipt is not current has no value to compare.
        let replaced = BTreeMap::from([(
            name("kubernetes"),
            observed_numbered("kubernetes", 1, Some(hex_key('b'))),
        )]);
        assert!(changed_consumers(&stale, &replaced, &records, &BTreeMap::new(), at(1)).is_empty());
        // Without a record there is nothing recorded to differ from.
        assert!(changed_consumers(&stale, &observed, &BTreeMap::new(), &BTreeMap::new(), at(1)).is_empty());

        // A consumer blocked only on the value, which now has one, can resolve.
        let mut waiting = consumer(json!({}));
        waiting.resolved_spec = None;
        waiting.execution_key = None;
        waiting.readiness = Readiness::Blocked {
            reasons: vec![BlockedReason::new(
                "/values/address",
                waiting_for("kubernetes", "ip", "no address yet"),
            )],
        };
        let arrived = desired(waiting.clone());
        assert_eq!(
            changed_consumers(&arrived, &observed, &records, &BTreeMap::new(), at(1)),
            [ChangedConsumer {
                consumer: name("dns"),
                producer: ProducerRef::local(name("kubernetes")),
                observation: "ip".to_owned(),
                arrived: true
            }]
        );
        // Still blocked when the value is not there, or when something else blocks it too.
        assert!(changed_consumers(&arrived, &observed, &BTreeMap::new(), &BTreeMap::new(), at(1)).is_empty());
        let mut also = waiting;
        if let Readiness::Blocked { reasons } = &mut also.readiness {
            reasons.push(BlockedReason::new(
                "/dependsOn/0",
                Blocker::no_current_receipt(ProducerRef::local(name("network"))),
            ));
        }
        assert!(changed_consumers(&desired(also), &observed, &records, &BTreeMap::new(), at(1)).is_empty());
    }

    /// Catches a consumer that waits for an observed value of another
    /// environment's unit never being recognized as able to resolve when the
    /// value arrives (the producer was once read back from text that
    /// `UnitName` rejects), and a same-named unit of the consumer's own
    /// environment standing in for the other environment's producer.
    #[test]
    fn test_a_consumer_waiting_for_another_environments_value_resolves_when_it_arrives() {
        let infra = EnvironmentName::new("infra").unwrap();
        let mut declared_object = producer();
        declared_object.spec = json!({"observations": {"ip": {"type": "object", "from": {}}}});
        declared_object.resolved_spec = Some(declared_object.spec.clone());
        let receipt = receipt_a();
        let record = settle(&SettleInputs {
            unit: &declared_object,
            receipt: &receipt,
            declared: &declarations(&declared_object.spec).unwrap(),
            stored: None,
            readings: &readings(&[("ip", value(json!({"host": "203.0.113.20"})))]),
            now: at(0),
            refresh: Duration::from_mins(60),
        })
        .record
        .unwrap();
        let producer_current = observed_numbered("kubernetes", 1, Some(hex_key('a')));
        let infra_with_value = EnvironmentSnapshot {
            desired: BTreeMap::from([(name("kubernetes"), declared_object.clone())]),
            observed: BTreeMap::from([(name("kubernetes"), producer_current.clone())]),
            observations: BTreeMap::from([(name("kubernetes"), record.clone())]),
            ..EnvironmentSnapshot::default()
        };
        let infra_without_value = EnvironmentSnapshot {
            observations: BTreeMap::new(),
            ..infra_with_value.clone()
        };
        let mut waiting = consumer(json!({}));
        waiting.resolved_spec = None;
        waiting.execution_key = None;
        waiting.readiness = Readiness::Blocked {
            reasons: vec![BlockedReason::new(
                "/values/address",
                waiting_for("infra/kubernetes", "ip", "no address yet"),
            )],
        };
        let app = BTreeMap::from([(name("dns"), waiting)]);
        let none = BTreeMap::new();
        let changed = |foreign: &BTreeMap<EnvironmentName, EnvironmentSnapshot>| {
            changed_consumers(&app, &BTreeMap::new(), &none, foreign, at(1))
        };

        assert_eq!(
            changed(&BTreeMap::from([(infra.clone(), infra_with_value)])),
            [ChangedConsumer {
                consumer: name("dns"),
                producer: ProducerRef::parse("infra/kubernetes").unwrap(),
                observation: "ip".to_owned(),
                arrived: true
            }]
        );
        assert!(changed(&BTreeMap::from([(infra, infra_without_value)])).is_empty());
        assert!(
            changed(&BTreeMap::new()).is_empty(),
            "an environment that was not loaded"
        );

        // The consumer's own environment holds a `kubernetes` with a value, which is not the producer.
        let local = BTreeMap::from([(name("kubernetes"), declared_object)]);
        let local_observed = BTreeMap::from([(name("kubernetes"), producer_current)]);
        let mut with_local = app.clone();
        with_local.extend(local);
        let stored_here = BTreeMap::from([(name("kubernetes"), record)]);
        assert!(changed_consumers(&with_local, &local_observed, &stored_here, &BTreeMap::new(), at(1)).is_empty());
    }
}
