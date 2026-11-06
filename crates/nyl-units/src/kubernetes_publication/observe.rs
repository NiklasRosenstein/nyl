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

/// The longest one reading of the reconciler takes.
const READING: Duration = Duration::from_secs(30);

/// Whether a spec observes the reconciler: it declares `observe`.
pub(super) fn observes(spec: &Value) -> bool {
    spec.get("observe").is_some()
}

/// The facts a unit records when it observes: the JSON of
/// [`ReconcilerFacts`] for `rendered`, and the recorded fields that do not key
/// the unit. They say how the reconciler and the publication branch are read,
/// never what the unit produces, so renaming a kube context, moving the API
/// server, or changing the Cluster's access never re-executes the unit; the
/// stored document still carries them. Which fields those are is the
/// reconciler kind's ([`ReconcilerFacts::unkeyed`]); only the deployments and
/// anchors the render derives, with the reconciler's identity and the
/// publication's branch and prefix, key the unit.
pub(super) fn recorded(rendered: &RenderedTarget) -> DriverResult<(Value, Vec<String>)> {
    let facts = ReconcilerFacts::of(rendered).map_err(|error| {
        DriverError::before_effects(format!("cannot read the reconciler facts of the target: {error}"))
    })?;
    let unkeyed = facts.unkeyed(RECONCILER);
    let facts = serde_json::to_value(facts)
        .map_err(|error| DriverError::before_effects(format!("cannot record the reconciler facts: {error}")))?;
    Ok((facts, unkeyed))
}

pub(super) fn observation_of(unit: &DesiredUnit) -> Result<ArgoCdObservation, String> {
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

/// Every namespace the target's Applications live in, with the Applications
/// each holds, as `reader` reports them.
pub(super) fn read_all(
    reader: &dyn ArgoReader,
    observation: &ArgoCdObservation,
) -> Result<std::collections::BTreeMap<(String, String), Value>, ReadError> {
    let namespaces = observation
        .applications
        .iter()
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

/// Fail unless every one of `applications` that the reading lacks is one the
/// credential could have seen. A listing leaves out what the credential may
/// not get and still succeeds, so an absence proves nothing until the reader
/// says the credential may get the Application (`can-i` for the API server).
pub(super) fn ensure_gone_is_visible<'a>(
    reader: &dyn ArgoReader,
    observation: &ArgoCdObservation,
    live: &std::collections::BTreeMap<(String, String), Value>,
    applications: impl Iterator<Item = &'a GeneratedApplication>,
) -> Result<(), ReadError> {
    for application in applications {
        if live.contains_key(&(application.namespace.clone(), application.name.clone())) {
            continue;
        }
        if application.project.is_empty() {
            return Err(ReadError::new(
                ReadErrorKind::Declaration,
                "the unit records no Argo CD project for its Applications: it was resolved before it did; reconcile it so it records them".to_owned(),
            ));
        }
        let rbac = rbac_name(
            &application.project,
            &application.namespace,
            &application.name,
            &observation.location.namespace,
        );
        if !reader.can_get(&rbac)? {
            return Err(ReadError::new(
                ReadErrorKind::Forbidden,
                format!(
                    "the credential may not get Application {rbac}, so its absence from the reading proves nothing \
                     (Argo CD leaves out what a token may not get and still answers success): grant its account or \
                     project role `get` on {rbac}"
                ),
            ));
        }
    }
    Ok(())
}

/// Fail unless the reading shows something phase 1 of a teardown keeps and
/// that only the Argo CD running this target has, so that a credential or
/// context for another Argo CD (an empty list that is a success) is never
/// mistaken for a target whose Applications are gone:
///
/// - the catalog Application, when the target generates one, must be in the
///   reading;
/// - without one (`catalogApplication.shared`, or `enabled: false`), the
///   AppProjects the target's Applications belong to must be visible: phase 1
///   keeps the generated ones, and an externally owned one is a project the
///   target depends on. The `default` project exists in every Argo CD and
///   anchors nothing;
/// - with neither, the Kubernetes method fails closed (a context cannot be
///   shown to reach this target), and the API method relies on `can-i` alone.
///
/// A target with no workload Application has nothing to conclude and needs no
/// anchor.
pub(super) fn ensure_anchored(
    reader: &dyn ArgoReader,
    observation: &ArgoCdObservation,
    live: &std::collections::BTreeMap<(String, String), Value>,
) -> Result<(), ReadError> {
    if observation.workload_applications().next().is_none() {
        return Ok(());
    }
    if let Some(catalog) = observation
        .applications
        .iter()
        .find(|application| application.role == ApplicationRole::Catalog)
    {
        if live.contains_key(&(catalog.namespace.clone(), catalog.name.clone())) {
            return Ok(());
        }
        return Err(ReadError::new(
            ReadErrorKind::Forbidden,
            format!(
                "the catalog Application {}/{} is not in the reading, so the credential or context does not see this \
                 target's Applications and their absence would prove nothing (if the catalog was never applied, \
                 apply it first): check that the ArgoCDInstance's access reaches the Argo CD that runs this target \
                 and may read its Applications",
                catalog.namespace, catalog.name
            ),
        ));
    }
    let projects = observation
        .applications
        .iter()
        .map(|application| application.project.as_str())
        .filter(|project| !project.is_empty() && *project != "default")
        .collect::<std::collections::BTreeSet<_>>();
    for project in &projects {
        if !reader.project_exists(&observation.location.namespace, project)? {
            return Err(ReadError::new(
                ReadErrorKind::Forbidden,
                format!(
                    "the target generates no catalog Application, and its AppProject {project} is not visible in \
                     namespace {}, so the credential or context does not see this target's Argo CD and the absence of \
                     its Applications would prove nothing: check that the ArgoCDInstance's access reaches the Argo CD \
                     that runs this target and may read its projects",
                    observation.location.namespace
                ),
            ));
        }
    }
    if projects.is_empty() && observation.location.access.method == ArgoCDAccessMethod::Kubernetes {
        return Err(ReadError::new(
            ReadErrorKind::Declaration,
            "the target generates no catalog Application and no AppProject other than `default`, so nothing shows that \
             the Kubernetes context reaches the Argo CD that runs it; Nyl will not conclude anything from an empty list: \
             declare the ArgoCDInstance's access with method api, which asks Argo CD whether the token may get each \
             Application"
                .to_owned(),
        ));
    }
    Ok(())
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
