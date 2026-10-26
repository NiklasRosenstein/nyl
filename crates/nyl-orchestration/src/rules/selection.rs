//! Which units a wave executes.
//!
//! Contract: [Execution](../../../../design/orchestration-core.md#execution),
//! step 3 and `--unit`. A unit is ready when its desired document is ready
//! and active, it has no current receipt, every receipt its provenance cites
//! is still current, and it does not await approval.
//!
//! - An `uncertain` condition of the unit's current incarnation makes it a
//!   candidate whatever its receipt, even a current one: under the
//!   `converge` and `inspect` recovery policies it is selected (the run
//!   shell inspects `inspect` units first); under `manual` it waits for
//!   `recover` ([`Skipped::Uncertain`]).
//! - Non-retryable failed units wait for `recover`; retryable failed units
//!   are selected again (once per run).
//! - With `--unit`, only the named units execute: their dependencies and
//!   dependents never do.
//! - A `--local` run executes a unit whose execution recovery policy is not
//!   `converge` only when `--approve <unit>` names it; otherwise the unit
//!   awaits approval ([`Skipped::LocalApproval`]). Contract:
//!   [Local runs](../../../../design/orchestration-core.md#local-runs).

use std::collections::{BTreeMap, BTreeSet};

use nyl_core::orchestration::driver::{DriverSet, RecoveryPolicy};
use nyl_core::orchestration::ids::UnitName;
use nyl_core::orchestration::ports::CommitIdentity;
use nyl_core::orchestration::state::{
    ApprovalIdentity, ApprovalRecord, Condition, DesiredUnit, Lifecycle, Observation, ObservationType, ObservedUnit,
    Provenance, Readiness,
};
use nyl_core::resources::environment::DriftRepair;

use super::freshness::{receipt_is_current, same_incarnation};

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
    /// A `--local` run: the units `--approve` names. Units whose execution
    /// recovery policy is not `converge` and that are not named await
    /// approval ([`Skipped::LocalApproval`]). `None` for an ordinary run.
    /// Contract: [Local runs](../../../../design/orchestration-core.md#local-runs).
    pub local: Option<&'a BTreeSet<UnitName>>,
    /// The latest observations per unit and type (`nyl verify`'s drift
    /// observations select drifted units again). Contract:
    /// [Drift](../../../../design/orchestration-core.md#drift).
    pub observations: &'a BTreeMap<UnitName, BTreeMap<ObservationType, Observation>>,
    /// The environment's `driftRepair`, as rendered at S.
    pub drift_repair: DriftRepair,
    /// `nyl reconcile --reapply`: units to execute again although their
    /// receipt is current.
    pub reapply: &'a BTreeSet<UnitName>,
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
    AlreadyAttempted,
    /// A `--local` run executes it only with `--approve <unit>`: its
    /// execution recovery policy is not `converge`. Reported as
    /// `awaiting-approval`.
    LocalApproval,
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
    // A condition of an earlier incarnation does not apply to this one.
    let condition = same_incarnation(desired, observed).and_then(|observed| observed.condition.as_ref());
    match condition {
        // Uncertain: a candidate whatever its receipt, by recovery policy.
        Some(Condition::Uncertain { .. }) => {
            let recovers = matches!(
                execution_recovery(candidates.drivers, desired),
                Some(RecoveryPolicy::Converge | RecoveryPolicy::Inspect)
            );
            if !recovers {
                return Err(Skipped::Uncertain);
            }
        }
        Some(Condition::Failed { retryable: false, .. }) => return Err(Skipped::FailedNonRetryable),
        Some(Condition::Failed { retryable: true, .. }) | None => {
            if receipt_is_current(desired, observed) {
                return Err(Skipped::Current);
            }
        }
    }
    if !citations_current(candidates, desired) {
        return Err(Skipped::Blocked);
    }
    if let Some(approved) = candidates.local {
        if execution_recovery(candidates.drivers, desired) != Some(RecoveryPolicy::Converge) && !approved.contains(name)
        {
            return Err(Skipped::LocalApproval);
        }
    }
    Ok(())
}

/// The units of `approve` (a `--local` run's `--approve`) that a
/// reconcile may execute: an approval is recorded with the commit identity,
/// so without one nothing is approved and the units await approval.
pub fn local_approve<'a>(
    approve: &'a BTreeSet<UnitName>,
    identity: Option<&CommitIdentity>,
) -> Option<&'a BTreeSet<UnitName>> {
    static NONE: BTreeSet<UnitName> = BTreeSet::new();
    Some(if identity.is_some() { approve } else { &NONE })
}

/// The approval a `--local` run records, in the receipt and in the local
/// transition commit's summary, for each of `units` that `--approve` names
/// and whose execution recovery policy is not `converge`:
/// `{by: ["Name <email>"], identity: local, source: "local", digest: null,
/// reviewed: false}`, with the commit identity. Nothing without an identity
/// ([`local_approve`]). Contract:
/// [Local runs](../../../../design/orchestration-core.md#local-runs).
pub fn local_approvals<'u>(
    approve: &BTreeSet<UnitName>,
    identity: Option<&CommitIdentity>,
    drivers: &DriverSet,
    units: impl IntoIterator<Item = &'u DesiredUnit>,
) -> BTreeMap<UnitName, ApprovalRecord> {
    let Some(identity) = identity else {
        return BTreeMap::new();
    };
    units
        .into_iter()
        .filter(|unit| approve.contains(&unit.unit.name))
        .filter(|unit| execution_recovery(drivers, unit) != Some(RecoveryPolicy::Converge))
        .map(|unit| {
            let record = ApprovalRecord {
                by: vec![format!("{} <{}>", identity.name, identity.email)],
                identity: ApprovalIdentity::Local,
                source: Some("local".to_owned()),
                digest: None,
                reviewed: false,
            };
            (unit.unit.name.clone(), record)
        })
        .collect()
}

/// The unit's execution recovery policy, from its resolved spec when it has
/// one; `None` when no driver serves its kind.
fn execution_recovery(drivers: &DriverSet, desired: &DesiredUnit) -> Option<RecoveryPolicy> {
    let spec = desired.resolved_spec.as_ref().unwrap_or(&desired.spec);
    drivers
        .get(&desired.unit.api_version, &desired.unit.kind)
        .map(|driver| driver.recovery(spec).execution)
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
    use std::sync::Arc;

    use nyl_core::orchestration::state::{FailureCategory, ReceiptProvenance};
    use serde_json::{json, Value};

    use super::*;
    use crate::rules::fixtures::*;

    struct World {
        desired: BTreeMap<UnitName, DesiredUnit>,
        observed: BTreeMap<UnitName, ObservedUnit>,
        drivers: DriverSet,
        attempted: BTreeSet<UnitName>,
        local: Option<BTreeSet<UnitName>>,
    }

    impl World {
        fn new() -> Self {
            let mut drivers = DriverSet::new();
            drivers.register(Arc::new(TestDriver { kind: "Command" }));
            Self {
                desired: BTreeMap::new(),
                observed: BTreeMap::new(),
                drivers,
                attempted: BTreeSet::new(),
                local: None,
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
                local: self.local.as_ref(),
                observations: &BTreeMap::new(),
                drift_repair: DriftRepair::default(),
                reapply: &BTreeSet::new(),
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

    /// Catches an uncertain unit being skipped as current because its last
    /// receipt still matches: its effects may differ from that receipt, so
    /// its recovery policy decides, whatever the receipt.
    #[test]
    fn test_select_recovers_uncertain_units_with_a_current_receipt_by_policy() {
        let mut world = World::new();
        for (unit, spec) in [
            ("converge", json!({"idempotent": true})),
            ("inspect", json!({"inspect": true})),
            ("manual", json!({})),
        ] {
            world.add(unit, spec.clone());
            let mut document = observed(unit, "Command", Some(receipt(unit, spec, Vec::new())));
            document.condition = Some(uncertain());
            world.observed.insert(name(unit), document);
        }
        let (ready, skipped) = world.select(None);
        assert_eq!(ready, [name("converge"), name("inspect")]);
        assert_eq!(skipped, BTreeMap::from([(name("manual"), Skipped::Uncertain)]));
    }

    /// Catches a `--local` run executing a unit that is not safe to re-run
    /// without the operator naming it, or refusing one it named.
    #[test]
    fn test_select_local_run_needs_approve_for_units_that_do_not_converge() {
        let mut world = World::new();
        world.add("converge", json!({"idempotent": true}));
        world.add("approved", json!({}));
        world.add("unapproved", json!({}));
        world.local = Some(BTreeSet::from([name("approved")]));
        let (ready, skipped) = world.select(None);
        assert_eq!(ready, [name("approved"), name("converge")]);
        assert_eq!(skipped, BTreeMap::from([(name("unapproved"), Skipped::LocalApproval)]));
    }

    /// Catches a `--local` run's approved unit executing without the audit
    /// record decision 3 requires (receipt and summary), or with an empty
    /// `by` when the run has no commit identity.
    #[test]
    fn test_local_approvals_record_the_commit_identity_for_approved_units_that_do_not_converge() {
        let mut world = World::new();
        world.add("converge", json!({"idempotent": true}));
        world.add("approved", json!({}));
        world.add("unapproved", json!({}));
        let approve = BTreeSet::from([name("approved"), name("converge")]);
        let identity: CommitIdentity = "Jane Doe <jane@example.invalid>".parse().unwrap();

        let approvals = local_approvals(&approve, Some(&identity), &world.drivers, world.desired.values());
        let expected = ApprovalRecord {
            by: vec!["Jane Doe <jane@example.invalid>".to_owned()],
            identity: ApprovalIdentity::Local,
            source: Some("local".to_owned()),
            digest: None,
            reviewed: false,
        };
        assert_eq!(approvals, BTreeMap::from([(name("approved"), expected)]));
        assert_eq!(local_approve(&approve, Some(&identity)), Some(&approve));

        // Without a commit identity nothing is approved: the unit awaits
        // approval instead of executing without a record of who approved it.
        assert!(local_approvals(&approve, None, &world.drivers, world.desired.values()).is_empty());
        world.local = local_approve(&approve, None).cloned();
        let (ready, skipped) = world.select(None);
        assert_eq!(ready, [name("converge")]);
        assert_eq!(skipped[&name("approved")], Skipped::LocalApproval);
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
