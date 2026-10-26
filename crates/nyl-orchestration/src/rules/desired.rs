//! Desired documents: unit selection, the ownership set, incarnations, and
//! the environment record.
//!
//! Contract: [Units](../../../../design/orchestration-core.md#units),
//! [Identity](../../../../design/orchestration-core.md#identity), and
//! [Desired unit](../../../../design/orchestration-core.md#desired-unit).
//!
//! - The renderer selects units on their literal envelope and renders each
//!   selected spec once; a document that renders to nothing, or whose
//!   `enabled` is false after rendering, is not in the ownership set.
//! - Every unit's kind must have a driver, and its spec must pass the
//!   driver's `validate_spec` and the common fields' validation.
//! - A unit keeps the uid of its desired document while its `apiVersion` and
//!   `kind` stay; a new unit, or one whose kind changed, gets a new uid from
//!   the caller. (Ending the old incarnation is part 5.)
//! - Ownership scopes of the ownership set must not overlap
//!   ([`Code::UnitOwnershipOverlap`](nyl_core::orchestration::codes::Code::UnitOwnershipOverlap)):
//!   equal scopes overlap, and publication scopes overlap when their
//!   repository and revision are equal and one path prefix contains the other.
//! - Units in the snapshot's desired state that leave the ownership set are
//!   reported as leaving (`removed`, `deselected`, or `disabled`) with the
//!   consequence of their effective deletion policy (`teardown`, `dropped`,
//!   or `retained`), and a deselected unit with the selector labels it no
//!   longer matches; their deletion is part 5.
//! - M3 restriction: a unit's `env.secrets` is rejected with
//!   [`Code::UnitSecretsUnsupported`](nyl_core::orchestration::codes::Code::UnitSecretsUnsupported);
//!   secrets providers for units arrive in a later milestone.

use std::collections::{BTreeMap, BTreeSet};

use nyl_core::orchestration::codes::{Code, Finding};
use nyl_core::orchestration::driver::{Driver, DriverSet, OwnershipScope};
use nyl_core::orchestration::ids::{Uid, UnitName};
use nyl_core::orchestration::ports::{EnvironmentSnapshot, RenderedSource, RenderedUnit};
use nyl_core::orchestration::state::{
    DesiredUnit, DesiredUnitKind, EnvironmentRecord, EnvironmentRecordKind, HoldPending, Lifecycle, Readiness,
    SourceChoice, StateV1, Tombstone, UnitIdentity,
};
use nyl_core::resources::units::{
    is_builtin_unit_api_version, CommandSpec, CommonUnitFields, DeletionPolicy, KindSupport, KIND_COMMAND,
};
use serde_json::Value;

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
    /// Every unit of the ownership set: `spec` rendered with references kept
    /// as expressions, `resolved_spec`, `execution_key`, and `provenance`
    /// empty, `readiness` ready, lifecycle active, `source_commit` and
    /// `source_revision` set to S, and a uid.
    pub units: BTreeMap<UnitName, DesiredUnit>,
    /// Units of the snapshot's desired state outside the ownership set.
    pub leaving: Vec<LeavingUnit>,
    /// Errors that stop the run before anything executes (exit 1).
    pub errors: Vec<Finding>,
    /// Lifecycle changes this run writes, decided through
    /// [`super::lifecycle::transition`]: units leaving the ownership set
    /// (deleting, retained, released), units returning to it, and kind
    /// changes. Contract: [Deletion](../../../../design/orchestration-core.md#deletion).
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
    /// the same name (another kind) to be removed before they start.
    pub fenced: BTreeSet<UnitName>,
}

/// One lifecycle change of a run.
#[derive(Debug, Clone, PartialEq)]
pub struct LifecycleWrite {
    /// The desired document to write; `None` removes the desired file.
    pub desired: Option<DesiredUnit>,
    /// Whether the observed file goes: a retained tombstone embeds the
    /// receipt, and a released unit leaves no state.
    pub remove_observed: bool,
    /// What happened, for the report and the transition summary.
    pub outcome: LifecycleOutcome,
}

/// What a lifecycle change did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LifecycleOutcome {
    /// Deleting, its teardown pending.
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
/// would change nothing.
pub fn hold_pending(
    held: &HeldUnit,
    source_commit: &nyl_core::orchestration::ids::CommitId,
    resolved: Option<&DesiredUnit>,
) -> Option<HoldPending> {
    let _ = (held, source_commit, resolved);
    todo!("lifecycle")
}

/// The receipt a returning unit adopts from its tombstone: the tombstone's
/// receipt when `resolved` has the same execution key (the execution key
/// alone decides freshness; producer identity never does); `None`
/// otherwise, so the unit runs as an ordinary change of the same
/// incarnation. Contract: [Deletion](../../../../design/orchestration-core.md#deletion),
/// "Retain, declared", as amended by part 5 decision Q3.
pub fn adopt_tombstone(
    resolved: &DesiredUnit,
    tombstone: &Tombstone,
) -> Option<nyl_core::orchestration::state::Receipt> {
    let _ = (resolved, tombstone);
    todo!("lifecycle")
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

/// `enabled` as the rendered spec says it; absent means true.
fn is_enabled(spec: &Value) -> bool {
    spec.get("enabled").and_then(Value::as_bool) != Some(false)
}

/// Build the desired documents of one run. `new_uid` allocates a uid for a
/// new incarnation; it is the only identifier source, so the result is a
/// function of the inputs and the identifiers it hands out.
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
        let uid = inputs
            .snapshot
            .desired
            .get(name)
            .filter(|previous| previous.unit.api_version == *api_version && previous.unit.kind == *kind)
            .and_then(|previous| previous.unit.uid.clone())
            .unwrap_or_else(&mut *new_uid);
        units.insert(
            name.clone(),
            DesiredUnit {
                api_version: StateV1,
                kind: DesiredUnitKind,
                unit: UnitIdentity {
                    environment: environment.clone(),
                    name: name.clone(),
                    uid: Some(uid),
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
            },
        );
    }
    errors.extend(overlaps(&scopes));

    let leaving = leaving_units(inputs, &set);

    let ownership = units
        .iter()
        .map(|(name, unit)| (name.clone(), unit.unit.uid.clone()))
        .collect();
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
        lifecycle: BTreeMap::new(),
        held: BTreeMap::new(),
        tombstones: BTreeMap::new(),
        fenced: BTreeSet::new(),
    }
}

/// The state-free checks of one environment's rendered units, which
/// `plan`, `reconcile`, and `nyl validate` share: every unit's kind has a
/// driver and its spec passes the driver's and the common fields'
/// validation, `env.secrets` is rejected, ownership scopes do not overlap,
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

/// The units of the snapshot's desired state that leave the ownership set.
fn leaving_units(inputs: &DesireInputs<'_>, set: &BTreeMap<&UnitName, &RenderedUnit>) -> Vec<LeavingUnit> {
    let source = inputs.source;
    inputs
        .snapshot
        .desired
        .iter()
        .filter(|(name, desired)| desired.lifecycle == Lifecycle::Active && !set.contains_key(name))
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
    Secrets(Vec<String>),
}

impl UnitError {
    fn into_finding(self, name: &UnitName, source_path: &str) -> Finding {
        match self {
            Self::Invalid(message) => Finding::new(
                Code::UnitInvalid,
                format!("unit {name} ({source_path}): {message}"),
                "correct the unit's spec so that it matches its kind's schema",
            ),
            Self::Secrets(variables) => Finding::new(
                Code::UnitSecretsUnsupported,
                format!(
                    "unit {name} ({source_path}) reads env.secrets ({}), which units cannot use yet",
                    variables.join(", ")
                ),
                "remove env.secrets; secrets providers for units arrive in a later milestone",
            ),
        }
    }
}

/// The driver's validation, the common fields' validation, and the M3
/// rejection of `env.secrets`.
fn validate_unit(driver: &dyn Driver, api_version: &str, kind: &str, spec: &Value) -> Result<(), UnitError> {
    let invalid = |error: &dyn std::fmt::Display| UnitError::Invalid(error.to_string());
    driver.validate_spec(spec).map_err(UnitError::Invalid)?;
    let common = if is_builtin_unit_api_version(api_version) && kind == KIND_COMMAND {
        let command: CommandSpec = serde_json::from_value(spec.clone()).map_err(|error| invalid(&error))?;
        command.validate().map_err(|error| invalid(&error))?;
        command.common
    } else {
        let common: CommonUnitFields = serde_json::from_value(spec.clone()).map_err(|error| invalid(&error))?;
        let support = KindSupport {
            // Whether a kind lets units declare outputs is its driver's to check in `validate_spec`.
            declared_outputs: true,
            teardown: driver.capabilities(spec).teardown,
        };
        common.validate(support).map_err(|error| invalid(&error))?;
        common
    };
    if !common.env.secrets.is_empty() {
        return Err(UnitError::Secrets(common.env.secrets.into_keys().collect()));
    }
    Ok(())
}

/// One finding for every pair of units whose ownership scopes overlap.
fn overlaps(scopes: &[(OwnershipScope, UnitName)]) -> Vec<Finding> {
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
/// policy: `teardown`, `retained` for a declared `Retain`, which leaves a
/// tombstone, or `dropped` for a kind that cannot tear down and declares no
/// policy.
fn leaving_consequence(drivers: &DriverSet, desired: &DesiredUnit) -> LeavingConsequence {
    let common = serde_json::from_value::<CommonUnitFields>(desired.spec.clone()).unwrap_or_default();
    let teardown = drivers
        .get(&desired.unit.api_version, &desired.unit.kind)
        .is_some_and(|driver| driver.capabilities(&desired.spec).teardown);
    let policy = common.effective_deletion_policy(KindSupport {
        declared_outputs: true,
        teardown,
    });
    match (policy, common.deletion_policy) {
        (DeletionPolicy::Teardown, _) => LeavingConsequence::Teardown,
        (DeletionPolicy::Retain, Some(DeletionPolicy::Retain)) => LeavingConsequence::Retained,
        (DeletionPolicy::Retain, _) => LeavingConsequence::Dropped,
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
    /// invalidate its receipt) or keeping it across a kind change.
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
        let uid_of = |unit: &str| desired.units[&name(unit)].unit.uid.clone().unwrap();
        assert_eq!(uid_of("same"), uid(1));
        assert_eq!(uid_of("changed"), uid(101));
        assert_eq!(uid_of("new"), uid(102));
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

    /// Catches a unit reading secrets the shell cannot provide yet, and
    /// unknown kinds or invalid specs reaching execution.
    #[test]
    fn test_invalid_units_stop_the_run_with_their_codes() {
        let source = source(vec![
            (
                "secret",
                rendered(
                    KIND_COMMAND,
                    true,
                    Some(command(json!({"env": {"secrets": {"TOKEN": "api-token"}}}))),
                ),
            ),
            ("unknown", rendered("Mystery", true, Some(json!({})))),
            ("broken", rendered("Publication", true, Some(json!({"broken": 1})))),
            ("argv", rendered(KIND_COMMAND, true, Some(json!({"command": []})))),
        ]);
        let desired = run_desire(&source, &EnvironmentSnapshot::default());
        let codes: BTreeMap<Code, &Finding> = desired.errors.iter().map(|error| (error.code, error)).collect();
        let secrets = codes[&Code::UnitSecretsUnsupported];
        assert!(secrets.message.contains("TOKEN"), "{}", secrets.message);
        assert!(secrets.fix.contains("remove env.secrets"));
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
