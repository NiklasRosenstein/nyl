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
//!    ownership index records (`prepare_target_removal`).
//!
//! When the wait ends is the orchestration shell's decision
//! (`rules::teardown_wait`), read from `teardownWait`.
//!
//! A publication that loses the race for the branch head, before or at its
//! push, built on a head that moved, so nothing was published; it is built
//! again on the new head, a bounded number of times. Any other failure ends
//! the call: one before the push is a failure before effects, and a failed
//! push may have published.

use nyl_core::constants::API_VERSION_ARTIFACTS;
use nyl_core::orchestration::driver::{
    Change, DriverError, DriverResult, ExecutionContext, KeyInputs, Outcome, PlanReport, ResolutionContext,
};
use nyl_core::orchestration::ids::CommitId;
use nyl_core::orchestration::ports::CommitIdentity;
use nyl_core::orchestration::state::{ArtifactDocument, ArtifactMetadata, ArtifactUnit, DesiredUnit};
use nyl_core::orchestration::time::Timestamp;
use nyl_render::gitops::inputs::InputSnapshot;
use nyl_render::gitops::publication::{
    prepare_target_publication, prepare_target_removal, render_target, without_workload_applications,
    PublicationAuthor, RenderedTarget, TargetRender,
};
use serde_json::{json, Value};

use crate::command::{is_deleting, resolved_spec};

/// The phase a teardown waits in after publishing phase 1.
pub(crate) const PHASE_APPLICATIONS_REMOVED: &str = "applications-removed";

/// How often a publication is built again on a branch head that moved.
const PUBLISH_ATTEMPTS: usize = 3;

/// The DeploymentTarget a resolved spec names.
pub(crate) fn unit_target(spec: &Value) -> DriverResult<&str> {
    spec.get("target")
        .and_then(Value::as_str)
        .ok_or_else(|| DriverError::before_effects("spec.target must be a string"))
}

/// The resolved `spec.releases` as the pinned input snapshot of the render.
pub(crate) fn resolved_snapshot(spec: &Value) -> DriverResult<InputSnapshot> {
    let Some(releases) = spec.get("releases") else {
        return Ok(InputSnapshot::new());
    };
    serde_json::from_value::<InputSnapshot>(releases.clone())
        .map_err(|error| DriverError::before_effects(format!("spec.releases: {error}")))
}

/// Render the target `spec` names at `worktree`, with its resolved Release
/// inputs as a pinned snapshot.
pub(crate) fn render(
    catalogue: &nyl_render::config::ResourceCatalogue,
    worktree: &std::path::Path,
    project_dir: &std::path::Path,
    cache_dir: &std::path::Path,
    spec: &Value,
) -> DriverResult<RenderedTarget> {
    let target = unit_target(spec)?;
    let inputs = resolved_snapshot(spec)?;
    render_target(&TargetRender {
        selection: nyl_render::gitops::tree::partial::PublicationSelection::default(),
        publication_base: None,
        worktree,
        project_dir,
        target,
        inputs: &inputs,
        cache_dir: Some(cache_dir),
        resource_catalogue: catalogue.clone(),
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
    let rendered = render(catalogue, &ctx.worktree, &ctx.project_dir, &ctx.cache_dir, spec)?;
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

/// Build with `prepare` and push with `push`, building again when the
/// branch head moved. `prepared` and `pushed` complete "cannot prepare ..."
/// and "cannot ..." in the failure messages.
fn with_retries<P, R>(
    prepared: &str,
    pushed: &str,
    prepare: impl Fn() -> nyl_render::Result<P>,
    push: impl Fn(P) -> nyl_render::Result<R>,
) -> DriverResult<R> {
    let mut attempts = 0;
    loop {
        attempts += 1;
        let more = attempts < PUBLISH_ATTEMPTS;
        let ready = match prepare() {
            Ok(ready) => ready,
            Err(error) if error.is_stale_publication() && more => continue,
            Err(error) => {
                return Err(DriverError::before_effects(format!(
                    "cannot prepare {prepared}: {error}"
                )))
            }
        };
        match push(ready) {
            Ok(result) => return Ok(result),
            Err(error) if error.is_stale_publication() && more => {}
            // The compare-and-swap refused it, so nothing was published.
            Err(error) if error.is_stale_publication() => {
                return Err(DriverError::before_effects(format!(
                    "cannot {pushed}: the publication branch moved {PUBLISH_ATTEMPTS} times while publishing: {error}"
                )))
            }
            Err(error) => return Err(DriverError::after_effects(format!("cannot {pushed}: {error}"))),
        }
    }
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
    let rendered = render(
        catalogue,
        &ctx.worktree,
        &ctx.project_dir,
        &ctx.cache_dir,
        resolved_spec(unit)?,
    )?;
    let message = format!("nyl: publish {}/{}", unit.unit.environment, unit.unit.name);
    // Credential variables come from the execution's admitted environment
    // (PATH and the unit's declared passthrough), never from the process
    // environment.
    let variable = |name: &str| ctx.env.get(name).cloned();
    let published = with_retries(
        "the publication",
        "publish the tree",
        || prepare_target_publication(&rendered, &ctx.project_dir, &message, &author, &variable),
        nyl_render::gitops::publication::PreparedTarget::push,
    )?;
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
    let variable = |name: &str| ctx.env.get(name).cloned();
    if ctx.teardown_wait.is_some() {
        // Phase 2: the wait ended; remove exactly what the index owns.
        let rendered = render(catalogue, &ctx.worktree, &ctx.project_dir, &ctx.cache_dir, spec)?;
        let message = format!(
            "nyl: teardown {}/{}: remove the owned files",
            unit.unit.environment, unit.unit.name
        );
        with_retries(
            "the removal",
            "remove the tree",
            || prepare_target_removal(&rendered, &ctx.project_dir, &message, &author, &variable),
            nyl_render::gitops::publication::PreparedRemoval::push,
        )?;
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
    let rendered = render(catalogue, &ctx.worktree, &ctx.project_dir, &ctx.cache_dir, spec)?;
    let phase1 = without_workload_applications(&rendered)
        .map_err(|error| DriverError::before_effects(format!("cannot prepare phase 1 of the teardown: {error}")))?;
    let message = format!(
        "nyl: teardown {}/{}: remove the workload Applications",
        unit.unit.environment, unit.unit.name
    );
    let published = with_retries(
        "phase 1 of the teardown",
        "publish phase 1 of the teardown",
        || prepare_target_publication(&phase1, &ctx.project_dir, &message, &author, &variable),
        nyl_render::gitops::publication::PreparedTarget::push,
    )?;
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
mod tests {
    use chrono::TimeZone;
    use serde_json::json;

    use super::*;

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

    /// Catches a publication that lost the race for a shared branch being
    /// reported as failed (or worse, uncertain): the loser builds again on
    /// the new head and publishes.
    #[test]
    fn test_a_publication_that_lost_the_branch_head_is_built_again() {
        let prepares = std::cell::Cell::new(0);
        let pushes = std::cell::Cell::new(0);
        let published = with_retries(
            "the publication",
            "publish the tree",
            || {
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
        )
        .unwrap();
        // The first build lost at its check, the second at its push, and the third published.
        assert_eq!((published, prepares.get(), pushes.get()), (3, 3, 2));
    }

    /// Catches a branch that never stops moving ending as an uncertain
    /// effect: nothing was pushed, so the failure is before effects.
    #[test]
    fn test_a_publication_that_keeps_losing_the_branch_head_fails_before_effects() {
        let attempts = std::cell::Cell::new(0);
        let error = with_retries(
            "the publication",
            "publish the tree",
            || Ok(()),
            |()| -> nyl_render::Result<()> {
                attempts.set(attempts.get() + 1);
                Err(moved())
            },
        )
        .unwrap_err();
        assert_eq!(attempts.get(), PUBLISH_ATTEMPTS);
        assert!(!error.effects_possible(), "{error}");
        assert!(error.to_string().contains("cannot publish the tree"), "{error}");
    }

    /// Catches an ordinary failure being retried or classified wrongly: a
    /// failure to prepare changed nothing, and a failed push may have published.
    #[test]
    fn test_other_failures_end_the_call_with_their_effects() {
        let attempts = std::cell::Cell::new(0);
        let prepare_error = with_retries(
            "the publication",
            "publish the tree",
            || -> nyl_render::Result<()> {
                attempts.set(attempts.get() + 1);
                Err(nyl_render::NylError::config("an owned file was modified outside Nyl"))
            },
            Ok,
        )
        .unwrap_err();
        assert_eq!(attempts.get(), 1);
        assert!(!prepare_error.effects_possible());
        assert!(prepare_error.to_string().contains("cannot prepare the publication"));

        let attempts = std::cell::Cell::new(0);
        let push_error = with_retries(
            "the publication",
            "publish the tree",
            || Ok(()),
            |()| -> nyl_render::Result<()> {
                attempts.set(attempts.get() + 1);
                Err(nyl_render::NylError::config("the remote hung up"))
            },
        )
        .unwrap_err();
        assert_eq!(attempts.get(), 1);
        assert!(push_error.effects_possible());
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
