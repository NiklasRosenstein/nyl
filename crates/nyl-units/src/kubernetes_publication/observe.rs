//! The publication driver's observation of the reconciler: `nyl attest` and
//! the `observe` teardown wait.
//!
//! Contract: [Kubernetes publications](../../../../design/promotion.md#kubernetes-publications)
//! and the [Kubernetes publication unit](../../../../design/orchestration-core.md#kubernetes-publication-unit)
//! (`observe`, Teardown).
//!
//! A unit that observes (`observe: {}`, which also allows the `observe`
//! `teardownWait` strategy) records [`ReconcilerFacts`] under `/reconciler` of
//! its resolved spec when it is resolved: which reconciler runs its target
//! and where, the publication destination, the deployments the target expects
//! and the anchors of a trustworthy reading. The flag says nothing about
//! which reconciler that is: it follows the reconciler of the unit's target.
//! Both operations then read the stored document, so neither renders, and a
//! unit torn down from its stored document observes what it recorded.
//!
//! Neither operation knows a reconciler. Each builds the reader the recorded
//! location asks for ([`reconciler::reader`]), reads, and gives the neutral
//! reading to the shared rules ([`reconciler::evaluate`]):
//!
//! - [`attest`] reads every expected deployment, fetches the publication
//!   branch, and judges the reading against the receipt's publication commit
//!   ([`evaluate`]). It records nothing it could not establish: an
//!   unreachable or unauthorized reconciler, a reading that does not prove
//!   what its absences mean, an unreadable branch is
//!   [`AttestationCheck::Unknown`], never a failing attestation, so a stale
//!   `pass` is never replaced by a `fail` that only says "could not look".
//! - [`observe_removal`] reports which workload deployments the reconciler
//!   still has, for the wait that ends when none remains ([`removal`]).

use std::time::Duration;

use nyl_core::orchestration::driver::{
    AttestationCheck, DriverError, DriverResult, ExecutionContext, RemovalContext, RemovalObservation,
};
use nyl_core::orchestration::state::DesiredUnit;
use nyl_render::gitops::observation::{GitPublicationHistory, ReconcilerFacts};
use nyl_render::gitops::publication::RenderedTarget;
use serde_json::Value;

use super::reconciler::evaluate::{ensure_provable, evaluate, removal, Conclusion, EvaluateError};
use super::reconciler::{reader, ReadRequest};

/// The pointer, in the resolved spec, of the recorded reconciler facts.
pub(super) const RECONCILER: &str = "/reconciler";

/// The recorded facts that do not key the unit: they say how the reconciler
/// and the publication branch are read, never what the unit produces, so
/// renaming a kube context, moving the API server, or changing the Cluster's
/// access never re-executes the unit. The stored document still carries
/// them. Only the deployments and anchors the render derives, with the
/// reconciler's identity and the publication's branch and prefix, key it.
pub(super) const UNKEYED: [&str; 3] = [
    "/reconciler/access",
    "/reconciler/context",
    "/reconciler/publication/url",
];

/// The longest one reading of the reconciler takes.
const READING: Duration = Duration::from_secs(30);

/// Whether a spec observes the reconciler: it declares `observe`.
pub(super) fn observes(spec: &Value) -> bool {
    spec.get("observe").is_some()
}

/// The facts a unit records when it observes: the JSON of
/// [`ReconcilerFacts`] for `rendered`.
pub(super) fn recorded(rendered: &RenderedTarget) -> DriverResult<Value> {
    let facts = ReconcilerFacts::of(rendered).map_err(|error| {
        DriverError::before_effects(format!("cannot read the reconciler facts of the target: {error}"))
    })?;
    serde_json::to_value(facts)
        .map_err(|error| DriverError::before_effects(format!("cannot record the reconciler facts: {error}")))
}

pub(super) fn facts_of(unit: &DesiredUnit) -> Result<ReconcilerFacts, String> {
    let spec = unit.resolved_spec.as_ref().unwrap_or(&unit.spec);
    let facts = spec.pointer(RECONCILER).ok_or_else(|| {
        "the unit records no reconciler facts: it was resolved before it observed the reconciler; reconcile it so it records them".to_owned()
    })?;
    serde_json::from_value(facts.clone()).map_err(|error| format!("spec.reconciler: {error}"))
}

/// What a reading may take: until `deadline`, at most [`READING`], at
/// least one second.
pub(super) fn budget(
    now: nyl_core::orchestration::time::Timestamp,
    deadline: nyl_core::orchestration::time::Timestamp,
) -> Duration {
    (deadline - now)
        .to_std()
        .unwrap_or_default()
        .clamp(Duration::from_secs(1), READING)
}

/// `nyl attest`: `accepted` and `healthy` of the receipt's publication.
pub(super) fn attest(ctx: &ExecutionContext<'_>, unit: &DesiredUnit) -> DriverResult<AttestationCheck> {
    let unknown = |reason: String| Ok(AttestationCheck::Unknown { reason });
    let facts = match facts_of(unit) {
        Ok(facts) => facts,
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
    let reader = match reader(&facts.location, &ctx.env, budget(ctx.now, ctx.deadline)) {
        Ok(reader) => reader,
        Err(error) => return unknown(error.message),
    };
    let reading = match reader.read(&ReadRequest {
        deployments: &facts.expected.deployments,
        anchors: &facts.expected.anchors,
        concluding_absence: false,
    }) {
        Ok(reading) => reading,
        Err(error) => return unknown(error.message),
    };
    // Before the branch is fetched: a reading that proves nothing about its
    // absences is unknown whatever the branch holds.
    if let Err(error) = ensure_provable(
        &facts.expected,
        facts.expected.deployments.iter(),
        &reading,
        Conclusion::Presence,
    ) {
        return unknown(error.message);
    }
    let variable = |name: &str| ctx.env.get(name).cloned();
    let history = match GitPublicationHistory::fetch(&facts.expected.publication, &ctx.project_dir, &variable) {
        Ok(history) => history,
        Err(error) => {
            return unknown(format!(
                "cannot read the publication branch to match what the reconciler runs: {error}"
            ))
        }
    };
    match evaluate(&facts.expected, commit, &reading, &history) {
        Ok(evaluation) => Ok(AttestationCheck::Attested {
            outcomes: evaluation.outcomes(&facts.location.summary()),
        }),
        Err(EvaluateError::Unprovable(error)) => unknown(error.message),
        Err(EvaluateError::History(error)) => unknown(format!(
            "cannot match what the reconciler runs to the publication branch: {error}"
        )),
    }
}

/// The workload deployments the reconciler still has, for the `observe`
/// teardown wait.
pub(super) fn observe_removal(ctx: &RemovalContext, unit: &DesiredUnit) -> RemovalObservation {
    let facts = match facts_of(unit) {
        Ok(facts) => facts,
        Err(reason) => return RemovalObservation::Failed(reason),
    };
    let reader = match reader(&facts.location, &ctx.env, budget(ctx.now, ctx.deadline)) {
        Ok(reader) => reader,
        Err(error) => return RemovalObservation::Failed(error.message),
    };
    // Only the workload: the catalog does not count, and a deployment the
    // wait does not conclude from is not asked about.
    let workload = facts.expected.workload().cloned().collect::<Vec<_>>();
    let reading = match reader.read(&ReadRequest {
        deployments: &workload,
        anchors: &facts.expected.anchors,
        concluding_absence: true,
    }) {
        Ok(reading) => reading,
        Err(error) => return RemovalObservation::Failed(error.message),
    };
    match removal(&facts.expected, &reading) {
        Ok(remaining) => RemovalObservation::Remaining(remaining),
        Err(error) => RemovalObservation::Failed(error.message),
    }
}
