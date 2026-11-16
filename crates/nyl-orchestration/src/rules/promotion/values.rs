//! Reading a promoted value from a recorded state.
//!
//! Contract: [PromotionPath](../../../../../design/promotion.md#promotionpath)
//! ("A `select` names exactly one source field, with the same JSON Pointer
//! semantics and resolver as `fromUnit` references and `nyl get`") and
//! [Promotion paths](../../../../../design/promotion.md#promotion-paths).
//!
//! A value is read through [`resolve_reference`] (output and artifact
//! selectors) and [`resolve_input`] (input selectors), the one resolver every
//! reference form shares, over a [`ReferenceView`] of the recorded state's
//! documents. So a selector keeps the rules of a binding: the producer must
//! have a current receipt in that state, an output must be declared and not
//! sensitive, an artifact's document must match the digest its receipt lists,
//! and `pointer` selects inside the output or the artifact's `spec`.
//!
//! The record names where each value came from: the source unit and its
//! incarnation, and the receipt's execution key for an output or artifact, or
//! the digest of the selected input value (canonical JSON) for an input.
//!
//! Which unit *consumes* a value, and so whose evidence a value-only path
//! judges, is [`consumer`]. Today it is the selected unit itself (the
//! decision D7 of the M6 plan for artifact and output selectors). Tracing
//! `{unit: <publication>, input: /releases/<group>~1<release>/…}` to the
//! Application of that Release is the value-only specifics of M6's PR 11,
//! which replaces this function and the unit-level judgement with the
//! per-Application one.

use std::collections::{BTreeMap, BTreeSet};

use nyl_core::bindings::UnitInputSource;
use nyl_core::orchestration::codes::{Code, Finding};
use nyl_core::orchestration::ids::{EnvironmentName, UnitName};
use nyl_core::orchestration::promotion::{PromotedUnit, PromotedValue, PromotionSelect};
use nyl_core::orchestration::time::Timestamp;
use nyl_core::resources::promotion::SelectForm;

use super::history::RecordedState;
use crate::rules::references::{resolve_input, resolve_reference, ReferenceValue, ReferenceView, ResolutionError};

/// An artifact a promoted value reads, which `nyl promote` checks still exists
/// before recording (`verifyArtifacts`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactCheck {
    /// The value's name in the path.
    pub value: String,
    pub unit: UnitName,
    pub artifact: String,
    pub kind: Option<String>,
}

/// A value read from a state.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Read {
    pub value: PromotedValue,
    pub artifact: Option<ArtifactCheck>,
}

/// Why a value could not be read.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Unreadable {
    /// The selector or what it reads is invalid in this state (exit 1).
    Invalid(Finding),
    /// The producer has no current receipt in this state.
    Blocked { unit: UnitName, message: String },
}

/// The unit whose evidence a value-only path judges for `select`.
pub(super) fn consumer(select: &PromotionSelect) -> Option<UnitName> {
    select.unit.clone()
}

/// The consumer of `select`, or why the selector cannot be read at all: a
/// malformed one, or a target's Release input (M11).
pub(super) fn preflight(name: &str, select: &PromotionSelect) -> Result<UnitName, Finding> {
    let form = select.form().map_err(|reason| {
        Finding::new(
            Code::PromoteSelectorInvalid,
            format!("value {name:?}: the selector {reason}"),
            "fix the selector in the PromotionPath, then run `nyl validate`",
        )
    })?;
    match (form, consumer(select)) {
        (SelectForm::TargetInput, _) | (_, None) => Err(Finding::new(
            Code::PromoteUnsupported,
            format!("value {name:?} selects a Release input of a target, which M11 delivers"),
            "select a unit's input, output, or artifact of the source environment",
        )),
        (_, Some(unit)) => Ok(unit),
    }
}

/// The units a path's selectors read.
pub(super) fn units<'a>(selects: impl Iterator<Item = &'a PromotionSelect>) -> BTreeSet<UnitName> {
    selects.filter_map(|select| select.unit.clone()).collect()
}

fn invalid(error: ResolutionError, name: &str) -> Unreadable {
    Unreadable::Invalid(Finding::new(
        Code::PromoteSelectorInvalid,
        format!("value {name:?} {}", error.message),
        error.fix,
    ))
}

/// Read value `name`, selected by `select`, from `state` of `environment`.
pub(super) fn read(
    environment: &EnvironmentName,
    state: &RecordedState,
    name: &str,
    select: &PromotionSelect,
    now: Timestamp,
) -> Result<Read, Unreadable> {
    let unit = &preflight(name, select).map_err(Unreadable::Invalid)?;
    let form = select.form().expect("preflight checked the form");
    let snapshot = &state.snapshot;
    let environments = BTreeMap::new();
    let produced = BTreeSet::new();
    // A source state's path-less bindings read as its own runs read them.
    let source_paths = snapshot
        .environment
        .as_ref()
        .and_then(|record| crate::rules::sources::promoted_source_paths(&record.spec));
    let view = ReferenceView {
        environments: &environments,
        environment,
        desired: &snapshot.desired,
        observed: &snapshot.observed,
        artifacts: &snapshot.artifacts,
        attestations: &snapshot.attestations,
        observations: &snapshot.observations,
        now,
        observed_commit: state.observed_commit.as_ref(),
        run: None,
        produced_in_run: &produced,
        previous: &snapshot.desired,
        // A past state of the source, read with the records it held then.
        // Deliberately not the environment's current records
        // (`shell::promote::environment_promotions`), which runs read.
        promotions: &snapshot.promotions,
        source_paths: source_paths.as_ref(),
    };
    let resolved = match form {
        SelectForm::Input => {
            let pointer = select.input.as_deref().unwrap_or("");
            resolve_input(&view, unit.as_str(), pointer)
        }
        _ => resolve_reference(
            &view,
            &UnitInputSource {
                environment: None,
                unit: unit.to_string(),
                output: select.output.clone(),
                artifact: select.artifact.clone(),
                observation: None,
                kind: select.kind.clone(),
                pointer: select.pointer.clone(),
                evidence: None,
                attestations: Vec::new(),
            },
        ),
    };
    let (value, provenance) = match resolved.map_err(|error| invalid(error, name))? {
        ReferenceValue::Value { value, provenance } => (value, provenance),
        ReferenceValue::Blocked { unit, .. } => {
            return Err(Unreadable::Blocked {
                message: format!("unit {unit} has no current receipt in this state, so value {name:?} cannot be read"),
                unit,
            })
        }
    };
    let (execution_key, input_digest) = match form {
        // The digest of the canonical JSON of the selected value, the same
        // digest the ownership index records for a Release input.
        SelectForm::Input => {
            let bytes = nyl_core::digest::canonical_json_bytes(&value).expect("a JSON value serializes");
            (None, Some(nyl_core::digest::sha256_prefixed(&bytes)))
        }
        _ => (Some(provenance.execution_key.clone()), None),
    };
    let artifact = (form == SelectForm::Artifact).then(|| ArtifactCheck {
        value: name.to_owned(),
        unit: unit.clone(),
        artifact: select.artifact.clone().unwrap_or_default(),
        kind: select.kind.clone(),
    });
    Ok(Read {
        value: PromotedValue {
            value,
            select: select.clone(),
            unit: Some(PromotedUnit {
                name: unit.clone(),
                uid: Some(provenance.uid),
            }),
            execution_key,
            input_digest,
            state: None,
        },
        artifact,
    })
}
