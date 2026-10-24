//! The narrow rendering API of Kubernetes publication units: render one
//! DeploymentTarget with a pinned input snapshot, and publish the tree.
//!
//! Contract: [Kubernetes publication unit](../../../../design/orchestration-core.md#kubernetes-publication-unit)
//! and [Artifacts](../../../../design/orchestration-core.md#artifacts)
//! (`PublishedTree`). `FakePublication` uses it in M3 and the real
//! `KubernetesPublication` driver in M5. Rendering never learns about
//! environments, receipts, or state refs; the caller passes the resolved
//! values as an [`InputSnapshot`].
//!
//! These functions are synchronous and safe to call from any thread: the
//! asynchronous compile runs on a runtime of its own.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use nyl_core::orchestration::ports::CommitIdentity;
use nyl_core::orchestration::time::Timestamp;

use super::inputs::InputSnapshot;
use crate::Result;

/// One orchestrated render.
#[derive(Debug, Clone)]
pub struct TargetRender<'a> {
    /// A pinned checkout of the source commit.
    pub worktree: &'a Path,
    /// The directory containing `nyl.toml` inside `worktree`.
    pub project_dir: &'a Path,
    /// The DeploymentTarget's name.
    pub target: &'a str,
    /// Resolved values of the target's `fromUnit` (and later `fromPromotion`) bindings.
    pub inputs: &'a InputSnapshot,
    /// The render cache directory, from the invocation's workspace; `None`
    /// renders without a cache. Never the user's default cache, so parallel
    /// invocations and tests stay isolated and the result never depends on
    /// host state.
    pub cache_dir: Option<&'a Path>,
}

/// Who signs a publication commit, and when. Orchestrated publications pass
/// the identity the command line resolved for state commits and the injected
/// clock's time; `publish-tree` passes the signature it resolves itself, so
/// its commits stay as they are. This module reads neither the process
/// environment, Git configuration, nor the clock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicationSignature {
    pub identity: CommitIdentity,
    pub time: Timestamp,
}

/// Where a target publishes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicationDestination {
    /// The publication repository's credential-free URL.
    pub repository: String,
    /// The URL pushes go to (`publishURL`, else `repoURL`).
    pub push_url: String,
    pub branch: String,
    pub path_prefix: String,
}

/// A rendered target tree.
#[derive(Debug, Clone, PartialEq)]
pub struct RenderedTarget {
    pub destination: PublicationDestination,
    /// Files beneath the prefix, including `_nyl/index.json`, by path
    /// relative to the prefix.
    pub files: BTreeMap<PathBuf, Vec<u8>>,
    /// The render's input record in portable form: repository-relative paths
    /// (and other recorded inputs) with plain content digests; never machine
    /// paths, cache locations, or cache-local keyed fingerprints. Rendering
    /// the same commit from two worktrees with two cache directories yields
    /// the same record.
    pub input_record: BTreeMap<String, String>,
}

/// Render `request.target` at `request.worktree` with `request.inputs` as
/// [`super::inputs::OrchestratedInputs::Pinned`].
pub fn render_target(request: &TargetRender<'_>) -> Result<RenderedTarget> {
    let _ = request;
    todo!("publication")
}

/// A published tree: the `PublishedTree` artifact's spec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedTree {
    pub repository: String,
    pub branch: String,
    pub path_prefix: String,
    /// The publication commit; the branch head when the tree was unchanged.
    pub commit: String,
    /// `sha256:<hex>` over the canonical JSON of `<prefix>/_nyl/index.json`.
    pub index_digest: String,
}

/// Publish a rendered tree to its destination branch by compare-and-swap,
/// reconciling only files the previous ownership index recorded, as
/// `publish-tree` does. Publishing an unchanged tree creates no commit.
/// `project_dir` supplies the project's `[git]` credentials; `signature`
/// signs the commit.
///
/// Index v3 (owning environment and unit uid) is deferred to the
/// KubernetesPublication driver (M5): the index is written as `publish-tree`
/// writes it, so non-orchestrated output stays byte-identical.
pub fn publish_target(
    rendered: &RenderedTarget,
    project_dir: &Path,
    message: &str,
    signature: &PublicationSignature,
) -> Result<PublishedTree> {
    let _ = (rendered, project_dir, message, signature);
    todo!("publication")
}
