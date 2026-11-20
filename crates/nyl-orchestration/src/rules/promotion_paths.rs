//! The validation of PromotionPaths against the project.
//!
//! Contract: [PromotionPath](../../../../design/promotion.md#promotionpath),
//! "Validation checks every path", and [Promotion paths](../../../../design/promotion.md#promotion-paths),
//! "`plan` validates every path". `nyl validate` and `plan` call [`check`], so
//! they cannot disagree.
//!
//! Each failure has a stable code and a fix:
//!
//! - [`Code::PromoteUnsupported`]: `changeGate: pullRequest`, or a
//!   DeploymentTarget in `from` or `to`, which M11 delivers
//!   ([`PromotionPath::unsupported`]).
//! - [`Code::PromoteEnvironmentUnknown`]: the source or target is not a
//!   declared Environment; a template instance is never one.
//! - [`Code::PromoteSelectorUnit`]: a selector, or an `attestations` scope,
//!   names a unit the source environment does not select (or, for a scope,
//!   a unit the path does not cover).
//! - [`Code::PromoteSelectorInvalid`]: a pointer that resolves neither against
//!   the source unit's rendered spec nor against its kind's schema, or an
//!   output the unit does not declare or marks `sensitive`.
//! - [`Code::PromoteValueType`]: the type a selector reads does not fit a
//!   binding that reads the value.
//! - [`Code::PromoteBindingUnknown`]: a `$promotion` binding names a path or
//!   a value no PromotionPath declares.
//! - [`Code::AttestationUndeclared`]: the path requires an attestation its scope
//!   does not declare.
//! - [`Code::PromoteCoverageUnknown`]: `coverage` names a unit the source
//!   environment does not declare, select, or enable, a unit whose kind runs
//!   no deployments, or a group or deployment the unit's driver does not list
//!   ([`Driver::deployments`](nyl_core::orchestration::driver::Driver::deployments),
//!   gathered by the shell into [`DeploymentListings`]).
//! - [`Code::PromoteCoverageIneffective`]: a path that promotes only values
//!   sets a unit's `groups`, which only a source path uses; or the unit's
//!   kind has no running-scoped attestations for its spec, so its entry
//!   selects nothing (and a `require` would refuse every promotion).
//! - [`Code::PromoteCoverageUnchecked`]: the driver could not list a unit's
//!   deployments, such as when what its declaration renders from cannot be
//!   read. `nyl validate`
//!   refuses it; commands that work on one environment report it as a
//!   warning ([`split_unchecked`]).
//! - [`Code::UnitKindUnknown`]: `coverage` names a unit whose kind no
//!   registered driver serves.
//! - [`Code::PromoteValueTypeUnchecked`]: the driver could not read the
//!   declared type of a field a binding fills (gathered by the shell into
//!   [`BoundFieldTypes`]), such as a Release input declared in a remote
//!   source that is neither fetched nor cached, so a value of known type is
//!   not compared with it. `nyl validate` refuses it; commands that work on
//!   one environment report it as a warning ([`split_unchecked`]).
//!
//! Types are checked when both sides are known: what a selector reads (a
//! rendered literal, a declared output's type, the kind's schema at the
//! pointer, or the schema the producing kind declares for an artifact,
//! [`Driver::artifact_schema`]) against what the reading place accepts (the
//! kind's schema at the field a unit's `$promotion` binding replaces, or,
//! where the schema cannot type it, the type the unit's driver reads from
//! what declares the field, [`Driver::bound_field_types`], such as a Release
//! input's declaration). An unknown side fits anything.

use std::collections::{BTreeMap, BTreeSet};

use nyl_core::bindings::PromotionInputSource;
use nyl_core::orchestration::codes::{Code, Finding};
use nyl_core::orchestration::driver::{ArtifactSchema, BoundFieldType, Deployment, Driver, DriverSet};
use nyl_core::orchestration::ids::{EnvironmentName, UnitName};
use nyl_core::orchestration::ports::{RenderedSource, RenderedUnit};
use nyl_core::resources::promotion::{PromotionPath, SelectForm};
use nyl_core::resources::units::{OutputDeclaration, COMMON_UNIT_FIELDS};
use serde_json::Value;

use super::desired::ownership_set;

/// The JSON type of a promoted value or of the place that reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ValueType {
    String,
    Integer,
    Number,
    Boolean,
    Object,
    Array,
}

impl ValueType {
    fn name(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Integer => "integer",
            Self::Number => "number",
            Self::Boolean => "boolean",
            Self::Object => "object",
            Self::Array => "array",
        }
    }

    fn of_json(value: &Value) -> Option<Self> {
        match value {
            Value::String(_) => Some(Self::String),
            Value::Bool(_) => Some(Self::Boolean),
            Value::Number(number) if number.is_i64() || number.is_u64() => Some(Self::Integer),
            Value::Number(_) => Some(Self::Number),
            Value::Array(_) => Some(Self::Array),
            Value::Object(fields) if is_reference(fields) => None,
            Value::Object(_) => Some(Self::Object),
            Value::Null => None,
        }
    }

    /// Whether a value of type `self` fits a place that accepts `place`.
    fn fits(self, place: Self) -> bool {
        self == place || (self == Self::Integer && place == Self::Number)
    }
}

impl From<nyl_core::bindings::InputType> for ValueType {
    fn from(declared: nyl_core::bindings::InputType) -> Self {
        use nyl_core::bindings::InputType;
        match declared {
            InputType::String => Self::String,
            InputType::Integer => Self::Integer,
            InputType::Number => Self::Number,
            InputType::Boolean => Self::Boolean,
            InputType::Object => Self::Object,
            InputType::Array => Self::Array,
        }
    }
}

fn is_reference(fields: &serde_json::Map<String, Value>) -> bool {
    matches!(
        nyl_core::bindings::clause_key(fields),
        Ok(Some(
            nyl_core::bindings::Source::Unit | nyl_core::bindings::Source::Promotion
        ))
    )
}

/// A place that reads a promoted value with `$promotion`.
#[derive(Debug, Clone, PartialEq)]
pub struct ValueReader {
    /// Where the binding is, for messages.
    pub location: String,
    /// The environment that reads the value: a binding can read only a path
    /// into it.
    pub environment: EnvironmentName,
    pub binding: PromotionInputSource,
    /// The type the place accepts, when known.
    pub expected: Option<ValueType>,
    /// The unit whose spec holds the binding and its JSON Pointer there,
    /// when a unit's spec holds it: where [`BoundFieldTypes`] types a place
    /// the kind's schema cannot.
    pub field: Option<(UnitName, String)>,
    /// Why the type the place accepts could not be read, when its driver
    /// says it has one ([`BoundFieldType::Unchecked`]).
    pub unchecked: Option<String>,
}

/// The types the drivers of units read for the fields their specs' bindings
/// fill and their kinds' schemas cannot type ([`Driver::bound_field_types`]),
/// by environment and unit, as the shell gathered them for
/// [`untyped_fields`]: `Err` with why the driver could read none of them. A
/// unit or pointer missing here is not typed, which fits any value.
pub type BoundFieldTypes = BTreeMap<(EnvironmentName, UnitName), Result<BTreeMap<String, BoundFieldType>, String>>;

/// The values of `paths` whose type is known, by the path's target
/// environment, the path's name, and the value's name: the only values a
/// place's type is compared with, so the only ones worth asking a driver
/// about ([`untyped_fields`]).
pub fn typed_values(
    paths: &[PromotionPath],
    sources: &BTreeMap<EnvironmentName, RenderedSource>,
    drivers: &DriverSet,
) -> BTreeSet<(EnvironmentName, String, String)> {
    let mut typed = BTreeSet::new();
    for path in paths {
        let environment = |end: &Option<String>| end.as_deref().and_then(|name| EnvironmentName::new(name).ok());
        let (Some(from), Some(to)) = (
            environment(&path.spec.from.environment),
            environment(&path.spec.to.environment),
        ) else {
            continue;
        };
        let Some(source) = sources.get(&from) else { continue };
        for (name, value) in &path.spec.values {
            if value.select.shape_problem().is_none()
                && matches!(
                    select_type(drivers, source, from.as_str(), &value.select, ""),
                    Ok(Some(_))
                )
            {
                typed.insert((to.clone(), path.metadata.name.clone(), name.clone()));
            }
        }
    }
    typed
}

/// The fields whose types [`check`] asks the units' drivers for: each
/// binding in a unit's spec whose kind's schema cannot type it and that
/// reads a value of known type ([`typed_values`]) from a path into its
/// environment, by environment and unit, limited to `only`'s bindings when
/// set, as [`check`] limits them.
pub fn untyped_fields(
    readers: &[ValueReader],
    only: Option<&EnvironmentName>,
    typed: &BTreeSet<(EnvironmentName, String, String)>,
) -> BTreeMap<(EnvironmentName, UnitName), Vec<String>> {
    let reads_typed = |reader: &ValueReader| {
        typed.iter().any(|(to, path, value)| {
            *to == reader.environment
                && *value == reader.binding.value
                && reader.binding.path.as_ref().is_none_or(|named| named == path)
        })
    };
    let mut wanted: BTreeMap<(EnvironmentName, UnitName), Vec<String>> = BTreeMap::new();
    for reader in readers.iter().filter(|reader| {
        reader.expected.is_none() && only.is_none_or(|only| *only == reader.environment) && reads_typed(reader)
    }) {
        if let Some((unit, pointer)) = &reader.field {
            let pointers = wanted.entry((reader.environment.clone(), unit.clone())).or_default();
            if !pointers.contains(pointer) {
                pointers.push(pointer.clone());
            }
        }
    }
    wanted
}

/// `readers` with the types `bound` holds for the places their kinds'
/// schemas cannot type, and why a type could not be read.
fn typed_readers(readers: &[ValueReader], bound: &BoundFieldTypes) -> Vec<ValueReader> {
    readers
        .iter()
        .cloned()
        .map(|mut reader| {
            if reader.expected.is_some() {
                return reader;
            }
            let Some((unit, pointer)) = &reader.field else {
                return reader;
            };
            match bound.get(&(reader.environment.clone(), unit.clone())) {
                Some(Ok(types)) => match types.get(pointer) {
                    Some(BoundFieldType::Typed(declared)) => reader.expected = Some((*declared).into()),
                    Some(BoundFieldType::Unchecked(reason)) => reader.unchecked = Some(reason.clone()),
                    None => {}
                },
                // The driver could answer for none of the unit's pointers, so the
                // engine cannot tell which of them it would have typed: every
                // pointer it was asked about (only ones the kind's schema cannot
                // type, reading a typed value) is unchecked rather than guessed.
                Some(Err(reason)) => reader.unchecked = Some(reason.clone()),
                None => {}
            }
            reader
        })
        .collect()
}

/// What each unit a path's `coverage` names runs as, by the path's source
/// environment and the unit, as the unit's driver listed it
/// ([`Driver::deployments`]): `Ok(None)` for a kind that runs no
/// deployments, `Err` with why the driver could not list them.
pub type DeploymentListings = BTreeMap<(EnvironmentName, UnitName), Result<Option<Vec<Deployment>>, String>>;

/// What a unit a path's `coverage` names is in the source environment, as
/// far as it can be told without listing its deployments.
enum CoverageUnit<'a> {
    /// No unit of that name is declared.
    Missing,
    /// The environment's `unitSelector` does not select it.
    Unselected,
    /// It is selected but disabled, so it renders no spec.
    Disabled,
    /// No registered driver serves its kind.
    UnknownKind(&'a RenderedUnit),
    /// Its kind has no running-scoped attestations for its spec, so no
    /// promotion judges its deployments one by one.
    NotJudged(&'a RenderedUnit),
    /// Its driver can list its deployments.
    Listable(&'a UnitName),
}

fn coverage_unit<'a>(source: &'a RenderedSource, drivers: &DriverSet, unit: &str) -> CoverageUnit<'a> {
    let Some((name, rendered)) = source.units.iter().find(|(name, _)| name.as_str() == unit) else {
        return CoverageUnit::Missing;
    };
    if !rendered.selected {
        return CoverageUnit::Unselected;
    }
    if !ownership_set(source).contains_key(name) {
        return CoverageUnit::Disabled;
    }
    let spec = rendered.spec.as_ref().expect("the ownership set holds rendered specs");
    match drivers.get(&rendered.envelope.api_version, &rendered.envelope.kind) {
        None => CoverageUnit::UnknownKind(rendered),
        Some(driver) if driver.running_attestations(spec).is_empty() => CoverageUnit::NotJudged(rendered),
        Some(_) => CoverageUnit::Listable(name),
    }
}

/// The units whose deployments [`check`] needs listed for `paths`: each unit
/// a path's `coverage` names that its source environment selects and whose
/// kind judges its deployments (it has running-scoped attestations), so
/// nothing else is rendered.
pub fn coverage_units(
    paths: &[PromotionPath],
    sources: &BTreeMap<EnvironmentName, RenderedSource>,
    drivers: &DriverSet,
) -> BTreeSet<(EnvironmentName, UnitName)> {
    let mut wanted = BTreeSet::new();
    for path in paths {
        let Some(environment) = path
            .spec
            .from
            .environment
            .as_deref()
            .and_then(|name| EnvironmentName::new(name).ok())
        else {
            continue;
        };
        let Some(source) = sources.get(&environment) else {
            continue;
        };
        for unit in path.spec.coverage.keys() {
            if let CoverageUnit::Listable(name) = coverage_unit(source, drivers, unit) {
                wanted.insert((environment.clone(), name.clone()));
            }
        }
    }
    wanted
}

/// What [`check`] reads.
pub struct PathChecks<'a> {
    /// The paths to check.
    pub paths: &'a [PromotionPath],
    /// Every path the project declares, which bindings and sources resolve
    /// against: a command that checks the paths touching one environment
    /// still resolves a binding against all of them.
    pub known: &'a [PromotionPath],
    /// Check only the bindings and the promoted source of this environment,
    /// as a command working on one environment does; `None` checks them all.
    pub only: Option<&'a EnvironmentName>,
    /// The declared Environments: instances of an EnvironmentTemplate are not among them.
    pub declared: &'a BTreeSet<EnvironmentName>,
    /// Each environment's units rendered from the worktree.
    pub sources: &'a BTreeMap<EnvironmentName, RenderedSource>,
    pub drivers: &'a DriverSet,
    pub readers: &'a [ValueReader],
    /// The deployments of the units the paths' `coverage` names
    /// ([`coverage_units`]); a unit missing here is not checked.
    pub deployments: &'a DeploymentListings,
    /// The types the drivers read for the places in `readers` their kinds'
    /// schemas cannot type ([`untyped_fields`]).
    pub bound_types: &'a BoundFieldTypes,
}

/// Every finding for every path: all are listed, never the first only.
pub fn check(inputs: &PathChecks<'_>) -> Vec<Finding> {
    let mut findings = Vec::new();
    let typed = typed_readers(inputs.readers, inputs.bound_types);
    for path in inputs.paths {
        findings.extend(path.unsupported());
        findings.extend(check_path(inputs, &typed, path));
    }
    let readers = typed
        .iter()
        .filter(|reader| inputs.only.is_none_or(|only| *only == reader.environment))
        .cloned()
        .collect::<Vec<_>>();
    findings.extend(check_bindings(inputs.known, inputs.sources, &readers));
    let declared = inputs
        .declared
        .iter()
        .filter(|environment| inputs.only.is_none_or(|only| only == *environment))
        .cloned()
        .collect();
    findings.extend(check_sources(inputs.known, &declared, inputs.sources));
    findings
}

#[allow(clippy::too_many_lines)] // One path: endpoints, selectors, scopes, and readers, in the order the contract lists them.
fn check_path(inputs: &PathChecks<'_>, readers: &[ValueReader], path: &PromotionPath) -> Vec<Finding> {
    let name = &path.metadata.name;
    let mut findings = Vec::new();
    let mut endpoints = BTreeMap::new();
    for (role, endpoint) in [("from", &path.spec.from), ("to", &path.spec.to)] {
        let Some(wanted) = &endpoint.environment else { continue };
        let known = EnvironmentName::new(wanted.as_str())
            .ok()
            .filter(|environment| inputs.declared.contains(environment));
        if let Some(environment) = known {
            endpoints.insert(role, environment);
            continue;
        }
        let instance =
            EnvironmentName::new(wanted.as_str()).is_ok_and(|environment| inputs.sources.contains_key(&environment));
        findings.push(Finding::new(
            Code::PromoteEnvironmentUnknown,
            if instance {
                format!("PromotionPath {name} names {wanted:?} in `{role}`, an instance of an EnvironmentTemplate; a path connects declared Environments")
            } else {
                format!("PromotionPath {name} names Environment {wanted:?} in `{role}`, which the project does not declare")
            },
            format!("declare an Environment named {wanted}, or point `spec.{role}.environment` at a declared one (`nyl get environments` lists them)"),
        ));
    }

    for (value_name, value) in &path.spec.values {
        if let Some(problem) = value.select.shape_problem() {
            findings.push(Finding::new(
                Code::PromoteSelectorInvalid,
                format!("PromotionPath {name} value {value_name:?}: {problem}"),
                "set exactly one selector form: `{unit, input}`, `{unit, output, pointer?}`, or `{unit, artifact, kind?, pointer}`, with RFC 6901 pointers",
            ));
        }
    }
    let mut value_types: BTreeMap<&str, Option<ValueType>> = BTreeMap::new();
    let mut covered: BTreeSet<String> = BTreeSet::new();
    if let Some(source) = endpoints.get("from").and_then(|from| inputs.sources.get(from)) {
        let from = &path.spec.from.environment.clone().unwrap_or_default();
        for (value_name, value) in &path.spec.values {
            let at = format!("PromotionPath {name} value {value_name:?}");
            if value.select.shape_problem().is_some() {
                continue;
            }
            match select_type(inputs.drivers, source, from, &value.select, &at) {
                Ok(found) => {
                    if let Some(unit) = &value.select.unit {
                        covered.insert(unit.to_string());
                    }
                    value_types.insert(value_name, found);
                }
                Err(finding) => findings.push(finding),
            }
        }
        if let Some(target) = endpoints.get("to").and_then(|to| inputs.sources.get(to)) {
            let selected_by_target = ownership_set(target);
            covered.extend(
                ownership_set(source)
                    .keys()
                    .filter(|unit| selected_by_target.contains_key(**unit))
                    .map(ToString::to_string),
            );
        }
        findings.extend(check_attestations(inputs, path, source, from, &covered));
        let target = endpoints.get("to").and_then(|to| {
            inputs.sources.get(to).map(|target| {
                (
                    to.as_str(),
                    source_paths(target).is_some_and(|paths| paths.contains(name)),
                )
            })
        });
        findings.extend(check_coverage(inputs, path, source, from, target));
    }

    for reader in readers {
        findings.extend(check_reader(path, &endpoints, &value_types, reader));
    }
    findings
}

/// The type a selector reads, or the finding for why it reads nothing.
#[allow(clippy::too_many_lines)] // One arm per selector form, in the order the contract lists them.
fn select_type(
    drivers: &DriverSet,
    source: &RenderedSource,
    environment: &str,
    select: &nyl_core::orchestration::promotion::PromotionSelect,
    at: &str,
) -> Result<Option<ValueType>, Finding> {
    let invalid =
        |message: String, fix: &str| Finding::new(Code::PromoteSelectorInvalid, format!("{at}: {message}"), fix);
    let form = select
        .form()
        .map_err(|reason| invalid(format!("the selector {reason}"), "set exactly one selector form: `{unit, input}`, `{unit, output, pointer?}`, or `{unit, artifact, kind?, pointer}`"))?;
    let Some(unit_name) = &select.unit else {
        return Err(invalid(
            "the selector names no unit, which only a target source's Release input does (M11)".to_owned(),
            "select a unit of the source environment",
        ));
    };
    let rendered: Option<&RenderedUnit> = source.units.get(unit_name);
    let selected = ownership_set(source).get(unit_name).copied();
    let Some(unit) = selected else {
        let message = match rendered {
            None => format!("no unit named {unit_name} exists"),
            Some(unit) if !unit.selected => {
                format!("unit {unit_name} is not selected by Environment {environment} (its `unitSelector` does not match the unit's labels)")
            }
            Some(_) => format!("unit {unit_name} is disabled in Environment {environment}"),
        };
        return Err(Finding::new(
            Code::PromoteSelectorUnit,
            format!("{at}: {message}"),
            format!(
                "select a unit Environment {environment} runs, or give the unit the labels its unitSelector matches"
            ),
        ));
    };
    let spec = unit.spec.as_ref().expect("the ownership set holds rendered specs");
    let driver = drivers.get(&unit.envelope.api_version, &unit.envelope.kind);
    match form {
        SelectForm::Input => {
            let pointer = select.input.as_deref().unwrap_or_default();
            if let Some(found) = nyl_core::json_pointer::resolve(spec, pointer) {
                return Ok(ValueType::of_json(found));
            }
            let Some(driver) = driver else { return Ok(None) };
            match schema_type_at(driver, pointer) {
                SchemaType::Known(found) => Ok(Some(found)),
                SchemaType::Unknown => Ok(None),
                SchemaType::Missing => Err(invalid(
                    format!("the pointer {pointer:?} resolves neither in unit {unit_name}'s spec nor in the schema of kind {}", unit.envelope.kind),
                    "point at a field the unit's spec has or its kind defines; `nyl get unit -e <env> <unit>` shows the resolved spec",
                )),
            }
        }
        SelectForm::Output => {
            let output = select.output.as_deref().unwrap_or_default();
            let declared = spec
                .get("outputs")
                .and_then(|outputs| outputs.get(output))
                .and_then(|declaration| serde_json::from_value::<OutputDeclaration>(declaration.clone()).ok());
            let Some(declaration) = declared else {
                return Err(invalid(
                    format!("unit {unit_name} declares no output {output:?}"),
                    "select a declared output, or declare it under the unit's `outputs`",
                ));
            };
            if declaration.sensitive {
                return Err(invalid(
                    format!("output {output:?} of unit {unit_name} is sensitive and cannot be promoted"),
                    "select a non-sensitive output; a sensitive one is never persisted in a record",
                ));
            }
            if select.pointer.is_some() {
                return Ok(None);
            }
            Ok(Some(declaration.output_type.into()))
        }
        // An artifact's fields, as far as its producing kind describes them;
        // anything else is checked when `nyl promote` reads a receipt.
        SelectForm::Artifact => {
            let (Some(driver), Some(artifact)) = (driver, select.artifact.as_deref()) else {
                return Ok(None);
            };
            let pointer = select.pointer.as_deref().unwrap_or_default();
            let schema = match driver.artifact_schema(spec, artifact) {
                ArtifactSchema::Undescribed => return Ok(None),
                ArtifactSchema::NoSuchArtifact => return Err(invalid(
                    format!(
                        "unit {unit_name} of kind {} publishes no artifact named {artifact:?}",
                        unit.envelope.kind
                    ),
                    "name an artifact the unit's kind publishes; `nyl get artifacts -e <env>` lists those of a receipt",
                )),
                ArtifactSchema::Described(schema) => schema,
            };
            let Ok(schema) = serde_json::to_value(schema) else {
                return Ok(None);
            };
            match schema_type_in(&schema, pointer) {
                SchemaType::Known(found) => Ok(Some(found)),
                SchemaType::Unknown => Ok(None),
                SchemaType::Missing => Err(invalid(
                    format!(
                        "the pointer {pointer:?} names no field of artifact {artifact} of kind {}",
                        unit.envelope.kind
                    ),
                    "point at a field the artifact declares; the kind's documentation lists its artifacts' fields",
                )),
            }
        }
        SelectForm::TargetInput => Ok(None),
    }
}

fn check_attestations(
    inputs: &PathChecks<'_>,
    path: &PromotionPath,
    source: &RenderedSource,
    from: &str,
    covered: &BTreeSet<String>,
) -> Vec<Finding> {
    let name = &path.metadata.name;
    let mut findings = Vec::new();
    let units = ownership_set(source);
    for (unit, required) in &path.spec.attestations.units {
        let found = units
            .iter()
            .find(|(name, _)| name.as_str() == unit)
            .map(|(_, found)| *found);
        let Some(found) = found.filter(|_| covered.contains(unit)) else {
            findings.push(Finding::new(
                Code::PromoteSelectorUnit,
                format!("PromotionPath {name} requires attestations of unit {unit:?}, which the path does not cover: Environment {from} does not select it, or the target does not"),
                "name a unit that the source environment selects and the path's values or target cover",
            ));
            continue;
        };
        let declared = inputs
            .drivers
            .get(&found.envelope.api_version, &found.envelope.kind)
            .map(|driver| driver.attestations(found.spec.as_ref().expect("the ownership set holds rendered specs")))
            .unwrap_or_default();
        for attestation in required.iter().filter(|attestation| !declared.contains(attestation)) {
            findings.push(Finding::new(
                Code::AttestationUndeclared,
                format!(
                    "PromotionPath {name} requires attestation {attestation:?} of unit {unit}, which its kind does not declare (it declares: {})",
                    list(&declared)
                ),
                "require only the names the unit's kind produces for its spec",
            ));
        }
    }
    if let Some(required) = &path.spec.attestations.environment {
        let declared = &source.environment.spec.attestations;
        for attestation in required.iter().filter(|attestation| !declared.contains(attestation)) {
            findings.push(Finding::new(
                Code::AttestationUndeclared,
                format!(
                    "PromotionPath {name} requires environment attestation {attestation:?}, which Environment {from} does not declare (it declares: {})",
                    list(declared)
                ),
                format!("add {attestation} to Environment {from}'s `attestations`, or require only the names it declares"),
            ));
        }
    }
    findings
}

/// The path's `coverage` against what each named unit runs as: the unit is
/// one the source selects and enables, its kind is registered and judges
/// its deployments, and every group and deployment named is one its driver
/// lists. `target` is the path's target environment with whether the path
/// promotes its source, when the target is known: a value-only path takes no
/// `groups`. A listing the driver could not make is
/// [`Code::PromoteCoverageUnchecked`], which a command that needs only
/// another environment may report as a warning.
#[allow(clippy::too_many_lines)] // One entry: the unit, then each field, in the order the contract lists them.
fn check_coverage(
    inputs: &PathChecks<'_>,
    path: &PromotionPath,
    source: &RenderedSource,
    from: &str,
    target: Option<(&str, bool)>,
) -> Vec<Finding> {
    let name = &path.metadata.name;
    let mut findings = Vec::new();
    let environment = EnvironmentName::new(from).ok();
    for (unit, coverage) in &path.spec.coverage {
        let unknown = |message: String, fix: String| Finding::new(Code::PromoteCoverageUnknown, message, fix);
        if let (false, Some((to, false))) = (coverage.groups.is_empty(), target) {
            findings.push(Finding::new(
                Code::PromoteCoverageIneffective,
                format!(
                    "PromotionPath {name} sets coverage.{unit}.groups, but Environment {to} does not take its source from the path (its `source.promotion` does not name {name}), so the path promotes only values, and each value is judged for the deployment that consumes it, whatever its group"
                ),
                format!(
                    "remove coverage.{unit}.groups and list a deployment to leave out of a value's judgement in coverage.{unit}.exclude, or make the path promote {to}'s source with `source: {{promotion: {{path: {name}}}}}` on Environment {to}"
                ),
            ));
        }
        let unit_name = match coverage_unit(source, inputs.drivers, unit) {
            CoverageUnit::Missing => {
                findings.push(unknown(
                    format!("PromotionPath {name} names unit {unit:?} in `coverage`, but Environment {from} declares no such unit"),
                    format!("key coverage by a unit Environment {from} runs (`nyl get units -e {from}` lists them)"),
                ));
                continue;
            }
            CoverageUnit::Unselected => {
                findings.push(unknown(
                    format!("PromotionPath {name} names unit {unit:?} in `coverage`, which Environment {from} does not select (its `unitSelector` does not match the unit's labels)"),
                    format!("key coverage by a unit Environment {from} runs (`nyl get units -e {from}` lists them)"),
                ));
                continue;
            }
            CoverageUnit::Disabled => {
                findings.push(unknown(
                    format!("PromotionPath {name} names unit {unit:?} in `coverage`, which is disabled in Environment {from}"),
                    format!("enable unit {unit} in Environment {from}, or remove coverage.{unit}"),
                ));
                continue;
            }
            CoverageUnit::UnknownKind(found) => {
                findings.push(Finding::new(
                    Code::UnitKindUnknown,
                    format!(
                        "PromotionPath {name} names unit {unit} in `coverage`, whose kind {}/{} no registered driver serves",
                        found.envelope.api_version, found.envelope.kind
                    ),
                    "use a unit kind this Nyl supports (`nyl validate` lists the unit's own finding), or remove the coverage entry",
                ));
                continue;
            }
            CoverageUnit::NotJudged(found) => {
                let kind = &found.envelope.kind;
                for (field, names) in [("groups", &coverage.groups), ("exclude", &coverage.exclude)] {
                    if !names.is_empty() {
                        findings.push(Finding::new(
                            Code::PromoteCoverageIneffective,
                            format!(
                                "PromotionPath {name} sets coverage.{unit}.{field}, but unit {unit} (kind {kind}) records no attestation of what each of its deployments runs, so there is nothing for it to select"
                            ),
                            format!(
                                "turn on the unit's observation of what runs (an option of kind {kind}), or remove coverage.{unit}.{field}"
                            ),
                        ));
                    }
                }
                if !coverage.require.is_empty() {
                    findings.push(Finding::new(
                        Code::PromoteCoverageIneffective,
                        format!(
                            "PromotionPath {name} sets coverage.{unit}.require, but unit {unit} (kind {kind}) records no attestation of what each of its deployments runs, so every promotion along the path would be refused"
                        ),
                        format!(
                            "turn on the unit's observation of what runs (an option of kind {kind}), or remove coverage.{unit}.require"
                        ),
                    ));
                }
                continue;
            }
            CoverageUnit::Listable(unit_name) => unit_name,
        };
        let Some(listing) = environment
            .as_ref()
            .and_then(|environment| inputs.deployments.get(&(environment.clone(), unit_name.clone())))
        else {
            continue;
        };
        let deployments = match listing {
            Ok(Some(deployments)) => deployments,
            Ok(None) => {
                findings.push(unknown(
                    format!("PromotionPath {name} names unit {unit} in `coverage`, but its kind runs no deployments a path can select"),
                    format!("remove coverage.{unit}: only a unit whose kind lists its deployments takes coverage"),
                ));
                continue;
            }
            Err(error) => {
                findings.push(Finding::new(
                    Code::PromoteCoverageUnchecked,
                    format!("PromotionPath {name}: the deployments of unit {unit} in Environment {from} cannot be listed, so coverage.{unit} is not checked: {error}"),
                    format!("correct what the error names in unit {unit}'s declaration or what it renders from; until then a promotion still refuses a coverage.{unit} name no attestation lists"),
                ));
                continue;
            }
        };
        let groups: BTreeSet<&str> = deployments
            .iter()
            .filter_map(|deployment| deployment.group.as_deref())
            .collect();
        let names: BTreeSet<&str> = deployments.iter().map(|deployment| deployment.name.as_str()).collect();
        for group in coverage.groups.iter().filter(|group| !groups.contains(group.as_str())) {
            findings.push(unknown(
                format!(
                    "PromotionPath {name} names group {group:?} in coverage.{unit}.groups, but no deployment of unit {unit} belongs to it (its groups: {})",
                    list_set(&groups)
                ),
                format!("name one of the groups of unit {unit} listed in the message"),
            ));
        }
        for (field, named) in [("exclude", &coverage.exclude), ("require", &coverage.require)] {
            for deployment in named.iter().filter(|deployment| !names.contains(deployment.as_str())) {
                findings.push(unknown(
                    format!(
                        "PromotionPath {name} names deployment {deployment:?} in coverage.{unit}.{field}, but unit {unit} runs no such deployment (it runs: {})",
                        list_set(&names)
                    ),
                    format!("name a deployment of unit {unit} as its kind lists it, or key the entry by the unit that runs it"),
                ));
            }
        }
    }
    findings
}

/// Split `findings` into the errors of a command that works on one
/// environment and its warnings: a listing that could not be made
/// ([`Code::PromoteCoverageUnchecked`]) concerns the source environment's
/// declaration, which such a command does not need, and a declared type
/// that could not be read ([`Code::PromoteValueTypeUnchecked`]) is checked
/// again when the value resolves, so both warn there; `nyl validate` keeps
/// them errors.
pub fn split_unchecked(findings: Vec<Finding>) -> (Vec<Finding>, Vec<Finding>) {
    findings.into_iter().partition(|finding| !is_unchecked(finding.code))
}

/// Whether `code` says a check could not be made rather than failed:
/// what [`split_unchecked`] makes a warning.
pub fn is_unchecked(code: Code) -> bool {
    matches!(code, Code::PromoteCoverageUnchecked | Code::PromoteValueTypeUnchecked)
}

fn list_set(names: &BTreeSet<&str>) -> String {
    if names.is_empty() {
        "none".to_owned()
    } else {
        names.iter().copied().collect::<Vec<_>>().join(", ")
    }
}

fn list(names: &[String]) -> String {
    if names.is_empty() {
        "none".to_owned()
    } else {
        names.join(", ")
    }
}

fn check_reader(
    path: &PromotionPath,
    endpoints: &BTreeMap<&str, EnvironmentName>,
    value_types: &BTreeMap<&str, Option<ValueType>>,
    reader: &ValueReader,
) -> Vec<Finding> {
    let name = path.metadata.name.as_str();
    let binding = &reader.binding;
    // Which path a binding means: the one it names, else any path into the reading environment.
    let means_this = match &binding.path {
        Some(named) => named == name,
        None => true,
    } && endpoints.get("to") == Some(&reader.environment);
    if !means_this || !path.spec.values.contains_key(&binding.value) {
        return Vec::new();
    }
    let Some(Some(produced)) = value_types.get(binding.value.as_str()) else {
        return Vec::new();
    };
    if let Some(reason) = &reader.unchecked {
        return vec![Finding::new(
            Code::PromoteValueTypeUnchecked,
            format!(
                "PromotionPath {name} value {:?} is a {}, but the type that {} accepts cannot be read, so the two are not compared: {reason}",
                binding.value,
                produced.name(),
                reader.location
            ),
            "make what the error names readable, such as by running once without --offline or --local to cache a remote source; resolution still checks the value against the declared type",
        )];
    }
    let Some(expected) = reader.expected else {
        return Vec::new();
    };
    if produced.fits(expected) {
        return Vec::new();
    }
    vec![Finding::new(
        Code::PromoteValueType,
        format!(
            "PromotionPath {name} value {:?} is a {}, but {} accepts a {}",
            binding.value,
            produced.name(),
            reader.location,
            expected.name()
        ),
        format!(
            "select a source field of type {} for the value, or bind {} to a value of the type it accepts",
            expected.name(),
            reader.location
        ),
    )]
}

/// The paths an environment's `source.promotion` names, if its source is promoted.
fn source_paths(source: &RenderedSource) -> Option<Vec<String>> {
    let promoted = source.environment.spec.source.as_ref()?.promotion.as_ref()?;
    Some(promoted.path.iter().chain(&promoted.paths).cloned().collect())
}

/// Whether every `$promotion` binding can be supplied: Contract:
/// [Several paths into one environment](../../../../design/promotion.md#several-paths-into-one-environment).
///
/// - A binding that names a `path` reads only that path, which must exist,
///   promote into the binding's environment, and declare the value.
/// - A path-less binding reads the newest record into its environment that
///   carries the value. In an environment whose source is promoted, only the
///   source paths count, and every one of them must define the value, so the
///   source commit and the values proven with it always move together;
///   otherwise some path into the environment must define it.
pub fn check_bindings(
    paths: &[PromotionPath],
    sources: &BTreeMap<EnvironmentName, RenderedSource>,
    readers: &[ValueReader],
) -> Vec<Finding> {
    let into = |environment: &EnvironmentName| {
        paths
            .iter()
            .filter(|path| path.spec.to.environment.as_deref() == Some(environment.as_str()))
            .collect::<Vec<_>>()
    };
    let list = |paths: &[&PromotionPath]| {
        if paths.is_empty() {
            "none".to_owned()
        } else {
            paths
                .iter()
                .map(|path| path.metadata.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        }
    };
    let mut findings = Vec::new();
    for reader in readers {
        let (binding, environment) = (&reader.binding, &reader.environment);
        let value = &binding.value;
        let fix = "declare the value under `values` of a PromotionPath into the environment, or correct the binding's `path` and `value`";
        let problem = if let Some(named) = &binding.path {
            match paths.iter().find(|path| path.metadata.name == *named) {
                None => Some(format!(
                    "{} reads value {value:?} of PromotionPath {named}, which the project does not declare (it declares: {})",
                    reader.location,
                    list(&paths.iter().collect::<Vec<_>>())
                )),
                Some(path) if path.spec.to.environment.as_deref() != Some(environment.as_str()) => Some(format!(
                    "{} reads value {value:?} of PromotionPath {named}, which promotes into {}, not into Environment {environment}, so it would never have a record there (paths into {environment}: {})",
                    reader.location,
                    path.spec.to.environment.as_deref().unwrap_or("a target"),
                    list(&into(environment))
                )),
                Some(path) if !path.spec.values.contains_key(value) => Some(format!(
                    "{} reads value {value:?} of PromotionPath {named}, which declares no such value",
                    reader.location
                )),
                Some(_) => None,
            }
        } else {
            let candidates = match sources.get(environment).and_then(source_paths) {
                Some(names) => names
                    .iter()
                    .filter_map(|name| paths.iter().find(|path| path.metadata.name == *name))
                    .collect::<Vec<_>>(),
                None => into(environment),
            };
            let promoted = sources.get(environment).and_then(source_paths).is_some();
            let defines = |path: &&PromotionPath| path.spec.values.contains_key(value);
            if promoted {
                candidates.iter().find(|path| !defines(path)).map(|path| {
                    format!(
                        "{} reads value {value:?} without a path, but PromotionPath {} of Environment {environment}'s promoted source does not define it; every source path must (checked: {})",
                        reader.location,
                        path.metadata.name,
                        list(&candidates)
                    )
                })
            } else if candidates.iter().any(defines) {
                None
            } else {
                Some(format!(
                    "{} reads value {value:?}, which no PromotionPath into Environment {environment} declares (checked: {})",
                    reader.location,
                    list(&candidates)
                ))
            }
        };
        findings.extend(problem.map(|message| Finding::new(Code::PromoteBindingUnknown, message, fix)));
    }
    findings
}

/// The paths a promoted `source.promotion` names: each must exist and
/// promote into the environment whose source it is.
pub fn check_sources(
    paths: &[PromotionPath],
    declared: &BTreeSet<EnvironmentName>,
    sources: &BTreeMap<EnvironmentName, RenderedSource>,
) -> Vec<Finding> {
    let mut findings = Vec::new();
    for (environment, source) in sources
        .iter()
        .filter(|(environment, _)| declared.contains(*environment))
    {
        for name in source_paths(source).unwrap_or_default() {
            let message = match paths.iter().find(|path| path.metadata.name == name) {
                None => format!(
                    "Environment {environment} takes its source from PromotionPath {name}, which the project does not declare"
                ),
                Some(path) if path.spec.to.environment.as_deref() != Some(environment.as_str()) => format!(
                    "Environment {environment} takes its source from PromotionPath {name}, which promotes into {}, so the source would never have a record",
                    path.spec.to.environment.as_deref().unwrap_or("a target")
                ),
                Some(_) => continue,
            };
            findings.push(Finding::new(
                Code::PromoteBindingUnknown,
                message,
                format!("name PromotionPaths whose `to` is {environment} in `source.promotion`, or declare the path"),
            ));
        }
    }
    findings
}

/// The values that two or more paths into `target` promote whose selector
/// names no source commit, so the supersede check cannot check them
/// (`NYL-PROMOTE-UNCHECKED`, a warning `plan` reports before any promotion
/// runs). Contract: [Several paths into one environment](../../../../design/promotion.md#several-paths-into-one-environment),
/// "Source commit of a value". It mirrors what the check reads from a
/// recorded state ([`super::promotion::supersede::value_commits`]), over the
/// source environment's rendered units: an artifact or output has the
/// producing unit's Git source fields, an input the field that holds it or
/// the producer a `$unit` reference at or above it names. A value only one
/// path carries is never compared, so it is not listed.
pub fn unchecked(
    paths: &[PromotionPath],
    target: &EnvironmentName,
    sources: &BTreeMap<EnvironmentName, RenderedSource>,
    drivers: &DriverSet,
) -> Vec<Finding> {
    let into: Vec<&PromotionPath> = paths
        .iter()
        .filter(|path| path.spec.to.environment.as_deref() == Some(target.as_str()))
        .collect();
    let mut findings = Vec::new();
    for path in &into {
        let Some(source) = path
            .spec
            .from
            .environment
            .as_deref()
            .and_then(|name| EnvironmentName::new(name).ok())
            .and_then(|name| sources.get(&name))
        else {
            continue;
        };
        let units = ownership_set(source);
        for (name, value) in &path.spec.values {
            let others: Vec<&str> = into
                .iter()
                .filter(|other| other.metadata.name != path.metadata.name && other.spec.values.contains_key(name))
                .map(|other| other.metadata.name.as_str())
                .collect();
            if others.is_empty() {
                continue;
            }
            let Some(unit) = value.select.unit.as_ref() else {
                continue;
            };
            let has_fields = |unit: &RenderedUnit, within: Option<&str>| {
                let Some(spec) = unit.spec.as_ref() else {
                    return false;
                };
                drivers
                    .get(&unit.envelope.api_version, &unit.envelope.kind)
                    .is_some_and(|driver| {
                        driver
                            .source_fields(spec)
                            .iter()
                            .any(|field| within.is_none_or(|pointer| is_within(pointer, field)))
                    })
            };
            let Some(rendered) = units.get(unit) else {
                continue;
            };
            let checked = match value.select.input.as_deref() {
                None => has_fields(rendered, None),
                Some(pointer) => match cited_producer(rendered.spec.as_ref(), pointer) {
                    Some(Cited::Producer(producer)) => {
                        units.get(&producer).is_some_and(|producer| has_fields(producer, None))
                    }
                    Some(Cited::Elsewhere) => false,
                    None => has_fields(rendered, Some(pointer)),
                },
            };
            if !checked {
                findings.push(Finding::new(
                    Code::PromoteUnchecked,
                    format!(
                        "value {name:?} of path {} has no known source commit, so promoting it into {target} cannot be checked against what {} promoted there",
                        path.metadata.name,
                        others.join(", ")
                    ),
                    "select a value whose producer builds from a Git source (such as an image's artifact), or check by hand that a promotion keeps what the other path promoted",
                ));
            }
        }
    }
    findings
}

/// Whether `pointer` is `ancestor` or lies beneath it.
fn is_within(pointer: &str, ancestor: &str) -> bool {
    pointer == ancestor || pointer.strip_prefix(ancestor).is_some_and(|rest| rest.starts_with('/'))
}

/// What a reference at a pointer or above it names.
enum Cited {
    /// A producer in the same environment.
    Producer(nyl_core::orchestration::ids::UnitName),
    /// A producer in another environment, or a promoted value.
    Elsewhere,
}

/// The reference at `pointer` or above it, `None` without one.
fn cited_producer(spec: Option<&Value>, pointer: &str) -> Option<Cited> {
    let spec = spec?;
    let mut current = String::new();
    let mut cited = None;
    for token in pointer.split('/').skip(1) {
        current.push('/');
        current.push_str(token);
        let Some(Value::Object(fields)) = spec.pointer(&current) else {
            continue;
        };
        if is_reference(fields) {
            let reference = fields.get(nyl_core::bindings::Source::Unit.key());
            cited = Some(
                reference
                    .filter(|reference| reference.get("environment").is_none())
                    .and_then(|reference| reference.get("unit"))
                    .and_then(Value::as_str)
                    .and_then(|unit| nyl_core::orchestration::ids::UnitName::new(unit).ok())
                    .map_or(Cited::Elsewhere, Cited::Producer),
            );
        }
    }
    cited
}

/// The `$promotion` bindings in the specs of each environment's selected
/// units, with the type each place accepts where the unit's kind schema says.
pub fn unit_readers(sources: &BTreeMap<EnvironmentName, RenderedSource>, drivers: &DriverSet) -> Vec<ValueReader> {
    let mut readers = Vec::new();
    for (environment, source) in sources {
        for (unit, rendered) in ownership_set(source) {
            let spec = rendered.spec.as_ref().expect("the ownership set holds rendered specs");
            let driver = drivers.get(&rendered.envelope.api_version, &rendered.envelope.kind);
            walk(spec, "", &mut |pointer, binding| {
                readers.push(ValueReader {
                    location: format!("unit {unit} of Environment {environment} at {pointer:?}"),
                    environment: environment.clone(),
                    binding,
                    expected: driver.and_then(|driver| match schema_type_at(driver, pointer) {
                        SchemaType::Known(found) => Some(found),
                        _ => None,
                    }),
                    field: Some(((*unit).clone(), pointer.to_owned())),
                    unchecked: None,
                });
            });
        }
    }
    readers
}

/// The `$promotion` bindings of a unit's spec, by JSON Pointer, as
/// readers find them: `nyl get promotions` lists the record each reads.
pub fn bindings_in(spec: &Value) -> Vec<(String, PromotionInputSource)> {
    let mut found = Vec::new();
    walk(spec, "", &mut |pointer, binding| {
        found.push((pointer.to_owned(), binding));
    });
    found
}

/// Every `$promotion` source of a rendered unit spec, through the one walker.
/// A spec whose sources do not parse yields none here: the dependency graph
/// reports it ([`super::graph`]), and the unit does not run.
fn walk(value: &Value, pointer: &str, found: &mut dyn FnMut(&str, PromotionInputSource)) {
    let clauses = nyl_core::bindings::find_clauses(
        "spec",
        pointer,
        value,
        nyl_core::bindings::Context::UnitExecution.into(),
    )
    .unwrap_or_default();
    for clause in clauses {
        if let nyl_core::bindings::Clause::Promotion(binding) = clause.clause {
            found(&clause.pointer, binding);
        }
    }
}

/// What a walk of a kind's schema along a pointer found.
enum Walked {
    Node(Value),
    /// The schema says nothing about the member: any value may be there.
    Free,
    Missing,
}

/// What a kind's schema says about the field a pointer names.
enum SchemaType {
    /// The schema has no such field.
    Missing,
    /// The field may exist, with no single type to check against.
    Unknown,
    Known(ValueType),
}

/// The type of the field `pointer` names in the driver's schema.
fn schema_type_at(driver: &dyn Driver, pointer: &str) -> SchemaType {
    let Ok(root) = serde_json::to_value(driver.spec_schema()) else {
        return SchemaType::Unknown;
    };
    if pointer
        .split('/')
        .nth(1)
        .is_some_and(|first| COMMON_UNIT_FIELDS.contains(&first.replace("~1", "/").replace("~0", "~").as_str()))
    {
        return SchemaType::Unknown;
    }
    schema_type_in(&root, pointer)
}

/// The type of the field `pointer` names in the JSON Schema `root`.
fn schema_type_in(root: &Value, pointer: &str) -> SchemaType {
    let segments = pointer
        .split('/')
        .skip(1)
        .map(|segment| segment.replace("~1", "/").replace("~0", "~"))
        .collect::<Vec<_>>();
    let root = root.clone();
    let mut nodes = vec![root.clone()];
    for segment in &segments {
        let mut next = Vec::new();
        let mut free = false;
        for node in &nodes {
            match step(&root, node, segment) {
                Walked::Node(found) => next.push(found),
                Walked::Free => free = true,
                Walked::Missing => {}
            }
        }
        if next.is_empty() {
            return if free { SchemaType::Unknown } else { SchemaType::Missing };
        }
        nodes = next;
    }
    let types = nodes
        .iter()
        .flat_map(|node| types_of(&root, node))
        .collect::<BTreeSet<_>>();
    let mut types = types.into_iter();
    match (types.next(), types.next()) {
        (Some(only), None) => SchemaType::Known(only),
        _ => SchemaType::Unknown,
    }
}

/// The alternatives of a node: itself, or its `anyOf`, `oneOf`, and `allOf` branches.
fn alternatives(root: &Value, node: &Value) -> Vec<Value> {
    let node = resolve(root, node);
    let mut branches = Vec::new();
    for key in ["anyOf", "oneOf", "allOf"] {
        if let Some(Value::Array(items)) = node.get(key) {
            for item in items {
                branches.extend(alternatives(root, item));
            }
        }
    }
    if branches.is_empty() {
        vec![node]
    } else {
        branches
    }
}

fn resolve(root: &Value, node: &Value) -> Value {
    let Some(reference) = node.get("$ref").and_then(Value::as_str) else {
        return node.clone();
    };
    reference
        .strip_prefix('#')
        .and_then(|pointer| root.pointer(pointer))
        .map_or_else(|| node.clone(), |found| resolve(root, found))
}

fn step(root: &Value, node: &Value, segment: &str) -> Walked {
    let mut free = false;
    for branch in alternatives(root, node) {
        let branch = &branch;
        if let Some(property) = branch.get("properties").and_then(|properties| properties.get(segment)) {
            return Walked::Node(property.clone());
        }
        match branch.get("additionalProperties") {
            Some(Value::Object(_)) => return Walked::Node(branch["additionalProperties"].clone()),
            Some(Value::Bool(false)) => continue,
            _ => {}
        }
        if let Some(items) = branch.get("items") {
            if segment.parse::<usize>().is_ok() {
                return Walked::Node(items.clone());
            }
            continue;
        }
        let scalar = branch
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|kind| !matches!(kind, "object" | "array"));
        // An object that does not close itself accepts other members.
        if !scalar {
            free = true;
        }
    }
    if free {
        Walked::Free
    } else {
        Walked::Missing
    }
}

/// The value types a schema node allows, leaving out reference objects and `null`.
fn types_of(root: &Value, node: &Value) -> Vec<ValueType> {
    let mut found = Vec::new();
    for branch in alternatives(root, node) {
        let is_reference_object = branch
            .get("properties")
            .and_then(Value::as_object)
            .is_some_and(|properties| properties.contains_key("$unit") || properties.contains_key("$promotion"));
        if is_reference_object {
            continue;
        }
        let kinds = match branch.get("type") {
            Some(Value::String(kind)) => vec![kind.clone()],
            Some(Value::Array(kinds)) => kinds.iter().filter_map(Value::as_str).map(str::to_owned).collect(),
            _ if branch.get("properties").is_some() => vec!["object".to_owned()],
            _ => Vec::new(),
        };
        for kind in kinds {
            found.extend(match kind.as_str() {
                "string" => Some(ValueType::String),
                "integer" => Some(ValueType::Integer),
                "number" => Some(ValueType::Number),
                "boolean" => Some(ValueType::Boolean),
                "object" => Some(ValueType::Object),
                "array" => Some(ValueType::Array),
                _ => None,
            });
        }
    }
    found
}

#[cfg(test)]
mod tests;
