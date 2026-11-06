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
use nyl_render::gitops::publication::{
    prepare_target_publication_fenced, prepare_target_removal_fenced, render_target, without_workload_applications,
    PublicationAuthor, RenderedTarget, TargetRender,
};
use nyl_render::gitops::reconcile::RenderIndexOwner;
use nyl_render::gitops::units::PublicationTarget;
use serde_json::{json, Value};

use crate::command::{is_deleting, resolved_spec};

/// The phase a teardown waits in after publishing phase 1.
pub(crate) const PHASE_APPLICATIONS_REMOVED: &str = "applications-removed";

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
    Ok(RenderIndexOwner {
        environment: unit.unit.environment.to_string(),
        unit: unit.unit.name.to_string(),
        uid: uid.to_string(),
    })
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
        let (Ok(environment), Ok(unit), Ok(uid)) = (
            EnvironmentName::new(&owner.environment),
            UnitName::new(&owner.unit),
            Uid::new(&owner.uid),
        ) else {
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
    ]
    .into_iter()
    .filter_map(|(field, now, before)| match (now, before) {
        (Some(now), Some(before)) if now != before => Some(format!("{field} {before:?} became {now:?}")),
        _ => None,
    })
    .collect::<Vec<_>>();
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
pub(crate) fn publish(
    catalogue: &nyl_render::config::ResourceCatalogue,
    ctx: &ExecutionContext<'_>,
    unit: &DesiredUnit,
    outputs: Value,
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
    )
}

/// [`publish`] rendering with `render`, which a retry calls again.
fn publish_with(
    render: &dyn Fn() -> DriverResult<RenderedTarget>,
    ctx: &ExecutionContext<'_>,
    unit: &DesiredUnit,
    outputs: Value,
) -> DriverResult<Outcome> {
    author(ctx, unit)?;
    let rendered = render()?;
    let target = unit_target(resolved_spec(unit)?)?;
    if let Some(outcome) = target_changed(ctx, unit, &rendered, &target) {
        return Ok(outcome);
    }
    let message = format!("nyl: publish {}/{}", unit.unit.environment, unit.unit.name);
    publish_rendered(render, rendered, &message, ctx, unit, outputs)
}

/// Publish `rendered`, which `render` produces again when the branch head
/// moves, as one commit with `message`, and record the `PublishedTree`
/// artifact `tree`. Shared by an orchestrated publication and `nyl build
/// --push`, which differ in what they render (the owner the index records)
/// and in the checks before the publication.
pub(crate) fn publish_rendered(
    render: &dyn Fn() -> DriverResult<RenderedTarget>,
    mut rendered: RenderedTarget,
    message: &str,
    ctx: &ExecutionContext<'_>,
    unit: &DesiredUnit,
    outputs: Value,
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
            spec: json!({
                "repository": published.repository,
                "branch": published.branch,
                "pathPrefix": published.path_prefix,
                "target": target,
                "commit": published.commit,
                "indexDigest": published.index_digest,
            }),
        }],
        attestations: Vec::new(),
    })
}

/// One call of the unit's two-phase teardown, as the contract's Teardown
/// section describes.
pub(crate) fn teardown(
    catalogue: &nyl_render::config::ResourceCatalogue,
    ctx: &ExecutionContext<'_>,
    unit: &DesiredUnit,
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
    let before = state_inputs(&rendered)?;
    let stateful = rendered.publication_base.is_some();
    let phase1_of = |rendered: RenderedTarget| {
        without_workload_applications(&rendered)
            .map_err(|error| DriverError::before_effects(format!("cannot prepare phase 1 of the teardown: {error}")))
    };
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
        let outcome = publish_with(&render, &ctx, &unit, json!({})).unwrap();
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
