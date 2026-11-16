//! `nyl attest -e <env> [--unit <u>] --name <n> --result pass|fail`: the
//! external form, which records a result the caller already knows.
//!
//! Contract: [Attestations](../../../../design/orchestration-core.md#attestations)
//! and the [promotion contract](../../../../design/promotion.md#attestations).
//! What is recorded, and the refusals, belong to
//! [`crate::rules::external_attest`]. This module takes the environment's
//! lease like every state-writing operation (`--wait-lease` waits for a
//! running reconcile; a held lease is exit 2), reads the snapshot under it,
//! and writes one observed commit (operation `attest`): the unit's
//! `observed/units/<unit>/attestations.yaml`, or the environment's
//! `observed/attestations.yaml`. A recorded result exits 0 whether it passes
//! or fails (what a failing result blocks is reported by `promote` or the
//! dependent); a refusal exits 1.

use std::collections::BTreeMap;

use nyl_core::orchestration::ids::{EnvironmentName, UnitName};
use nyl_core::orchestration::ports::TransitionChanges;
use nyl_core::orchestration::state::{AttestationResult, Operation, StateLocation};
use serde_json::json;

use super::local::{check_record, check_remote_tips};
use super::state::{commit, read, summary, with_lease, Requested};
use super::{store_error, Context};
use crate::report::StateOperationReport;
use crate::rules::external_attest::{self, External, Recording};

/// Record `request` for `unit`'s current receipt, or for the environment's
/// current state without a unit.
pub fn record(
    context: &Context<'_>,
    environment: &EnvironmentName,
    location: &StateLocation,
    unit: Option<&UnitName>,
    request: &External,
    requested: &Requested,
) -> StateOperationReport {
    with_lease(
        context,
        environment,
        Operation::Attest,
        location,
        requested,
        |held, report| {
            let snapshot = read(context, location, environment)?;
            check_record(context.store, snapshot.record.as_ref(), environment, location)?;
            check_remote_tips(context.store, location, environment)?;
            // The state an environment attestation names is the newest commit that
            // changed this environment's desired documents, not the ref's tip.
            let state_commit = context
                .store
                .state_commit(location, &snapshot.tips)
                .map_err(|error| store_error(environment, &error))?;
            let recording = external_attest::record(
                environment,
                &snapshot,
                unit,
                state_commit.as_ref(),
                request,
                context.clock.now(),
            )?;
            let result = match request.result {
                AttestationResult::Pass => "pass",
                AttestationResult::Fail => "fail",
            };
            let mut changes = TransitionChanges::default();
            let mut summary = summary(Operation::Attest, environment, &format!("{} {result}", request.name));
            match recording {
                Recording::Unit { unit, set } => {
                    report.details.insert("unit".to_owned(), json!(unit));
                    summary.units = BTreeMap::from([(
                        unit.clone(),
                        json!({"attestation": request.name, "result": result, "source": "external"}),
                    )]);
                    changes.attestations.insert(unit, Some(set));
                }
                Recording::Environment { set } => {
                    summary.details.insert(
                        "attestation".to_owned(),
                        json!({"name": request.name, "result": result}),
                    );
                    changes.environment_attestations = Some(set);
                }
            }
            report.details.insert("name".to_owned(), json!(request.name));
            report.details.insert("result".to_owned(), json!(result));
            report.details.insert(
                "scope".to_owned(),
                json!(if unit.is_some() { "unit" } else { "environment" }),
            );
            report.details.insert("attester".to_owned(), json!(request.attester));
            commit(context, location, held, &snapshot, summary, changes, requested, report)
        },
    )
}
