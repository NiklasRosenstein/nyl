//! Desired documents: unit selection, the ownership set, incarnations,
//! lifecycle changes, and the environment record.
//!
//! Contract: [Units](../../../../design/orchestration-core.md#units),
//! [Identity](../../../../design/orchestration-core.md#identity),
//! [Desired unit](../../../../design/orchestration-core.md#desired-unit), and
//! [Deletion](../../../../design/orchestration-core.md#deletion), as amended
//! by the part 5 decisions (Q3, Q6, Q19, Q26).
//!
//! - The renderer selects units on their literal envelope and renders each
//!   selected spec once; a document that renders to nothing, or whose
//!   `enabled` is false after rendering, is not in the ownership set.
//! - Every unit's kind must have a driver, and its spec must pass the
//!   driver's `validate_spec` and the common fields' validation.
//! - A unit keeps the uid of its desired document while its `apiVersion` and
//!   `kind` stay; a new unit gets a new uid from the caller.
//! - Ownership scopes of the ownership set must not overlap
//!   ([`Code::UnitOwnershipOverlap`](nyl_core::orchestration::codes::Code::UnitOwnershipOverlap)):
//!   equal scopes overlap, and publication scopes overlap when their
//!   repository and revision are equal and one path prefix contains the other.
//! - Units in the snapshot's desired state that leave the ownership set are
//!   reported as leaving (`removed`, `deselected`, or `disabled`) with the
//!   consequence of their effective deletion policy (`teardown`, `dropped`,
//!   or `retained`), and a deselected unit with the selector labels it no
//!   longer matches. An active unit with an incarnation becomes `deleting`
//!   with `reason: omission` and the intent of its policy, keeping its uid
//!   and receipt; a uid-less file outside the set (a held marker or a held
//!   file after `resume`) is removed. Held units are kept as they are, and
//!   deleting and retained units stay until the run's leaving waves act on
//!   them ([`super::teardown`]).
//! - A unit that returns to the set while its omission or kind-change
//!   teardown is pending is active again with its uid and receipt. A
//!   returning `retained` tombstone of the same kind is active again with
//!   its uid and adopts the tombstone's receipt when the execution key is
//!   unchanged ([`adopt_tombstone`]): the key alone decides (Q3).
//! - A kind change under the same name ends the old incarnation: it is
//!   `deleting` with `reason: kind-change`, and the new kind is fenced
//!   ([`Desired::fenced`]) until the old file is removed; a deleting
//!   incarnation of any kind fences a new one, and a `retained` tombstone of
//!   another kind is dropped once no deleting unit depends on it.
//! - Held units (held markers included) never progress: they stay out of
//!   [`Desired::units`] and are kept in [`Desired::held`], with this run's
//!   rendering so the run can record what source would change
//!   ([`hold_pending`]).

use std::collections::{BTreeMap, BTreeSet};

use nyl_core::orchestration::codes::{Code, Finding};
use nyl_core::orchestration::driver::{Driver, DriverSet, OwnershipScope};
use nyl_core::orchestration::ids::{CommitId, EnvironmentName, Uid, UnitName};
use nyl_core::orchestration::ports::{EnvironmentSnapshot, RenderedSource, RenderedUnit};
use nyl_core::orchestration::state::{
    BlockedReason, DeletionIntent, DeletionReason, DesiredUnit, DesiredUnitKind, EnvironmentRecord,
    EnvironmentRecordKind, HoldPending, Lifecycle, ObservedUnit, Readiness, Receipt, SourceChoice, StateV1, Tombstone,
    UnitIdentity,
};
use nyl_core::orchestration::time::Timestamp;
use nyl_core::resources::units::{
    is_builtin_unit_api_version, CommandSpec, CommonUnitFields, DeletionPolicy, KindSupport, KIND_COMMAND,
};
use serde_json::Value;

use super::freshness::same_incarnation;
use super::lifecycle::{transition, Event, Next};
use crate::run_report::{LeavingConsequence, LeavingUnit};

/// What desiring one run's units reads.
pub struct DesireInputs<'a> {
    /// Units rendered at S, with the Environment as S declares it.
    pub source: &'a RenderedSource,
    /// The Environment spec the record keeps: S's Environment with group
    /// defaults applied and the entry worktree's `source`, `state`,
    /// `protectedRefs`, `allowUnprotectedSource`, and `admission`.
    pub environment_spec: &'a serde_json::Value,
    /// How S was chosen, recorded in the environment record.
    pub source_choice: SourceChoice,
    pub snapshot: &'a EnvironmentSnapshot,
    pub drivers: &'a DriverSet,
}

/// The desired state of one run before references are resolved.
#[derive(Debug, Clone, PartialEq)]
pub struct Desired {
    pub environment: EnvironmentRecord,
    /// Every unit of the ownership set that is not held: `spec` rendered
    /// with references kept as expressions, `resolved_spec`,
    /// `execution_key`, and `provenance` empty, `readiness` ready,
    /// lifecycle active, `source_commit` and `source_revision` set to S, and
    /// a uid. A fenced unit ([`Desired::fenced`]) is here without a uid and
    /// blocked, so consumers wait for it; the run never executes or writes
    /// it.
    pub units: BTreeMap<UnitName, DesiredUnit>,
    /// Units of the snapshot's desired state outside the ownership set.
    pub leaving: Vec<LeavingUnit>,
    /// Errors that stop the run before anything executes (exit 1).
    pub errors: Vec<Finding>,
    /// Lifecycle changes this run writes, decided through
    /// [`super::lifecycle::transition`]: units leaving the ownership set
    /// (deleting, or removed without an incarnation), units returning to it,
    /// and kind changes. Retentions, releases, and drops wait for the run's
    /// teardown plan ([`super::teardown::plan`]) and are decided by the run
    /// with [`write`]. For a unit in [`Desired::units`] the run writes the
    /// resolved document instead of [`LifecycleWrite::desired`]. Contract:
    /// [Deletion](../../../../design/orchestration-core.md#deletion).
    pub lifecycle: BTreeMap<UnitName, LifecycleWrite>,
    /// Held units, as stored (held markers included): a hold freezes the
    /// document, so they are not rendered into [`Desired::units`], but
    /// consumers resolve against them.
    pub held: BTreeMap<UnitName, HeldUnit>,
    /// Returning units adopting a `retained` tombstone of the same name and
    /// kind: the tombstone, whose receipt the run restores when
    /// [`adopt_tombstone`] finds it still current after resolution.
    pub tombstones: BTreeMap<UnitName, Tombstone>,
    /// Units of the ownership set that wait for a deleting incarnation of
    /// the same name (of the same or another kind), or for a `retained`
    /// tombstone of another kind to be dropped, before they start.
    pub fenced: BTreeSet<UnitName>,
}

/// One lifecycle change of a run.
#[derive(Debug, Clone, PartialEq)]
pub struct LifecycleWrite {
    /// The desired document to write; `None` removes the desired file.
    pub desired: Option<DesiredUnit>,
    /// Whether the observed file goes, with the unit's observations and
    /// attestations: a retained tombstone embeds the receipt (its artifact
    /// files stay, Q12), and a removed or released unit, or one whose
    /// incarnation ended, leaves no observed state (its artifacts go too).
    pub remove_observed: bool,
    /// What happened, for the report and the transition summary.
    pub outcome: LifecycleOutcome,
}

/// What a lifecycle change did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LifecycleOutcome {
    /// Deleting, its teardown (or its retention or release) pending.
    PendingTeardown,
    /// Replaced by a `retained` tombstone.
    Retained,
    /// A kind that cannot tear down left its resources in place, as `left` says.
    Released { left: Option<String> },
    /// Back in the ownership set: active again with its uid and receipt, or
    /// adopting its tombstone.
    Returned,
    /// Its kind changed: the old incarnation is deleting.
    KindChange,
    /// A file without an incarnation outside the ownership set (a held
    /// marker or held file after `resume`), or a torn-down unit: its files
    /// were removed.
    Removed,
}

/// A held unit as a run sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct HeldUnit {
    /// The stored desired document, which the run keeps.
    pub stored: DesiredUnit,
    /// The unit as source renders it at S, before resolution, so the run
    /// can record what it would change under `hold.pending`; `None` when the
    /// unit is outside the ownership set. A held marker's `stored` has no
    /// rendering ([`DesiredUnit::is_held_marker`]).
    pub rendered: Option<DesiredUnit>,
}

/// The `hold.pending` a run records for a held unit: S, the execution key of
/// the unit as S would resolve it (`None` while that resolution is
/// blocked), and whether it left the ownership set. `None` when source
/// would change nothing: the unit is in the set, has an incarnation, and
/// resolves to the execution key of the held document.
pub fn hold_pending(held: &HeldUnit, source_commit: &CommitId, resolved: Option<&DesiredUnit>) -> Option<HoldPending> {
    if held.rendered.is_none() {
        return Some(HoldPending {
            source_commit: source_commit.clone(),
            execution_key: None,
            deletion: true,
        });
    }
    let key = resolved.and_then(|resolved| resolved.execution_key.clone());
    // A held file without an incarnation would be created: always pending.
    if key.is_some() && key == held.stored.execution_key && held.stored.unit.uid.is_some() {
        return None;
    }
    Some(HoldPending {
        source_commit: source_commit.clone(),
        execution_key: key,
        deletion: false,
    })
}

/// Whether two pending records name the same pending change: the same
/// execution key and deletion. A run records `hold.pending` only when it
/// changes, so a newer source commit that changes nothing more writes
/// nothing.
pub fn same_pending(left: Option<&HoldPending>, right: Option<&HoldPending>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => left.execution_key == right.execution_key && left.deletion == right.deletion,
        _ => false,
    }
}

/// The receipt a returning unit adopts from its tombstone: the tombstone's
/// receipt when `resolved` has the same execution key (the execution key
/// alone decides freshness; producer identity never does); `None`
/// otherwise, so the unit runs as an ordinary change of the same
/// incarnation. Contract: [Deletion](../../../../design/orchestration-core.md#deletion),
/// "Retain, declared", as amended by part 5 decision Q3.
pub fn adopt_tombstone(resolved: &DesiredUnit, tombstone: &Tombstone) -> Option<Receipt> {
    let receipt = tombstone.receipt.as_ref()?;
    let key = resolved.execution_key.as_ref()?;
    (resolved.unit.uid.is_some() && receipt.execution_key == *key).then(|| receipt.clone())
}

/// The tombstone a declared `Retain` unit leaves: the receipt of its
/// incarnation, retained at `now`.
pub fn tombstone(unit: &DesiredUnit, observed: Option<&ObservedUnit>, now: Timestamp) -> Tombstone {
    Tombstone {
        since: now,
        receipt: same_incarnation(unit, observed).and_then(|observed| observed.receipt.clone()),
    }
}

/// What a released unit leaves in place, from its receipt's artifacts,
/// such as `ContainerImage image (sha256:…)`; `None` without artifacts.
pub fn released_left(unit: &DesiredUnit, observed: Option<&ObservedUnit>) -> Option<String> {
    let receipt = same_incarnation(unit, observed)?.receipt.as_ref()?;
    let left: Vec<String> = receipt
        .artifacts
        .iter()
        .map(|artifact| format!("{} {} ({})", artifact.kind, artifact.name, artifact.digest))
        .collect();
    (!left.is_empty()).then(|| left.join(", "))
}

/// The documents a lifecycle change of `stored` writes, as
/// [`super::lifecycle::Next`] says (the cross-package contract "Lifecycle
/// changes go through the table"): `Remove` drops every file of the unit,
/// `EndIncarnation` keeps the desired file without its uid, a `retained`
/// tombstone drops the observed file but keeps the artifacts, and
/// `Unchanged` writes nothing. A `RestoreReceipt` writes the desired file;
/// the caller writes the tombstone's receipt back as the observed file.
pub fn write(stored: &DesiredUnit, next: Next, outcome: LifecycleOutcome) -> Option<LifecycleWrite> {
    let with = |lifecycle: Lifecycle| DesiredUnit {
        lifecycle,
        ..stored.clone()
    };
    Some(match next {
        Next::Unchanged => return None,
        Next::Remove => LifecycleWrite {
            desired: None,
            remove_observed: true,
            outcome,
        },
        Next::EndIncarnation(lifecycle) => {
            let mut desired = with(lifecycle);
            desired.unit.uid = None;
            LifecycleWrite {
                desired: Some(desired),
                remove_observed: true,
                outcome,
            }
        }
        Next::Lifecycle(lifecycle) => LifecycleWrite {
            remove_observed: matches!(lifecycle, Lifecycle::Retained { .. }),
            desired: Some(with(lifecycle)),
            outcome,
        },
        Next::RestoreReceipt(lifecycle) => LifecycleWrite {
            desired: Some(with(lifecycle)),
            remove_observed: false,
            outcome,
        },
    })
}

/// The deletion intent of a unit's stored spec: `teardown` under an
/// effective `Teardown` policy, else `retain`.
pub fn deletion_intent(drivers: &DriverSet, unit: &DesiredUnit) -> DeletionIntent {
    match effective_policy(drivers, unit) {
        (DeletionPolicy::Teardown, _) => DeletionIntent::Teardown,
        (DeletionPolicy::Retain, _) => DeletionIntent::Retain,
    }
}

/// Whether a unit's stored spec declares `deletionPolicy: Retain`, which
/// leaves a tombstone, rather than retaining by kind, which leaves nothing.
pub fn declares_retain(unit: &DesiredUnit) -> bool {
    serde_json::from_value::<CommonUnitFields>(unit.spec.clone())
        .is_ok_and(|common| common.deletion_policy == Some(DeletionPolicy::Retain))
}

/// The effective deletion policy of a stored unit, and the declared one.
fn effective_policy(drivers: &DriverSet, unit: &DesiredUnit) -> (DeletionPolicy, Option<DeletionPolicy>) {
    let common = serde_json::from_value::<CommonUnitFields>(unit.spec.clone()).unwrap_or_default();
    let teardown = drivers
        .get(&unit.unit.api_version, &unit.unit.kind)
        .is_some_and(|driver| driver.capabilities(&unit.spec).teardown);
    let policy = common.effective_deletion_policy(KindSupport {
        declared_outputs: true,
        teardown,
    });
    (policy, common.deletion_policy)
}

/// The readiness of a fenced unit: blocked until the incarnation of the
/// same name it waits for is removed.
fn fenced_readiness(name: &UnitName, stored: &DesiredUnit) -> Readiness {
    let why = match &stored.lifecycle {
        Lifecycle::Retained { .. } => format!("its retained tombstone of kind {} is dropped first", stored.unit.kind),
        _ => format!(
            "the deleting incarnation of {name} (kind {}) is removed first",
            stored.unit.kind
        ),
    };
    Readiness::Blocked {
        reasons: vec![BlockedReason {
            pointer: String::new(),
            reason: why,
        }],
    }
}

/// The selected, enabled units: those rendered to a spec whose `enabled`
/// is not `false`.
pub fn ownership_set(source: &RenderedSource) -> BTreeMap<&UnitName, &RenderedUnit> {
    source
        .units
        .iter()
        .filter(|(_, unit)| unit.selected && unit.spec.as_ref().is_some_and(is_enabled))
        .collect()
}

/// Reject overlapping native resources across all declared environments,
/// using the same scope rules as a single environment's desired documents.
/// Contract: [Identity](../../../../design/orchestration-core.md#identity).
pub fn check_ownership(sources: &BTreeMap<EnvironmentName, RenderedSource>, drivers: &DriverSet) -> Vec<Finding> {
    check_ownership_with_snapshots(sources, &BTreeMap::new(), drivers)
}

/// Stored incarnations keep their claims while their source is unavailable.
pub fn check_ownership_with_snapshots(
    sources: &BTreeMap<EnvironmentName, RenderedSource>,
    snapshots: &BTreeMap<EnvironmentName, EnvironmentSnapshot>,
    drivers: &DriverSet,
) -> Vec<Finding> {
    let mut scopes = Vec::new();
    for (environment, source) in sources {
        for (name, unit) in ownership_set(source) {
            let Some(driver) = drivers.get(&unit.envelope.api_version, &unit.envelope.kind) else {
                continue;
            };
            for scope in driver.ownership_scopes(environment, unit.spec.as_ref().expect("ownership set")) {
                scopes.push((scope, format!("{environment}/{name}")));
            }
        }
    }
    for (environment, snapshot) in snapshots {
        for (name, unit) in &snapshot.desired {
            if unit.unit.uid.is_none() {
                continue;
            }
            let Some(driver) = drivers.get(&unit.unit.api_version, &unit.unit.kind) else {
                continue;
            };
            for scope in driver.ownership_scopes(environment, unit.resolved_spec.as_ref().unwrap_or(&unit.spec)) {
                scopes.push((scope, format!("{environment}/{name}")));
            }
        }
    }
    overlaps(&scopes)
}

/// Environments whose source units claim native resources. Without a failed
/// environment's source or state, these cannot prove disjoint ownership.
pub fn scope_owners(
    sources: &BTreeMap<EnvironmentName, RenderedSource>,
    drivers: &DriverSet,
) -> BTreeSet<EnvironmentName> {
    sources
        .iter()
        .filter(|(environment, source)| {
            ownership_set(source).values().any(|unit| {
                drivers
                    .get(&unit.envelope.api_version, &unit.envelope.kind)
                    .is_some_and(|driver| {
                        !driver
                            .ownership_scopes(environment, unit.spec.as_ref().expect("ownership set"))
                            .is_empty()
                    })
            })
        })
        .map(|(environment, _)| environment.clone())
        .collect()
}

/// `enabled` as the rendered spec says it; absent means true.
fn is_enabled(spec: &Value) -> bool {
    spec.get("enabled").and_then(Value::as_bool) != Some(false)
}

/// Build the desired documents of one run. `new_uid` allocates a uid for a
/// new incarnation; it is the only identifier source, so the result is a
/// function of the inputs and the identifiers it hands out.
#[allow(clippy::too_many_lines)] // One pass over the ownership set, then one over the units leaving it.
pub fn desire(inputs: &DesireInputs<'_>, new_uid: &mut dyn FnMut() -> Uid) -> Desired {
    let source = inputs.source;
    let environment = source
        .environment
        .name()
        .expect("discovery validates Environment names");
    let set = ownership_set(source);
    let mut errors = Vec::new();
    let mut units = BTreeMap::new();
    let mut scopes: Vec<(OwnershipScope, UnitName)> = Vec::new();
    let mut lifecycle = BTreeMap::new();
    let mut held = BTreeMap::new();
    let mut tombstones = BTreeMap::new();
    let mut fenced = BTreeSet::new();
    let mut ownership = BTreeMap::new();

    for (&name, &rendered) in &set {
        let spec = rendered.spec.as_ref().expect("the ownership set holds rendered specs");
        let api_version = &rendered.envelope.api_version;
        let kind = &rendered.envelope.kind;
        let Some(driver) = inputs.drivers.get(api_version, kind) else {
            errors.push(Finding::new(
                Code::UnitKindUnknown,
                format!(
                    "unit {name} ({}) has kind {kind} of {api_version}, which no driver serves",
                    rendered.source_path
                ),
                "use a unit kind Nyl provides, such as units.gitops.nyl/v1 Command, or correct the unit's apiVersion and kind",
            ));
            continue;
        };
        if let Err(error) = validate_unit(driver, api_version, kind, spec) {
            errors.push(error.into_finding(name, &rendered.source_path));
            continue;
        }
        for scope in driver.ownership_scopes(&environment, spec) {
            scopes.push((scope, name.clone()));
        }
        let document = |uid: Option<Uid>| DesiredUnit {
            api_version: StateV1,
            kind: DesiredUnitKind,
            unit: UnitIdentity {
                environment: environment.clone(),
                name: name.clone(),
                uid,
                api_version: api_version.clone(),
                kind: kind.clone(),
            },
            lifecycle: Lifecycle::Active,
            source_commit: Some(source.source_commit.clone()),
            source_revision: Some(source.source_commit.clone()),
            spec: spec.clone(),
            resolved_spec: None,
            provenance: BTreeMap::new(),
            execution_key: None,
            fingerprint: None,
            attestations: Vec::new(),
            readiness: Readiness::Ready,
        };
        let Some(stored) = inputs.snapshot.desired.get(name) else {
            let unit = document(Some(new_uid()));
            ownership.insert(name.clone(), unit.unit.uid.clone());
            units.insert(name.clone(), unit);
            continue;
        };
        let same_kind = stored.unit.api_version == *api_version && stored.unit.kind == *kind;
        let fence = |units: &mut BTreeMap<UnitName, DesiredUnit>, fenced: &mut BTreeSet<UnitName>| {
            let mut unit = document(None);
            unit.readiness = fenced_readiness(name, stored);
            units.insert(name.clone(), unit);
            fenced.insert(name.clone());
        };
        match (&stored.lifecycle, same_kind) {
            // A hold freezes the stored document, markers included.
            (Lifecycle::Held { .. }, _) => {
                let uid = if same_kind { stored.unit.uid.clone() } else { None };
                ownership.insert(name.clone(), stored.unit.uid.clone());
                held.insert(
                    name.clone(),
                    HeldUnit {
                        stored: stored.clone(),
                        rendered: Some(document(uid)),
                    },
                );
                continue;
            }
            (Lifecycle::Active, true) => {
                let unit = document(Some(stored.unit.uid.clone().unwrap_or_else(&mut *new_uid)));
                ownership.insert(name.clone(), unit.unit.uid.clone());
                units.insert(name.clone(), unit);
                continue;
            }
            // A uid-less file of another kind: nothing to end.
            (Lifecycle::Active, false) if stored.unit.uid.is_none() => {
                let unit = document(Some(new_uid()));
                ownership.insert(name.clone(), unit.unit.uid.clone());
                units.insert(name.clone(), unit);
                continue;
            }
            (Lifecycle::Active, false) => {
                let intent = deletion_intent(inputs.drivers, stored);
                let next = transition(
                    &stored.lifecycle,
                    Event::Left {
                        reason: DeletionReason::KindChange,
                        intent,
                    },
                );
                if let Some(write) = next
                    .ok()
                    .and_then(|next| write(stored, next, LifecycleOutcome::KindChange))
                {
                    lifecycle.insert(name.clone(), write);
                }
                fence(&mut units, &mut fenced);
            }
            (Lifecycle::Deleting { .. }, true) => match transition(&stored.lifecycle, Event::Returned) {
                Ok(Next::Lifecycle(Lifecycle::Active)) => {
                    let unit = document(Some(stored.unit.uid.clone().unwrap_or_else(&mut *new_uid)));
                    lifecycle.insert(
                        name.clone(),
                        LifecycleWrite {
                            desired: Some(unit.clone()),
                            remove_observed: false,
                            outcome: LifecycleOutcome::Returned,
                        },
                    );
                    ownership.insert(name.clone(), unit.unit.uid.clone());
                    units.insert(name.clone(), unit);
                    continue;
                }
                // A started or explicitly requested teardown continues;
                // the new incarnation waits for it.
                _ => fence(&mut units, &mut fenced),
            },

            (Lifecycle::Retained { tombstone }, true) => {
                let unit = document(Some(stored.unit.uid.clone().unwrap_or_else(&mut *new_uid)));
                if transition(&stored.lifecycle, Event::Returned) == Ok(Next::Lifecycle(Lifecycle::Active)) {
                    tombstones.insert(name.clone(), tombstone.clone());
                    lifecycle.insert(
                        name.clone(),
                        LifecycleWrite {
                            desired: Some(unit.clone()),
                            remove_observed: false,
                            outcome: LifecycleOutcome::Returned,
                        },
                    );
                }
                ownership.insert(name.clone(), unit.unit.uid.clone());
                units.insert(name.clone(), unit);
                continue;
            }
            // A deleting incarnation of another kind fences the new one; the
            // tombstone of another kind is dropped by the run once no
            // deleting unit depends on it ([`super::teardown::plan`]).
            (Lifecycle::Deleting { .. } | Lifecycle::Retained { .. }, false) => fence(&mut units, &mut fenced),
        }
        ownership.insert(name.clone(), None);
    }
    errors.extend(overlaps(&scopes));

    // Units outside the ownership set.
    for (name, stored) in &inputs.snapshot.desired {
        if set.contains_key(name) {
            continue;
        }
        match &stored.lifecycle {
            Lifecycle::Held { .. } => {
                held.insert(
                    name.clone(),
                    HeldUnit {
                        stored: stored.clone(),
                        rendered: None,
                    },
                );
            }
            Lifecycle::Active if stored.unit.uid.is_none() => {
                let next = transition(&stored.lifecycle, Event::LeftWithoutIncarnation);
                if let Some(write) = next
                    .ok()
                    .and_then(|next| write(stored, next, LifecycleOutcome::Removed))
                {
                    lifecycle.insert(name.clone(), write);
                }
            }
            Lifecycle::Active => {
                let event = Event::Left {
                    reason: DeletionReason::Omission,
                    intent: deletion_intent(inputs.drivers, stored),
                };
                let next = transition(&stored.lifecycle, event);
                if let Some(write) = next
                    .ok()
                    .and_then(|next| write(stored, next, LifecycleOutcome::PendingTeardown))
                {
                    lifecycle.insert(name.clone(), write);
                }
            }
            Lifecycle::Deleting { .. } | Lifecycle::Retained { .. } => {}
        }
    }

    let leaving = leaving_units(inputs, &set);

    Desired {
        environment: EnvironmentRecord {
            api_version: StateV1,
            kind: EnvironmentRecordKind,
            environment,
            spec: inputs.environment_spec.clone(),
            source_commit: source.source_commit.clone(),
            source_chosen_by: inputs.source_choice.clone(),
            applied_promotion_sequence: None,
            ownership,
        },
        units,
        leaving,
        errors,
        lifecycle,
        held,
        tombstones,
        fenced,
    }
}

/// The state-free checks of one environment's rendered units, which
/// `plan`, `reconcile`, and `nyl validate` share: every unit's kind has a
/// driver and its spec passes the driver's and the common fields'
/// validation, ownership scopes do not overlap,
/// and the dependency graph ([`super::graph::extract`]) has no unknown unit,
/// cycle, undeclared or sensitive output reference, or unsupported evidence
/// requirement. `plan` and `reconcile` reach the same checks through
/// [`desire`] and `extract`; this runs them without state.
pub fn check_units(source: &RenderedSource, drivers: &DriverSet) -> Vec<Finding> {
    let snapshot = EnvironmentSnapshot::default();
    let mut next = 0u64;
    let desired = desire(
        &DesireInputs {
            source,
            environment_spec: &Value::Null,
            source_choice: SourceChoice::Entry,
            snapshot: &snapshot,
            drivers,
        },
        &mut || {
            next += 1;
            Uid::new(format!("00000000-0000-7000-8000-{next:012x}")).expect("a UUID")
        },
    );
    if !desired.errors.is_empty() {
        return desired.errors;
    }
    match super::graph::extract(&desired.environment.environment, &source.source_commit, &desired.units) {
        Ok(_) => Vec::new(),
        Err(findings) => findings,
    }
}

/// The units of the snapshot's desired state that leave the ownership set:
/// active units with an incarnation that leave in this run, held units with
/// a pending deletion, and deleting units outside the set.
fn leaving_units(inputs: &DesireInputs<'_>, set: &BTreeMap<&UnitName, &RenderedUnit>) -> Vec<LeavingUnit> {
    let source = inputs.source;
    inputs
        .snapshot
        .desired
        .iter()
        .filter(|(name, desired)| {
            !set.contains_key(name)
                && match &desired.lifecycle {
                    Lifecycle::Active => desired.unit.uid.is_some(),
                    Lifecycle::Held { .. } => !desired.is_held_marker() && desired.unit.uid.is_some(),
                    Lifecycle::Deleting { .. } => true,
                    Lifecycle::Retained { .. } => false,
                }
        })
        .map(|(name, desired)| {
            let rendered = source.units.get(name);
            LeavingUnit {
                unit: name.clone(),
                kind: desired.unit.kind.clone(),
                reason: leaving_reason(rendered).to_owned(),
                consequence: leaving_consequence(inputs.drivers, desired),
                labels: rendered
                    .filter(|rendered| !rendered.selected)
                    .map(|rendered| unmatched_labels(&source.environment, rendered))
                    .unwrap_or_default(),
            }
        })
        .collect()
}

/// Why a unit's spec is not accepted.
enum UnitError {
    Invalid(String),
}

impl UnitError {
    fn into_finding(self, name: &UnitName, source_path: &str) -> Finding {
        match self {
            Self::Invalid(message) => Finding::new(
                Code::UnitInvalid,
                format!("unit {name} ({source_path}): {message}"),
                "correct the unit's spec so that it matches its kind's schema",
            ),
        }
    }
}

/// The driver's validation and the common fields' validation.
fn validate_unit(driver: &dyn Driver, api_version: &str, kind: &str, spec: &Value) -> Result<(), UnitError> {
    let invalid = |error: &dyn std::fmt::Display| UnitError::Invalid(error.to_string());
    driver.validate_spec(spec).map_err(UnitError::Invalid)?;
    let common: CommonUnitFields = serde_json::from_value(spec.clone()).map_err(|error| invalid(&error))?;
    if common.approval.bind == nyl_core::resources::units::ApprovalBind::Plan
        && !driver.capabilities(spec).change_digest
    {
        return Err(UnitError::Invalid(
            "approval bind: plan requires a kind that reports change digests".to_owned(),
        ));
    }
    if is_builtin_unit_api_version(api_version) && kind == KIND_COMMAND {
        let command: CommandSpec = serde_json::from_value(spec.clone()).map_err(|error| invalid(&error))?;
        command.validate().map_err(|error| invalid(&error))?;
    } else {
        let common: CommonUnitFields = serde_json::from_value(spec.clone()).map_err(|error| invalid(&error))?;
        let support = KindSupport {
            // Whether a kind lets units declare outputs is its driver's to check in `validate_spec`.
            declared_outputs: true,
            teardown: driver.capabilities(spec).teardown,
        };
        common.validate(support).map_err(|error| invalid(&error))?;
    }
    Ok(())
}

/// One finding for every pair of units whose ownership scopes overlap.
fn overlaps<N: Ord + std::fmt::Display>(scopes: &[(OwnershipScope, N)]) -> Vec<Finding> {
    let mut findings = Vec::new();
    let mut reported = BTreeSet::new();
    for (index, (scope, unit)) in scopes.iter().enumerate() {
        for (other_scope, other_unit) in &scopes[index + 1..] {
            if unit == other_unit || !scopes_overlap(scope, other_scope) {
                continue;
            }
            let pair = if unit < other_unit {
                (unit, other_unit)
            } else {
                (other_unit, unit)
            };
            if !reported.insert(pair) {
                continue;
            }
            findings.push(Finding::new(
                Code::UnitOwnershipOverlap,
                format!(
                    "units {} and {} claim overlapping resources: {} and {}",
                    pair.0,
                    pair.1,
                    describe_scope(scope),
                    describe_scope(other_scope)
                ),
                "give each unit resources of its own, such as a distinct backend key or publication path prefix, or remove one of the units",
            ));
        }
    }
    findings
}

fn scopes_overlap(left: &OwnershipScope, right: &OwnershipScope) -> bool {
    if left == right {
        return true;
    }
    match (left, right) {
        (
            OwnershipScope::Publication {
                repository,
                revision,
                path_prefix,
            },
            OwnershipScope::Publication {
                repository: other_repository,
                revision: other_revision,
                path_prefix: other_prefix,
            },
        ) => repository == other_repository && revision == other_revision && prefixes_nest(path_prefix, other_prefix),
        _ => false,
    }
}

/// Whether one path prefix equals or contains the other, by path segment.
fn prefixes_nest(left: &str, right: &str) -> bool {
    let segments = |prefix: &str| -> Vec<String> {
        prefix
            .split('/')
            .filter(|segment| !segment.is_empty() && *segment != ".")
            .map(str::to_owned)
            .collect()
    };
    let (left, right) = (segments(left), segments(right));
    left.starts_with(&right) || right.starts_with(&left)
}

fn describe_scope(scope: &OwnershipScope) -> String {
    match scope {
        OwnershipScope::TerraformBackend { backend, workspace } => {
            format!("Terraform backend {backend} workspace {workspace}")
        }
        OwnershipScope::OciRepository { repository, tag_prefix } => {
            format!("OCI repository {repository} tags {tag_prefix}*")
        }
        OwnershipScope::Publication {
            repository,
            revision,
            path_prefix,
        } => format!("publication {repository} {revision} {path_prefix}"),
    }
}

/// `removed` when S no longer declares the unit, `deselected` when the
/// Environment no longer selects it, else `disabled` (`enabled: false`, or a
/// spec that renders to nothing).
fn leaving_reason(rendered: Option<&RenderedUnit>) -> &'static str {
    match rendered {
        None => "removed",
        Some(rendered) if !rendered.selected => "deselected",
        Some(_) => "disabled",
    }
}

/// What happens to a leaving unit, by its stored spec's effective deletion
/// policy (or a deleting unit's recorded intent): `teardown`, `retained`
/// for a declared `Retain`, which leaves a tombstone, or `dropped` for a
/// kind that cannot tear down and declares no policy.
fn leaving_consequence(drivers: &DriverSet, desired: &DesiredUnit) -> LeavingConsequence {
    let intent = match &desired.lifecycle {
        Lifecycle::Deleting { deletion } => deletion.intent,
        _ => deletion_intent(drivers, desired),
    };
    match intent {
        DeletionIntent::Teardown => LeavingConsequence::Teardown,
        DeletionIntent::Retain if declares_retain(desired) => LeavingConsequence::Retained,
        DeletionIntent::Retain => LeavingConsequence::Dropped,
    }
}

/// The Environment's `unitSelector` labels that a deselected unit's literal
/// labels do not match.
fn unmatched_labels(
    environment: &nyl_core::resources::environment::Environment,
    rendered: &RenderedUnit,
) -> BTreeMap<String, String> {
    environment
        .spec
        .unit_selector
        .match_labels
        .iter()
        .filter(|(key, value)| rendered.envelope.metadata.labels.get(*key) != Some(*value))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use nyl_core::constants::API_VERSION_UNITS;
    use nyl_core::orchestration::ids::{CommitId, EnvironmentName};
    use nyl_core::resources::units::{UnitEnvelope, UnitMetadata};
    use serde_json::json;

    use super::*;
    use crate::rules::fixtures::{name, uid_numbered as uid, TestDriver};
    use crate::rules::lifecycle::{transition, Event, Next};
    use nyl_core::orchestration::state::{DeletionIntent, DeletionReason};

    fn drivers() -> DriverSet {
        let mut drivers = DriverSet::new();
        drivers.register(Arc::new(TestDriver { kind: KIND_COMMAND }));
        drivers.register(Arc::new(TestDriver { kind: "Publication" }));
        drivers
    }

    fn commit(digit: char) -> CommitId {
        CommitId::new(digit.to_string().repeat(40)).unwrap()
    }

    fn rendered(kind: &str, selected: bool, spec: Option<Value>) -> RenderedUnit {
        RenderedUnit {
            envelope: UnitEnvelope {
                api_version: API_VERSION_UNITS.to_owned(),
                kind: kind.to_owned(),
                metadata: UnitMetadata {
                    name: "unused".to_owned(),
                    labels: BTreeMap::new(),
                },
            },
            source_path: "/nyl/units.yaml".to_owned(),
            selected,
            spec,
        }
    }

    fn source(units: Vec<(&str, RenderedUnit)>) -> RenderedSource {
        RenderedSource {
            source_commit: commit('a'),
            project_dir: "/nyl".to_owned(),
            environment: serde_json::from_value(json!({
                "apiVersion": "gitops.nyl/v1",
                "kind": "Environment",
                "metadata": {"name": "dev"}
            }))
            .unwrap(),
            units: units
                .into_iter()
                .map(|(unit, rendered)| (name(unit), rendered))
                .collect(),
        }
    }

    fn stored(unit: &str, kind: &str, uid_number: u64, spec: Value) -> DesiredUnit {
        DesiredUnit {
            api_version: StateV1,
            kind: DesiredUnitKind,
            unit: UnitIdentity {
                environment: EnvironmentName::new("dev").unwrap(),
                name: name(unit),
                uid: Some(uid(uid_number)),
                api_version: API_VERSION_UNITS.to_owned(),
                kind: kind.to_owned(),
            },
            lifecycle: Lifecycle::Active,
            source_commit: Some(commit('b')),
            source_revision: Some(commit('b')),
            spec,
            resolved_spec: None,
            provenance: BTreeMap::new(),
            execution_key: None,
            fingerprint: None,
            attestations: Vec::new(),
            readiness: Readiness::Ready,
        }
    }

    fn run_desire(source: &RenderedSource, snapshot: &EnvironmentSnapshot) -> Desired {
        let drivers = drivers();
        let mut next = 100;
        desire(
            &DesireInputs {
                source,
                environment_spec: &json!({}),
                source_choice: SourceChoice::Entry,
                snapshot,
                drivers: &drivers,
            },
            &mut || {
                next += 1;
                uid(next)
            },
        )
    }

    fn command(extra: Value) -> Value {
        let mut spec = json!({"command": ["./run.sh"]});
        if let Value::Object(extra) = extra {
            spec.as_object_mut().unwrap().extend(extra);
        }
        spec
    }

    /// Catches a disabled, deselected, or empty unit being executed (or
    /// deleted as if removed) instead of leaving the ownership set.
    #[test]
    fn test_ownership_set_holds_selected_units_with_an_enabled_spec() {
        let source = source(vec![
            ("enabled", rendered(KIND_COMMAND, true, Some(command(json!({}))))),
            (
                "explicit",
                rendered(KIND_COMMAND, true, Some(command(json!({"enabled": true})))),
            ),
            (
                "disabled",
                rendered(KIND_COMMAND, true, Some(command(json!({"enabled": false})))),
            ),
            ("empty", rendered(KIND_COMMAND, true, None)),
            ("deselected", rendered(KIND_COMMAND, false, Some(command(json!({}))))),
        ]);
        let set: Vec<&str> = ownership_set(&source).keys().map(|unit| unit.as_str()).collect();
        assert_eq!(set, ["enabled", "explicit"]);
    }

    /// Catches a unit losing its incarnation on every run (which would
    /// invalidate its receipt) or keeping it across a kind change, whose
    /// new incarnation waits for the old one to be removed.
    #[test]
    fn test_uid_is_kept_while_the_kind_stays_and_new_after_a_kind_change() {
        let source = source(vec![
            ("same", rendered(KIND_COMMAND, true, Some(command(json!({}))))),
            ("changed", rendered("Publication", true, Some(json!({"prefix": "dev"})))),
            ("new", rendered(KIND_COMMAND, true, Some(command(json!({}))))),
        ]);
        let snapshot = EnvironmentSnapshot {
            desired: BTreeMap::from([
                (name("same"), stored("same", KIND_COMMAND, 1, command(json!({})))),
                (name("changed"), stored("changed", KIND_COMMAND, 2, command(json!({})))),
            ]),
            ..EnvironmentSnapshot::default()
        };
        let desired = run_desire(&source, &snapshot);
        assert!(desired.errors.is_empty(), "{:?}", desired.errors);
        let uid_of = |unit: &str| desired.units[&name(unit)].unit.uid.clone();
        assert_eq!(uid_of("same"), Some(uid(1)));
        assert_eq!(uid_of("changed"), None, "fenced until the old incarnation is removed");
        assert_eq!(uid_of("new"), Some(uid(101)));
        assert_eq!(desired.environment.ownership[&name("same")], Some(uid(1)));
        let unit = &desired.units[&name("same")];
        assert_eq!(unit.source_revision, Some(commit('a')));
        assert_eq!(unit.readiness, Readiness::Ready);
        assert!(unit.resolved_spec.is_none() && unit.execution_key.is_none());
    }

    /// Catches two units publishing into the same or a nested path, which
    /// would make them overwrite and prune each other's files.
    #[test]
    fn test_overlapping_publication_scopes_are_rejected_naming_both_units() {
        let source = source(vec![
            (
                "apps",
                rendered("Publication", true, Some(json!({"prefix": "clusters/dev"}))),
            ),
            (
                "nested",
                rendered("Publication", true, Some(json!({"prefix": "clusters/dev/web"}))),
            ),
            (
                "other",
                rendered("Publication", true, Some(json!({"prefix": "clusters/devx"}))),
            ),
        ]);
        let desired = run_desire(&source, &EnvironmentSnapshot::default());
        assert_eq!(desired.errors.len(), 1, "{:?}", desired.errors);
        let error = &desired.errors[0];
        assert_eq!(error.code, Code::UnitOwnershipOverlap);
        assert!(
            error.message.contains("apps") && error.message.contains("nested"),
            "{}",
            error.message
        );
    }

    /// The same unit name in two environments must not overwrite or prune
    /// the other environment's publication. Deselected units claim nothing.
    #[test]
    fn test_cross_environment_ownership_names_both_addresses() {
        let dev = source(vec![(
            "publish",
            rendered("Publication", true, Some(json!({"prefix": "dev"}))),
        )]);
        let mut prod = source(vec![
            (
                "publish",
                rendered("Publication", true, Some(json!({"prefix": "dev/web"}))),
            ),
            (
                "ignored",
                rendered("Publication", false, Some(json!({"prefix": "dev"}))),
            ),
        ]);
        let prod_name = EnvironmentName::new("prod").unwrap();
        prod.environment.metadata.name = prod_name.to_string();
        let mut sources = BTreeMap::from([(EnvironmentName::new("dev").unwrap(), dev), (prod_name.clone(), prod)]);
        let findings = check_ownership(&sources, &drivers());
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].code, Code::UnitOwnershipOverlap);
        assert!(findings[0].message.contains("dev/publish") && findings[0].message.contains("prod/publish"));
        sources
            .get_mut(&prod_name)
            .unwrap()
            .units
            .get_mut(&name("publish"))
            .unwrap()
            .spec = Some(json!({"prefix": "prod"}));
        assert!(check_ownership(&sources, &drivers()).is_empty());
    }

    #[test]
    fn test_unavailable_source_retains_stored_ownership_claims() {
        let dev = source(vec![(
            "publish",
            rendered("Publication", true, Some(json!({"prefix": "apps"}))),
        )]);
        let sources = BTreeMap::from([(EnvironmentName::new("dev").unwrap(), dev)]);
        let prod = EnvironmentName::new("prod").unwrap();
        let mut snapshots = BTreeMap::from([(
            prod.clone(),
            snapshot_of(vec![stored("old", "Publication", 1, json!({"prefix": "apps/prod"}))]),
        )]);
        let findings = check_ownership_with_snapshots(&sources, &snapshots, &drivers());
        assert_eq!(findings.len(), 1);
        assert!(findings[0].message.contains("dev/publish") && findings[0].message.contains("prod/old"));
        snapshots
            .get_mut(&prod)
            .unwrap()
            .desired
            .get_mut(&name("old"))
            .unwrap()
            .spec = json!({"prefix": "separate"});
        assert!(check_ownership_with_snapshots(&sources, &snapshots, &drivers()).is_empty());
        assert_eq!(
            scope_owners(&sources, &drivers()),
            BTreeSet::from([EnvironmentName::new("dev").unwrap()])
        );
        let job = source(vec![("job", rendered(KIND_COMMAND, true, Some(command(json!({})))))]);
        assert!(scope_owners(
            &BTreeMap::from([(EnvironmentName::new("dev").unwrap(), job)]),
            &drivers()
        )
        .is_empty());
    }

    /// Catches leaving units being dropped silently or reported with the
    /// wrong reason or deletion consequence.
    #[test]
    fn test_leaving_units_carry_their_reason_and_deletion_policy() {
        let mut deselected = rendered(KIND_COMMAND, false, Some(command(json!({}))));
        deselected
            .envelope
            .metadata
            .labels
            .insert("dev".to_owned(), "true".to_owned());
        let mut source = source(vec![
            ("deselected", deselected),
            (
                "disabled",
                rendered("Publication", true, Some(json!({"enabled": false, "prefix": "dev"}))),
            ),
        ]);
        source
            .environment
            .spec
            .unit_selector
            .match_labels
            .insert("dve".to_owned(), "true".to_owned());
        let mut held = stored("held", KIND_COMMAND, 4, command(json!({})));
        held.lifecycle = Lifecycle::Retained {
            tombstone: serde_json::from_value(json!({"since": "2026-01-01T00:00:00Z"})).unwrap(),
        };
        let snapshot = EnvironmentSnapshot {
            desired: BTreeMap::from([
                (
                    name("deselected"),
                    stored("deselected", KIND_COMMAND, 1, command(json!({}))),
                ),
                (
                    name("disabled"),
                    stored("disabled", "Publication", 2, json!({"prefix": "dev"})),
                ),
                (
                    name("removed"),
                    stored("removed", KIND_COMMAND, 3, command(json!({"deletionPolicy": "Retain"}))),
                ),
                (name("held"), held),
            ]),
            ..EnvironmentSnapshot::default()
        };
        let desired = run_desire(&source, &snapshot);
        let leaving: Vec<(&str, &str, LeavingConsequence)> = desired
            .leaving
            .iter()
            .map(|unit| (unit.unit.as_str(), unit.reason.as_str(), unit.consequence))
            .collect();
        assert_eq!(
            leaving,
            [
                ("deselected", "deselected", LeavingConsequence::Dropped),
                ("disabled", "disabled", LeavingConsequence::Teardown),
                ("removed", "removed", LeavingConsequence::Retained),
            ]
        );
        // The unit's labels are {dev: "true"}; the selector asks for a typo.
        assert_eq!(
            desired.leaving[0].labels,
            BTreeMap::from([("dve".to_owned(), "true".to_owned())])
        );
        assert!(desired.leaving[1].labels.is_empty());
        assert!(desired.units.is_empty());
        assert_eq!(
            serde_json::to_value(&desired.leaving[0]).unwrap(),
            json!({"unit": "deselected", "kind": "Command", "reason": "deselected", "consequence": "dropped", "labels": {"dve": "true"}})
        );
    }

    fn stored_with(unit: &str, kind: &str, uid_number: u64, spec: Value, lifecycle: Lifecycle) -> DesiredUnit {
        DesiredUnit {
            lifecycle,
            ..stored(unit, kind, uid_number, spec)
        }
    }

    fn hold() -> nyl_core::orchestration::state::Hold {
        serde_json::from_value(json!({"since": "2026-09-24T12:00:00Z", "by": "alice"})).unwrap()
    }

    fn snapshot_of(units: Vec<DesiredUnit>) -> EnvironmentSnapshot {
        EnvironmentSnapshot {
            desired: units.into_iter().map(|unit| (unit.unit.name.clone(), unit)).collect(),
            ..EnvironmentSnapshot::default()
        }
    }

    fn deletion_of(unit: &DesiredUnit) -> &nyl_core::orchestration::state::Deletion {
        match &unit.lifecycle {
            Lifecycle::Deleting { deletion } => deletion,
            other => panic!("not deleting: {other:?}"),
        }
    }

    /// Omission: catches a Teardown unit leaving the set losing its uid
    /// (its teardown would not find its resources) or tearing down without
    /// a pending state that `--allow-teardown` gates.
    #[test]
    fn test_a_teardown_unit_leaving_becomes_deleting_with_its_uid() {
        let source = source(vec![]);
        let snapshot = snapshot_of(vec![stored("api", "Publication", 1, json!({"prefix": "dev"}))]);
        let desired = run_desire(&source, &snapshot);
        let write = &desired.lifecycle[&name("api")];
        assert_eq!(write.outcome, LifecycleOutcome::PendingTeardown);
        assert!(!write.remove_observed);
        let document = write.desired.as_ref().unwrap();
        assert_eq!(document.unit.uid, Some(uid(1)));
        let deletion = deletion_of(document);
        assert_eq!(
            (deletion.reason, deletion.intent, deletion.teardown),
            (
                DeletionReason::Omission,
                DeletionIntent::Teardown,
                Some(nyl_core::orchestration::state::TeardownProgress::Pending)
            )
        );
    }

    /// "Retain, declared" and "Retain, by kind": catches a declared Retain
    /// unit losing its receipt instead of becoming a tombstone that keeps
    /// it (and its artifacts), or a kind without teardown leaving a
    /// tombstone it never adopts.
    #[test]
    fn test_retain_leaves_a_tombstone_and_a_kind_without_teardown_is_released() {
        let source = source(vec![]);
        let declared = stored("worker", KIND_COMMAND, 1, command(json!({"deletionPolicy": "Retain"})));
        let by_kind = stored("image", KIND_COMMAND, 2, command(json!({})));
        let snapshot = snapshot_of(vec![declared.clone(), by_kind.clone()]);
        let desired = run_desire(&source, &snapshot);
        for unit in ["worker", "image"] {
            let deleting = desired.lifecycle[&name(unit)].desired.as_ref().unwrap();
            assert_eq!(deletion_of(deleting).intent, DeletionIntent::Retain, "{unit}");
        }
        let worker = desired.lifecycle[&name("worker")].desired.clone().unwrap();
        assert!(declares_retain(&worker));
        let receipt = crate::rules::fixtures::receipt("worker", json!({}), Vec::new());
        let mut observed = crate::rules::fixtures::observed("worker", KIND_COMMAND, Some(receipt.clone()));
        observed.unit.uid = Some(uid(1));
        let now: Timestamp = "2026-09-25T10:00:00Z".parse().unwrap();
        let event = Event::Retain {
            tombstone: tombstone(&worker, Some(&observed), now),
        };
        let retained = write(
            &worker,
            transition(&worker.lifecycle, event).unwrap(),
            LifecycleOutcome::Retained,
        )
        .unwrap();
        assert!(retained.remove_observed, "the observed file goes");
        let Lifecycle::Retained { tombstone } = &retained.desired.as_ref().unwrap().lifecycle else {
            panic!("a tombstone: {retained:?}");
        };
        assert_eq!(tombstone.receipt, Some(receipt));
        assert_eq!(retained.desired.as_ref().unwrap().unit.uid, Some(uid(1)));

        let image = desired.lifecycle[&name("image")].desired.clone().unwrap();
        assert!(!declares_retain(&image));
        let released = write(
            &image,
            transition(&image.lifecycle, Event::Released { hold: None }).unwrap(),
            LifecycleOutcome::Released { left: None },
        )
        .unwrap();
        assert!(released.desired.is_none() && released.remove_observed);
    }

    /// Omission: catches restoring a unit before its teardown runs giving
    /// it a new incarnation, which would execute it again.
    #[test]
    fn test_a_returning_pending_unit_keeps_its_uid() {
        let source = source(vec![(
            "api",
            rendered("Publication", true, Some(json!({"prefix": "dev"}))),
        )]);
        let mut pending = stored("api", "Publication", 1, json!({"prefix": "dev"}));
        pending.lifecycle = transition(
            &Lifecycle::Active,
            Event::Left {
                reason: DeletionReason::Omission,
                intent: DeletionIntent::Teardown,
            },
        )
        .map(|next| match next {
            Next::Lifecycle(lifecycle) => lifecycle,
            other => panic!("{other:?}"),
        })
        .unwrap();
        let desired = run_desire(&source, &snapshot_of(vec![pending]));
        assert_eq!(desired.units[&name("api")].unit.uid, Some(uid(1)));
        assert_eq!(desired.units[&name("api")].lifecycle, Lifecycle::Active);
        assert_eq!(desired.lifecycle[&name("api")].outcome, LifecycleOutcome::Returned);
        assert!(desired.fenced.is_empty());
    }

    /// Q3: catches a returning tombstone re-running although its execution
    /// key is unchanged (a changed producer uid alone never makes it stale),
    /// or adopting a receipt for changed inputs.
    #[test]
    fn test_tombstone_adoption_is_decided_by_the_execution_key_alone() {
        let source = source(vec![("worker", rendered(KIND_COMMAND, true, Some(command(json!({})))))]);
        let mut receipt = crate::rules::fixtures::receipt("worker", json!({}), Vec::new());
        receipt.provenance.insert(
            nyl_core::orchestration::ids::JsonPointer::new("/values/cache").unwrap(),
            serde_json::from_value(
                json!({"unit": "cache", "uid": uid(7), "executionKey": crate::rules::fixtures::key("cache")}),
            )
            .unwrap(),
        );
        let tombstone = Tombstone {
            since: "2026-09-24T10:00:00Z".parse().unwrap(),
            receipt: Some(receipt.clone()),
        };
        let retained = stored_with(
            "worker",
            KIND_COMMAND,
            1,
            command(json!({})),
            Lifecycle::Retained {
                tombstone: tombstone.clone(),
            },
        );
        let desired = run_desire(&source, &snapshot_of(vec![retained]));
        let mut resolved = desired.units[&name("worker")].clone();
        assert_eq!(resolved.unit.uid, Some(uid(1)), "the tombstone's incarnation returns");
        assert_eq!(desired.tombstones[&name("worker")], tombstone);
        // The producer was recreated with another uid, but the key is unchanged.
        resolved.provenance.insert(
            nyl_core::orchestration::ids::JsonPointer::new("/values/cache").unwrap(),
            serde_json::from_value(
                json!({"unit": "cache", "uid": uid(8), "executionKey": crate::rules::fixtures::key("cache")}),
            )
            .unwrap(),
        );
        resolved.execution_key = Some(receipt.execution_key.clone());
        assert_eq!(adopt_tombstone(&resolved, &tombstone), Some(receipt));
        resolved.execution_key = Some(crate::rules::fixtures::key("changed"));
        assert_eq!(adopt_tombstone(&resolved, &tombstone), None);
    }

    /// Fencing: catches a kind change reusing the old incarnation or
    /// starting the new kind while the old one's resources still exist.
    #[test]
    fn test_a_kind_change_ends_the_old_incarnation_and_fences_the_new_kind() {
        let source = source(vec![(
            "api",
            rendered("Publication", true, Some(json!({"prefix": "dev"}))),
        )]);
        let old = stored("api", KIND_COMMAND, 1, command(json!({"teardown": ["./down.sh"]})));
        let desired = run_desire(&source, &snapshot_of(vec![old]));
        let write = &desired.lifecycle[&name("api")];
        assert_eq!(write.outcome, LifecycleOutcome::KindChange);
        let deleting = write.desired.as_ref().unwrap();
        assert_eq!(deleting.unit.kind, KIND_COMMAND);
        assert_eq!(deleting.unit.uid, Some(uid(1)));
        assert_eq!(deletion_of(deleting).reason, DeletionReason::KindChange);
        assert!(desired.fenced.contains(&name("api")));
        let fenced = &desired.units[&name("api")];
        assert_eq!(fenced.unit.uid, None);
        assert!(matches!(fenced.readiness, Readiness::Blocked { .. }));
        assert_eq!(desired.environment.ownership[&name("api")], None);
    }

    /// Holds: catches a held unit (or a held marker) being rendered into
    /// the run's units, which would execute it, or dropped while it is
    /// outside the set.
    #[test]
    fn test_held_units_and_markers_are_kept_out_of_the_run() {
        let source = source(vec![
            ("api", rendered(KIND_COMMAND, true, Some(command(json!({}))))),
            ("new", rendered(KIND_COMMAND, true, Some(command(json!({}))))),
        ]);
        let held = stored_with(
            "api",
            KIND_COMMAND,
            1,
            command(json!({})),
            Lifecycle::Held {
                hold: hold(),
                deletion: None,
            },
        );
        let marker = DesiredUnit::held_marker(EnvironmentName::new("dev").unwrap(), name("new"), hold());
        let gone = stored_with(
            "gone",
            KIND_COMMAND,
            3,
            command(json!({})),
            Lifecycle::Held {
                hold: hold(),
                deletion: None,
            },
        );
        let desired = run_desire(&source, &snapshot_of(vec![held.clone(), marker.clone(), gone]));
        assert!(desired.units.is_empty(), "{:?}", desired.units.keys());
        assert_eq!(desired.held[&name("api")].stored, held);
        assert!(desired.held[&name("api")].rendered.is_some());
        assert_eq!(desired.held[&name("new")].stored, marker);
        assert!(desired.held[&name("gone")].rendered.is_none());
        assert!(desired.lifecycle.is_empty());
        assert_eq!(
            hold_pending(&desired.held[&name("gone")], &commit('a'), None).map(|pending| pending.deletion),
            Some(true)
        );
    }

    /// Q26: catches a uid-less file left outside the set after `resume`
    /// staying in state forever.
    #[test]
    fn test_a_uid_less_file_outside_the_set_is_removed() {
        let mut resumed = stored("old", KIND_COMMAND, 1, command(json!({})));
        resumed.unit.uid = None;
        let desired = run_desire(&source(vec![]), &snapshot_of(vec![resumed]));
        let write = &desired.lifecycle[&name("old")];
        assert_eq!(write.outcome, LifecycleOutcome::Removed);
        assert!(write.desired.is_none() && write.remove_observed);
        assert!(desired.leaving.is_empty());
    }

    /// Holds: catches a run recording a pending change for a held unit
    /// whose source resolves to what it already runs.
    #[test]
    fn test_hold_pending_names_only_what_source_would_change() {
        let mut stored_unit = stored("api", KIND_COMMAND, 1, command(json!({})));
        stored_unit.execution_key = Some(crate::rules::fixtures::key("a"));
        let held = HeldUnit {
            rendered: Some(stored_unit.clone()),
            stored: stored_unit.clone(),
        };
        assert_eq!(hold_pending(&held, &commit('c'), Some(&stored_unit)), None);
        let mut changed = stored_unit.clone();
        changed.execution_key = Some(crate::rules::fixtures::key("b"));
        let pending = hold_pending(&held, &commit('c'), Some(&changed)).unwrap();
        assert_eq!(pending.execution_key, changed.execution_key);
        assert!(!pending.deletion);
    }

    /// Catches unknown kinds or invalid specs reaching execution.
    #[test]
    fn test_invalid_units_stop_the_run_with_their_codes() {
        let source = source(vec![
            ("unknown", rendered("Mystery", true, Some(json!({})))),
            ("broken", rendered("Publication", true, Some(json!({"broken": 1})))),
            ("argv", rendered(KIND_COMMAND, true, Some(json!({"command": []})))),
        ]);
        let desired = run_desire(&source, &EnvironmentSnapshot::default());
        let codes: BTreeMap<Code, &Finding> = desired.errors.iter().map(|error| (error.code, error)).collect();
        assert!(codes.contains_key(&Code::UnitKindUnknown));
        assert_eq!(
            desired
                .errors
                .iter()
                .filter(|error| error.code == Code::UnitInvalid)
                .count(),
            2
        );
        assert!(desired
            .errors
            .iter()
            .any(|error| error.message.contains("/nyl/units.yaml")));
        assert!(desired.units.is_empty());
    }
}
