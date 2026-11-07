//! Rendering, publishing, and tearing down one target's tree.
//!
//! Contract: [Kubernetes publication unit](../../../../design/orchestration-core.md#kubernetes-publication-unit),
//! including Teardown. Every call goes through `nyl_render::gitops::publication`.
//!
//! Teardown runs the contract's two phases through Git, each a
//! compare-and-swap publication of the target's prefix:
//!
//! 1. Without a wait completion and without a recorded wait, publish the
//!    tree without its workload Applications
//!    (`without_workload_applications`) and report `Outcome::Waiting` with
//!    the phase 1 commit; with a recorded wait, report the same wait again
//!    and publish nothing.
//! 2. Once the run passes the wait's completion
//!    (`ExecutionContext::teardown_wait`), remove exactly the files the
//!    ownership index records (`prepare_target_removal_fenced`).
//!
//! When the wait ends is the orchestration shell's decision
//! (`rules::teardown_wait`), read from `teardownWait`.
//!
//! **Owner fencing.** Every tree this module renders for a unit records the
//! unit incarnation in its ownership index (environment, unit, uid). A publication or removal over a prefix whose index names another
//! incarnation takes it over only when the incarnation directory
//! (`ExecutionContext::incarnations`) reports it gone from its environment's
//! state, and otherwise fails with `NYL-KUBERNETES-OWNER-FENCED`
//! ([`fence`]); a prefix whose index names no owner (version 2, or version 3 as
//! `publish-tree` writes it) is taken over and upgraded.
//!
//! A publication that loses the race for the branch head, before or at its
//! push, built on a head that moved, so nothing was published. A tree that
//! read no state is built again as it is. A tree that read `fromPublication`
//! state is rendered again on the new head: when the state it read is
//! unchanged, it publishes there; when the state changed, the call ends
//! without publishing as a retryable failure (`NYL-KUBERNETES-STATE-CHANGED`),
//! and the next run resolves the unit again with the new state. Each retry is
//! bounded in number and by the unit's timeout. Any other failure ends the
//! call: one before the push is a failure before effects, and a failed push
//! may have published.

use nyl_core::constants::API_VERSION_ARTIFACTS;
use nyl_core::orchestration::driver::{
    Change, DriverError, DriverResult, ExecutionContext, IncarnationStatus, KeyInputs, Outcome, PlanReport,
    ResolutionContext,
};
use nyl_core::orchestration::ids::{CommitId, EnvironmentName, Uid, UnitName};
use nyl_core::orchestration::ports::CommitIdentity;
use nyl_core::orchestration::state::{ArtifactDocument, ArtifactMetadata, ArtifactUnit, DesiredUnit, FailureCategory};
use nyl_core::orchestration::time::Timestamp;
use nyl_render::gitops::inputs::InputSnapshot;
use nyl_render::gitops::observation::{direct_deployments, Delivery, ReconcilerLocation};
use nyl_render::gitops::publication::{
    inspect_shared_catalog, prepare_target_publication_fenced, prepare_target_removal_fenced, render_target,
    without_workload_applications, without_workloads, PublicationAuthor, RenderedTarget, SharedCatalogState,
    TargetRender,
};
use nyl_render::gitops::reconcile::RenderIndexOwner;
use nyl_render::gitops::units::PublicationTarget;
use serde_json::{json, Value};

use super::direct::apply::{Direct, Step};
use super::direct::ClusterAccess;
use super::health_wait::{self, Waited};
use crate::command::{is_deleting, resolved_spec};

/// The phase a teardown waits in after publishing phase 1.
pub(crate) const PHASE_APPLICATIONS_REMOVED: &str = "applications-removed";

/// The phase a direct target's teardown waits in after phase 1.
pub(crate) const PHASE_RESOURCES_DELETED: &str = "resources-deleted";

/// The recorded reconciler kind of a stored unit: what it was resolved for, which
/// a teardown follows rather than the declaration as it reads now.
fn recorded_kind(unit: &DesiredUnit) -> Option<&str> {
    unit.resolved_spec
        .as_ref()
        .unwrap_or(&unit.spec)
        .pointer("/reconciler/kind")
        .and_then(Value::as_str)
}

/// The refusal of a teardown whose declaration now renders another reconciler
/// than the one the unit recorded.
fn reconciler_mismatch(unit: &DesiredUnit, rendered: &RenderedTarget) -> Option<Outcome> {
    let recorded = recorded_kind(unit).unwrap_or("argocd");
    (recorded != rendered.delivery.kind()).then(|| Outcome::Failed {
        category: FailureCategory::Ownership,
        retryable: true,
        message: teardown_reconciler_changed(recorded, rendered.delivery.kind()),
    })
}

/// The cluster side of a direct unit's teardown, when the unit recorded one.
fn direct_of<'a, 'c>(
    ctx: &'a ExecutionContext<'c>,
    unit: &DesiredUnit,
    rendered: &RenderedTarget,
    access: &'a ClusterAccess,
) -> DriverResult<Option<Direct<'a, 'c>>> {
    let Delivery::Direct(declared) = &rendered.delivery else {
        return Ok(None);
    };
    // A teardown acts on what the stored document recorded, never on the
    // declaration as it reads now: the recorded Cluster, context, and server
    // decide where it deletes. A document without them (resolved before a
    // direct unit recorded its facts) falls back to the declaration.
    let recorded = super::observe::facts_of(unit)
        .ok()
        .and_then(|facts| match facts.location {
            ReconcilerLocation::Direct(location) => Some(location),
            ReconcilerLocation::ArgoCd(_) => None,
        });
    let owner = unit_owner(unit)?;
    Ok(Some(Direct::new(
        ctx,
        access,
        recorded.as_ref().unwrap_or(declared),
        owner,
        rendered,
    )))
}

/// How often a publication is built again on a branch head that moved.
const PUBLISH_ATTEMPTS: usize = 3;

/// The DeploymentTarget a resolved spec names or declares inline.
pub(crate) fn publication_target(spec: &Value) -> DriverResult<PublicationTarget> {
    PublicationTarget::from_spec(spec).map_err(DriverError::before_effects)
}

/// The name of the DeploymentTarget a resolved spec names or declares inline.
pub(crate) fn unit_target(spec: &Value) -> DriverResult<String> {
    publication_target(spec).map(|target| target.name().to_owned())
}

/// The incarnation a unit's published trees record as their owner.
pub(crate) fn unit_owner(unit: &DesiredUnit) -> DriverResult<RenderIndexOwner> {
    let uid = unit
        .unit
        .uid
        .as_ref()
        .ok_or_else(|| DriverError::before_effects("an executing unit has a uid"))?;
    Ok(RenderIndexOwner::unit(
        unit.unit.environment.to_string(),
        unit.unit.name.to_string(),
        uid.to_string(),
    ))
}

/// Owner fencing: whether `own` may publish to, or remove, a prefix whose
/// ownership index names `owner`. `Ok` once that incarnation no longer
/// exists in its environment's state; otherwise why the prefix stays fenced,
/// with its stable code and the fix: `NYL-KUBERNETES-OWNER-FENCED` while the
/// owner exists, and `NYL-KUBERNETES-OWNER-UNKNOWN`, which fails closed, when
/// state cannot say, such as for an environment that was renamed or removed.
pub(crate) fn fence<'a>(
    ctx: &'a ExecutionContext<'_>,
) -> impl Fn(&RenderIndexOwner) -> std::result::Result<(), String> + 'a {
    move |owner| {
        let RenderIndexOwner::Unit { environment, unit, uid } = owner else {
            // A shared catalog is no unit incarnation and never ceases to exist
            // by itself: its prefix is released only by removing the files or
            // the index's `owner` member by hand.
            return Err(format!(
                "NYL-KUBERNETES-OWNER-FENCED: the prefix is owned by {owner}, which no unit's teardown releases; a target's \
                 publication prefix must not be the shared catalog's (_nyl/shared/<name>/). {}",
                nyl_render::gitops::reconcile::OWNED_PREFIX_FIX
            ));
        };
        let (Ok(environment), Ok(unit), Ok(uid)) =
            (EnvironmentName::new(environment), UnitName::new(unit), Uid::new(uid))
        else {
            return Err(format!(
                "the ownership index names {owner}, which is not a valid unit identity"
            ));
        };
        match ctx.incarnations.status(&environment, &unit, &uid) {
            IncarnationStatus::Gone => Ok(()),
            IncarnationStatus::Exists => Err(format!(
                "NYL-KUBERNETES-OWNER-FENCED: the prefix is owned by {owner}, which still exists in the state of environment \
                 {environment}; it stays fenced until that unit's teardown completes or it is released with `nyl state forget`"
            )),
            IncarnationStatus::Unknown(reason) => Err(format!(
                "NYL-KUBERNETES-OWNER-UNKNOWN: the prefix is owned by {owner}, and whether it still exists cannot be checked: \
                 {reason}. If the environment was renamed or removed, declare it again and tear the unit down, or {}",
                nyl_render::gitops::reconcile::OWNED_PREFIX_FIX
            )),
        }
    }
}

/// The resolved `spec.releases` as the pinned input snapshot of the render.
pub(crate) fn resolved_snapshot(spec: &Value) -> DriverResult<InputSnapshot> {
    let Some(releases) = spec.get("releases") else {
        return Ok(InputSnapshot::new());
    };
    serde_json::from_value::<InputSnapshot>(releases.clone())
        .map_err(|error| DriverError::before_effects(format!("spec.releases: {error}")))
}

/// Render the target `spec` names or declares inline at `worktree`, with its
/// resolved Release inputs as a pinned snapshot. The tree's ownership index
/// records `owner`; resolution renders without one, because it only reads
/// the input record and publishes nothing.
pub(crate) fn render(
    catalogue: &nyl_render::config::ResourceCatalogue,
    worktree: &std::path::Path,
    project_dir: &std::path::Path,
    cache_dir: &std::path::Path,
    instances: &nyl_core::orchestration::instance::Instances,
    spec: &Value,
    owner: Option<RenderIndexOwner>,
) -> DriverResult<RenderedTarget> {
    render_at(
        catalogue,
        worktree,
        project_dir,
        cache_dir,
        instances,
        spec,
        owner,
        None,
    )
}

/// [`render`] with the commit the ownership index records as the tree's
/// source, for a worktree that is a snapshot of files, not a checkout of
/// it. `None` reads the worktree repository's head.
#[allow(clippy::too_many_arguments)] // The render's inputs are independent facts of the call; a struct would only rename them.
pub(crate) fn render_at(
    catalogue: &nyl_render::config::ResourceCatalogue,
    worktree: &std::path::Path,
    project_dir: &std::path::Path,
    cache_dir: &std::path::Path,
    instances: &nyl_core::orchestration::instance::Instances,
    spec: &Value,
    owner: Option<RenderIndexOwner>,
    source_commit: Option<&str>,
) -> DriverResult<RenderedTarget> {
    let target = publication_target(spec)?;
    let inputs = resolved_snapshot(spec)?;
    let inline = match &target {
        PublicationTarget::Inline(inline) => Some(inline),
        PublicationTarget::Static(_) => None,
    };
    let target = target.name().to_owned();
    render_target(&TargetRender {
        source_commit,
        selection: nyl_render::gitops::tree::partial::PublicationSelection::default(),
        publication_base: None,
        worktree,
        project_dir,
        target: &target,
        inline,
        owner,
        inputs: &inputs,
        cache_dir: Some(cache_dir),
        resource_catalogue: catalogue.clone(),
        instances,
    })
    .map_err(|error| DriverError::before_effects(format!("cannot render DeploymentTarget {target:?}: {error}")))
}

/// Render the target at resolution and read the execution key's inputs from
/// the render: its portable input record, which covers every file,
/// directory listing, value, chart, Kubernetes capability, resolved source,
/// and tool that can affect rendered bytes. A render whose record does not
/// cover it executes every time, as the contract requires of an
/// uncacheable render: its key carries the resolution time, which no later
/// resolution repeats.
pub(crate) fn resolution_inputs(
    catalogue: &nyl_render::config::ResourceCatalogue,
    ctx: &ResolutionContext<'_>,
    spec: &Value,
) -> DriverResult<(RenderedTarget, KeyInputs)> {
    let rendered = render(
        catalogue,
        &ctx.worktree,
        &ctx.project_dir,
        &ctx.cache_dir,
        ctx.instances,
        spec,
        None,
    )?;
    let inputs = KeyInputs {
        files: rendered.input_record.clone(),
        fingerprint: uncacheable_fingerprint(rendered.cacheable, ctx.now),
        ..KeyInputs::default()
    };
    Ok((rendered, inputs))
}

/// A render whose input record does not cover it has no stable key: the
/// fingerprint names the resolution time, which no later resolution repeats.
fn uncacheable_fingerprint(cacheable: bool, now: Timestamp) -> Option<String> {
    (!cacheable).then(|| format!("uncacheable render resolved at {}", now.to_rfc3339()))
}

/// The commit author a driver call that writes needs, or a failure before
/// effects without one.
fn author(ctx: &ExecutionContext<'_>, unit: &DesiredUnit) -> DriverResult<PublicationAuthor> {
    let identity: &CommitIdentity = ctx.commit_identity.ok_or_else(|| {
        DriverError::before_effects(format!(
            "unit {} publishes a commit and needs a commit identity",
            unit.unit.name
        ))
    })?;
    Ok(PublicationAuthor {
        identity: identity.clone(),
        time: ctx.now,
    })
}

/// What rebuilding a tree on a branch head that moved found.
enum Rebuilt {
    /// The tree is ready to be prepared again.
    Ready,
    /// The state the tree was rendered from changed; nothing may be published from it.
    StateChanged(String),
}

/// How a call that builds again ended.
enum Attempted<R> {
    Done(R),
    StateChanged(String),
    /// The prefix is owned by another unit incarnation that still exists.
    Fenced(String),
}

/// The time a call may spend, from the context's timeout, measured from its start.
struct Budget {
    started: std::time::Instant,
    allowed: std::time::Duration,
}

impl Budget {
    fn new(ctx: &ExecutionContext<'_>) -> Self {
        Self {
            started: std::time::Instant::now(),
            allowed: (ctx.deadline - ctx.now).to_std().unwrap_or_default(),
        }
    }

    fn spent(&self) -> bool {
        self.started.elapsed() >= self.allowed
    }
}

/// Prepare `tree` and push it, building again when the branch head moved:
/// `rebuild` brings `tree` up to date with the new head, or reports that the
/// state it was rendered from changed. `prepared` and `pushed` complete
/// "cannot prepare ..." and "cannot ..." in the failure messages.
fn with_retries<T, P, R>(
    budget: &Budget,
    tree: &mut T,
    (prepared, pushed): (&str, &str),
    prepare: impl Fn(&T) -> nyl_render::Result<P>,
    push: impl Fn(P) -> nyl_render::Result<R>,
    rebuild: impl Fn(&mut T) -> DriverResult<Rebuilt>,
) -> DriverResult<Attempted<R>> {
    let mut attempts = 0;
    loop {
        attempts += 1;
        let more = attempts < PUBLISH_ATTEMPTS;
        let lost = match prepare(tree) {
            Ok(ready) => match push(ready) {
                Ok(result) => return Ok(Attempted::Done(result)),
                Err(error) if error.is_stale_publication() => error,
                Err(error) => return Err(DriverError::after_effects(format!("cannot {pushed}: {error}"))),
            },
            Err(error) if error.is_stale_publication() => error,
            Err(error) if error.is_owner_fenced() => return Ok(Attempted::Fenced(error.to_string())),
            Err(error) => {
                return Err(DriverError::before_effects(format!(
                    "cannot prepare {prepared}: {error}"
                )))
            }
        };
        // The branch head moved, so nothing was published.
        if !more {
            return Err(DriverError::before_effects(format!(
                "cannot {pushed}: the publication branch moved {PUBLISH_ATTEMPTS} times while publishing: {lost}"
            )));
        }
        if budget.spent() {
            return Err(DriverError::before_effects(format!(
                "cannot {pushed}: the unit's timeout passed while the publication branch kept moving: {lost}"
            )));
        }
        match rebuild(tree)? {
            Rebuilt::Ready => {}
            Rebuilt::StateChanged(message) => return Ok(Attempted::StateChanged(message)),
        }
    }
}

/// Render the target again on the branch head that moved and compare the
/// state it reads with `before`; `derive` makes the tree that is published
/// from the new render.
fn rebuild_from_state<'a>(
    render: &'a dyn Fn() -> DriverResult<RenderedTarget>,
    before: &'a std::collections::BTreeMap<String, String>,
    stateful: bool,
    derive: impl Fn(RenderedTarget) -> DriverResult<RenderedTarget> + 'a,
) -> impl Fn(&mut RenderedTarget) -> DriverResult<Rebuilt> + 'a {
    move |tree| {
        if !stateful {
            // Nothing the tree read lives on the branch: the same tree is built on the new head.
            return Ok(Rebuilt::Ready);
        }
        let fresh = render()?;
        let after = state_inputs(&fresh)?;
        if &after != before {
            let changed = before
                .keys()
                .chain(after.keys())
                .filter(|key| before.get(*key) != after.get(*key))
                .cloned()
                .collect::<std::collections::BTreeSet<_>>();
            return Ok(Rebuilt::StateChanged(format!(
                "the state the tree was rendered from changed on the publication branch ({})",
                changed.into_iter().collect::<Vec<_>>().join(", ")
            )));
        }
        *tree = derive(fresh)?;
        Ok(Rebuilt::Ready)
    }
}

fn state_inputs(rendered: &RenderedTarget) -> DriverResult<std::collections::BTreeMap<String, String>> {
    rendered
        .state_inputs()
        .map_err(|error| DriverError::before_effects(format!("cannot read the inputs of the render: {error}")))
}

/// The outcome of a call that ended without publishing because the state its
/// render read changed.
fn state_changed(message: &str) -> Outcome {
    Outcome::Failed {
        category: FailureCategory::ToolError,
        retryable: true,
        message: format!(
            "NYL-KUBERNETES-STATE-CHANGED: {message}; nothing was published, and the next run resolves the unit again with the new state"
        ),
    }
}

/// The outcome of a call that ended without publishing or removing anything
/// because another unit incarnation still owns the prefix. Nothing was
/// written, and the next run reads state again, so it retries on its own once
/// the other incarnation is gone.
fn fenced(message: &str) -> Outcome {
    Outcome::Failed {
        category: FailureCategory::Ownership,
        retryable: true,
        message: if message.starts_with("NYL-") {
            message.to_owned()
        } else {
            format!("NYL-KUBERNETES-OWNER-FENCED: {message}")
        },
    }
}

/// The destination the unit's current receipt published to, compared with
/// the one its spec renders now. A unit publishes to one destination: its
/// target's name and publication repository, branch, and prefix. Nothing
/// in the engine replaces a unit when the destination changes, so the driver
/// fails closed before any effect, instead of publishing a second tree under
/// the new destination and leaving the old one, with its catalog, behind (or
/// failing in the ownership index). Without a receipt, or one that predates
/// the recorded `target`, only what it recorded is compared.
fn target_changed(
    ctx: &ExecutionContext<'_>,
    unit: &DesiredUnit,
    rendered: &RenderedTarget,
    target: &str,
) -> Option<Outcome> {
    let published = ctx.artifacts.get("tree")?;
    let recorded = |field: &str| published.spec.get(field).and_then(Value::as_str);
    let now = &rendered.destination;
    let changes = [
        ("target", Some(target), recorded("target")),
        ("repository", Some(now.repository.as_str()), recorded("repository")),
        ("branch", Some(now.branch.as_str()), recorded("branch")),
        ("pathPrefix", Some(now.path_prefix.as_str()), recorded("pathPrefix")),
        ("cluster", Some(rendered.workload.cluster.as_str()), recorded("cluster")),
    ]
    .into_iter()
    .filter_map(|(field, now, before)| match (now, before) {
        (Some(now), Some(before)) if now != before => Some(format!("{field} {before:?} became {now:?}")),
        _ => None,
    })
    .collect::<Vec<_>>();
    if let (kind, Some(before)) = (rendered.delivery.kind(), recorded("reconciler")) {
        if kind != before {
            return Some(Outcome::Failed {
                category: FailureCategory::Ownership,
                retryable: true,
                message: reconciler_changed(unit, before, kind),
            });
        }
    }
    if changes.is_empty() {
        return None;
    }
    Some(Outcome::Failed {
        category: FailureCategory::Ownership,
        retryable: true,
        message: format!(
            "NYL-KUBERNETES-TARGET-CHANGED: the unit published to a destination that its spec no longer renders ({}); \
             nothing was published. Restore the previous target, or replace the unit: run \
             `nyl teardown -e {env} --unit {name}`, which removes the tree it published, then reconcile to publish the new target",
            changes.join(", "),
            env = unit.unit.environment,
            name = unit.unit.name,
        ),
    })
}

/// The refusal of a reconcile whose target changed reconciler.
fn reconciler_changed(unit: &DesiredUnit, before: &str, now: &str) -> String {
    format!(
        "NYL-KUBERNETES-RECONCILER-CHANGED: the unit published for the {before} reconciler and its target now declares {now}; \
         nothing was changed, because removing one reconciler's files lets it delete what the other runs. Restore the \
         declaration, or replace the unit: run `nyl teardown -e {env} --unit {name}`, which tears down what it published, \
         then reconcile",
        env = unit.unit.environment,
        name = unit.unit.name,
    )
}

/// The refusal of a teardown whose target changed reconciler. A teardown
/// cannot be the remedy for itself, so the only way out is the declaration.
fn teardown_reconciler_changed(before: &str, now: &str) -> String {
    format!(
        "NYL-KUBERNETES-RECONCILER-CHANGED: the unit published for the {before} reconciler and its target now declares {now}; \
         nothing was changed, because removing one reconciler's files lets it delete what the other runs. Restore the \
         declaration of the {before} reconciler so the teardown can run, then tear the unit down again"
    )
}

/// The warnings of Releases a teardown left alone because this unit may not remove them.
fn skipped_warnings(
    unit: &DesiredUnit,
    skipped: &[super::direct::apply::Left],
) -> Vec<nyl_core::orchestration::codes::Finding> {
    skipped
        .iter()
        .map(|left| {
            nyl_core::orchestration::codes::Finding::new(
                nyl_core::orchestration::codes::Code::KubernetesReleaseLeftBehind,
                format!(
                    "{}/{}: the teardown left {} alone: {}",
                    unit.unit.environment, unit.unit.name, left.what, left.reason
                ),
                "remove it with `nyl release delete --take-over` if it is stale, or leave it to its owner",
            )
        })
        .collect()
}

/// What `plan` reports: the destroy a deleting unit's two phases amount to,
/// or whether the published tree is current for the execution key.
pub(crate) fn plan(ctx: &ExecutionContext<'_>, unit: &DesiredUnit) -> PlanReport {
    if is_deleting(unit) {
        let summary = match ctx.observed.and_then(|observed| observed.waiting.as_ref()) {
            Some(waiting) => format!(
                "teardown waits after phase 1 ({}); then remove the files the ownership index records",
                waiting.phase1_commit.as_ref().map_or("unpublished", CommitId::as_str)
            ),
            None => "teardown in two phases: publish the catalog without workload Applications, then, once teardownWait ends, remove the files the ownership index records".to_owned(),
        };
        return PlanReport {
            changes: Change::Destroy,
            summary,
            change_digest: None,
            blocked_by: Vec::new(),
        };
    }
    let receipt = ctx.observed.and_then(|observed| observed.receipt.as_ref());
    let current = receipt.is_some_and(|receipt| Some(&receipt.execution_key) == unit.execution_key.as_ref());
    let (changes, summary) = match receipt {
        _ if current => (Change::None, "published tree is current"),
        Some(_) => (Change::Update, "publish a changed tree"),
        None => (Change::Create, "publish the tree"),
    };
    PlanReport {
        changes,
        summary: summary.to_owned(),
        change_digest: None,
        blocked_by: Vec::new(),
    }
}

/// Publish the unit's tree and record the `PublishedTree` artifact `tree`,
/// with `outputs` as the outputs the call reports.
#[cfg(any(test, feature = "test-kinds"))]
pub(crate) fn publish(
    catalogue: &nyl_render::config::ResourceCatalogue,
    ctx: &ExecutionContext<'_>,
    unit: &DesiredUnit,
    outputs: Value,
) -> DriverResult<Outcome> {
    publish_via(catalogue, ctx, unit, outputs, &ClusterAccess::Live)
}

/// [`publish`] reaching a direct target's cluster through `access`.
pub(crate) fn publish_via(
    catalogue: &nyl_render::config::ResourceCatalogue,
    ctx: &ExecutionContext<'_>,
    unit: &DesiredUnit,
    outputs: Value,
    access: &ClusterAccess,
) -> DriverResult<Outcome> {
    let spec = resolved_spec(unit)?;
    let owner = unit_owner(unit)?;
    publish_with(
        &|| {
            render(
                catalogue,
                &ctx.worktree,
                &ctx.project_dir,
                &ctx.cache_dir,
                ctx.instances,
                spec,
                Some(owner.clone()),
            )
        },
        ctx,
        unit,
        outputs,
        access,
    )
}

/// [`publish`] rendering with `render`, which a retry calls again.
fn publish_with(
    render: &dyn Fn() -> DriverResult<RenderedTarget>,
    ctx: &ExecutionContext<'_>,
    unit: &DesiredUnit,
    outputs: Value,
    access: &ClusterAccess,
) -> DriverResult<Outcome> {
    author(ctx, unit)?;
    let rendered = render()?;
    let target = unit_target(resolved_spec(unit)?)?;
    if let Some(outcome) = target_changed(ctx, unit, &rendered, &target) {
        return Ok(outcome);
    }
    let message = format!("nyl: publish {}/{}", unit.unit.environment, unit.unit.name);
    // A direct target is applied after its tree is published: the cluster is
    // checked first (connection, server, and the fence of every Release),
    // because that is cheap and undoes nothing, and the Releases are applied
    // from the bytes the push consumed, once it landed.
    let direct = match rendered.delivery.clone() {
        Delivery::Direct(location) => {
            let owner = unit_owner(unit)?;
            let direct = Direct::new(ctx, access, &location, owner, &rendered);
            let releases = direct_deployments(&rendered).map_err(|error| {
                DriverError::before_effects(format!("cannot read the Releases of the target: {error}"))
            })?;
            if let Some(outcome) = direct.preflight(&releases)? {
                return Ok(outcome);
            }
            Some(direct)
        }
        Delivery::ArgoCd(_) => None,
    };
    // After the push (and, for a direct target, the apply), a unit that asks
    // to wait polls its reconciler until it is healthy, and attests what it found.
    let wait = health_wait::timeout_of(resolved_spec(unit)?);
    publish_rendered_with(render, rendered, &message, ctx, unit, outputs, &mut |pushed, commit| {
        let mut step = match &direct {
            Some(direct) => {
                let releases = direct_deployments(pushed).map_err(|error| {
                    DriverError::after_effects(format!("cannot read the Releases of the pushed tree: {error}"))
                })?;
                direct.apply(pushed, commit, &releases)?
            }
            None => Step::Continue {
                warnings: Vec::new(),
                attestations: Vec::new(),
            },
        };
        if let (Step::Continue { attestations, .. }, Some(timeout)) = (&mut step, wait) {
            match health_wait::wait_for_target(ctx, unit, commit, timeout, access) {
                Waited::Healthy(found) => *attestations = found,
                Waited::Failed(outcome) => return Ok(Step::Stop(outcome)),
            }
        }
        Ok(step)
    })
}

/// What publishing a target did about the shared catalog it names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SharedCatalogResult {
    /// The branch held no shared catalog, so the publication created it.
    Created,
    /// The branch holds the manifest this render would write.
    Current,
    /// The branch holds another manifest, left as it is: the manifest has one
    /// writer, `nyl build -- --shared-catalog --push`.
    Differs,
}

impl SharedCatalogResult {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Current => "current",
            Self::Differs => "differs",
        }
    }
}

/// The recorded, unkeyed member that holds what the branch holds of the shared catalog.
pub(crate) const SHARED_CATALOG_READING: &str = "/sharedCatalogReading";

/// What the branch holds of the shared catalog a target names, as the teardown
/// readiness reads it: `{result, settings, problem?}`. Argo CD runs what the
/// branch holds, not what one target renders, so the settings are those of the
/// published Application (the render's when the branch holds it, none checked
/// while the manifest is missing). A branch that cannot be read, or a manifest
/// that is no Application, is recorded as `problem`, which blocks a teardown.
/// A prefix another owner holds, or another shared catalog on the branch, is
/// an error of its own. `None` for a target that shares no catalog.
///
/// The reading is recorded but never keyed: it follows the branch, which the
/// writer changes, and a change there must not re-execute every preview.
///
/// Contract: [Catalog ownership](../../../../design/orchestration-core.md#kubernetes-publication-unit).
pub(crate) fn read_shared_catalog(
    env: &std::collections::BTreeMap<String, String>,
    project_dir: &std::path::Path,
    rendered: &RenderedTarget,
) -> DriverResult<Option<Value>> {
    use nyl_render::gitops::publication::SharedCatalogView;
    let Some(shared) = rendered.shared_catalog.as_deref() else {
        return Ok(None);
    };
    let variable = |name: &str| env.get(name).cloned();
    let (result, view, problem) = match inspect_shared_catalog(shared, project_dir, &variable) {
        Ok(inspection) => match (inspection.state, inspection.published) {
            (SharedCatalogState::Current, _) => ("current", SharedCatalogView::Rendered, None),
            (SharedCatalogState::Differs(_), Some(application)) => {
                ("differs", SharedCatalogView::Published(application), None)
            }
            (SharedCatalogState::Differs(_), None) => (
                "unparseable",
                SharedCatalogView::NotChecked,
                Some("the manifest on the branch is not a readable Application".to_owned()),
            ),
            (SharedCatalogState::Missing, _) => ("missing", SharedCatalogView::NotChecked, None),
        },
        Err(error)
            if error.is_owner_fenced()
                || error
                    .to_string()
                    .contains(nyl_render::gitops::shared_catalog::SHARED_CATALOG_CONFLICT_CODE) =>
        {
            return Err(DriverError::before_effects(format!(
                "the shared catalog {}: {error}",
                shared.destination.path_prefix
            )))
        }
        Err(error) => (
            "unreadable",
            SharedCatalogView::NotChecked,
            Some(format!("cannot read the publication branch: {error}")),
        ),
    };
    let settings = nyl_render::gitops::publication::teardown_settings_with(rendered, &view).map_err(|error| {
        DriverError::before_effects(format!("cannot read the shared catalog's teardown settings: {error}"))
    })?;
    let mut reading = json!({"result": result, "settings": settings});
    if let Some(problem) = problem {
        reading["problem"] = Value::String(problem);
    }
    Ok(Some(reading))
}

/// Before a teardown starts, read the shared catalog again: the settings
/// recorded at the last resolution may be older than the writer's last push,
/// and an instance that expired is removed from its stored document without
/// resolving. Held, with the readiness code, when the catalog no longer prunes
/// or deletes with its Applications' finalizers, or cannot be read. Only the
/// catalog's own rows hold it here; the Namespace rows and `--allow-incomplete`
/// are the shell's preflight.
fn shared_catalog_hold(
    ctx: &ExecutionContext<'_>,
    unit: &DesiredUnit,
    rendered: &RenderedTarget,
) -> DriverResult<Option<Outcome>> {
    use nyl_core::orchestration::codes::Code;
    let Some(reading) = read_shared_catalog(&ctx.env, &ctx.project_dir, rendered)? else {
        return Ok(None);
    };
    let readiness = super::readiness::evaluate_reading(&unit.unit.environment, &unit.unit.name, &reading)
        .map_err(DriverError::before_effects)?;
    let held = readiness.blocking.iter().find(|finding| {
        matches!(
            finding.code,
            Code::TeardownCatalogManualSync | Code::TeardownApplicationsOrphaned
        )
    });
    Ok(held.map(|finding| Outcome::Failed {
        category: FailureCategory::Ownership,
        retryable: true,
        message: format!("{}: {} ({})", finding.code, finding.message, finding.fix),
    }))
}

/// The warning of a publication that found another manifest at the shared catalog's prefix.
fn shared_catalog_differs(unit: &DesiredUnit, prefix: &str) -> nyl_core::orchestration::codes::Finding {
    nyl_core::orchestration::codes::Finding::new(
        nyl_core::orchestration::codes::Code::KubernetesSharedCatalogDiffers,
        format!(
            "{}/{}: the branch holds another manifest at the shared catalog {prefix} than this target renders (another Nyl version or declaration wrote it); the publication left it alone",
            unit.unit.environment, unit.unit.name
        ),
        "replace it from the revision that should win with `nyl build <unit> -e <environment> -- --shared-catalog --push`",
    )
}

/// Create the shared catalog `shared` renders when the branch holds none, and
/// otherwise leave it alone: a target's publication never overwrites a
/// manifest, so pull requests running different Nyl versions never overwrite
/// each other. The creation is a compare-and-swap: a publication that loses the
/// race for the branch head reads the new head again, and stops as `Current` or
/// `Differs` once another has created the manifest. A manifest that differs from
/// this render is reported in the result, which the published tree's artifact
/// records. The inner `Err` is the outcome of a call that ended without
/// publishing, such as a prefix another owner holds.
///
/// Contract: [Catalog ownership](../../../../design/orchestration-core.md#kubernetes-publication-unit).
fn ensure_shared_catalog(
    ctx: &ExecutionContext<'_>,
    unit: &DesiredUnit,
    shared: &RenderedTarget,
) -> DriverResult<std::result::Result<SharedCatalogResult, Outcome>> {
    ensure_shared_catalog_between(ctx, unit, shared, &|| {})
}

/// [`ensure_shared_catalog`], calling `between` after a creation is prepared
/// and before it is pushed, which is where a test lets another publication win.
fn ensure_shared_catalog_between(
    ctx: &ExecutionContext<'_>,
    unit: &DesiredUnit,
    shared: &RenderedTarget,
    between: &dyn Fn(),
) -> DriverResult<std::result::Result<SharedCatalogResult, Outcome>> {
    let author = author(ctx, unit)?;
    let variable = |name: &str| ctx.env.get(name).cloned();
    let message = format!(
        "nyl: create the shared catalog {} for {}/{}",
        shared.destination.path_prefix, unit.unit.environment, unit.unit.name
    );
    let budget = Budget::new(ctx);
    let mut attempts = 0;
    loop {
        attempts += 1;
        match inspect_shared_catalog(shared, &ctx.project_dir, &variable) {
            Ok(inspection) => match inspection.state {
                SharedCatalogState::Current => return Ok(Ok(SharedCatalogResult::Current)),
                SharedCatalogState::Differs(_) => return Ok(Ok(SharedCatalogResult::Differs)),
                SharedCatalogState::Missing => {}
            },
            Err(error) if error.is_owner_fenced() => return Ok(Err(fenced(&error.to_string()))),
            Err(error) => {
                return Err(DriverError::before_effects(format!(
                    "cannot read the shared catalog {}: {error}",
                    shared.destination.path_prefix
                )))
            }
        }
        let prepared =
            match prepare_target_publication_fenced(shared, &ctx.project_dir, &message, &author, &variable, None) {
                Ok(prepared) => prepared,
                Err(error) if error.is_owner_fenced() => return Ok(Err(fenced(&error.to_string()))),
                Err(error) if error.is_stale_publication() => {
                    if attempts < PUBLISH_ATTEMPTS && !budget.spent() {
                        continue;
                    }
                    return Err(DriverError::before_effects(format!(
                        "cannot create the shared catalog: the publication branch kept moving: {error}"
                    )));
                }
                Err(error) => {
                    return Err(DriverError::before_effects(format!(
                        "cannot prepare the shared catalog: {error}"
                    )))
                }
            };
        between();
        match prepared.push() {
            Ok(_) => return Ok(Ok(SharedCatalogResult::Created)),
            // Another publication moved the branch: it may have created the manifest.
            Err(error) if error.is_stale_publication() && attempts < PUBLISH_ATTEMPTS && !budget.spent() => {}
            Err(error) => {
                return Err(DriverError::after_effects(format!(
                    "cannot create the shared catalog: {error}"
                )))
            }
        }
    }
}

/// Publish the shared catalog tree `shared` as one commit, creating or
/// replacing its manifest: what `nyl build -- --shared-catalog --push` does,
/// and what a target's publication does when the branch holds none. A prefix
/// another owner holds is fenced, and nothing takes it over.
pub(crate) fn publish_shared_catalog(
    ctx: &ExecutionContext<'_>,
    unit: &DesiredUnit,
    shared: &RenderedTarget,
    message: &str,
) -> DriverResult<std::result::Result<nyl_render::gitops::publication::PublishedTree, Outcome>> {
    let author = author(ctx, unit)?;
    let variable = |name: &str| ctx.env.get(name).cloned();
    // Another shared catalog on the branch, or a prefix another owner holds, refuses the writer too.
    match inspect_shared_catalog(shared, &ctx.project_dir, &variable) {
        Ok(_) => {}
        Err(error) if error.is_owner_fenced() => return Ok(Err(fenced(&error.to_string()))),
        Err(error) => {
            return Err(DriverError::before_effects(format!(
                "cannot publish the shared catalog {}: {error}",
                shared.destination.path_prefix
            )))
        }
    }
    let mut tree = shared.clone();
    let published = with_retries(
        &Budget::new(ctx),
        &mut tree,
        ("the shared catalog", "publish the shared catalog"),
        |tree| prepare_target_publication_fenced(tree, &ctx.project_dir, message, &author, &variable, None),
        nyl_render::gitops::publication::PreparedTarget::push,
        |_| Ok(Rebuilt::Ready),
    )?;
    Ok(match published {
        Attempted::Done(published) => Ok(published),
        Attempted::StateChanged(message) => Err(state_changed(&message)),
        Attempted::Fenced(message) => Err(fenced(&message)),
    })
}

/// Publish `rendered`, which `render` produces again when the branch head
/// moves, as one commit with `message`, and record the `PublishedTree`
/// artifact `tree`. Shared by an orchestrated publication and `nyl build
/// --push`, which differ in what they render (the owner the index records)
/// and in the checks before the publication.
pub(crate) fn publish_rendered(
    render: &dyn Fn() -> DriverResult<RenderedTarget>,
    rendered: RenderedTarget,
    message: &str,
    ctx: &ExecutionContext<'_>,
    unit: &DesiredUnit,
    outputs: Value,
) -> DriverResult<Outcome> {
    publish_rendered_with(render, rendered, message, ctx, unit, outputs, &mut |_, _| {
        Ok(Step::Continue {
            warnings: Vec::new(),
            attestations: Vec::new(),
        })
    })
}

/// [`publish_rendered`] that, once the tree is pushed, hands the pushed tree
/// and its commit to `then`, which may end the call with an outcome (a direct
/// target's apply) or add warnings to the receipt.
pub(crate) fn publish_rendered_with(
    render: &dyn Fn() -> DriverResult<RenderedTarget>,
    mut rendered: RenderedTarget,
    message: &str,
    ctx: &ExecutionContext<'_>,
    unit: &DesiredUnit,
    outputs: Value,
    then: &mut dyn FnMut(&RenderedTarget, &str) -> DriverResult<Step>,
) -> DriverResult<Outcome> {
    let author = author(ctx, unit)?;
    let key = unit
        .execution_key
        .as_ref()
        .ok_or_else(|| DriverError::before_effects("an executing unit has an execution key"))?;
    let uid = unit
        .unit
        .uid
        .clone()
        .ok_or_else(|| DriverError::before_effects("an executing unit has a uid"))?;
    let target = unit_target(resolved_spec(unit)?)?;
    let before = state_inputs(&rendered)?;
    let stateful = rendered.publication_base.is_some();
    // Credential variables come from the execution's admitted environment
    // (PATH and the unit's declared passthrough), never from the process
    // environment.
    let variable = |name: &str| ctx.env.get(name).cloned();
    let takeover = fence(ctx);
    // The shared catalog the target names comes first, so the manifest exists
    // when Argo CD syncs the target's catalog directory.
    let shared = match rendered.shared_catalog.clone() {
        Some(shared) => match ensure_shared_catalog(ctx, unit, &shared)? {
            Ok(result) => Some((shared.destination.path_prefix.clone(), result)),
            Err(outcome) => return Ok(outcome),
        },
        None => None,
    };
    let published = with_retries(
        &Budget::new(ctx),
        &mut rendered,
        ("the publication", "publish the tree"),
        |tree| prepare_target_publication_fenced(tree, &ctx.project_dir, message, &author, &variable, Some(&takeover)),
        nyl_render::gitops::publication::PreparedTarget::push,
        rebuild_from_state(render, &before, stateful, Ok),
    )?;
    let published = match published {
        Attempted::Done(published) => published,
        Attempted::StateChanged(message) => return Ok(state_changed(&message)),
        Attempted::Fenced(message) => return Ok(fenced(&message)),
    };
    let (mut extra, attestations) = match then(&rendered, &published.commit)? {
        Step::Continue { warnings, attestations } => (warnings, attestations),
        Step::Stop(outcome) => return Ok(outcome),
    };
    // A manifest that is not the one this target renders is the writer's to replace, and worth a warning.
    let mut warnings = shared
        .iter()
        .filter(|(_, result)| *result == SharedCatalogResult::Differs)
        .map(|(prefix, _)| shared_catalog_differs(unit, prefix))
        .collect::<Vec<_>>();
    warnings.append(&mut extra);
    Ok(Outcome::Succeeded {
        versions: std::collections::BTreeMap::new(),
        outputs,
        artifacts: vec![ArtifactDocument {
            api_version: API_VERSION_ARTIFACTS.to_owned(),
            kind: "PublishedTree".to_owned(),
            metadata: ArtifactMetadata {
                name: "tree".to_owned(),
                unit: ArtifactUnit {
                    environment: unit.unit.environment.clone(),
                    name: unit.unit.name.clone(),
                    uid,
                },
            },
            execution_key: key.clone(),
            spec: {
                let mut spec = json!({
                    "repository": published.repository,
                    "branch": published.branch,
                    "pathPrefix": published.path_prefix,
                    "target": target,
                    "reconciler": rendered.delivery.kind(),
                    "cluster": rendered.workload.cluster,
                    "commit": published.commit,
                    "indexDigest": published.index_digest,
                });
                if let Some((prefix, result)) = shared {
                    spec["sharedCatalog"] = json!({"pathPrefix": prefix, "result": result.as_str()});
                }
                spec
            },
        }],
        attestations,
        warnings,
    })
}

/// One call of the unit's two-phase teardown, as the contract's Teardown
/// section describes.
#[allow(clippy::too_many_lines)] // Two phases and their effects stay in one visible order.
pub(crate) fn teardown(
    catalogue: &nyl_render::config::ResourceCatalogue,
    ctx: &ExecutionContext<'_>,
    unit: &DesiredUnit,
    access: &ClusterAccess,
) -> DriverResult<Outcome> {
    let author = author(ctx, unit)?;
    let spec = resolved_spec(unit)?;
    let owner = unit_owner(unit)?;
    let render = || {
        render(
            catalogue,
            &ctx.worktree,
            &ctx.project_dir,
            &ctx.cache_dir,
            ctx.instances,
            spec,
            Some(owner.clone()),
        )
    };
    let variable = |name: &str| ctx.env.get(name).cloned();
    let takeover = fence(ctx);
    if ctx.teardown_wait.is_some() {
        // Phase 2: the wait ended; remove exactly what the index owns. The
        // render only identifies the owner and the destination, so a moved
        // head needs nothing rebuilt.
        let mut rendered = render()?;
        if let Some(outcome) = reconciler_mismatch(unit, &rendered) {
            return Ok(outcome);
        }
        // A direct target's release state and the Namespaces that held it go
        // first, so the index's owner keeps fencing until the cluster is done.
        let mut skipped = Vec::new();
        if let Some(direct) = direct_of(ctx, unit, &rendered, access)? {
            let plan = direct.teardown_plan(&rendered)?;
            let result = direct.teardown_remove_state(&plan)?;
            if let Some(outcome) = result.outcome {
                return Ok(outcome);
            }
            skipped = result.skipped;
        }
        let message = format!(
            "nyl: teardown {}/{}: remove the owned files",
            unit.unit.environment, unit.unit.name
        );
        let removed = with_retries(
            &Budget::new(ctx),
            &mut rendered,
            ("the removal", "remove the tree"),
            |tree| prepare_target_removal_fenced(tree, &ctx.project_dir, &message, &author, &variable, Some(&takeover)),
            nyl_render::gitops::publication::PreparedRemoval::push,
            |_| Ok(Rebuilt::Ready),
        )?;
        if let Attempted::Fenced(message) = removed {
            return Ok(fenced(&message));
        }
        return Ok(Outcome::Succeeded {
            versions: std::collections::BTreeMap::new(),
            outputs: json!({}),
            artifacts: Vec::new(),
            attestations: Vec::new(),
            warnings: skipped_warnings(unit, &skipped),
        });
    }
    if let Some(waiting) = ctx.observed.and_then(|observed| observed.waiting.as_ref()) {
        // Phase 1 is published already; the wait continues.
        return Ok(Outcome::Waiting {
            phase: waiting.phase.clone(),
            reason: waiting.reason.clone(),
            until: None,
            published: waiting.phase1_commit.clone(),
        });
    }
    let rendered = render()?;
    if let Some(outcome) = reconciler_mismatch(unit, &rendered) {
        return Ok(outcome);
    }
    if let Some(held) = shared_catalog_hold(ctx, unit, &rendered)? {
        return Ok(held);
    }
    let before = state_inputs(&rendered)?;
    let stateful = rendered.publication_base.is_some();
    let direct_target = rendered.delivery.kind() == "direct";
    let phase1_of = |rendered: RenderedTarget| {
        if rendered.delivery.kind() == "direct" {
            // A direct target's first phase publishes the tree without its
            // Releases, so Git never lags the cluster, and then purges.
            return Ok(without_workloads(&rendered));
        }
        without_workload_applications(&rendered)
            .map_err(|error| DriverError::before_effects(format!("cannot prepare phase 1 of the teardown: {error}")))
    };
    let direct = direct_of(ctx, unit, &rendered, access)?;
    // What the teardown removes is read before anything is published or
    // deleted, so a failure to read it leaves everything as it was.
    let plan = direct
        .as_ref()
        .map(|direct| direct.teardown_plan(&rendered))
        .transpose()?;
    let rendered_for_cluster = direct_target.then(|| rendered.clone());
    let mut phase1 = phase1_of(rendered)?;
    let message = format!(
        "nyl: teardown {}/{}: remove the workload Applications",
        unit.unit.environment, unit.unit.name
    );
    let published = with_retries(
        &Budget::new(ctx),
        &mut phase1,
        ("phase 1 of the teardown", "publish phase 1 of the teardown"),
        |tree| prepare_target_publication_fenced(tree, &ctx.project_dir, &message, &author, &variable, Some(&takeover)),
        nyl_render::gitops::publication::PreparedTarget::push,
        rebuild_from_state(&render, &before, stateful, phase1_of),
    )?;
    let published = match published {
        Attempted::Done(published) => published,
        Attempted::StateChanged(message) => return Ok(state_changed(&message)),
        Attempted::Fenced(message) => return Ok(fenced(&message)),
    };
    let commit = CommitId::new(published.commit)
        .map_err(|error| DriverError::after_effects(format!("phase 1 commit: {error}")))?;
    if let (Some(direct), Some(rendered), Some(plan)) = (&direct, &rendered_for_cluster, &plan) {
        let result = direct.teardown_delete(plan)?;
        if let Some(outcome) = result.outcome {
            return Ok(outcome);
        }
        let left = if result.skipped.is_empty() {
            String::new()
        } else {
            format!(
                "; it left {} alone",
                result
                    .skipped
                    .iter()
                    .map(|left| format!("{} ({})", left.what, left.reason))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        return Ok(Outcome::Waiting {
            phase: PHASE_RESOURCES_DELETED.to_owned(),
            reason: format!(
                "phase 1 published {} without its Releases and deleted their resources from Cluster {}{left}",
                published.path_prefix, rendered.workload.cluster
            ),
            until: None,
            published: Some(commit),
        });
    }
    Ok(Outcome::Waiting {
        phase: PHASE_APPLICATIONS_REMOVED.to_owned(),
        reason: format!(
            "phase 1 removed the workload Applications from the catalog of {}",
            published.path_prefix
        ),
        until: None,
        published: Some(commit),
    })
}

#[cfg(test)]
#[path = "../../tests/publication_world/mod.rs"]
mod world;

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use serde_json::json;

    use super::*;
    use crate::kubernetes_publication::run::world;

    /// Catches "create only when absent" being a blind push: a publication
    /// that loses the race for the branch head, after another created the
    /// manifest, reads the new head again and stops as `Differs` (or `Current`),
    /// leaving the winner's manifest in place.
    #[test]
    fn test_the_loser_of_the_race_to_create_the_shared_catalog_leaves_the_winners_manifest() {
        let world = world::World::new();
        let platform = world.work().join("nyl/platform.yaml");
        let text = std::fs::read_to_string(&platform).unwrap();
        let anchor = "  publication:\n    repositoryRef: {name: platform}\n";
        assert!(text.contains(anchor));
        let render_with = |shared: &str| {
            std::fs::write(
                &platform,
                text.replace(anchor, &format!("  catalogApplication: {{shared: {shared}}}\n{anchor}")),
            )
            .unwrap();
            let mut descriptors = nyl_render::resources::catalog::resource_catalogue();
            descriptors.extend_from_slice(crate::catalog::RESOURCES);
            render(
                &nyl_render::config::ResourceCatalogue(descriptors),
                &world.work(),
                &world.work().join("nyl"),
                &world.root.path().join("cache"),
                &nyl_core::orchestration::instance::NO_INSTANCES,
                &world::spec(),
                None,
            )
            .unwrap()
        };
        let loser = render_with("{name: previews}");
        let winner = render_with("{name: previews, syncPolicy: {automated: {prune: true}}}");
        let identity: CommitIdentity = "Nyl Orchestration <nyl@example.invalid>".parse().unwrap();
        let mut ctx = world::context(
            &world,
            nyl_core::orchestration::driver::DriverOperation::Reconcile,
            Some(&identity),
        );
        ctx.deadline = ctx.now + chrono::Duration::minutes(10);
        let unit = world::desired(
            world::spec(),
            nyl_core::orchestration::ids::ExecutionKey::from_digest_of(b"k"),
        );
        let won = std::cell::Cell::new(false);
        let between = || {
            if !won.replace(true) {
                let winner = winner.shared_catalog.as_deref().unwrap();
                nyl_render::gitops::publication::publish_target(
                    winner,
                    &world.work().join("nyl"),
                    "nyl: the winner creates it",
                    &PublicationAuthor {
                        identity: identity.clone(),
                        time: ctx.now,
                    },
                    &|_| None,
                )
                .unwrap();
            }
        };
        let result = ensure_shared_catalog_between(&ctx, &unit, loser.shared_catalog.as_deref().unwrap(), &between)
            .unwrap()
            .unwrap();
        assert_eq!(result, SharedCatalogResult::Differs);
        assert!(won.get(), "the race was staged");
        let published = world::git(&world.bare(), &["show", "deploy:_nyl/shared/previews/catalog.yaml"]);
        assert!(published.contains("prune: true"), "{published}");
    }

    /// Catches a shared catalog's prefix being taken over by a unit: its owner
    /// is no unit incarnation, so no state can report it gone, and the fence
    /// holds with the stable code, whatever the incarnation directory answers.
    #[test]
    fn test_a_shared_catalog_owner_is_never_taken_over() {
        let world = world::World::new();
        let ctx = world::context(
            &world,
            nyl_core::orchestration::driver::DriverOperation::Reconcile,
            None,
        );
        let message = fence(&ctx)(&RenderIndexOwner::catalog("previews")).unwrap_err();
        assert!(message.starts_with("NYL-KUBERNETES-OWNER-FENCED:"), "{message}");
        assert!(message.contains("shared catalog previews"), "{message}");
    }

    /// Catches an uncacheable render keeping one key across resolutions,
    /// which would skip a publication whose inputs it cannot see.
    #[test]
    fn test_uncacheable_render_never_repeats_its_key() {
        let (first, second) = (
            chrono::Utc.timestamp_opt(1, 0).unwrap(),
            chrono::Utc.timestamp_opt(2, 0).unwrap(),
        );
        assert_eq!(uncacheable_fingerprint(true, first), None);
        let one = uncacheable_fingerprint(false, first).unwrap();
        let two = uncacheable_fingerprint(false, second).unwrap();
        assert_ne!(one, two);
    }

    fn moved() -> nyl_render::NylError {
        nyl_render::NylError::PublicationMoved("the branch advanced".to_owned())
    }

    fn plenty() -> Budget {
        Budget {
            started: std::time::Instant::now(),
            allowed: std::time::Duration::from_secs(3600),
        }
    }

    #[allow(clippy::unnecessary_wraps)] // The signature of a rebuild.
    fn unchanged(_: &mut ()) -> DriverResult<Rebuilt> {
        Ok(Rebuilt::Ready)
    }

    /// Catches a publication that lost the race for a shared branch being
    /// reported as failed (or worse, uncertain): the loser builds again on
    /// the new head and publishes.
    #[test]
    fn test_a_publication_that_lost_the_branch_head_is_built_again() {
        let prepares = std::cell::Cell::new(0);
        let pushes = std::cell::Cell::new(0);
        let Attempted::Done(published) = with_retries(
            &plenty(),
            &mut (),
            ("the publication", "publish the tree"),
            |()| {
                prepares.set(prepares.get() + 1);
                if prepares.get() == 1 {
                    Err(moved())
                } else {
                    Ok(prepares.get())
                }
            },
            |prepared| {
                pushes.set(pushes.get() + 1);
                if pushes.get() == 1 {
                    Err(moved())
                } else {
                    Ok(prepared)
                }
            },
            unchanged,
        )
        .unwrap() else {
            panic!("the publication is built again");
        };
        // The first build lost at its check, the second at its push, and the third published.
        assert_eq!((published, prepares.get(), pushes.get()), (3, 3, 2));
    }

    /// Catches a branch that never stops moving ending as an uncertain
    /// effect: nothing was pushed, so the failure is before effects.
    #[test]
    fn test_a_publication_that_keeps_losing_the_branch_head_fails_before_effects() {
        let attempts = std::cell::Cell::new(0);
        let error = with_retries(
            &plenty(),
            &mut (),
            ("the publication", "publish the tree"),
            |()| Ok(()),
            |()| -> nyl_render::Result<()> {
                attempts.set(attempts.get() + 1);
                Err(moved())
            },
            unchanged,
        )
        .err()
        .expect("the publication fails");
        assert_eq!(attempts.get(), PUBLISH_ATTEMPTS);
        assert!(!error.effects_possible(), "{error}");
        assert!(error.to_string().contains("cannot publish the tree"), "{error}");
    }

    /// Catches retries outliving the unit's timeout: once it passed, a
    /// moved branch ends the call instead of building again.
    #[test]
    fn test_a_spent_timeout_stops_building_again() {
        let attempts = std::cell::Cell::new(0);
        let spent = Budget {
            started: std::time::Instant::now(),
            allowed: std::time::Duration::ZERO,
        };
        let error = with_retries(
            &spent,
            &mut (),
            ("the publication", "publish the tree"),
            |()| Ok(()),
            |()| -> nyl_render::Result<()> {
                attempts.set(attempts.get() + 1);
                Err(moved())
            },
            unchanged,
        )
        .err()
        .expect("the timeout passed");
        assert_eq!(attempts.get(), 1);
        assert!(!error.effects_possible());
        assert!(error.to_string().contains("timeout"), "{error}");
    }

    /// Catches an ordinary failure being retried or classified wrongly: a
    /// failure to prepare changed nothing, and a failed push may have published.
    #[test]
    fn test_other_failures_end_the_call_with_their_effects() {
        let attempts = std::cell::Cell::new(0);
        let prepare_error = with_retries(
            &plenty(),
            &mut (),
            ("the publication", "publish the tree"),
            |()| -> nyl_render::Result<()> {
                attempts.set(attempts.get() + 1);
                Err(nyl_render::NylError::config("the remote refused the clone"))
            },
            Ok,
            unchanged,
        )
        .err()
        .expect("preparing fails");
        assert_eq!(attempts.get(), 1);
        assert!(!prepare_error.effects_possible());
        assert!(prepare_error.to_string().contains("cannot prepare the publication"));

        let attempts = std::cell::Cell::new(0);
        let push_error = with_retries(
            &plenty(),
            &mut (),
            ("the publication", "publish the tree"),
            |()| Ok(()),
            |()| -> nyl_render::Result<()> {
                attempts.set(attempts.get() + 1);
                Err(nyl_render::NylError::config("the remote hung up"))
            },
            unchanged,
        )
        .err()
        .expect("pushing fails");
        assert_eq!(attempts.get(), 1);
        assert!(push_error.effects_possible());
    }

    /// A project whose `databaseHost` input reads `state/db.json` from the
    /// publication branch, which holds that file.
    fn stateful_world() -> world::World {
        let world = world::World::new();
        let platform = world.work().join("nyl/platform.yaml");
        let text = std::fs::read_to_string(&platform).unwrap();
        let bound = text.replace(
            "databaseHost: {fromUnit: {unit: database, output: host}}",
            "databaseHost: {fromPublication: {path: state/db.json, pointer: /host}}",
        );
        assert_ne!(text, bound);
        std::fs::write(&platform, bound).unwrap();
        world::git(&world.work(), &["add", "--all"]);
        world::git(
            &world.work(),
            &["commit", "--quiet", "--message", "Read the database host from state"],
        );
        push_to_deploy(&world, &[("dev/state/db.json", "{\"host\": \"db.state.internal\"}\n")]);
        world
    }

    /// Commit `files` to the `deploy` branch as another tool would.
    fn push_to_deploy(world: &world::World, files: &[(&str, &str)]) {
        let directory = tempfile::TempDir::new().unwrap();
        let clone = directory.path().join("other");
        let url = world.project_url();
        let exists = !world::git(world.root.path(), &["ls-remote", "--heads", &url, "deploy"]).is_empty();
        if exists {
            world::git(
                directory.path(),
                &["clone", "--quiet", "--branch", "deploy", &url, "other"],
            );
        } else {
            world::git(directory.path(), &["clone", "--quiet", &url, "other"]);
            world::git(&clone, &["checkout", "--quiet", "--orphan", "deploy"]);
            world::git(&clone, &["rm", "-rf", "--quiet", "."]);
        }
        for (path, content) in files {
            let file = clone.join(path);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, content).unwrap();
        }
        world::git(&clone, &["add", "--all"]);
        world::git(&clone, &["commit", "--quiet", "--message", "Another tool"]);
        world::git(&clone, &["push", "--quiet", "origin", "HEAD:refs/heads/deploy"]);
    }

    /// Publish the stateful world's unit while `concurrent` runs once, after
    /// its first render read the state. Returns the outcome and how often it rendered.
    fn publish_while(world: &world::World, concurrent: impl Fn()) -> (Outcome, usize) {
        let catalogue = crate::fake::test_catalogue();
        let spec = json!({
            "target": "dev",
            "releases": {"web/web": {"image": "registry.invalid:5000/web@sha256:0123"}}
        });
        let unit = world::desired(
            spec.clone(),
            nyl_core::orchestration::ids::ExecutionKey::from_digest_of(b"key"),
        );
        let identity: CommitIdentity = "Nyl Orchestration <nyl@example.invalid>".parse().unwrap();
        let mut ctx = world::context(
            world,
            nyl_core::orchestration::driver::DriverOperation::Reconcile,
            Some(&identity),
        );
        ctx.deadline = ctx.now + chrono::Duration::minutes(10);
        let renders = std::cell::Cell::new(0);
        let render = || {
            let rendered = super::render(
                &catalogue,
                &ctx.worktree,
                &ctx.project_dir,
                &ctx.cache_dir,
                ctx.instances,
                &spec,
                Some(unit_owner(&unit).unwrap()),
            );
            renders.set(renders.get() + 1);
            if renders.get() == 1 {
                concurrent();
            }
            rendered
        };
        let outcome = publish_with(&render, &ctx, &unit, json!({}), &ClusterAccess::Live).unwrap();
        (outcome, renders.get())
    }

    /// Catches a `fromPublication` target failing, as the contract forbids,
    /// because an unrelated target published to the shared branch first: it
    /// renders again on the new head, reads the same state, and publishes.
    #[test]
    fn test_an_unrelated_push_to_the_shared_branch_rebuilds_a_stateful_publication() {
        let world = stateful_world();
        let (outcome, renders) = publish_while(&world, || {
            push_to_deploy(&world, &[("other-target/state.json", "{}\n")]);
        });
        assert!(matches!(outcome, Outcome::Succeeded { .. }), "{outcome:?}");
        assert_eq!(renders, 2);
        let files = world.files("deploy");
        assert!(files.iter().any(|path| path.starts_with("dev/web/")), "{files:?}");
        assert!(files.contains(&"other-target/state.json".to_owned()), "{files:?}");
        let deployment = files
            .iter()
            .filter(|path| path.starts_with("dev/web/"))
            .map(|path| world::git(&world.bare(), &["show", &format!("deploy:{path}")]))
            .find(|text| text.contains("kind: Deployment"))
            .unwrap();
        assert!(deployment.contains("db.state.internal"), "{deployment}");
    }

    /// Catches a publication built from state that no longer holds: when the
    /// state file it read changed, it ends without publishing, retryably and
    /// with its own code, so the next run resolves the unit again.
    #[test]
    fn test_a_changed_state_file_ends_a_stateful_publication_without_publishing() {
        let world = stateful_world();
        let (outcome, renders) = publish_while(&world, || {
            push_to_deploy(&world, &[("dev/state/db.json", "{\"host\": \"db.moved.internal\"}\n")]);
        });
        let Outcome::Failed { retryable, message, .. } = outcome else {
            panic!("the publication ends without publishing: {outcome:?}");
        };
        assert!(retryable);
        assert!(message.starts_with("NYL-KUBERNETES-STATE-CHANGED:"), "{message}");
        assert!(message.contains("@publication/state/db.json"), "{message}");
        assert_eq!(renders, 2);
        let files = world.files("deploy");
        assert!(!files.iter().any(|path| path.starts_with("dev/web/")), "{files:?}");
    }

    /// Catches the render reading its pinned inputs from anywhere but the
    /// resolved `releases`, the pointer `/releases/<group>~1<release>/<input>`
    /// that bindings resolve into.
    #[test]
    fn test_snapshot_reads_resolved_releases() {
        let snapshot = resolved_snapshot(&json!({"target": "dev", "releases": {"web/web": {"image": "x"}}})).unwrap();
        assert_eq!(snapshot["web/web"]["image"], json!("x"));
        assert!(resolved_snapshot(&json!({"target": "dev"})).unwrap().is_empty());
        assert!(resolved_snapshot(&json!({"releases": {"web/web": "x"}})).is_err());
    }
}
