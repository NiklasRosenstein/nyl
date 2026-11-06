//! The publication driver's observation of Argo CD: `nyl attest` and the
//! `observe` teardown wait.
//!
//! Contract: [Kubernetes publications](../../../../design/promotion.md#kubernetes-publications)
//! and the [Kubernetes publication unit](../../../../design/orchestration-core.md#kubernetes-publication-unit)
//! (`mode: observe`, Teardown).
//!
//! A unit that observes (`mode: observe`, or `teardownWait: {strategy:
//! observe}`) records an [`ArgoCdObservation`] under `/argocd` of its
//! resolved spec when it is resolved: the instance, its access, the
//! publication destination, and the Applications the target generates. Both
//! operations then read it from the stored document, so neither renders, and
//! a unit torn down from its stored document observes what it recorded.
//!
//! - [`attest`] reads every generated Application, fetches the publication
//!   branch, and judges the reading against the receipt's publication
//!   commit ([`evaluate`]). It records nothing it could not establish: an
//!   unreachable or unauthorized Argo CD, a partial reading, an unreadable
//!   branch is [`AttestationCheck::Unknown`], never a failing attestation,
//!   so a stale `pass` is never replaced by a `fail` that only says "could
//!   not look".
//! - [`observe_removal`] reports which workload Applications Argo CD still
//!   has, for the wait that ends when none remains.

use std::time::Duration;

use nyl_core::orchestration::driver::{
    AttestationCheck, DriverError, DriverResult, ExecutionContext, RemovalContext, RemovalObservation,
};
use nyl_core::orchestration::state::DesiredUnit;
use nyl_render::gitops::observation::{ArgoCdObservation, GitPublicationHistory};
use nyl_render::gitops::publication::RenderedTarget;
use serde_json::Value;

use super::argocd::evaluate::evaluate;
use super::argocd::{reader, ArgoReader, ReadError};

/// The pointer, in the resolved spec, of the recorded observation facts.
pub(super) const ARGOCD: &str = "/argocd";

/// The longest one reading of Argo CD takes.
const READING: Duration = Duration::from_secs(30);

/// Whether a spec observes Argo CD: its `mode` is `observe`, or its
/// `teardownWait` strategy is.
pub(super) fn observes(spec: &Value) -> bool {
    spec.get("mode").and_then(Value::as_str) == Some("observe")
        || spec.pointer("/teardownWait/strategy").and_then(Value::as_str) == Some("observe")
}

/// The facts a unit records when it observes: the JSON of
/// [`ArgoCdObservation`] for `rendered`.
pub(super) fn recorded(rendered: &RenderedTarget) -> DriverResult<Value> {
    let observation = ArgoCdObservation::of(rendered).map_err(|error| {
        DriverError::before_effects(format!("cannot read the Argo CD facts of the target: {error}"))
    })?;
    serde_json::to_value(observation)
        .map_err(|error| DriverError::before_effects(format!("cannot record the Argo CD facts: {error}")))
}

fn observation_of(unit: &DesiredUnit) -> Result<ArgoCdObservation, String> {
    let spec = unit.resolved_spec.as_ref().unwrap_or(&unit.spec);
    let facts = spec.pointer(ARGOCD).ok_or_else(|| {
        "the unit records no Argo CD facts: it was resolved before it observed Argo CD; reconcile it so it records them".to_owned()
    })?;
    serde_json::from_value(facts.clone()).map_err(|error| format!("spec.argocd: {error}"))
}

/// What a reading may take: until `deadline`, at most [`READING`], at
/// least one second.
fn budget(
    now: nyl_core::orchestration::time::Timestamp,
    deadline: nyl_core::orchestration::time::Timestamp,
) -> Duration {
    (deadline - now)
        .to_std()
        .unwrap_or_default()
        .clamp(Duration::from_secs(1), READING)
}

/// Every namespace the target's Applications live in, with the Applications
/// each holds, as `reader` reports them.
fn read_all(
    reader: &dyn ArgoReader,
    observation: &ArgoCdObservation,
    only_workload: bool,
) -> Result<std::collections::BTreeMap<(String, String), Value>, ReadError> {
    let applications = observation.applications.iter().filter(|application| {
        !only_workload || application.role == nyl_render::gitops::observation::ApplicationRole::Workload
    });
    let namespaces = applications
        .clone()
        .map(|application| application.namespace.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    let mut live = std::collections::BTreeMap::new();
    for namespace in namespaces {
        for (name, application) in reader.applications(namespace)? {
            live.insert((namespace.to_owned(), name), application);
        }
    }
    Ok(live)
}

/// `nyl attest`: `accepted` and `healthy` of the receipt's publication.
pub(super) fn attest(ctx: &ExecutionContext<'_>, unit: &DesiredUnit) -> DriverResult<AttestationCheck> {
    let unknown = |reason: String| Ok(AttestationCheck::Unknown { reason });
    let observation = match observation_of(unit) {
        Ok(observation) => observation,
        Err(reason) => return unknown(reason),
    };
    let Some(commit) = ctx
        .artifacts
        .get("tree")
        .and_then(|tree| tree.spec.get("commit"))
        .and_then(Value::as_str)
    else {
        return unknown(
            "the receipt records no PublishedTree artifact, so there is no publication to attest".to_owned(),
        );
    };
    let reader = match reader(&observation, &ctx.env, budget(ctx.now, ctx.deadline)) {
        Ok(reader) => reader,
        Err(error) => return unknown(error.message),
    };
    let live = match read_all(reader.as_ref(), &observation, false) {
        Ok(live) => live,
        Err(error) => return unknown(error.message),
    };
    let variable = |name: &str| ctx.env.get(name).cloned();
    let history = match GitPublicationHistory::fetch(&observation.publication, &ctx.project_dir, &variable) {
        Ok(history) => history,
        Err(error) => {
            return unknown(format!(
                "cannot read the publication branch to match what Argo CD runs: {error}"
            ))
        }
    };
    match evaluate(&observation, commit, &live, &history) {
        Ok(evaluation) => Ok(AttestationCheck::Attested {
            outcomes: evaluation.outcomes(&observation),
        }),
        Err(error) => unknown(format!(
            "cannot match what Argo CD runs to the publication branch: {error}"
        )),
    }
}

/// The workload Applications Argo CD still has, for the `observe` teardown wait.
pub(super) fn observe_removal(ctx: &RemovalContext, unit: &DesiredUnit) -> RemovalObservation {
    let observation = match observation_of(unit) {
        Ok(observation) => observation,
        Err(reason) => return RemovalObservation::Failed(reason),
    };
    let reader = match reader(&observation, &ctx.env, budget(ctx.now, ctx.deadline)) {
        Ok(reader) => reader,
        Err(error) => return RemovalObservation::Failed(error.message),
    };
    let live = match read_all(reader.as_ref(), &observation, true) {
        Ok(live) => live,
        Err(error) => return RemovalObservation::Failed(error.message),
    };
    RemovalObservation::Remaining(
        observation
            .workload_applications()
            .filter(|application| live.contains_key(&(application.namespace.clone(), application.name.clone())))
            .map(|application| {
                if application.namespace == observation.location.namespace {
                    application.name.clone()
                } else {
                    format!("{}/{}", application.namespace, application.name)
                }
            })
            .collect(),
    )
}
