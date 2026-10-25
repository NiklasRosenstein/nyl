//! Reading state without the lease: `status -e` and `get -e`.
//!
//! Contract: [Inspection](../../../../design/orchestration-core.md#inspection).
//! Both read one snapshot of the state refs (and, for status, the lease and
//! run refs) and never write. `get output` and `get artifact` resolve
//! exactly like `fromUnit`, through `rules::references::resolve_reference`.

use std::collections::{BTreeMap, BTreeSet};

use nyl_core::bindings::UnitInputSource;
use nyl_core::orchestration::codes::{Code, Finding};
use nyl_core::orchestration::ids::{EnvironmentName, UnitName};
use nyl_core::orchestration::ports::{EnvironmentSnapshot, StateStore, StateTips};
use nyl_core::orchestration::state::{DesiredUnit, ObservedUnit, StateLocation};
use serde_json::Value;

use super::{store_error, Context};
use crate::rules::references::{resolve_reference, ReferenceValue, ReferenceView};
use crate::run_report::{EnvironmentRunReport, ReportKind, RunReport};

/// `nyl status -e <env>`, with a `NYL-STATE-LOCAL-DIVERGED` warning when
/// the clone's local state branches diverged from the remote. Through a
/// [`super::local::LocalStore`] (`--local`) it reports the local view.
pub fn status(context: &Context<'_>, environment: &EnvironmentName, location: &StateLocation) -> RunReport {
    let read = || {
        let snapshot = context
            .store
            .snapshot(location)
            .map_err(|error| store_error(environment, &error))?;
        super::local::check_record(context.store, snapshot.record.as_ref(), environment, location)?;
        let coordination = context
            .store
            .coordination(location, environment)
            .map_err(|error| store_error(environment, &error))?;
        Ok(crate::rules::unit_state::status(
            environment,
            &snapshot,
            &coordination,
            context.clock.now(),
        ))
    };
    let mut environment_report = read().unwrap_or_else(|finding: Finding| {
        let mut failed = EnvironmentRunReport::new(environment.clone());
        failed.errors.push(finding);
        failed.finish(ReportKind::StatusReport);
        failed
    });
    // Diverged local branches are reported, never an exit category.
    match super::local::divergence(context.store, location, environment) {
        Ok(Some(warning)) => environment_report.warnings.push(warning),
        Ok(None) => {}
        Err(finding) => environment_report.warnings.push(finding),
    }
    let mut report = RunReport::new(ReportKind::StatusReport);
    report.environments.push(environment_report);
    report
}

/// One snapshot, at the refs' tips or at `--state-revision`.
pub fn snapshot(
    store: &dyn StateStore,
    location: &StateLocation,
    at: Option<&StateTips>,
) -> Result<EnvironmentSnapshot, Finding> {
    let read = match at {
        None => store.snapshot(location),
        Some(tips) => store.snapshot_at(location, tips),
    };
    read.map_err(|error| {
        Finding::new(
            Code::StateUnavailable,
            format!("cannot read the state at {}: {error}", location.repository),
            "check access to the state repository and that --state-revision names a state commit",
        )
    })
}

/// The persisted documents of one unit, exactly as stored.
#[derive(Debug, Clone, PartialEq)]
pub struct UnitDocuments {
    pub desired: Option<DesiredUnit>,
    pub observed: Option<ObservedUnit>,
}

/// `nyl get unit <u> -e <env>`; unknown units are
/// [`Code::UnitUnknown`](nyl_core::orchestration::codes::Code::UnitUnknown).
pub fn unit_documents(snapshot: &EnvironmentSnapshot, unit: &UnitName) -> Result<UnitDocuments, Finding> {
    let documents = UnitDocuments {
        desired: snapshot.desired.get(unit).cloned(),
        observed: snapshot.observed.get(unit).cloned(),
    };
    if documents.desired.is_none() && documents.observed.is_none() {
        return Err(Finding::new(
            Code::UnitUnknown,
            format!("the state has no unit {unit}"),
            "list the units in state with `nyl get units -e <environment>`",
        ));
    }
    Ok(documents)
}

/// A value form's answer.
#[derive(Debug, Clone, PartialEq)]
pub enum GetValue {
    Value(Value),
    /// `--allow-stale`: the latest value, with a
    /// [`Code::ReferenceStale`](nyl_core::orchestration::codes::Code::ReferenceStale) warning.
    Stale {
        value: Value,
        warning: Finding,
    },
}

/// `nyl get output u/o` and `nyl get artifact u/n`: resolve `source` like a
/// `fromUnit` reference against `snapshot`. Unresolvable is
/// [`Code::ReferenceUnresolved`](nyl_core::orchestration::codes::Code::ReferenceUnresolved),
/// unless `allow_stale` finds an older value.
///
/// The stale form resolves through the same resolver, against a view in
/// which the producer's desired document names its latest receipt's uid and
/// execution key, so the declaration, digest, and pointer checks still apply.
pub fn get_value(
    environment: &EnvironmentName,
    snapshot: &EnvironmentSnapshot,
    source: &UnitInputSource,
    allow_stale: bool,
) -> Result<GetValue, Finding> {
    let unresolved = |message: String| {
        Finding::new(
            Code::ReferenceUnresolved,
            format!("{}: {message}", describe(source)),
            "run nyl reconcile so the unit has a current receipt, or pass --allow-stale for its latest value",
        )
    };
    let produced = BTreeSet::new();
    let resolve = |desired: &BTreeMap<UnitName, DesiredUnit>| {
        resolve_reference(
            &ReferenceView {
                environment,
                desired,
                observed: &snapshot.observed,
                artifacts: &snapshot.artifacts,
                observed_commit: snapshot.tips.observed.as_ref(),
                run: None,
                produced_in_run: &produced,
                previous: &snapshot.desired,
            },
            source,
        )
    };
    let reason = match resolve(&snapshot.desired) {
        Ok(ReferenceValue::Value { value, .. }) => return Ok(GetValue::Value(value)),
        Ok(ReferenceValue::Blocked { reason, .. }) => reason,
        Err(message) => return Err(unresolved(message)),
    };
    if !allow_stale {
        return Err(unresolved(reason));
    }
    let latest = UnitName::new(&source.unit).ok().and_then(|unit| {
        let observed = snapshot.observed.get(&unit)?;
        let receipt = observed.receipt.as_ref()?;
        let mut desired = snapshot.desired.get(&unit)?.clone();
        desired.unit.uid.clone_from(&observed.unit.uid);
        desired.execution_key = Some(receipt.execution_key.clone());
        let mut view = snapshot.desired.clone();
        view.insert(unit, desired);
        Some(view)
    });
    let Some(view) = latest else {
        return Err(unresolved(reason));
    };
    match resolve(&view) {
        Ok(ReferenceValue::Value { value, .. }) => Ok(GetValue::Stale {
            value,
            warning: Finding::new(
                Code::ReferenceStale,
                format!("{}: {reason}; this is the latest receipt's value", describe(source)),
                "run nyl reconcile to make the receipt current",
            ),
        }),
        Ok(ReferenceValue::Blocked { reason, .. }) => Err(unresolved(reason)),
        Err(message) => Err(unresolved(message)),
    }
}

/// `unit/output` or `unit/artifact`, as the command names it.
fn describe(source: &UnitInputSource) -> String {
    let name = source
        .output
        .as_ref()
        .or(source.artifact.as_ref())
        .map_or("", String::as_str);
    format!("{}/{name}", source.unit)
}

#[cfg(test)]
mod tests {
    use nyl_core::orchestration::state::Receipt;
    use serde_json::json;

    use super::*;

    const UID: &str = "00000000-0000-7000-8000-000000000001";
    const RUN: &str = "00000000-0000-7000-9000-000000000001";

    fn key(digit: char) -> String {
        format!("sha256:{}", digit.to_string().repeat(64))
    }

    fn snapshot(desired_key: char, receipt_key: char) -> EnvironmentSnapshot {
        let identity = json!({
            "environment": "dev", "name": "network", "uid": UID,
            "apiVersion": "units.gitops.nyl/v1", "kind": "Command"
        });
        let desired: DesiredUnit = serde_json::from_value(json!({
            "apiVersion": "gitops.nyl/v1",
            "kind": "DesiredUnit",
            "unit": identity,
            "lifecycle": {"state": "active"},
            "sourceCommit": "a".repeat(40),
            "sourceRevision": "a".repeat(40),
            "spec": {"command": ["./run.sh"], "outputs": {"vpcId": {"type": "string"}}},
            "executionKey": key(desired_key),
            "readiness": {"state": "ready"}
        }))
        .unwrap();
        let receipt: Receipt = serde_json::from_value(json!({
            "executionKey": key(receipt_key),
            "behaviorVersion": 1,
            "run": RUN,
            "startedAt": "2026-09-24T10:00:00Z",
            "finishedAt": "2026-09-24T10:01:00Z",
            "outputs": {"vpcId": "vpc-1"}
        }))
        .unwrap();
        let observed: ObservedUnit = serde_json::from_value(json!({
            "apiVersion": "gitops.nyl/v1",
            "kind": "ObservedUnit",
            "unit": identity,
            "receipt": receipt
        }))
        .unwrap();
        let name = UnitName::new("network").unwrap();
        EnvironmentSnapshot {
            desired: BTreeMap::from([(name.clone(), desired)]),
            observed: BTreeMap::from([(name, observed)]),
            ..EnvironmentSnapshot::default()
        }
    }

    fn output() -> UnitInputSource {
        serde_json::from_value(json!({"unit": "network", "output": "vpcId"})).unwrap()
    }

    fn dev() -> EnvironmentName {
        EnvironmentName::new("dev").unwrap()
    }

    /// Catches `get output` printing a value no `fromUnit` would resolve,
    /// or failing for a current receipt.
    #[test]
    fn test_get_value_resolves_like_a_reference() {
        assert_eq!(
            get_value(&dev(), &snapshot('a', 'a'), &output(), false).unwrap(),
            GetValue::Value(json!("vpc-1"))
        );
        let stale = snapshot('b', 'a');
        let error = get_value(&dev(), &stale, &output(), false).unwrap_err();
        assert_eq!(error.code, Code::ReferenceUnresolved);
        match get_value(&dev(), &stale, &output(), true).unwrap() {
            GetValue::Stale { value, warning } => {
                assert_eq!(value, json!("vpc-1"));
                assert_eq!(warning.code, Code::ReferenceStale);
            }
            other @ GetValue::Value(_) => panic!("expected a stale value, got {other:?}"),
        }
        let undeclared: UnitInputSource =
            serde_json::from_value(json!({"unit": "network", "output": "subnet"})).unwrap();
        assert!(get_value(&dev(), &stale, &undeclared, true).is_err());
    }

    /// Catches `get unit` synthesizing documents for a unit state lacks.
    #[test]
    fn test_unit_documents_are_the_stored_documents() {
        let snapshot = snapshot('a', 'a');
        let documents = unit_documents(&snapshot, &UnitName::new("network").unwrap()).unwrap();
        assert_eq!(documents.desired.as_ref(), snapshot.desired.values().next());
        let missing = unit_documents(&snapshot, &UnitName::new("web").unwrap()).unwrap_err();
        assert_eq!(missing.code, Code::UnitUnknown);
    }
}
