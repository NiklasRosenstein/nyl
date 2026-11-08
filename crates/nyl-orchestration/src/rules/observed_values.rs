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
//!   the producer's receipt still holds. A consumer in another environment
//!   that resolved another value ([`changed_consumers_reading`], for `observe`
//!   of the producer's environment, and [`changed_consumers_across`], for
//!   `status` of the consumer's) is judged from the typed reference and its
//!   resolved spec, against the snapshots the caller read.
//! - **Reasons.** A consumer's blocked reason reads `observation <name> of
//!   <producer> is pending: <why>`; [`parse_reason`] reads one back for
//!   reports, which show the stable reason [`OBSERVATION_PENDING`] and the
//!   warnings [`warnings`] gives ([`Code::ObservationPending`]).

use std::collections::{BTreeMap, BTreeSet};

use nyl_core::bindings::unit_references;
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

/// The blocked reason of a consumer waiting for `observation` of `producer`,
/// which a cross-environment consumer writes as `<environment>/<unit>`.
pub fn reason(observation: &str, producer: &str, why: &str) -> String {
    format!("observation {observation} of {producer} is pending: {why}")
}

/// An observation reason, read back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaitingFor {
    pub observation: String,
    /// The producer, `<environment>/<unit>` across environments.
    pub producer: String,
    /// The driver's reason.
    pub why: String,
}

/// Read a stored blocked reason that [`reason`] wrote.
pub fn parse_reason(text: &str) -> Option<WaitingFor> {
    let rest = text.strip_prefix("observation ")?;
    let (observation, rest) = rest.split_once(" of ")?;
    let (producer, why) = rest.split_once(" is pending: ")?;
    (!observation.contains(' ') && !producer.contains(' ')).then(|| WaitingFor {
        observation: observation.to_owned(),
        producer: producer.to_owned(),
        why: why.to_owned(),
    })
}

/// The warnings of a blocked unit that waits for observed values: each names
/// the observation, the driver's reason, and what to do about it.
pub fn warnings(unit: &UnitName, environment: &str, reasons: &[BlockedReason]) -> Vec<Finding> {
    reasons
        .iter()
        .filter_map(|blocked| parse_reason(&blocked.reason))
        .map(|waiting| {
            Finding::new(
                Code::ObservationPending,
                format!(
                    "unit {unit} waits for observed value {} of {}: {}",
                    waiting.observation, waiting.producer, waiting.why
                ),
                format!(
                    "once the system holds it, `nyl observe -e {environment}` records it and the next `nyl reconcile -e {environment}` resolves {unit}; `observationWait` on {} makes a run that executes it wait for the value",
                    waiting.producer
                ),
            )
        })
        .collect()
}

/// A consumer whose inputs differ from what is recorded for an observation.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ChangedConsumer {
    pub consumer: UnitName,
    pub producer: UnitName,
    /// The producer's environment when it is not the consumer's: the
    /// consumer reads it through a cross-environment reference.
    pub producer_environment: Option<EnvironmentName>,
    pub observation: String,
    /// The value was not there when the consumer last resolved (it blocked
    /// on it); otherwise it resolved another value.
    pub arrived: bool,
}

impl ChangedConsumer {
    /// The producer as a reference names it: `<environment>/<unit>` across environments.
    pub fn producer_label(&self) -> String {
        match &self.producer_environment {
            Some(environment) => format!("{environment}/{}", self.producer),
            None => self.producer.to_string(),
        }
    }
}

/// What one environment records of its units' observed values: their
/// desired and observed documents and the observation records. A value is
/// usable only as these show it for the producer's current receipt.
#[derive(Clone, Copy)]
pub struct ProducerView<'a> {
    pub desired: &'a BTreeMap<UnitName, DesiredUnit>,
    pub observed: &'a BTreeMap<UnitName, ObservedUnit>,
    pub records: &'a BTreeMap<UnitName, ObservationRecord>,
}

/// What `producer`'s observation holds now in `view`: its value, if usable
/// (the producer has a current receipt, and the value is recorded for it and
/// not older than its `maxAge`).
fn usable_value(view: ProducerView<'_>, producer: &UnitName, observation: &str, now: Timestamp) -> Option<Value> {
    let producer_document = view.desired.get(producer)?;
    if !receipt_is_current(producer_document, view.observed.get(producer)) {
        return None;
    }
    let receipt = view.observed.get(producer)?.receipt.as_ref()?;
    let declaration = declarations(spec_of(producer_document)).ok()?.remove(observation)?;
    match standing(
        producer_document,
        receipt,
        &declaration,
        observation,
        view.records.get(producer),
        now,
    ) {
        Standing::Value { value, .. } => Some(value),
        Standing::Pending { .. } => None,
    }
}

/// [`usable_value`] for a cross-environment reference, which the resolver
/// refuses for a producer being deleted or retained (outside the ownership
/// set): such a producer reads nothing.
fn usable_external(view: ProducerView<'_>, producer: &UnitName, observation: &str, now: Timestamp) -> Option<Value> {
    let outside = view
        .desired
        .get(producer)
        .is_some_and(|unit| matches!(unit.lifecycle, Lifecycle::Deleting { .. } | Lifecycle::Retained { .. }));
    if outside {
        return None;
    }
    usable_value(view, producer, observation, now)
}

/// The observation references of the active consumers in `desired` whose
/// stored resolution differs from what `value_of` reads now, found from the
/// typed references and resolved specs alone. `value_of` receives the
/// producer's environment (`None` for the consumer's own, which includes a
/// reference that names it), the producer, and the observation, and returns
/// the whole value, `None` when there is no usable one.
fn stale_references(
    own: Option<&EnvironmentName>,
    desired: &BTreeMap<UnitName, DesiredUnit>,
    mut value_of: impl FnMut(Option<&EnvironmentName>, &UnitName, &str) -> Option<Value>,
) -> Vec<ChangedConsumer> {
    let mut changed = Vec::new();
    for (consumer, document) in desired {
        if !matches!(document.lifecycle, Lifecycle::Active) {
            continue;
        }
        let Some(resolved) = document.resolved_spec.as_ref() else {
            continue;
        };
        let Ok(references) = unit_references("", &document.spec) else {
            continue;
        };
        for reference in references {
            let source = reference.source;
            let Some(observation) = source.observation.as_deref() else {
                continue;
            };
            let Ok(producer) = UnitName::new(&source.unit) else {
                continue;
            };
            let producer_environment = match source.environment.as_deref() {
                None => None,
                Some(named) if own.is_some_and(|own| own.as_str() == named) => None,
                Some(named) => {
                    let Ok(named) = EnvironmentName::new(named) else {
                        continue;
                    };
                    Some(named)
                }
            };
            let Some(value) = value_of(producer_environment.as_ref(), &producer, observation) else {
                continue;
            };
            let selected = nyl_core::json_pointer::resolve(&value, source.pointer.as_deref().unwrap_or(""));
            if selected != resolved.pointer(&reference.pointer) {
                changed.push(ChangedConsumer {
                    consumer: consumer.clone(),
                    producer,
                    producer_environment,
                    observation: observation.to_owned(),
                    arrived: false,
                });
            }
        }
    }
    changed
}

/// The consumers whose stored resolution of an observation differs from what
/// is recorded now, found from the stored documents alone (no rendering):
///
/// - each active consumer of this environment with a resolved spec, for each
///   of its `fromUnit: {unit, observation}` references whose producer has a
///   current receipt and a usable value that is not the value the consumer
///   resolved;
/// - each active consumer that is blocked only on observations (see
///   [`reason`]) that now all have a usable value: `arrived`.
///
/// A producer without a usable value leaves its consumer to resolution,
/// which blocks it. References to another environment are judged by
/// [`changed_consumers_across`], which has that environment's documents.
pub fn changed_consumers(
    desired: &BTreeMap<UnitName, DesiredUnit>,
    observed: &BTreeMap<UnitName, ObservedUnit>,
    records: &BTreeMap<UnitName, ObservationRecord>,
    now: Timestamp,
) -> Vec<ChangedConsumer> {
    // What a producer's observation holds now: its value, if usable.
    let usable = |producer: &UnitName, observation: &str| -> Option<Value> {
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
                let waits: Vec<_> = reasons.iter().map(|reason| parse_reason(&reason.reason)).collect();
                let arrived: Vec<ChangedConsumer> = waits
                    .iter()
                    .flatten()
                    .filter_map(|waiting| {
                        let producer = UnitName::new(&waiting.producer).ok()?;
                        usable(&producer, &waiting.observation)?;
                        Some(ChangedConsumer {
                            consumer: consumer.clone(),
                            producer,
                            producer_environment: None,
                            observation: waiting.observation.clone(),
                            arrived: true,
                        })
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
            let Some(value) = usable(&producer, observation) else {
                continue;
            };
            let selected = nyl_core::json_pointer::resolve(&value, source.pointer.as_deref().unwrap_or(""));
            if selected != resolved.pointer(&reference.pointer) {
                changed.push(ChangedConsumer {
                    consumer: consumer.clone(),
                    producer,
                    producer_environment: None,
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

/// [`changed_consumers`] for the consumers of `environment`, whose stored
/// documents are `snapshot`, with each of their cross-environment references
/// to an observation compared to what `producers` (snapshots of the
/// environments they name, see [`observed_producer_environments`]) record
/// now: a consumer is ready for its next reconcile when the value it
/// resolved is not the producer's current one.
///
/// Across environments only a changed value is judged, from the typed
/// reference and the consumer's resolved spec. A consumer that waits for
/// another environment's value is not found: its stored blocked reason names
/// `<environment>/<unit>`, which the reason reader cannot attribute to a
/// producer (issue #248), and this never reads it.
pub fn changed_consumers_across(
    environment: &EnvironmentName,
    snapshot: &EnvironmentSnapshot,
    producers: &BTreeMap<EnvironmentName, EnvironmentSnapshot>,
    now: Timestamp,
) -> Vec<ChangedConsumer> {
    let mut changed = changed_consumers(&snapshot.desired, &snapshot.observed, &snapshot.observations, now);
    changed.extend(stale_references(
        Some(environment),
        &snapshot.desired,
        |producer_environment, producer, observation| {
            let producer_snapshot = producers.get(producer_environment?)?;
            usable_external(
                ProducerView {
                    desired: &producer_snapshot.desired,
                    observed: &producer_snapshot.observed,
                    records: &producer_snapshot.observations,
                },
                producer,
                observation,
                now,
            )
        },
    ));
    changed.sort();
    changed.dedup();
    changed
}

/// The environments whose observed values the active consumers in `desired`
/// (the documents of environment `own`) read through cross-environment
/// references: the snapshots [`changed_consumers_across`] needs.
pub fn observed_producer_environments(
    own: &EnvironmentName,
    desired: &BTreeMap<UnitName, DesiredUnit>,
) -> BTreeSet<EnvironmentName> {
    let mut environments = BTreeSet::new();
    stale_references(Some(own), desired, |environment, _, _| {
        environments.extend(environment.cloned());
        None
    });
    environments
}

/// A consumer in `environment` that resolved another value than its
/// producer's environment records now.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ChangedExternalConsumer {
    pub environment: EnvironmentName,
    pub changed: ChangedConsumer,
}

/// The consumers in `consumers` (other environments' snapshots) that read an
/// observed value of `producer_environment` through a cross-environment
/// reference and resolved another value than `view` records now. The
/// documents are the caller's snapshots, so this is read-only: it needs no
/// lease and writes nothing there. As for [`changed_consumers_across`], a
/// consumer that waits for a value is not found.
pub fn changed_consumers_reading(
    producer_environment: &EnvironmentName,
    view: ProducerView<'_>,
    consumers: &BTreeMap<EnvironmentName, EnvironmentSnapshot>,
    now: Timestamp,
) -> Vec<ChangedExternalConsumer> {
    let mut found: Vec<ChangedExternalConsumer> = consumers
        .iter()
        .filter(|(environment, _)| *environment != producer_environment)
        .flat_map(|(environment, snapshot)| {
            stale_references(Some(environment), &snapshot.desired, |named, producer, observation| {
                if named? != producer_environment {
                    return None;
                }
                usable_external(view, producer, observation, now)
            })
            .into_iter()
            .map(|changed| ChangedExternalConsumer {
                environment: environment.clone(),
                changed,
            })
        })
        .collect();
    found.sort();
    found.dedup();
    found
}

/// The `NYL-OBSERVATION-CHANGED` warning of a consumer whose resolved value
/// no longer equals the recorded one: the next reconcile executes it.
/// `environment` is the consumer's; the report of another environment names
/// the consumer with it (`qualify`).
fn changed_warning(
    environment: &EnvironmentName,
    consumer: &UnitName,
    qualify: bool,
    changed: &ChangedConsumer,
) -> Finding {
    let label = if qualify {
        format!("{environment}/{consumer}")
    } else {
        consumer.to_string()
    };
    let producer = changed.producer_label();
    Finding::new(
        Code::ObservationChanged,
        if changed.arrived {
            format!(
                "observed value {} of {producer} has a value now, so {label} can resolve",
                changed.observation
            )
        } else {
            format!(
                "observed value {} of {producer} changed, so {label} resolves another value than it executed with",
                changed.observation
            )
        },
        format!(
            "run `nyl reconcile -e {environment}`: it executes {consumer} with the new value, under its own approval"
        ),
    )
}

/// The warnings of `consumer` in `environment` (the environment of the report).
pub fn changed_warnings(
    environment: &EnvironmentName,
    consumer: &UnitName,
    changed: &[&ChangedConsumer],
) -> Vec<Finding> {
    changed
        .iter()
        .map(|changed| changed_warning(environment, consumer, false, changed))
        .collect()
}

/// The warning of a consumer in another environment than the report's.
pub fn external_changed_warning(changed: &ChangedExternalConsumer) -> Finding {
    changed_warning(&changed.environment, &changed.changed.consumer, true, &changed.changed)
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

    /// Catches a reason that does not survive storage in a desired
    /// document, where status and the reports read it back.
    #[test]
    fn test_reasons_round_trip_with_the_drivers_words() {
        let text = reason(
            "ip",
            "kubernetes",
            "field-not-set: /status/loadBalancer: of the Service",
        );
        assert_eq!(
            parse_reason(&text),
            Some(WaitingFor {
                observation: "ip".to_owned(),
                producer: "kubernetes".to_owned(),
                why: "field-not-set: /status/loadBalancer: of the Service".to_owned()
            })
        );
        assert_eq!(parse_reason("kubernetes has no current receipt"), None);
        assert_eq!(parse_reason("attestation healthy of kubernetes is pending"), None);
        let warnings = warnings(
            &name("dns"),
            "dev",
            &[BlockedReason {
                pointer: "/values/address".to_owned(),
                reason: text,
            }],
        );
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].code, Code::ObservationPending);
        assert!(warnings[0].fix.contains("nyl observe -e dev"), "{}", warnings[0].fix);
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
            changed_consumers(&stale, &observed, &records, at(1)),
            [ChangedConsumer {
                consumer: name("dns"),
                producer: name("kubernetes"),
                producer_environment: None,
                observation: "ip".to_owned(),
                arrived: false
            }]
        );
        let same = desired(consumer(json!({"values": {"address": "203.0.113.20"}})));
        assert!(changed_consumers(&same, &observed, &records, at(1)).is_empty());
        // A producer whose receipt is not current has no value to compare.
        let replaced = BTreeMap::from([(
            name("kubernetes"),
            observed_numbered("kubernetes", 1, Some(hex_key('b'))),
        )]);
        assert!(changed_consumers(&stale, &replaced, &records, at(1)).is_empty());
        // Without a record there is nothing recorded to differ from.
        assert!(changed_consumers(&stale, &observed, &BTreeMap::new(), at(1)).is_empty());

        // A consumer blocked only on the value, which now has one, can resolve.
        let mut waiting = consumer(json!({}));
        waiting.resolved_spec = None;
        waiting.execution_key = None;
        waiting.readiness = Readiness::Blocked {
            reasons: vec![BlockedReason {
                pointer: "/values/address".to_owned(),
                reason: reason("ip", "kubernetes", "no address yet"),
            }],
        };
        let arrived = desired(waiting.clone());
        assert_eq!(
            changed_consumers(&arrived, &observed, &records, at(1)),
            [ChangedConsumer {
                consumer: name("dns"),
                producer: name("kubernetes"),
                producer_environment: None,
                observation: "ip".to_owned(),
                arrived: true
            }]
        );
        // Still blocked when the value is not there, or when something else blocks it too.
        assert!(changed_consumers(&arrived, &observed, &BTreeMap::new(), at(1)).is_empty());
        let mut also = waiting;
        if let Readiness::Blocked { reasons } = &mut also.readiness {
            reasons.push(BlockedReason {
                pointer: "/dependsOn/0".to_owned(),
                reason: "network has no current receipt".to_owned(),
            });
        }
        assert!(changed_consumers(&desired(also), &observed, &records, at(1)).is_empty());
    }

    /// Catches a consumer in another environment left `current` after the
    /// value it resolved from this environment changed (judged from the typed
    /// reference and the resolved spec alone, without reading a blocked
    /// reason), a consumer named when its value is the current one or its
    /// producer is outside the ownership set, and one read from the wrong
    /// environment.
    #[test]
    fn test_changed_consumers_across_environments_compare_the_producers_value_with_the_resolved_one() {
        let dev = EnvironmentName::new("dev").unwrap();
        let prod = EnvironmentName::new("prod").unwrap();
        let mut producer_document = producer();
        producer_document.spec = json!({"observations": {"ip": {"type": "string", "from": {}}}});
        producer_document.resolved_spec = Some(producer_document.spec.clone());
        let receipt = receipt_a();
        let record = settle(&SettleInputs {
            unit: &producer_document,
            receipt: &receipt,
            declared: &declarations(&producer_document.spec).unwrap(),
            stored: None,
            readings: &readings(&[("ip", value(json!("203.0.113.20")))]),
            now: at(0),
            refresh: Duration::from_mins(60),
        })
        .record
        .unwrap();
        let desired = BTreeMap::from([(name("kubernetes"), producer_document)]);
        let observed = BTreeMap::from([(
            name("kubernetes"),
            observed_numbered("kubernetes", 1, Some(hex_key('a'))),
        )]);
        let records = BTreeMap::from([(name("kubernetes"), record)]);
        let producing = ProducerView {
            desired: &desired,
            observed: &observed,
            records: &records,
        };

        let consuming = |resolved: &str, environment: &str| {
            let mut unit = desired_numbered("dns", 2, Some(hex_key('c')));
            unit.spec = json!({"values": {"address": {"fromUnit": {"environment": environment, "unit": "kubernetes", "observation": "ip"}}}});
            unit.resolved_spec = Some(json!({"values": {"address": resolved}}));
            EnvironmentSnapshot {
                desired: BTreeMap::from([(name("dns"), unit)]),
                ..EnvironmentSnapshot::default()
            }
        };
        let expected = ChangedExternalConsumer {
            environment: prod.clone(),
            changed: ChangedConsumer {
                consumer: name("dns"),
                producer: name("kubernetes"),
                producer_environment: Some(dev.clone()),
                observation: "ip".to_owned(),
                arrived: false,
            },
        };
        let stale = BTreeMap::from([(prod.clone(), consuming("203.0.113.10", "dev"))]);
        assert_eq!(
            changed_consumers_reading(&dev, producing, &stale, at(1)),
            std::slice::from_ref(&expected)
        );
        assert_eq!(expected.changed.producer_label(), "dev/kubernetes");
        // The same view from the consumer's environment finds the same consumer.
        let producers = BTreeMap::from([(
            dev.clone(),
            EnvironmentSnapshot {
                desired: desired.clone(),
                observed: observed.clone(),
                observations: records.clone(),
                ..EnvironmentSnapshot::default()
            },
        )]);
        assert_eq!(
            changed_consumers_across(&prod, &stale[&prod], &producers, at(1)),
            std::slice::from_ref(&expected.changed)
        );
        assert_eq!(
            observed_producer_environments(&prod, &stale[&prod].desired),
            BTreeSet::from([dev.clone()])
        );
        // This environment's own consumers are left to the same-environment rule,
        // whether the reference names the environment or not.
        assert!(changed_consumers(&stale[&prod].desired, &observed, &records, at(1)).is_empty());
        let own = BTreeMap::from([(dev.clone(), consuming("203.0.113.10", "dev"))]);
        assert!(changed_consumers_reading(&dev, producing, &own, at(1)).is_empty());
        assert!(observed_producer_environments(&dev, &own[&dev].desired).is_empty());
        // A consumer that resolved the current value, or another environment's, is not named.
        let same = BTreeMap::from([(prod.clone(), consuming("203.0.113.20", "dev"))]);
        assert!(changed_consumers_reading(&dev, producing, &same, at(1)).is_empty());
        let elsewhere = BTreeMap::from([(prod.clone(), consuming("203.0.113.10", "staging"))]);
        assert!(changed_consumers_reading(&dev, producing, &elsewhere, at(1)).is_empty());
        // Without a usable value nothing is compared: resolution blocks the consumer.
        let empty = BTreeMap::new();
        let unrecorded = ProducerView {
            records: &empty,
            ..producing
        };
        assert!(changed_consumers_reading(&dev, unrecorded, &stale, at(1)).is_empty());
        // A producer being deleted is outside the ownership set, which resolution refuses.
        let mut leaving = desired.clone();
        leaving.get_mut(&name("kubernetes")).unwrap().lifecycle = Lifecycle::Retained {
            tombstone: nyl_core::orchestration::state::Tombstone {
                since: at(0),
                receipt: None,
            },
        };
        let retained = ProducerView {
            desired: &leaving,
            ..producing
        };
        assert!(changed_consumers_reading(&dev, retained, &stale, at(1)).is_empty());
    }
}
