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
use nyl_render::gitops::observation::{
    ApplicationRole, ArgoCdObservation, GeneratedApplication, GitPublicationHistory,
};
use nyl_render::gitops::publication::RenderedTarget;
use nyl_render::resources::ArgoCDAccessMethod;
use serde_json::Value;

use super::argocd::evaluate::evaluate;
use super::argocd::{rbac_name, reader, ArgoReader, ReadError, ReadErrorKind};

/// The pointer, in the resolved spec, of the recorded observation facts.
pub(super) const ARGOCD: &str = "/argocd";

/// The recorded facts that do not key the unit: they say how Argo CD and the
/// publication branch are read, never what the unit produces, so renaming a
/// kube context, moving the API server, or changing the Cluster's access never
/// re-executes the unit. The stored document still carries them.
pub(super) const UNKEYED: [&str; 3] = ["/argocd/access", "/argocd/context", "/argocd/publication/url"];

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
fn ensure_gone_is_visible<'a>(
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
fn ensure_anchored(
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
    let live = match read_all(reader.as_ref(), &observation).and_then(|live| {
        // The Kubernetes method has no per-Application `can-i`, so a context
        // for another Argo CD is told apart by what only this target's Argo CD
        // has. The API method asks Argo CD about each missing Application.
        if observation.location.access.method == ArgoCDAccessMethod::Kubernetes {
            ensure_anchored(reader.as_ref(), &observation, &live)?;
        }
        ensure_gone_is_visible(reader.as_ref(), &observation, &live, observation.applications.iter())?;
        Ok(live)
    }) {
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
    let live = match read_all(reader.as_ref(), &observation).and_then(|live| {
        ensure_anchored(reader.as_ref(), &observation, &live)?;
        ensure_gone_is_visible(
            reader.as_ref(),
            &observation,
            &live,
            observation.workload_applications(),
        )?;
        Ok(live)
    }) {
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
