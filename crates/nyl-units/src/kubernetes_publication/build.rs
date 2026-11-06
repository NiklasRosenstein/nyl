//! `nyl build` for a publication unit.
//!
//! Contract: [Direct builds](../../../../design/orchestration-core.md#direct-builds)
//! and the [Kubernetes publication unit](../../../../design/orchestration-core.md#kubernetes-publication-unit),
//! Direct builds of a publication.
//!
//! A direct build renders the unit's target through the path a run takes
//! ([`render_at`]: the same renderer, the same pinned Release inputs, no
//! process environment, no secrets), without an owner, a lease, a receipt, or
//! a state write. The driver defines three modes by the arguments after `--`:
//!
//! - none: render, and print a `RenderedTree` document that digests the files.
//! - `--diff`: compare the render with the tree published at the destination
//!   and print a `TreeDiff` document. It writes nothing, so it applies to a
//!   target an environment owns as well.
//! - `--push`: publish the render, only for a target no environment owns
//!   ([`refuse_owned`]), from a committed source. It prints the
//!   `PublishedTree` document a run records.
//!
//! `--shared-catalog` selects the shared catalog Application of a target with
//! `catalogApplication.shared` instead of the target's own tree: it composes
//! with `--diff` and `--push`, and is the one writer that replaces the shared
//! manifest (a target's publication only creates it). No environment owns the
//! shared catalog, so `--push` is never refused as `NYL-KUBERNETES-BUILD-TARGET-OWNED`.
//!
//! The documents are local build information, never recorded or referenced.

use std::ffi::OsString;

use nyl_core::constants::API_VERSION_ARTIFACTS;
use nyl_core::orchestration::driver::{DriverError, DriverResult, ExecutionContext, Outcome};
use nyl_core::orchestration::state::{ArtifactDocument, ArtifactMetadata, ArtifactUnit, DesiredUnit};
use nyl_render::gitops::publication::{diff_target_publication, index_digest, FileChange, RenderedTarget};
use nyl_render::gitops::reconcile::{parse_index, DEFAULT_INDEX_PATH};
use serde_json::{json, Value};

use super::run::{publish_rendered, publish_shared_catalog, render_at, unit_target};
use crate::command::resolved_spec;

/// The code of a build argument the driver does not accept.
const ARGUMENT: &str = "NYL-KUBERNETES-BUILD-ARGUMENT";
/// The code of a `--push` of a target an environment owns.
const TARGET_OWNED: &str = "NYL-KUBERNETES-BUILD-TARGET-OWNED";
/// The code of a `--push` from a worktree with uncommitted changes.
const DIRTY_SOURCE: &str = "NYL-KUBERNETES-BUILD-DIRTY-SOURCE";
/// The code of a `--push` without an identity to author the commit with.
const IDENTITY: &str = "NYL-KUBERNETES-BUILD-IDENTITY";
/// The code of `--shared-catalog` for a target that names none.
const NO_SHARED_CATALOG: &str = "NYL-KUBERNETES-BUILD-NO-SHARED-CATALOG";

/// What a build does, from the arguments after `--`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Render,
    Diff,
    Push,
}

/// The arguments after `--`: the mode, and whether the build is of the shared
/// catalog the target names instead of the target's tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Arguments {
    mode: Mode,
    shared_catalog: bool,
}

impl Arguments {
    fn parse(args: &[OsString]) -> DriverResult<Self> {
        let mut mode = None;
        let mut shared_catalog = false;
        for arg in args {
            let next = match arg.to_str() {
                Some("--diff") => Mode::Diff,
                Some("--push") => Mode::Push,
                Some("--shared-catalog") => {
                    shared_catalog = true;
                    continue;
                }
                _ => {
                    return Err(DriverError::before_effects(format!(
                    "{ARGUMENT}: the publication driver accepts --diff, --push, or --shared-catalog after `--`, not {}",
                    arg.to_string_lossy()
                )))
                }
            };
            if mode.is_some_and(|mode| mode != next) {
                return Err(DriverError::before_effects(format!(
                    "{ARGUMENT}: --diff and --push are alternatives: compare first, then publish"
                )));
            }
            mode = Some(next);
        }
        Ok(Self {
            mode: mode.unwrap_or(Mode::Render),
            shared_catalog,
        })
    }
}

/// Build the unit's target as the arguments say.
pub(crate) fn build(
    catalogue: &nyl_render::config::ResourceCatalogue,
    ctx: &ExecutionContext<'_>,
    unit: &DesiredUnit,
    args: &[OsString],
) -> DriverResult<Vec<ArtifactDocument>> {
    let Arguments { mode, shared_catalog } = Arguments::parse(args)?;
    let spec = resolved_spec(unit)?;
    let target = unit_target(spec)?;
    if mode == Mode::Push && !shared_catalog {
        refuse_owned(catalogue, ctx, unit, &target)?;
    }
    // The ownership index names the commit the files stand for, never the
    // snapshot's own.
    let source = unit
        .source_commit
        .as_ref()
        .map(nyl_core::orchestration::ids::CommitId::as_str);
    let render = || {
        render_at(
            catalogue,
            &ctx.worktree,
            &ctx.project_dir,
            &ctx.cache_dir,
            ctx.instances,
            spec,
            None,
            source,
        )
    };
    let rendered = render()?;
    if shared_catalog {
        return build_shared_catalog(ctx, unit, mode, &rendered);
    }
    match mode {
        Mode::Render => Ok(vec![rendered_tree(unit, &target, &rendered)?]),
        Mode::Diff => Ok(vec![tree_diff(ctx, unit, &target, &rendered)?]),
        Mode::Push => {
            let message = format!("nyl: build --push {}", unit.unit.name);
            match publish_rendered(&render, rendered, &message, ctx, unit, json!({}))? {
                Outcome::Succeeded { artifacts, .. } => Ok(artifacts),
                Outcome::Failed { message, .. } => Err(DriverError::before_effects(message)),
                other => Err(DriverError::after_effects(format!(
                    "the publication ended without a result: {other:?}"
                ))),
            }
        }
    }
}

/// `--shared-catalog`: render, compare, or publish the shared catalog the
/// target names. Publishing needs a committed source and an identity like any
/// build's `--push`, and replaces the manifest, which a target's publication
/// never does.
fn build_shared_catalog(
    ctx: &ExecutionContext<'_>,
    unit: &DesiredUnit,
    mode: Mode,
    rendered: &RenderedTarget,
) -> DriverResult<Vec<ArtifactDocument>> {
    let Some(shared) = rendered.shared_catalog.as_deref() else {
        return Err(DriverError::before_effects(format!(
            "{NO_SHARED_CATALOG}: the target of unit {} declares no catalogApplication.shared, so there is no shared catalog to build",
            unit.unit.name
        )));
    };
    let label = shared_label(shared);
    match mode {
        Mode::Render => Ok(vec![rendered_tree(unit, &label, shared)?]),
        Mode::Diff => Ok(vec![tree_diff(ctx, unit, &label, shared)?]),
        Mode::Push => {
            if ctx.dirty_source {
                return Err(DriverError::before_effects(format!(
                    "{DIRTY_SOURCE}: the worktree has uncommitted changes, so the shared catalog cannot be attributed to reviewed source; \
                     commit them, or build a committed revision with `nyl build {name} --source <revision> -- --shared-catalog --push`",
                    name = unit.unit.name,
                )));
            }
            if ctx.commit_identity.is_none() {
                return Err(DriverError::before_effects(format!(
                    "{IDENTITY}: --push commits to the publication branch and needs an identity to author the commit; \
                     set user.name and user.email in the repository's Git configuration"
                )));
            }
            let message = format!("nyl: build --shared-catalog --push {}", shared.destination.path_prefix);
            match publish_shared_catalog(ctx, unit, shared, &message)? {
                Ok(published) => Ok(vec![document(
                    unit,
                    "PublishedTree",
                    "tree",
                    json!({
                        "repository": published.repository,
                        "branch": published.branch,
                        "pathPrefix": published.path_prefix,
                        "target": label,
                        "commit": published.commit,
                        "indexDigest": published.index_digest,
                    }),
                )?]),
                Err(Outcome::Failed { message, .. }) => Err(DriverError::before_effects(message)),
                Err(other) => Err(DriverError::after_effects(format!(
                    "the publication ended without a result: {other:?}"
                ))),
            }
        }
    }
}

/// What a shared catalog's documents name as their `target`: its owner, `catalog:<name>`.
fn shared_label(shared: &RenderedTarget) -> String {
    shared
        .destination
        .path_prefix
        .rsplit('/')
        .next()
        .map_or_else(String::new, |name| format!("catalog:{name}"))
}

/// Refuse `--push` unless publishing from this call is the only claim on the
/// target. A direct publication is outside orchestration, so a target an
/// environment owns must go through `reconcile`, which holds the lease and
/// records the receipt. Ownership is read from both places that declare it:
///
/// - the declaration: a publication unit an environment selects places its
///   target into that environment, even a unit being torn down (checked here,
///   first, so the most useful refusal comes before any about the author);
/// - the published ownership index: a prefix that names a unit incarnation
///   refuses a publication that records no owner (`NYL-TREE-OWNED-BY-UNIT`,
///   found when the publication prepares, after the build's own render).
///
/// The source must also be committed: the index attributes the tree to a
/// commit, which a worktree with uncommitted changes does not reproduce.
/// Last, the commit needs an author.
fn refuse_owned(
    catalogue: &nyl_render::config::ResourceCatalogue,
    ctx: &ExecutionContext<'_>,
    unit: &DesiredUnit,
    target: &str,
) -> DriverResult<()> {
    let inventory = nyl_render::gitops::discover_gitops_inventory(&ctx.project_dir, None, catalogue.clone())
        .map_err(|error| DriverError::before_effects(format!("cannot discover the project: {error}")))?
        .with_instances(ctx.instances.clone());
    let placements = nyl_render::gitops::units::target_placements(&inventory).map_err(|error| {
        DriverError::before_effects(format!("cannot read which environments place targets: {error}"))
    })?;
    if let Some(environment) = placements.get(target) {
        return Err(DriverError::before_effects(format!(
            "{TARGET_OWNED}: DeploymentTarget {target} is owned by Environment {environment}, whose publication unit places it; \
             a direct build cannot publish it. Publish it with `nyl reconcile -e {environment}`, or deselect the unit from \
             that environment first"
        )));
    }
    if ctx.dirty_source {
        return Err(DriverError::before_effects(format!(
            "{DIRTY_SOURCE}: the worktree has uncommitted changes, so the tree cannot be attributed to the source commit; \
             commit them, or build a committed revision with `nyl build {name} --source <revision> -- --push`",
            name = unit.unit.name,
        )));
    }
    if ctx.commit_identity.is_none() {
        return Err(DriverError::before_effects(format!(
            "{IDENTITY}: --push commits to the publication branch and needs an identity to author the commit; \
             set user.name and user.email in the repository's Git configuration"
        )));
    }
    Ok(())
}

/// The artifact document shape of a build's output; nothing records it.
fn document(unit: &DesiredUnit, kind: &str, name: &str, spec: Value) -> DriverResult<ArtifactDocument> {
    Ok(ArtifactDocument {
        api_version: API_VERSION_ARTIFACTS.to_owned(),
        kind: kind.to_owned(),
        metadata: ArtifactMetadata {
            name: name.to_owned(),
            unit: ArtifactUnit {
                environment: unit.unit.environment.clone(),
                name: unit.unit.name.clone(),
                uid: unit
                    .unit
                    .uid
                    .clone()
                    .ok_or_else(|| DriverError::before_effects("a built unit has a uid"))?,
            },
        },
        execution_key: unit
            .execution_key
            .clone()
            .ok_or_else(|| DriverError::before_effects("a built unit has an execution key"))?,
        spec,
    })
}

/// A file's path as the publication branch names it.
fn branch_path(prefix: &str, relative: &std::path::Path) -> DriverResult<String> {
    let relative = nyl_core::resources::relative_path_to_posix("rendered output path", relative)
        .map_err(|error| DriverError::before_effects(error.to_string()))?;
    Ok(match prefix.trim_end_matches('/') {
        "" => relative,
        prefix => format!("{prefix}/{relative}"),
    })
}

/// The `RenderedTree` document: where the tree publishes and the digest of
/// every file, with the digest the ownership index would have.
fn rendered_tree(unit: &DesiredUnit, target: &str, rendered: &RenderedTarget) -> DriverResult<ArtifactDocument> {
    let destination = &rendered.destination;
    let index = rendered
        .files
        .get(std::path::Path::new(DEFAULT_INDEX_PATH))
        .ok_or_else(|| DriverError::before_effects("the render has no ownership index"))?;
    let index = parse_index(index, DEFAULT_INDEX_PATH)
        .and_then(|index| index_digest(&index))
        .map_err(|error| DriverError::before_effects(format!("cannot digest the ownership index: {error}")))?;
    let mut files = serde_json::Map::new();
    for (path, bytes) in &rendered.files {
        if path.as_path() == std::path::Path::new(DEFAULT_INDEX_PATH) {
            continue;
        }
        files.insert(
            branch_path(&destination.path_prefix, path)?,
            Value::String(nyl_core::digest::sha256_prefixed(bytes)),
        );
    }
    document(
        unit,
        "RenderedTree",
        "tree",
        json!({
            "repository": destination.repository,
            "branch": destination.branch,
            "pathPrefix": destination.path_prefix,
            "target": target,
            "indexDigest": index,
            "files": files,
        }),
    )
}

/// The `TreeDiff` document: what publishing the render would change.
/// `changes` is `none` for a destination that already holds the tree,
/// `create` before anything is published at the prefix, and `update`
/// otherwise; a pipeline reads it from the printed document.
fn tree_diff(
    ctx: &ExecutionContext<'_>,
    unit: &DesiredUnit,
    target: &str,
    rendered: &RenderedTarget,
) -> DriverResult<ArtifactDocument> {
    // Credential variables come from the build's admitted environment, never the process.
    let variable = |name: &str| ctx.env.get(name).cloned();
    let diff = diff_target_publication(rendered, &ctx.project_dir, &variable).map_err(|error| {
        DriverError::before_effects(format!(
            "cannot compare the rendered tree with {}@{}: {error}",
            rendered.destination.repository, rendered.destination.branch
        ))
    })?;
    let changes = if diff.files.is_empty() {
        "none"
    } else if diff.published {
        "update"
    } else {
        "create"
    };
    let mut files = Vec::new();
    for (path, change) in &diff.files {
        files.push(json!({
            "path": branch_path(&rendered.destination.path_prefix, path)?,
            "change": FileChange::as_str(*change),
        }));
    }
    let destination = &rendered.destination;
    document(
        unit,
        "TreeDiff",
        "diff",
        json!({
            "repository": destination.repository,
            "branch": destination.branch,
            "pathPrefix": destination.path_prefix,
            "target": target,
            "base": diff.base,
            "changes": changes,
            // A unit incarnation as an object, a shared catalog as `catalog:<name>`.
            "owner": diff.published_owner.as_ref().map(|owner| json!(owner)),
            "files": files,
        }),
    )
}
