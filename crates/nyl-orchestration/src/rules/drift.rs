//! Drift repair: which units with a current receipt execute again because
//! their latest drift observation is `drifted` or `--reapply` forces them,
//! the `drifted` unit state, and the summary's `driftRepaired` and
//! `reapplied`.
//!
//! Contract: [Drift](../../../../design/orchestration-core.md#drift) and
//! [Execution](../../../../design/orchestration-core.md#execution), step 3,
//! as amended by the part 5 decisions:
//! - `drifted` is a unit state in status, plan, and reports: work for the
//!   next reconcile, like a changed unit (`nyl status` exits 2; `nyl plan`
//!   lists it as a unit that would execute).
//! - `driftRepair: converge` (the default): the next reconcile executes
//!   drifted units whose recovery policy is `converge`. A drifted unit that
//!   is not safe to re-run is `awaiting-approval` (exit 2) while the rest
//!   of the run continues; a `--local` run executes it with `--approve`.
//! - `driftRepair: manual`: reconcile repairs no drift by itself; drifted
//!   units stay `drifted` until a forced run.
//! - `nyl reconcile --unit <u>… --reapply --reason <text>` forces the named
//!   units to execute although they are current; a non-converging named
//!   unit follows the approval rule above and never fails the run.
//! - A new receipt drops the unit's drift observation in the same commit.
//!
//! - `nyl status` reports a current unit whose latest drift observation of
//!   its receipt is `drifted` as `drifted` ([`status_drift`]), whatever the
//!   recovery policy and `driftRepair`.

use std::collections::BTreeMap;

use nyl_core::orchestration::driver::DriverSet;
use nyl_core::orchestration::ids::UnitName;
use nyl_core::orchestration::ports::EnvironmentSnapshot;
use nyl_core::orchestration::state::{
    Condition, DesiredUnit, Drift, EnvironmentRecord, Observation, ObservationType, ObservedUnit,
};
use nyl_core::resources::environment::{DriftRepair, Environment};

use super::freshness::{receipt_is_current, same_incarnation};
use super::selection::Candidates;
use crate::run_report::DriftAttribute;

/// The environment's effective `driftRepair`: as S declares it (group
/// defaults applied by `effective_environment`), else `converge`.
pub fn drift_repair(environment: &Environment) -> DriftRepair {
    environment.spec.drift_repair.unwrap_or_default()
}

/// The `driftRepair` the environment's state records (`environment.yaml`,
/// written by the last run from the S-rendered Environment with group
/// defaults applied), else `converge`. Commands that render nothing, such
/// as `nyl verify` and `nyl status`, read it here.
pub fn stored_drift_repair(record: Option<&EnvironmentRecord>) -> DriftRepair {
    record
        .and_then(|record| record.spec.get("driftRepair"))
        .and_then(|value| serde_json::from_value::<DriftRepair>(value.clone()).ok())
        .unwrap_or_default()
}

/// Why a unit with a current receipt was selected again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Repair {
    /// Its latest drift observation is `drifted`, under `driftRepair: converge`
    /// and a `converge` recovery policy: listed under `driftRepaired`.
    Drift,
    /// `--reapply` forced it (a unit named with `--unit`): listed under `reapplied`.
    Reapply,
}

/// Why `desired`, whose receipt is current, is a candidate again: `--reapply`
/// names it (that wins), or its latest drift observation of the current
/// receipt is `drifted`. `None` keeps it current. The caller,
/// [`super::selection::select`], decides under `driftRepair` and the
/// recovery policy whether a candidate is selected.
pub(crate) fn repair(
    candidates: &Candidates<'_>,
    name: &UnitName,
    desired: &DesiredUnit,
    observed: Option<&ObservedUnit>,
) -> Option<Repair> {
    if !receipt_is_current(desired, observed) {
        return None;
    }
    if candidates.reapply.contains(name) {
        return Some(Repair::Reapply);
    }
    let drift = latest_drift(desired, observed, candidates.observations.get(name))?;
    matches!(drift.result, Drift::Drifted { .. }).then_some(Repair::Drift)
}

/// The drift observation about `observed`'s receipt, which must be current
/// for `desired`: an observation of another incarnation or another
/// execution key no longer describes it.
fn latest_drift<'o>(
    desired: &DesiredUnit,
    observed: Option<&ObservedUnit>,
    observations: Option<&'o BTreeMap<ObservationType, Observation>>,
) -> Option<&'o Observation> {
    let receipt = observed?.receipt.as_ref()?;
    observations?
        .get(&ObservationType::Drift)
        .filter(|drift| drift.unit.uid == desired.unit.uid && drift.execution_key == receipt.execution_key)
}

/// Of the units [`super::selection::select`] made ready, those it selected
/// only for drift repair or `--reapply`. A unit selected because of an
/// `uncertain` condition is not a repair, even with a current receipt.
pub fn repairs(candidates: &Candidates<'_>, ready: &[UnitName]) -> BTreeMap<UnitName, Repair> {
    ready
        .iter()
        .filter_map(|name| {
            let desired = candidates.desired.get(name)?;
            let observed = candidates.observed.get(name);
            let uncertain = same_incarnation(desired, observed)
                .is_some_and(|observed| matches!(observed.condition, Some(Condition::Uncertain { .. })));
            if uncertain {
                return None;
            }
            repair(candidates, name, desired, observed).map(|repair| (name.clone(), repair))
        })
        .collect()
}

/// The observation changes of a transition: a unit whose receipt this run
/// replaced loses the drift observation about the old receipt; its other
/// observations stay. The value is the unit's complete remaining set, or
/// `None` when nothing remains. Units whose observed file the transition
/// removes lose every observation through the lifecycle changes, which the
/// run path applies after these.
pub fn observation_changes(
    snapshot: &EnvironmentSnapshot,
    observed: &BTreeMap<UnitName, ObservedUnit>,
) -> BTreeMap<UnitName, Option<BTreeMap<ObservationType, Observation>>> {
    observed
        .iter()
        .filter(|(name, after)| {
            after.receipt.is_some()
                && snapshot.observed.get(*name).and_then(|before| before.receipt.as_ref()) != after.receipt.as_ref()
        })
        .filter_map(|(name, _)| {
            let stored = snapshot.observations.get(name)?;
            if !stored.contains_key(&ObservationType::Drift) {
                return None;
            }
            let remaining: BTreeMap<_, _> = stored
                .iter()
                .filter(|(observation_type, _)| **observation_type != ObservationType::Drift)
                .map(|(observation_type, observation)| (*observation_type, observation.clone()))
                .collect();
            Some((name.clone(), (!remaining.is_empty()).then_some(remaining)))
        })
        .collect()
}

/// The drift attribute `status` and `get units -e` show for a unit with a
/// current receipt: its latest drift observation's result (`clean`,
/// `drifted`, or `unknown`). A unit whose result is `drifted` has the unit
/// state `drifted` ([`crate::run_report::UnitState::Drifted`]);
/// `rules::unit_state` sets it from this attribute. `None` without a
/// current receipt or without an observation of it. The state does not
/// depend on the recovery policy or `driftRepair`: a drifted unit is work
/// for the next reconcile either way (repaired, awaiting approval, or
/// awaiting `--reapply`), so `drivers` and `repair` do not change it.
pub fn status_drift(
    desired: &DesiredUnit,
    observed: Option<&ObservedUnit>,
    observations: Option<&BTreeMap<ObservationType, Observation>>,
    drivers: &DriverSet,
    repair: DriftRepair,
) -> Option<DriftAttribute> {
    let _ = (drivers, repair);
    if !receipt_is_current(desired, observed) {
        return None;
    }
    let result = match latest_drift(desired, observed, observations)?.result {
        Drift::Clean => "clean",
        Drift::Drifted { .. } => "drifted",
        Drift::Unknown { .. } => "unknown",
    };
    Some(DriftAttribute {
        result: result.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::Arc;

    use nyl_core::orchestration::ids::RunId;
    use nyl_core::orchestration::state::{ObservationKind, Receipt, StateV1};
    use proptest::prelude::*;
    use serde_json::{json, Value};

    use super::*;
    use crate::rules::fixtures::*;
    use crate::rules::selection::{select, Skipped};

    fn drivers() -> DriverSet {
        let mut drivers = DriverSet::new();
        drivers.register(Arc::new(TestDriver { kind: "Command" }));
        drivers
    }

    fn observation(desired: &DesiredUnit, observation_type: ObservationType, result: Drift) -> Observation {
        Observation {
            api_version: StateV1,
            kind: ObservationKind,
            unit: desired.unit.clone(),
            observation_type,
            execution_key: desired.execution_key.clone().unwrap(),
            at: "2026-09-24T12:00:00Z".parse().unwrap(),
            result,
            details: Value::Null,
        }
    }

    fn drifted() -> Drift {
        Drift::Drifted {
            summary: "1 change".to_owned(),
        }
    }

    /// One unit `unit` with a current receipt and `spec`, observed with
    /// `drift` (and a clean health observation when `health`).
    struct World {
        desired: BTreeMap<UnitName, DesiredUnit>,
        observed: BTreeMap<UnitName, ObservedUnit>,
        observations: BTreeMap<UnitName, BTreeMap<ObservationType, Observation>>,
        drivers: DriverSet,
        drift_repair: DriftRepair,
        reapply: BTreeSet<UnitName>,
        local: Option<BTreeSet<UnitName>>,
    }

    impl World {
        fn new() -> Self {
            Self {
                desired: BTreeMap::new(),
                observed: BTreeMap::new(),
                observations: BTreeMap::new(),
                drivers: drivers(),
                drift_repair: DriftRepair::Converge,
                reapply: BTreeSet::new(),
                local: None,
            }
        }

        fn current(&mut self, unit: &str, spec: Value, drift: Option<Drift>, health: bool) {
            let document = desired(unit, "Command", spec.clone(), unit);
            let mut stored = BTreeMap::new();
            if let Some(drift) = drift {
                stored.insert(
                    ObservationType::Drift,
                    observation(&document, ObservationType::Drift, drift),
                );
            }
            if health {
                stored.insert(
                    ObservationType::Health,
                    observation(&document, ObservationType::Health, Drift::Clean),
                );
            }
            if !stored.is_empty() {
                self.observations.insert(name(unit), stored);
            }
            self.desired.insert(name(unit), document);
            self.observed.insert(
                name(unit),
                observed(unit, "Command", Some(receipt(unit, spec, Vec::new()))),
            );
        }

        fn with<T>(&self, attempted: &BTreeSet<UnitName>, f: impl FnOnce(&Candidates<'_>) -> T) -> T {
            f(&Candidates {
                desired: &self.desired,
                observed: &self.observed,
                drivers: &self.drivers,
                named: None,
                attempted,
                local: self.local.as_ref(),
                observations: &self.observations,
                drift_repair: self.drift_repair,
                reapply: &self.reapply,
            })
        }

        /// The snapshot these documents form.
        fn snapshot(&self) -> EnvironmentSnapshot {
            EnvironmentSnapshot {
                desired: self.desired.clone(),
                observed: self.observed.clone(),
                observations: self.observations.clone(),
                ..EnvironmentSnapshot::default()
            }
        }

        /// Execute `unit` again: a new receipt of the same key by another
        /// run, with [`observation_changes`] applied as the run path does.
        fn repair(&mut self, unit: &str) {
            let snapshot = self.snapshot();
            let observed = self.observed.get_mut(&name(unit)).unwrap();
            let receipt = observed.receipt.as_mut().unwrap();
            *receipt = Receipt {
                run: RunId::new(RUN_2).unwrap(),
                ..receipt.clone()
            };
            for (name, remaining) in observation_changes(&snapshot, &self.observed) {
                match remaining {
                    Some(remaining) => self.observations.insert(name, remaining),
                    None => self.observations.remove(&name),
                };
            }
        }
    }

    /// Catches a repaired unit keeping the drift observation about its old
    /// receipt (it would read `drifted` against the new one), losing its
    /// other observations, or an unchanged unit losing its observation.
    #[test]
    fn test_observation_changes_drop_drift_only_for_new_receipts() {
        let mut world = World::new();
        world.current("repaired", json!({}), Some(drifted()), true);
        world.current("alone", json!({}), Some(drifted()), false);
        world.current("unchanged", json!({}), Some(drifted()), true);
        let snapshot = world.snapshot();
        let mut after = world.observed.clone();
        for unit in ["repaired", "alone"] {
            let receipt = after.get_mut(&name(unit)).unwrap().receipt.as_mut().unwrap();
            receipt.run = RunId::new(RUN_2).unwrap();
        }
        let changes = observation_changes(&snapshot, &after);
        assert_eq!(
            changes,
            BTreeMap::from([
                (name("alone"), None),
                (
                    name("repaired"),
                    Some(BTreeMap::from([(
                        ObservationType::Health,
                        world.observations[&name("repaired")][&ObservationType::Health].clone()
                    )]))
                ),
            ])
        );
    }

    /// Catches a unit selected because it is uncertain being reported as a
    /// drift repair, and `--reapply` not winning over drift in the summary.
    #[test]
    fn test_repairs_classify_why_a_current_unit_was_selected() {
        let mut world = World::new();
        world.current("drifted", json!({"idempotent": true}), Some(drifted()), false);
        world.current("reapplied", json!({"idempotent": true}), Some(drifted()), false);
        world.current("uncertain", json!({"idempotent": true}), Some(drifted()), false);
        world.observed.get_mut(&name("uncertain")).unwrap().condition = Some(Condition::Uncertain {
            since: "2026-09-24T11:12:00Z".parse().unwrap(),
            run: run(RUN_1),
            reason: "interrupted".to_owned(),
        });
        world.reapply.insert(name("reapplied"));
        let repairs = world.with(&BTreeSet::new(), |candidates| {
            let (ready, _) = select(candidates);
            assert_eq!(ready.len(), 3, "{ready:?}");
            repairs(candidates, &ready)
        });
        assert_eq!(
            repairs,
            BTreeMap::from([(name("drifted"), Repair::Drift), (name("reapplied"), Repair::Reapply)])
        );
    }

    /// Catches `status` showing a stale observation's result, or none for
    /// the current receipt's.
    #[test]
    fn test_status_drift_reports_the_current_receipts_observation() {
        let mut world = World::new();
        world.current("clean", json!({}), Some(Drift::Clean), false);
        world.current("drifted", json!({}), Some(drifted()), false);
        world.current(
            "unknown",
            json!({}),
            Some(Drift::Unknown { reason: "x".to_owned() }),
            false,
        );
        world.current("unobserved", json!({}), None, true);
        world.current("stale", json!({}), Some(drifted()), false);
        world.desired.get_mut(&name("stale")).unwrap().execution_key = Some(key("changed"));
        let result = |unit: &str| {
            status_drift(
                &world.desired[&name(unit)],
                world.observed.get(&name(unit)),
                world.observations.get(&name(unit)),
                &world.drivers,
                DriftRepair::Manual,
            )
            .map(|drift| drift.result)
        };
        assert_eq!(result("clean").as_deref(), Some("clean"));
        assert_eq!(result("drifted").as_deref(), Some("drifted"));
        assert_eq!(result("unknown").as_deref(), Some("unknown"));
        assert_eq!(result("unobserved"), None);
        assert_eq!(result("stale"), None);
    }

    proptest! {
        /// Catches a repaired unit being selected again on the strength of
        /// the observation its repair answered: after a new receipt and the
        /// observation changes, only a new `drifted` observation (or
        /// `--reapply`) selects it again, whatever its policy, `driftRepair`,
        /// approvals, and other observations.
        #[test]
        fn test_a_repair_is_not_selected_again_without_new_drift(
            idempotent in any::<bool>(),
            manual in any::<bool>(),
            approved in any::<bool>(),
            health in any::<bool>(),
        ) {
            let mut world = World::new();
            world.current("unit", json!({"idempotent": idempotent}), Some(drifted()), health);
            world.drift_repair = if manual { DriftRepair::Manual } else { DriftRepair::Converge };
            world.local = approved.then(|| BTreeSet::from([name("unit")]));
            world.repair("unit");
            let skipped = world.with(&BTreeSet::new(), |candidates| select(candidates).1);
            prop_assert_eq!(skipped.get(&name("unit")), Some(&Skipped::Current));
            prop_assert_eq!(
                world.observations.get(&name("unit")).map(|stored| stored.contains_key(&ObservationType::Health)),
                health.then_some(true)
            );
        }
    }
}
