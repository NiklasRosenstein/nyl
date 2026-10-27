//! Which `deleting` units a run tears down, in which order, and why the
//! others wait.
//!
//! Contract: [Deletion](../../../../design/orchestration-core.md#deletion),
//! "Teardown" (reverse creation order: a unit is torn down only after every
//! deleting unit that depends on it; a publication's `teardownWait` gates
//! the units it depends on, transitively; units without a path to a waiting
//! unit proceed in the same run; omission and kind-change teardown need
//! `--allow-teardown`), "Retained dependents" (`NYL-TEARDOWN-HAS-DEPENDENTS`
//! unless `--allow-dependents`), "Explicit teardown" and "Holds" (a held
//! unit never progresses: an explicit teardown of it only records the
//! request, see [`super::lifecycle`]), [Recovery](../../../../design/orchestration-core.md#recovery)
//! (the driver's teardown recovery policy), and
//! [Kubernetes publication unit](../../../../design/orchestration-core.md#kubernetes-publication-unit),
//! teardown readiness (an unmet blocking requirement keeps the unit pending
//! unless `--allow-incomplete`).
//!
//! One uniform reverse-dependency rule (part 5 decision Q6): every drop
//! (a release, or a kind change's removal of a tombstone) and every declared
//! retention waits for its deleting dependents exactly as a teardown does,
//! so this plan covers every unit leaving with any intent, and the run
//! writes a drop or retention only once it is in [`TeardownPlan::ready`].
//!
//! Dependencies between deleting units come from their stored specs
//! ([`dependencies`]), not from a resolution of source, so a teardown never
//! needs the units' current declarations.

use std::collections::{BTreeMap, BTreeSet};

use nyl_core::bindings::unit_references;
use nyl_core::orchestration::codes::{Code, Finding};
use nyl_core::orchestration::driver::{DriverSet, RecoveryPolicy};
use nyl_core::orchestration::ids::{EnvironmentName, UnitName};
use nyl_core::orchestration::state::{
    Condition, DeletionIntent, DeletionReason, DesiredUnit, Lifecycle, ObservedUnit, Provenance, TeardownProgress,
};
use nyl_core::resources::units::{ApprovalMode, CommonUnitFields};

use super::freshness::same_incarnation;
use super::references::depends_on;

/// What the invocation allows.
#[derive(Debug, Clone, Copy, Default)]
#[allow(clippy::struct_excessive_bools)] // Independent command-line switches.
pub struct TeardownOptions<'a> {
    /// `--allow-teardown`: omission and kind-change teardowns proceed.
    pub allow_teardown: bool,
    /// `--allow-dependents`: a unit a retained tombstone cites is torn down anyway.
    pub allow_dependents: bool,
    /// `--allow-incomplete`: an unmet blocking readiness requirement does not stop the teardown.
    pub allow_incomplete: bool,
    /// `nyl teardown --unit`: only these units are torn down; `None` acts on
    /// every deleting unit the invocation may tear down.
    pub named: Option<&'a BTreeSet<UnitName>>,
    /// An explicitly requested teardown (`nyl teardown`, `state delete
    /// --teardown`), which needs no `--allow-teardown`. Held units are
    /// pending either way.
    pub explicit: bool,
}

/// What deciding a run's teardowns reads.
pub struct TeardownInputs<'a> {
    pub environment: &'a EnvironmentName,
    /// Every desired document after this run's lifecycle changes: deleting,
    /// held, retained, and active units.
    pub desired: &'a BTreeMap<UnitName, DesiredUnit>,
    /// The observed documents, with this run's checkpoints.
    pub observed: &'a BTreeMap<UnitName, ObservedUnit>,
    pub drivers: &'a DriverSet,
    pub options: TeardownOptions<'a>,
    /// Units this run already tore down or tried to.
    pub attempted: &'a BTreeSet<UnitName>,
    /// Units whose teardown waits under a `teardownWait` that has not
    /// completed; the units they depend on wait with them.
    pub waiting: &'a BTreeSet<UnitName>,
    /// Unmet blocking readiness requirements per unit, from the preflight
    /// ([`super::teardown_readiness`]).
    pub readiness: &'a BTreeMap<UnitName, Vec<Finding>>,
    /// `retained` tombstones the run drops (a kind change of their name, or
    /// a release under `teardown --all`): they wait for their deleting
    /// dependents like every other leaving unit (Q6).
    pub drops: &'a BTreeSet<UnitName>,
}

/// Why a deleting unit is not torn down in this wave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pending {
    /// Omission or kind change without `--allow-teardown`.
    NeedsAllowTeardown,
    /// Retained tombstones cite it, without `--allow-dependents`.
    HasDependents { tombstones: Vec<UnitName> },
    /// Deleting units that depend on it are not torn down yet.
    Dependents { units: Vec<UnitName> },
    /// A publication it is a dependency of, directly or transitively, waits under its `teardownWait`.
    Waiting { units: Vec<UnitName> },
    /// Held: a held unit never progresses; a teardown request on it is
    /// recorded and carried out after `nyl resume`.
    Held,
    /// Unmet blocking readiness requirements, without `--allow-incomplete`.
    Readiness { findings: Vec<Finding> },
    /// An uncertain teardown whose teardown recovery policy is not
    /// `converge`, or an uncertain unit a retention or release would drop.
    Uncertain,
    /// A non-retryable failed teardown waits for `nyl recover --retry`.
    FailedNonRetryable,
    /// Not named by `nyl teardown --unit`.
    NotNamed,
    /// Already torn down or tried in this run.
    AlreadyAttempted,
    /// The unit declares `approval: manual`; teardown approvals (`nyl
    /// teardown --approve`, destroy-plan digests) arrive with M3 part 6, so
    /// until then it waits (exit 2, open question Q50).
    AwaitingApproval,
}

/// The decision of one wave.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TeardownPlan {
    /// Units to tear down now, in order: consumers before producers.
    pub ready: Vec<UnitName>,
    /// Every other deleting unit and why it waits.
    pub pending: BTreeMap<UnitName, Pending>,
    /// Units torn down although retained tombstones cite them
    /// (`--allow-dependents`), recorded in the transition commit.
    pub overrides: BTreeMap<UnitName, Vec<UnitName>>,
}

/// Decide which leaving units to tear down, retain, release, or drop now.
pub fn plan(inputs: &TeardownInputs<'_>) -> TeardownPlan {
    let desired = inputs.desired;
    let dependencies = dependencies(desired);
    // Deleting units, and held units with a pending deletion, whose
    // consumers go first.
    let leaving: BTreeSet<&UnitName> = desired
        .iter()
        .filter(|(_, unit)| deleting(unit))
        .map(|(name, _)| name)
        .collect();
    let candidates: Vec<&UnitName> = desired
        .iter()
        // A unit whose wait goes on is tearing down; the run reports it.
        .filter(|(name, _)| !inputs.waiting.contains(*name))
        .filter(|(name, unit)| {
            deleting(unit)
                || inputs.drops.contains(*name)
                || (matches!(unit.lifecycle, Lifecycle::Held { .. })
                    && inputs.options.named.is_some_and(|named| named.contains(*name)))
        })
        .map(|(name, _)| name)
        .collect();
    let waiting_on = waiting_dependencies(&dependencies, inputs.waiting);

    let mut plan = TeardownPlan::default();
    let mut open = Vec::new();
    for name in candidates {
        let unit = &desired[name];
        if let Some(pending) = gate(inputs, name, unit, &mut plan.overrides) {
            plan.pending.insert(name.clone(), pending);
        } else if let Some(units) = waiting_on.get(name) {
            plan.pending.insert(
                name.clone(),
                Pending::Waiting {
                    units: units.iter().cloned().collect(),
                },
            );
        } else {
            open.push(name);
        }
    }
    // Consumers first: a unit is ready once every deleting unit that depends
    // on it is gone or ready earlier in this order.
    let mut ready: BTreeSet<&UnitName> = BTreeSet::new();
    loop {
        let before = ready.len();
        for name in &open {
            if ready.contains(name) {
                continue;
            }
            let blocked = leaving.iter().any(|dependent| {
                *dependent != *name && depends(&dependencies, dependent, name) && !ready.contains(dependent)
            });
            if !blocked {
                ready.insert(name);
                plan.ready.push((*name).clone());
            }
        }
        if ready.len() == before {
            break;
        }
    }
    for name in open.into_iter().filter(|name| !ready.contains(name)) {
        let units = leaving
            .iter()
            .filter(|dependent| {
                **dependent != name && depends(&dependencies, dependent, name) && !ready.contains(*dependent)
            })
            .map(|dependent| (*dependent).clone())
            .collect();
        plan.overrides.remove(name);
        plan.pending.insert(name.clone(), Pending::Dependents { units });
    }
    plan.overrides.retain(|name, _| plan.ready.contains(name));
    plan
}

/// Whether a desired document is on its way out: deleting, or held with a
/// pending deletion.
fn deleting(unit: &DesiredUnit) -> bool {
    matches!(
        unit.lifecycle,
        Lifecycle::Deleting { .. } | Lifecycle::Held { deletion: Some(_), .. }
    )
}

fn depends(dependencies: &BTreeMap<UnitName, BTreeSet<UnitName>>, consumer: &UnitName, producer: &UnitName) -> bool {
    dependencies
        .get(consumer)
        .is_some_and(|producers| producers.contains(producer))
}

/// For every unit a waiting unit depends on, directly or transitively, the
/// waiting units it waits with.
fn waiting_dependencies(
    dependencies: &BTreeMap<UnitName, BTreeSet<UnitName>>,
    waiting: &BTreeSet<UnitName>,
) -> BTreeMap<UnitName, BTreeSet<UnitName>> {
    let mut gated: BTreeMap<UnitName, BTreeSet<UnitName>> = BTreeMap::new();
    for waiter in waiting {
        let mut stack: Vec<&UnitName> = dependencies.get(waiter).into_iter().flatten().collect();
        let mut seen = BTreeSet::new();
        while let Some(producer) = stack.pop() {
            if !seen.insert(producer) {
                continue;
            }
            gated.entry(producer.clone()).or_default().insert(waiter.clone());
            stack.extend(dependencies.get(producer).into_iter().flatten());
        }
    }
    gated
}

/// The static reasons a leaving unit waits, in order; `None` when only its
/// dependents and waits decide. Records an `--allow-dependents` override.
fn gate(
    inputs: &TeardownInputs<'_>,
    name: &UnitName,
    unit: &DesiredUnit,
    overrides: &mut BTreeMap<UnitName, Vec<UnitName>>,
) -> Option<Pending> {
    let options = inputs.options;
    if matches!(unit.lifecycle, Lifecycle::Held { .. }) {
        return Some(Pending::Held);
    }
    if options.named.is_some_and(|named| !named.contains(name)) {
        return Some(Pending::NotNamed);
    }
    if inputs.attempted.contains(name) {
        return Some(Pending::AlreadyAttempted);
    }
    let condition = same_incarnation(unit, inputs.observed.get(name)).and_then(|observed| observed.condition.as_ref());
    // Retentions, releases, and drops run nothing: only their dependents
    // gate them, and an `uncertain` condition, whose evidence a retention or
    // release would drop (conservative reading; `nyl recover` resolves it).
    let deletion = match &unit.lifecycle {
        Lifecycle::Deleting { deletion } if deletion.intent == DeletionIntent::Teardown => deletion,
        Lifecycle::Retained { .. } => return None,
        _ => return matches!(condition, Some(Condition::Uncertain { .. })).then_some(Pending::Uncertain),
    };
    let caused_by_source = matches!(
        deletion.reason,
        DeletionReason::Omission | DeletionReason::KindChange | DeletionReason::Expired
    );
    let explicitly_named = options.explicit && options.named.is_some_and(|named| named.contains(name));
    // `--allow-teardown` gates only the start (Q8, Q10).
    if caused_by_source
        && deletion.teardown == Some(TeardownProgress::Pending)
        && !options.allow_teardown
        && !explicitly_named
    {
        return Some(Pending::NeedsAllowTeardown);
    }
    let spec = unit.resolved_spec.as_ref().unwrap_or(&unit.spec);
    match condition {
        Some(Condition::Uncertain { .. }) => {
            let converges = inputs
                .drivers
                .get(&unit.unit.api_version, &unit.unit.kind)
                .is_some_and(|driver| driver.recovery(spec).teardown == RecoveryPolicy::Converge);
            if !converges {
                return Some(Pending::Uncertain);
            }
        }
        Some(Condition::Failed { retryable: false, .. }) => return Some(Pending::FailedNonRetryable),
        Some(Condition::Failed { retryable: true, .. }) | None => {}
    }
    let manual = serde_json::from_value::<CommonUnitFields>(unit.spec.clone())
        .is_ok_and(|common| common.approval.mode == ApprovalMode::Manual);
    if manual {
        return Some(Pending::AwaitingApproval);
    }
    if !options.allow_incomplete {
        if let Some(findings) = inputs.readiness.get(name).filter(|findings| !findings.is_empty()) {
            return Some(Pending::Readiness {
                findings: findings.clone(),
            });
        }
    }
    let tombstones = citing_tombstones(inputs.desired, unit);
    if !tombstones.is_empty() {
        if !options.allow_dependents {
            return Some(Pending::HasDependents { tombstones });
        }
        overrides.insert(name.clone(), tombstones);
    }
    None
}

/// The retained tombstones whose receipt cites `unit`'s incarnation in its
/// provenance, which a teardown of `unit` would leave without their producer.
pub fn citing_tombstones(desired: &BTreeMap<UnitName, DesiredUnit>, unit: &DesiredUnit) -> Vec<UnitName> {
    let Some(uid) = &unit.unit.uid else {
        return Vec::new();
    };
    desired
        .iter()
        .filter(|(name, _)| **name != unit.unit.name)
        .filter(|(_, other)| {
            let Lifecycle::Retained { tombstone } = &other.lifecycle else {
                return false;
            };
            tombstone.receipt.as_ref().is_some_and(|receipt| {
                receipt.provenance.values().any(|provenance| {
                    matches!(provenance, Provenance::Receipt(cited)
                        if cited.environment.is_none() && cited.unit == unit.unit.name && cited.uid == *uid)
                })
            })
        })
        .map(|(name, _)| name.clone())
        .collect()
}

/// The dependencies among `units` from their stored specs (`fromUnit`
/// references and `dependsOn`), ignoring producers outside `units`: for
/// each unit, the units it depends on.
pub fn dependencies(units: &BTreeMap<UnitName, DesiredUnit>) -> BTreeMap<UnitName, BTreeSet<UnitName>> {
    units
        .iter()
        .map(|(name, unit)| {
            let mut producers = BTreeSet::new();
            for reference in unit_references("", &unit.spec).unwrap_or_default() {
                if reference.source.environment.is_none() {
                    producers.extend(UnitName::new(reference.source.unit.as_str()).ok());
                }
            }
            for entry in depends_on(&unit.spec).unwrap_or_default() {
                if entry.environment().is_none() {
                    producers.extend(UnitName::new(entry.unit()).ok());
                }
            }
            producers.retain(|producer| producer != name && units.contains_key(producer));
            (name.clone(), producers)
        })
        .collect()
}

/// The warning a pending unit reports, with its stable code and fix, such
/// as `NYL-TEARDOWN-PENDING` naming `--allow-teardown` or
/// `NYL-TEARDOWN-HAS-DEPENDENTS` naming the tombstones and `--allow-dependents`.
/// `None` for reasons the unit's result or condition already says
/// (not named, already attempted, uncertain, failed).
pub fn finding(environment: &EnvironmentName, unit: &UnitName, pending: &Pending) -> Option<Finding> {
    let list = |units: &[UnitName]| units.iter().map(UnitName::as_str).collect::<Vec<_>>().join(", ");
    Some(match pending {
        Pending::NeedsAllowTeardown => Finding::new(
            Code::TeardownPending,
            format!("unit {unit} of {environment} left the ownership set; its teardown waits for --allow-teardown"),
            format!(
                "pass --allow-teardown to nyl reconcile to tear it down, restore it in source to keep it, or let it go \
                 without a teardown with `nyl state forget -e {environment} --unit {unit}`"
            ),
        ),
        Pending::Dependents { units } => Finding::new(
            Code::TeardownPending,
            format!(
                "the teardown of unit {unit} of {environment} waits for the deleting units that depend on it: {}",
                list(units)
            ),
            format!(
                "tear the dependents down first, for example with `nyl teardown -e {environment} --unit {}`",
                list(units)
            ),
        ),
        Pending::AwaitingApproval => Finding::new(
            Code::TeardownPending,
            format!(
                "unit {unit} of {environment} declares approval: manual; its teardown waits, since teardown approvals \
                 are not supported yet"
            ),
            format!(
                "tear its resources down by hand and drop it with `nyl state forget -e {environment} --unit {unit}`, \
                 or keep it until teardown approvals arrive"
            ),
        ),
        Pending::HasDependents { tombstones } => Finding::new(
            Code::TeardownHasDependents,
            format!(
                "unit {unit} of {environment} is cited by the retained tombstones {}, whose resources may still use it",
                list(tombstones)
            ),
            format!(
                "pass --allow-dependents to tear it down anyway, or drop the tombstones first with `nyl state forget -e \
                 {environment} --unit <tombstone>`"
            ),
        ),
        Pending::Waiting { units } => Finding::new(
            Code::TeardownWaiting,
            format!(
                "the teardown of unit {unit} of {environment} waits for {} to complete under its teardownWait",
                list(units)
            ),
            format!(
                "run nyl reconcile or nyl teardown again once the wait completes, or confirm the removal with `nyl \
                 teardown -e {environment} --unit {} --confirm-removed`",
                list(units)
            ),
        ),
        Pending::Held => Finding::new(
            Code::UnitHeld,
            format!("unit {unit} of {environment} is held; its pending teardown is carried out after nyl resume"),
            format!("lift the hold with `nyl resume -e {environment} --unit {unit}`"),
        ),
        Pending::Readiness { findings } => Finding::new(
            Code::TeardownIncomplete,
            format!(
                "the teardown of unit {unit} of {environment} cannot complete: {}",
                findings
                    .iter()
                    .map(|finding| finding.message.as_str())
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
            "meet the requirements the other warnings name, or pass --allow-incomplete to tear it down anyway and \
             accept what may remain",
        ),
        Pending::Uncertain | Pending::FailedNonRetryable | Pending::NotNamed | Pending::AlreadyAttempted => return None,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use nyl_core::orchestration::ids::JsonPointer;
    use nyl_core::orchestration::state::{Deletion, Hold, Request, Tombstone};
    use serde_json::{json, Value};

    use super::*;
    use crate::rules::fixtures::{self, env, name, TestDriver};

    fn drivers() -> DriverSet {
        let mut drivers = DriverSet::new();
        drivers.register(Arc::new(TestDriver { kind: "Publication" }));
        drivers.register(Arc::new(TestDriver { kind: "Command" }));
        drivers
    }

    fn reads(producer: &str) -> Value {
        json!({"values": {"input": {"fromUnit": {"unit": producer, "output": "value"}}}})
    }

    fn deleting(unit: &str, spec: Value, reason: DeletionReason, intent: DeletionIntent) -> DesiredUnit {
        let mut document = fixtures::desired(unit, "Publication", spec, unit);
        document.lifecycle = Lifecycle::Deleting {
            deletion: Deletion {
                hold: false,
                reason,
                intent,
                teardown: (intent == DeletionIntent::Teardown).then_some(TeardownProgress::Pending),
                requested: None,
            },
        };
        document
    }

    fn teardown(unit: &str, spec: Value) -> DesiredUnit {
        deleting(unit, spec, DeletionReason::Teardown, DeletionIntent::Teardown)
    }

    fn map(units: Vec<DesiredUnit>) -> BTreeMap<UnitName, DesiredUnit> {
        units.into_iter().map(|unit| (unit.unit.name.clone(), unit)).collect()
    }

    struct Case {
        desired: BTreeMap<UnitName, DesiredUnit>,
        options: TeardownOptions<'static>,
        waiting: BTreeSet<UnitName>,
        drops: BTreeSet<UnitName>,
    }

    impl Case {
        fn new(units: Vec<DesiredUnit>) -> Self {
            Self {
                desired: map(units),
                options: TeardownOptions {
                    allow_teardown: true,
                    ..TeardownOptions::default()
                },
                waiting: BTreeSet::new(),
                drops: BTreeSet::new(),
            }
        }

        fn plan(&self) -> TeardownPlan {
            plan(&TeardownInputs {
                environment: &env(),
                desired: &self.desired,
                observed: &BTreeMap::new(),
                drivers: &drivers(),
                options: self.options,
                attempted: &BTreeSet::new(),
                waiting: &self.waiting,
                readiness: &BTreeMap::new(),
                drops: &self.drops,
            })
        }
    }

    fn names(units: &[&str]) -> Vec<UnitName> {
        units.iter().map(|unit| name(unit)).collect()
    }

    /// Teardown order: catches a producer torn down while a deleting
    /// consumer still uses it.
    #[test]
    fn test_consumers_are_torn_down_before_producers() {
        let case = Case::new(vec![
            teardown("network", json!({})),
            teardown("database", reads("network")),
            teardown("api", reads("database")),
        ]);
        assert_eq!(case.plan().ready, names(&["api", "database", "network"]));
    }

    /// Q6: catches a retention or a drop happening while a deleting
    /// consumer still needs the unit.
    #[test]
    fn test_retentions_and_drops_wait_for_their_deleting_dependents() {
        let mut case = Case::new(vec![
            deleting("image", json!({}), DeletionReason::Omission, DeletionIntent::Retain),
            deleting(
                "kubernetes",
                reads("image"),
                DeletionReason::Omission,
                DeletionIntent::Teardown,
            ),
        ]);
        case.options.allow_teardown = false;
        // Without --allow-teardown the consumer stays, and so does the image.
        let plan = case.plan();
        assert_eq!(plan.pending[&name("kubernetes")], Pending::NeedsAllowTeardown);
        assert_eq!(
            plan.pending[&name("image")],
            Pending::Dependents {
                units: names(&["kubernetes"])
            }
        );
        case.options.allow_teardown = true;
        assert_eq!(case.plan().ready, names(&["kubernetes", "image"]));

        let mut tombstone = fixtures::desired("cache", "Command", json!({}), "cache");
        tombstone.lifecycle = Lifecycle::Retained {
            tombstone: Tombstone {
                since: "2026-09-24T10:00:00Z".parse().unwrap(),
                receipt: None,
            },
        };
        let mut drop = Case::new(vec![
            tombstone,
            deleting(
                "worker",
                reads("cache"),
                DeletionReason::Omission,
                DeletionIntent::Teardown,
            ),
        ]);
        drop.drops = BTreeSet::from([name("cache")]);
        drop.options.allow_teardown = false;
        assert_eq!(
            drop.plan().pending[&name("cache")],
            Pending::Dependents {
                units: names(&["worker"])
            }
        );
        drop.options.allow_teardown = true;
        assert_eq!(drop.plan().ready, names(&["worker", "cache"]));
    }

    /// `teardownWait`: catches a waiting publication's dependencies being
    /// torn down under it, or an unrelated unit waiting needlessly.
    #[test]
    fn test_a_waiting_publication_gates_only_its_transitive_dependencies() {
        let mut case = Case::new(vec![
            teardown("cluster", json!({})),
            teardown("network", json!({})),
            teardown("database", reads("network")),
            teardown("kubernetes", reads("database")),
            teardown("logs", json!({})),
        ]);
        case.desired
            .get_mut(&name("kubernetes"))
            .unwrap()
            .spec
            .as_object_mut()
            .unwrap()
            .insert("dependsOn".to_owned(), json!(["cluster"]));
        case.waiting = BTreeSet::from([name("kubernetes")]);
        let plan = case.plan();
        assert_eq!(plan.ready, names(&["logs"]));
        for unit in ["cluster", "database", "network"] {
            assert_eq!(
                plan.pending[&name(unit)],
                Pending::Waiting {
                    units: names(&["kubernetes"])
                },
                "{unit}"
            );
        }
    }

    /// Q11: catches `teardown --unit X` tearing X down under a deleting
    /// dependent that was not named.
    #[test]
    fn test_a_named_teardown_waits_for_its_deleting_dependents() {
        let named = Box::leak(Box::new(BTreeSet::from([name("cache")])));
        let mut case = Case::new(vec![teardown("cache", json!({})), teardown("worker", reads("cache"))]);
        case.options = TeardownOptions {
            named: Some(named),
            explicit: true,
            ..TeardownOptions::default()
        };
        let plan = case.plan();
        assert!(plan.ready.is_empty());
        assert_eq!(plan.pending[&name("worker")], Pending::NotNamed);
        assert_eq!(
            plan.pending[&name("cache")],
            Pending::Dependents {
                units: names(&["worker"])
            }
        );
        let finding = finding(&env(), &name("cache"), &plan.pending[&name("cache")]).unwrap();
        assert_eq!(finding.code, Code::TeardownPending);
        assert!(finding.fix.contains("--unit worker"), "{}", finding.fix);
    }

    /// Retained dependents: catches a producer torn down under a tombstone
    /// whose resources may still use it, and the override going unrecorded.
    #[test]
    fn test_a_cited_unit_needs_allow_dependents_and_records_the_override() {
        let cache = teardown("cache", json!({}));
        let mut receipt = fixtures::receipt("worker", json!({}), Vec::new());
        receipt.provenance.insert(
            JsonPointer::new("/values/cache").unwrap(),
            serde_json::from_value(
                json!({"unit": "cache", "uid": cache.unit.uid, "executionKey": fixtures::key("cache")}),
            )
            .unwrap(),
        );
        let mut worker = fixtures::desired("worker", "Command", json!({}), "worker");
        worker.lifecycle = Lifecycle::Retained {
            tombstone: Tombstone {
                since: "2026-09-24T10:00:00Z".parse().unwrap(),
                receipt: Some(receipt),
            },
        };
        let mut case = Case::new(vec![cache, worker]);
        let refused = case.plan();
        assert_eq!(
            refused.pending[&name("cache")],
            Pending::HasDependents {
                tombstones: names(&["worker"])
            }
        );
        let warning = finding(&env(), &name("cache"), &refused.pending[&name("cache")]).unwrap();
        assert_eq!(warning.code, Code::TeardownHasDependents);
        assert!(warning.fix.contains("--allow-dependents"));
        case.options.allow_dependents = true;
        let allowed = case.plan();
        assert_eq!(allowed.ready, names(&["cache"]));
        assert_eq!(allowed.overrides[&name("cache")], names(&["worker"]));
    }

    /// Omission: catches an unintended omission destroying anything without
    /// `--allow-teardown`, while an explicit request needs none.
    #[test]
    fn test_omission_needs_allow_teardown_and_explicit_requests_do_not() {
        let mut case = Case::new(vec![
            deleting("api", json!({}), DeletionReason::Omission, DeletionIntent::Teardown),
            teardown("old", json!({})),
        ]);
        case.options.allow_teardown = false;
        let plan = case.plan();
        assert_eq!(plan.pending[&name("api")], Pending::NeedsAllowTeardown);
        assert_eq!(plan.ready, names(&["old"]));
        let warning = finding(&env(), &name("api"), &Pending::NeedsAllowTeardown).unwrap();
        assert_eq!(warning.code, Code::TeardownPending);
        assert!(warning.fix.contains("--allow-teardown") && warning.fix.contains("nyl state forget"));
    }

    /// Catches a release dropping the only record of an uncertain unit
    /// (such as one a dead run created and the next run no longer selects).
    #[test]
    fn test_an_uncertain_unit_is_not_released_without_recovery() {
        let image = deleting("image", json!({}), DeletionReason::Omission, DeletionIntent::Retain);
        let mut observed = fixtures::observed("image", "Publication", None);
        observed.condition = Some(Condition::Uncertain {
            since: "2026-09-24T10:00:00Z".parse().unwrap(),
            run: fixtures::run(fixtures::RUN_1),
            reason: "its run died".to_owned(),
        });
        let case = Case::new(vec![image]);
        let observed = BTreeMap::from([(name("image"), observed)]);
        let plan = plan(&TeardownInputs {
            environment: &env(),
            desired: &case.desired,
            observed: &observed,
            drivers: &drivers(),
            options: case.options,
            attempted: &BTreeSet::new(),
            waiting: &case.waiting,
            readiness: &BTreeMap::new(),
            drops: &case.drops,
        });
        assert_eq!(plan.pending[&name("image")], Pending::Uncertain);
    }

    /// Q26: catches an explicit teardown of a held unit destroying anything.
    #[test]
    fn test_a_held_unit_is_pending_under_an_explicit_teardown() {
        let mut api = teardown("api", json!({}));
        api.lifecycle = Lifecycle::Held {
            hold: Hold {
                since: "2026-09-24T10:00:00Z".parse().unwrap(),
                by: "alice".to_owned(),
                reason: None,
                pending: None,
            },
            deletion: Some(Deletion {
                hold: false,
                reason: DeletionReason::Teardown,
                intent: DeletionIntent::Teardown,
                teardown: Some(TeardownProgress::Pending),
                requested: Some(Request {
                    by: "alice".to_owned(),
                    at: "2026-09-24T10:00:00Z".parse().unwrap(),
                    reason: None,
                }),
            }),
        };
        let named = Box::leak(Box::new(BTreeSet::from([name("api")])));
        let mut case = Case::new(vec![api]);
        case.options = TeardownOptions {
            named: Some(named),
            explicit: true,
            ..TeardownOptions::default()
        };
        let plan = case.plan();
        assert_eq!(plan.pending[&name("api")], Pending::Held);
        assert_eq!(
            finding(&env(), &name("api"), &Pending::Held).map(|finding| finding.code),
            Some(Code::UnitHeld)
        );
    }
}
