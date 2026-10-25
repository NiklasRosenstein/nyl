//! Which units a wave executes.
//!
//! Contract: [Execution](../../../../design/orchestration-core.md#execution),
//! step 3 and `--unit`. A unit is ready when its desired document is ready
//! and active, it has no current receipt, every receipt its provenance cites
//! is still current, and it does not await approval. Uncertain units are
//! skipped unless their recovery policy is `converge`; non-retryable failed
//! units wait for `recover`; retryable failed units are selected again (once
//! per run). With `--unit`, only the named units execute: their
//! dependencies and dependents never do.

use std::collections::{BTreeMap, BTreeSet};

use nyl_core::orchestration::driver::{DriverSet, RecoveryPolicy};
use nyl_core::orchestration::ids::UnitName;
use nyl_core::orchestration::state::{Condition, DesiredUnit, Lifecycle, ObservedUnit, Provenance, Readiness};

use super::freshness::receipt_is_current;

/// What selection reads.
pub struct Candidates<'a> {
    pub desired: &'a BTreeMap<UnitName, DesiredUnit>,
    /// The snapshot's observed documents overlaid with this run's checkpoints.
    pub observed: &'a BTreeMap<UnitName, ObservedUnit>,
    pub drivers: &'a DriverSet,
    /// `--unit`/`--units`; `None` selects every unit.
    pub named: Option<&'a BTreeSet<UnitName>>,
    /// Units this run already executed, whatever the outcome.
    pub attempted: &'a BTreeSet<UnitName>,
    /// A `--local` run next to remote state: units whose execution recovery
    /// policy is not `converge` are skipped ([`Skipped::LocalNotConverge`]).
    /// Contract: [Local runs](../../../../design/orchestration-core.md#local-runs).
    pub local: bool,
}

/// Why a unit is not selected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Skipped {
    NotNamed,
    Blocked,
    Current,
    /// Uncertain, and its recovery policy is not `converge`.
    Uncertain,
    FailedNonRetryable,
    AwaitingApproval,
    AlreadyAttempted,
    /// A `--local` run does not execute it: its recovery policy is not
    /// `converge`, and `--approve` (part 6) is needed to run it locally.
    LocalNotConverge,
}

/// The ready units, by name, and why each other unit is skipped.
pub fn select(candidates: &Candidates<'_>) -> (Vec<UnitName>, BTreeMap<UnitName, Skipped>) {
    let mut ready = Vec::new();
    let mut skipped = BTreeMap::new();
    for (name, desired) in candidates.desired {
        match decide(candidates, name, desired) {
            Ok(()) => ready.push(name.clone()),
            Err(reason) => {
                skipped.insert(name.clone(), reason);
            }
        }
    }
    (ready, skipped)
}

/// `Ok` when `desired` is ready to execute in this wave.
fn decide(candidates: &Candidates<'_>, name: &UnitName, desired: &DesiredUnit) -> Result<(), Skipped> {
    if candidates.named.is_some_and(|named| !named.contains(name)) {
        return Err(Skipped::NotNamed);
    }
    if candidates.attempted.contains(name) {
        return Err(Skipped::AlreadyAttempted);
    }
    if matches!(desired.readiness, Readiness::Blocked { .. }) || desired.execution_key.is_none() {
        return Err(Skipped::Blocked);
    }
    if !matches!(desired.lifecycle, Lifecycle::Active) {
        return Err(Skipped::Blocked);
    }
    let observed = candidates.observed.get(name);
    if receipt_is_current(desired, observed) {
        return Err(Skipped::Current);
    }
    // A condition of an earlier incarnation does not apply to this one.
    let condition = observed
        .filter(|observed| observed.unit.uid == desired.unit.uid)
        .and_then(|observed| observed.condition.as_ref());
    match condition {
        Some(Condition::Uncertain { .. }) => {
            let spec = desired.resolved_spec.as_ref().unwrap_or(&desired.spec);
            let converges = candidates
                .drivers
                .get(&desired.unit.api_version, &desired.unit.kind)
                .is_some_and(|driver| driver.recovery(spec).execution == RecoveryPolicy::Converge);
            if !converges {
                return Err(Skipped::Uncertain);
            }
        }
        Some(Condition::Failed { retryable: false, .. }) => return Err(Skipped::FailedNonRetryable),
        Some(Condition::Failed { retryable: true, .. }) | None => {}
    }
    if !citations_current(candidates, desired) {
        return Err(Skipped::Blocked);
    }
    Ok(())
}

/// Whether every receipt `desired` cites still matches its producer's
/// desired uid and execution key. Cross-environment citations are checked by
/// their own environment's resolution.
fn citations_current(candidates: &Candidates<'_>, desired: &DesiredUnit) -> bool {
    desired.provenance.values().all(|provenance| match provenance {
        Provenance::Receipt(cited) if cited.environment.is_none() => {
            candidates.desired.get(&cited.unit).is_some_and(|producer| {
                producer.unit.uid.as_ref() == Some(&cited.uid)
                    && producer.execution_key.as_ref() == Some(&cited.execution_key)
            })
        }
        _ => true,
    })
}

/// Split the ready units into batches of at most `concurrency` (at least 1)
/// that run in parallel within one wave.
pub fn batches(ready: &[UnitName], concurrency: usize) -> Vec<Vec<UnitName>> {
    ready.chunks(concurrency.max(1)).map(<[UnitName]>::to_vec).collect()
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::sync::Arc;

    use nyl_core::orchestration::driver::{
        Capabilities, Driver, DriverResult, ExecStatus, ExecutionContext, Inspection, KeyInputs, Outcome,
        OwnershipScope, PlanReport, RecoveryPolicies, ResolutionContext, Supported, Verification,
    };
    use nyl_core::orchestration::ids::EnvironmentName;
    use nyl_core::orchestration::state::{ArtifactDocument, FailureCategory, Receipt, ReceiptProvenance};
    use serde_json::{json, Value};

    use super::super::references::fixtures::*;
    use super::*;

    /// A kind that converges exactly when its spec sets `idempotent: true`.
    struct Stub;

    impl Driver for Stub {
        fn api_version(&self) -> &'static str {
            "units.gitops.nyl/v1"
        }
        fn kind(&self) -> &'static str {
            "Command"
        }
        fn behavior_version(&self) -> u32 {
            1
        }
        fn capabilities(&self, _: &Value) -> Capabilities {
            Capabilities::default()
        }
        fn recovery(&self, spec: &Value) -> RecoveryPolicies {
            let execution = if spec["idempotent"] == json!(true) {
                RecoveryPolicy::Converge
            } else {
                RecoveryPolicy::Manual
            };
            RecoveryPolicies {
                execution,
                teardown: RecoveryPolicy::Manual,
            }
        }
        fn spec_schema(&self) -> schemars::Schema {
            schemars::json_schema!({})
        }
        fn validate_spec(&self, _: &Value) -> Result<(), String> {
            Ok(())
        }
        fn attestations(&self, _: &Value) -> Vec<String> {
            Vec::new()
        }
        fn ownership_scopes(&self, _: &EnvironmentName, _: &Value) -> Vec<OwnershipScope> {
            Vec::new()
        }
        fn execution_key_inputs(&self, _: &ResolutionContext<'_>, _: &Value) -> DriverResult<KeyInputs> {
            unimplemented!("selection never asks")
        }
        fn plan(&self, _: &ExecutionContext<'_>, _: &DesiredUnit) -> DriverResult<Supported<PlanReport>> {
            Ok(Supported::Unsupported)
        }
        fn reconcile(&self, _: &ExecutionContext<'_>, _: &DesiredUnit) -> DriverResult<Supported<Outcome>> {
            Ok(Supported::Unsupported)
        }
        fn verify(
            &self,
            _: &ExecutionContext<'_>,
            _: &DesiredUnit,
            _: &Receipt,
        ) -> DriverResult<Supported<Verification>> {
            Ok(Supported::Unsupported)
        }
        fn inspect(&self, _: &ExecutionContext<'_>, _: &DesiredUnit) -> DriverResult<Supported<Inspection>> {
            Ok(Supported::Unsupported)
        }
        fn teardown(&self, _: &ExecutionContext<'_>, _: &DesiredUnit) -> DriverResult<Supported<Outcome>> {
            Ok(Supported::Unsupported)
        }
        fn build(
            &self,
            _: &ExecutionContext<'_>,
            _: &DesiredUnit,
            _: bool,
        ) -> DriverResult<Supported<Vec<ArtifactDocument>>> {
            Ok(Supported::Unsupported)
        }
        fn exec(
            &self,
            _: &ExecutionContext<'_>,
            _: &DesiredUnit,
            _: &[OsString],
        ) -> DriverResult<Supported<ExecStatus>> {
            Ok(Supported::Unsupported)
        }
    }

    struct World {
        desired: BTreeMap<UnitName, DesiredUnit>,
        observed: BTreeMap<UnitName, ObservedUnit>,
        drivers: DriverSet,
        attempted: BTreeSet<UnitName>,
    }

    impl World {
        fn new() -> Self {
            let mut drivers = DriverSet::new();
            drivers.register(Arc::new(Stub));
            Self {
                desired: BTreeMap::new(),
                observed: BTreeMap::new(),
                drivers,
                attempted: BTreeSet::new(),
            }
        }

        fn add(&mut self, unit: &str, spec: Value) {
            self.desired.insert(name(unit), desired(unit, "Command", spec, unit));
        }

        fn condition(&mut self, unit: &str, condition: Condition) {
            let mut observed = observed(unit, "Command", None);
            observed.condition = Some(condition);
            self.observed.insert(name(unit), observed);
        }

        fn select(&self, named: Option<&BTreeSet<UnitName>>) -> (Vec<UnitName>, BTreeMap<UnitName, Skipped>) {
            select(&Candidates {
                desired: &self.desired,
                observed: &self.observed,
                drivers: &self.drivers,
                named,
                attempted: &self.attempted,
                local: false,
            })
        }
    }

    fn failed(retryable: bool) -> Condition {
        Condition::Failed {
            since: "2026-09-24T11:12:00Z".parse().unwrap(),
            run: run(RUN_1),
            category: FailureCategory::ToolError,
            retryable,
            message: "exited 1".to_owned(),
        }
    }

    fn uncertain() -> Condition {
        Condition::Uncertain {
            since: "2026-09-24T11:12:00Z".parse().unwrap(),
            run: run(RUN_1),
            reason: "interrupted".to_owned(),
        }
    }

    #[test]
    fn test_select_retries_retryable_failures_only() {
        let mut world = World::new();
        world.add("retry", json!({}));
        world.add("stuck", json!({}));
        world.condition("retry", failed(true));
        world.condition("stuck", failed(false));
        let (ready, skipped) = world.select(None);
        assert_eq!(ready, [name("retry")]);
        assert_eq!(skipped, BTreeMap::from([(name("stuck"), Skipped::FailedNonRetryable)]));
    }

    #[test]
    fn test_select_converges_uncertain_units_only_with_a_converge_policy() {
        let mut world = World::new();
        world.add("converge", json!({"idempotent": true}));
        world.add("manual", json!({}));
        world.condition("converge", uncertain());
        world.condition("manual", uncertain());
        let (ready, skipped) = world.select(None);
        assert_eq!(ready, [name("converge")]);
        assert_eq!(skipped, BTreeMap::from([(name("manual"), Skipped::Uncertain)]));
    }

    #[test]
    fn test_select_runs_only_named_and_unattempted_units() {
        let mut world = World::new();
        world.add("a", json!({}));
        world.add("b", json!({}));
        world.add("c", json!({}));
        world.attempted.insert(name("c"));
        let named = BTreeSet::from([name("b"), name("c")]);
        let (ready, skipped) = world.select(Some(&named));
        assert_eq!(ready, [name("b")]);
        assert_eq!(
            skipped,
            BTreeMap::from([(name("a"), Skipped::NotNamed), (name("c"), Skipped::AlreadyAttempted)])
        );
    }

    #[test]
    fn test_select_skips_current_blocked_and_stale_units() {
        let mut world = World::new();
        world.add("current", json!({}));
        world.observed.insert(
            name("current"),
            observed("current", "Command", Some(receipt("current", json!({}), Vec::new()))),
        );
        world.add("blocked", json!({}));
        world.desired.get_mut(&name("blocked")).unwrap().execution_key = None;
        world.add("held", json!({}));
        world.desired.get_mut(&name("held")).unwrap().lifecycle = Lifecycle::Retained {
            tombstone: nyl_core::orchestration::state::Tombstone {
                since: "2026-09-24T11:12:00Z".parse().unwrap(),
                receipt: None,
            },
        };
        // `stale` cites a receipt of `current` under an older execution key.
        world.add("stale", json!({}));
        world.desired.get_mut(&name("stale")).unwrap().provenance.insert(
            nyl_core::orchestration::ids::JsonPointer::new("/x").unwrap(),
            Provenance::Receipt(ReceiptProvenance {
                environment: None,
                unit: name("current"),
                uid: uid("current"),
                execution_key: key("current-old"),
                observed_commit: Some(commit(COMMIT)),
                run: None,
            }),
        );
        let (ready, skipped) = world.select(None);
        assert!(ready.is_empty(), "{ready:?}");
        assert_eq!(
            skipped,
            BTreeMap::from([
                (name("blocked"), Skipped::Blocked),
                (name("current"), Skipped::Current),
                (name("held"), Skipped::Blocked),
                (name("stale"), Skipped::Blocked),
            ])
        );
    }

    #[test]
    fn test_batches_split_by_concurrency_of_at_least_one() {
        let ready = [name("a"), name("b"), name("c")];
        assert_eq!(batches(&ready, 2), [vec![name("a"), name("b")], vec![name("c")]]);
        assert_eq!(batches(&ready, 0).len(), 3);
        assert!(batches(&[], 4).is_empty());
    }
}
