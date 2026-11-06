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
//!
//! `publish-tree` and orchestrated publications share one publication
//! implementation, [`prepare_publication`]: a compare-and-swap push of a
//! commit that reconciles only the files the previous ownership index
//! recorded, refusing owned files modified outside Nyl, and making no commit
//! for an unchanged tree.
//!
//! A publication's two-phase teardown uses the same path:
//! [`without_workload_applications`] gives phase 1's tree, published with
//! [`prepare_target_publication`], and [`prepare_target_removal`] removes
//! the index-owned files in phase 2. Trees published without orchestration
//! use versioned indexes with portable Release ownership and provenance.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use git2::build::CheckoutBuilder;
use git2::{IndexAddOption, Oid, Repository, ResetType, StatusOptions};

use nyl_core::orchestration::ports::CommitIdentity;
use nyl_core::orchestration::time::Timestamp;

use super::inputs::{verify_publication_base, InputSnapshot, OrchestratedInputs, PublicationBase};
use super::reconcile::{
    parse_index, read_index_owner, reconcile_rendered_tree_with_options, DesiredFiles, ReconcileOptions, RenderIndex,
    RenderIndexOwner, RenderIndexPublication, DEFAULT_INDEX_PATH,
};
use super::units::InlineTarget;
use super::{
    compile_target_tree_recorded, CacheMode, CompiledTargetTree, GitOpsCache, GitOpsInventory, TargetDependencies,
    TreeRenderOptions,
};
use crate::config::{ProjectConfig, ProjectFile};
use crate::{NylError, Result};
use nyl_git::GitAuth;

/// One orchestrated render.
#[derive(Debug, Clone)]
pub struct TargetRender<'a> {
    /// A pinned checkout of the source commit.
    pub worktree: &'a Path,
    /// The directory containing `nyl.toml` inside `worktree`.
    pub project_dir: &'a Path,
    /// The DeploymentTarget's name.
    pub target: &'a str,
    /// The target's declaration when the publication unit declares it
    /// inline: the target is then not discovered but added to the inventory
    /// under `target`, as every other target is compiled from there.
    pub inline: Option<&'a InlineTarget>,
    /// The commit the ownership index records as the tree's source. `None`
    /// reads the head of the worktree's repository, which is the source
    /// commit for a run's checkout; a direct build, whose worktree is a
    /// snapshot of files, names the commit the files stand for.
    pub source_commit: Option<&'a str>,
    /// The orchestration unit incarnation that publishes the tree; the
    /// ownership index then records it and fences the prefix.
    pub owner: Option<RenderIndexOwner>,
    /// Scoped replacement authority; empty selects the complete target.
    pub selection: super::tree::partial::PublicationSelection,
    /// Optional pinned destination base, shared with the caller's input snapshot.
    pub publication_base: Option<PublicationBase>,
    /// Resolved values of the target's `fromUnit` (and later `fromPromotion`) bindings.
    pub inputs: &'a InputSnapshot,
    /// The render cache directory, from the invocation's workspace; `None`
    /// renders without a cache. Never the user's default cache, so parallel
    /// invocations and tests stay isolated and the result never depends on
    /// host state.
    pub cache_dir: Option<&'a Path>,
    /// Resource APIs registered by the caller.
    pub resource_catalogue: crate::config::ResourceCatalogue,
    /// The instances of EnvironmentTemplates the caller knows, whose inline
    /// targets join the compile's checks across targets like every declared
    /// environment's.
    pub instances: &'a nyl_core::orchestration::instance::Instances,
}

/// Who authors and commits a publication commit, and when. Orchestrated publications pass
/// the identity the command line resolved for state commits and the injected
/// clock's time; `publish-tree` passes the author it resolves itself, so
/// its commits stay as they are. This module reads neither the process
/// environment, Git configuration, nor the clock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicationAuthor {
    pub identity: CommitIdentity,
    pub time: Timestamp,
}

impl PublicationAuthor {
    /// The Git signature: this identity at this time, in UTC.
    fn git(&self) -> Result<git2::Signature<'static>> {
        Ok(git2::Signature::new(
            &self.identity.name,
            &self.identity.email,
            &git2::Time::new(self.time.timestamp(), 0),
        )
        .map_err(crate::git::GitError::from)?)
    }
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
    pub report: super::tree::partial::PublicationReport,
    /// Files beneath the prefix, including `_nyl/index.json`, by path
    /// relative to the prefix.
    pub files: BTreeMap<PathBuf, Vec<u8>>,
    /// The render's input record in portable form, built from the compile's
    /// dependency recorder: every file, resource, template context value,
    /// resolved Release input and source identity, Kubernetes capability,
    /// and renderer tool fingerprint the render read, with plain content
    /// digests. It never names machine paths or cache locations and holds no
    /// cache-local keyed fingerprint, so rendering the same commit from two
    /// worktrees with two cache directories yields the same record.
    ///
    /// Files in the worktree are keyed by worktree-rooted path, such as
    /// `/nyl/platform.yaml`; files of remote sources and other cached
    /// checkouts by `@sources/<path>` or `@cache/<path>` beneath the cache;
    /// every other recorded input by its recorder name prefixed with `@`,
    /// such as `@resource:Cluster/dev` or `@tool:helm`.
    pub input_record: BTreeMap<String, String>,
    /// False when the render read an input it could not observe completely.
    /// The record then does not cover the render, and a caller keying work
    /// on it must treat the render as changed every time.
    pub cacheable: bool,
    /// How publication reconciles the tree: carried `fromPublication` state
    /// files are adopted and committed ones released, as `publish-tree` does.
    pub reconcile: ReconcileOptions,
    /// The publication branch head the render's `fromPublication` bindings
    /// read; publishing verifies the branch is still there.
    pub publication_base: Option<PublicationBase>,
    /// The target Cluster's recorded Kubernetes capabilities, which the
    /// cleanup-scope check uses to resolve resource scope.
    pub cluster_kubernetes: Option<crate::resources::ClusterKubernetesCapabilities>,
    /// The target's Argo CD instance and how Nyl reads it. Declaration only:
    /// it enters no rendered byte, and a driver that observes records it at
    /// resolution ([`super::observation::ArgoCdObservation`]).
    pub argocd: super::observation::ArgoCdLocation,
    /// The target's workload Cluster, where its rendered resources live, and
    /// that Cluster's local context. Declaration only, like `argocd`: it
    /// enters no rendered byte, and a driver that reads resources from the
    /// cluster records it at resolution.
    pub workload: super::observation::WorkloadCluster,
    /// The tree of the shared catalog Application this target's
    /// `catalogApplication.shared` names: its manifest and an index owned by
    /// `catalog:<name>`, at the reserved prefix `_nyl/shared/<name>` of the
    /// same publication branch. It is a tree of its own, never part of
    /// `files`, and published by [`prepare_shared_catalog`] under a rule of its own.
    pub shared_catalog: Option<Box<RenderedTarget>>,
}

impl RenderedTarget {
    /// The values this render read from state, as its ownership index
    /// records them: the digests of every resolved Release input
    /// (`@input/…`), of `fromPublication` state files read from the base
    /// commit (`@publication/…`), and of carried ones (`@carried/…`). Two
    /// renders with equal state inputs read the same state.
    pub fn state_inputs(&self) -> Result<BTreeMap<String, String>> {
        let bytes = self.files.get(Path::new(DEFAULT_INDEX_PATH)).ok_or_else(|| {
            NylError::config(format!(
                "Rendered target tree for {} has no {DEFAULT_INDEX_PATH}",
                self.destination.repository
            ))
        })?;
        let index = parse_index(bytes, DEFAULT_INDEX_PATH)?;
        Ok(index
            .inputs
            .into_iter()
            .filter(|(key, _)| {
                [
                    super::inputs::INDEX_INPUT_PREFIX,
                    super::inputs::INDEX_PUBLICATION_PREFIX,
                    super::inputs::INDEX_CARRIED_PREFIX,
                ]
                .iter()
                .any(|prefix| key.starts_with(prefix))
            })
            .collect())
    }
}

/// Render `request.target` at `request.worktree` with `request.inputs` as
/// [`super::inputs::OrchestratedInputs::Pinned`].
///
/// The compile runs on a dedicated thread with a current-thread runtime of
/// its own, so this is callable from synchronous code and from inside an
/// asynchronous runtime alike.
pub fn render_target(request: &TargetRender<'_>) -> Result<RenderedTarget> {
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
                runtime.block_on(render_target_on_runtime(request))
            })
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
    })
}

async fn render_target_on_runtime(request: &TargetRender<'_>) -> Result<RenderedTarget> {
    let project_dir = if request.project_dir.starts_with(request.worktree) {
        request.project_dir.to_path_buf()
    } else {
        // A worktree-rooted project directory, such as `/nyl`.
        let relative = request.project_dir.strip_prefix("/").unwrap_or(request.project_dir);
        request.worktree.join(relative)
    };
    let mut inventory = super::discover_gitops_inventory(&project_dir, None, request.resource_catalogue.clone())?
        .with_instances(request.instances.clone());
    // Every environment's inline targets join the inventory, so the compile's
    // checks across targets see them whichever unit renders. The target this
    // render is for comes from the document it was given.
    inventory =
        super::units::with_inline_targets_except(&inventory, request.inline.map(|inline| inline.name.as_str()))?;
    if let Some(inline) = request.inline {
        let spec = pinned_inline_spec(&inline.spec, request.inputs);
        inventory = inventory.with_inline_target(request.target, &inline.source_file, &spec)?;
    }
    super::resolve_deployment_target_name(&inventory, Some(request.target))?;

    // A render without a cache directory still keeps its source checkouts
    // in a temporary directory rather than the user's default cache.
    let temporary = tempfile::TempDir::new()?;
    let cache = match request.cache_dir {
        Some(directory) => {
            GitOpsCache::with_root(directory, CacheMode::Default)?.with_source_cache_root(directory.join("sources"))
        }
        None => GitOpsCache::with_root(temporary.path(), CacheMode::Disabled)?,
    };
    let options = TreeRenderOptions {
        selection: request.selection.clone(),
        publication_base: request.publication_base.clone(),
        orchestrated_inputs: Some(OrchestratedInputs::Pinned(request.inputs.clone())),
        ..TreeRenderOptions::default()
    };
    let (compiled, dependencies) = compile_target_tree_recorded(&inventory, request.target, &cache, options).await?;

    let source_commit = request.source_commit.map(str::to_owned).or_else(|| {
        Repository::discover(&inventory.worktree_root)
            .ok()
            .and_then(|repository| repository.head().ok().and_then(|head| head.target()))
            .map(|oid| oid.to_string())
    });
    let mut index = target_render_index(&inventory, &compiled, request.target, source_commit, false)?;
    if let Some(owner) = &request.owner {
        index = index.owned_by(owner.clone());
    }
    let desired = compiled.owned_files();
    index.files = desired
        .iter()
        .map(|(path, bytes)| {
            nyl_core::resources::relative_path_to_posix("rendered output path", path)
                .map(|path| (path, nyl_core::digest::sha256_hex(bytes)))
                .map_err(NylError::from)
        })
        .collect::<Result<_>>()?;
    // Recorded paths are canonical; a root the render never created holds
    // no recorded file.
    let canonical = |path: &Path| path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let mut roots = vec![(canonical(&inventory.worktree_root), "/")];
    if let Some(sources) = cache.external_cache_root() {
        roots.push((canonical(sources), "@sources/"));
    }
    roots.push((canonical(request.cache_dir.unwrap_or(temporary.path())), "@cache/"));
    let (input_record, cacheable) = portable_input_record(&dependencies, &roots);
    let mut files = desired
        .iter()
        .map(|(path, bytes)| (path.to_path_buf(), bytes.to_vec()))
        .collect::<BTreeMap<_, _>>();
    let mut index_bytes = serde_json::to_vec_pretty(&index)?;
    index_bytes.push(b'\n');
    files.insert(PathBuf::from(DEFAULT_INDEX_PATH), index_bytes);

    let revision = &compiled.target.spec.publication.revision;
    let mut rendered = RenderedTarget {
        report: compiled.publication_report.clone(),
        destination: PublicationDestination {
            repository: nyl_git::sanitize_url(&compiled.repository.repo_url),
            push_url: compiled
                .repository
                .publish_url
                .clone()
                .unwrap_or_else(|| compiled.repository.repo_url.clone()),
            branch: revision.strip_prefix("refs/heads/").unwrap_or(revision).to_owned(),
            path_prefix: compiled.target.publication_path_prefix().to_owned(),
        },
        files,
        input_record,
        cacheable,
        reconcile: compiled.reconcile_options(false),
        publication_base: compiled.publication_base.clone(),
        cluster_kubernetes: compiled.cluster.spec.kubernetes.clone(),
        argocd: super::tree::argocd_location(&inventory, &compiled.target)?,
        workload: super::tree::workload_cluster(&inventory, &compiled.target)?,
        shared_catalog: None,
    };
    if let Some(shared) = &compiled.shared_catalog {
        rendered.shared_catalog = Some(Box::new(shared.tree(&rendered)?));
    }
    Ok(rendered)
}

/// An inline target's spec without the Release input bindings the pinned
/// `inputs` supply. A unit's reference resolution replaces each `fromUnit`
/// binding in the stored spec with the value it resolved, which is no
/// binding any more; the render takes that value from the snapshot instead,
/// where an input's value takes an override's precedence over its binding.
fn pinned_inline_spec(spec: &serde_json::Value, inputs: &InputSnapshot) -> serde_json::Value {
    let mut spec = spec.clone();
    if let Some(bindings) = spec.get_mut("releaseInputs").and_then(serde_json::Value::as_object_mut) {
        for (release, pinned) in inputs {
            if let Some(release_bindings) = bindings.get_mut(release).and_then(serde_json::Value::as_object_mut) {
                release_bindings.retain(|input, _| !pinned.contains_key(input));
            }
        }
        bindings.retain(|_, release| release.as_object().is_none_or(|release| !release.is_empty()));
    }
    spec
}

/// The compile's dependency record in portable form (see
/// [`RenderedTarget::input_record`]), and whether it covers the render.
///
/// `roots` maps machine directories to the portable prefix their files take,
/// most specific first after sorting. A recorded path beneath none of them
/// cannot be named portably; it is left out and the record marked as not
/// covering the render. Keyed fingerprints (`secret` entries) are left out:
/// orchestrated renders admit neither project secrets nor the process
/// environment, so they fingerprint empty values, and their keys are local
/// to one cache.
fn portable_input_record(
    dependencies: &TargetDependencies,
    roots: &[(PathBuf, &str)],
) -> (BTreeMap<String, String>, bool) {
    let mut roots = roots.to_vec();
    roots.sort_by_key(|(path, _)| std::cmp::Reverse(path.as_os_str().len()));
    let portable_path = |path: &str| {
        let path = Path::new(path);
        roots.iter().find_map(|(root, prefix)| {
            let relative = path.strip_prefix(root).ok()?;
            Some(format!("{prefix}{}", relative.to_string_lossy().replace('\\', "/")))
        })
    };
    let mut cacheable = dependencies.cacheable;
    let mut record = BTreeMap::new();
    for (name, dependency) in &dependencies.dependencies {
        if dependency.kind == "secret" {
            continue;
        }
        let key = if let Some(path) = name.strip_prefix("file:") {
            portable_path(path)
        } else if let Some(path) = name.strip_prefix("directory:") {
            portable_path(path).map(|path| format!("@directory:{path}"))
        } else if let Some(path) = name.strip_prefix("vendor-lock:") {
            portable_path(path).map(|path| format!("@vendor-lock:{path}"))
        } else {
            Some(format!("@{name}"))
        };
        match key {
            Some(key) => {
                record.insert(key, dependency.digest.clone());
            }
            None => cacheable = false,
        }
    }
    (record, cacheable)
}

/// The ownership index of a compiled target before its files are recorded:
/// its owner (target, cluster, publication), provenance, and input digests.
/// Reconciliation fills in the file digests.
pub fn target_render_index(
    inventory: &GitOpsInventory,
    compiled: &CompiledTargetTree,
    target_name: &str,
    source_commit: Option<String>,
    source_dirty: bool,
) -> Result<RenderIndex> {
    let inputs = super::provenance::hash_inputs(inventory, compiled)?;
    let repository_identity = compiled
        .repository_name
        .clone()
        .unwrap_or_else(|| compiled.repository.repo_url.clone());
    let mut index = RenderIndex::new(
        target_name.to_owned(),
        compiled.cluster.metadata.name.clone(),
        RenderIndexPublication {
            repository: repository_identity,
            revision: compiled.target.spec.publication.revision.clone(),
            path_prefix: compiled.target.publication_path_prefix().to_owned(),
        },
        source_commit.clone(),
        source_dirty,
        inputs.clone(),
    );
    if let Some(snapshot) = &compiled.publication_snapshot {
        index.version = super::reconcile::COMPILED_RENDER_INDEX_VERSION;
        index.compilation = Some(super::tree::partial::stamp_snapshot(
            snapshot,
            source_commit.as_deref(),
            source_dirty,
            &inputs,
        )?);
        // Mixed trees have no single rendering source revision.
        if !snapshot
            .releases
            .values()
            .all(|release| release.source_commit.is_none())
        {
            index.source_commit = None;
            index.dirty = source_dirty || snapshot.releases.values().any(|release| release.dirty);
        }
    }
    Ok(index)
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
/// `project_dir` supplies the project's `[git]` credentials, whose
/// variables are read only through `variable`: an orchestrated publication
/// passes a lookup over the execution's admitted environment, `publish-tree`
/// one over the process environment. `author` is the commit's author and committer.
///
/// The published ownership index is the one `rendered` carries: it names an
/// owner only when the render was given one (an orchestrated publication), so
/// `publish-tree` output stays byte-identical.
pub fn publish_target(
    rendered: &RenderedTarget,
    project_dir: &Path,
    message: &str,
    author: &PublicationAuthor,
    variable: &dyn Fn(&str) -> Option<String>,
) -> Result<PublishedTree> {
    prepare_target_publication(rendered, project_dir, message, author, variable)?.push()
}

/// [`publish_target`] in two phases: everything before the push, which has no
/// effect outside a temporary checkout, and the push. A driver reports a
/// failure of the first phase as one before effects.
pub fn prepare_target_publication(
    rendered: &RenderedTarget,
    project_dir: &Path,
    message: &str,
    author: &PublicationAuthor,
    variable: &dyn Fn(&str) -> Option<String>,
) -> Result<PreparedTarget> {
    prepare_target_publication_fenced(rendered, project_dir, message, author, variable, None)
}

/// [`prepare_target_publication`] with the decision whether the tree's owner
/// may take the prefix over from another owner: owner fencing. Without one,
/// a prefix owned by another incarnation refuses with
/// [`NylError::OwnerFenced`].
pub fn prepare_target_publication_fenced(
    rendered: &RenderedTarget,
    project_dir: &Path,
    message: &str,
    author: &PublicationAuthor,
    variable: &dyn Fn(&str) -> Option<String>,
    takeover: Option<&OwnerTakeover<'_>>,
) -> Result<PreparedTarget> {
    let auth = project_git_auth(&load_project_file(project_dir)?, variable)?;
    let branch = writable_branch_name(&rendered.destination.branch)?;
    let index_bytes = rendered.files.get(Path::new(DEFAULT_INDEX_PATH)).ok_or_else(|| {
        NylError::config(format!(
            "Rendered target tree for {} has no {DEFAULT_INDEX_PATH}",
            rendered.destination.repository
        ))
    })?;
    let index = parse_index(index_bytes, DEFAULT_INDEX_PATH)?;
    let desired: DesiredFiles<'_> = rendered
        .files
        .iter()
        .filter(|(path, _)| path.as_path() != Path::new(DEFAULT_INDEX_PATH))
        .map(|(path, bytes)| (path.as_path(), bytes.as_slice()))
        .collect();
    let prepared = prepare_publication(
        &TreePublication {
            push_url: &rendered.destination.push_url,
            branch,
            path_prefix: &rendered.destination.path_prefix,
            desired: &desired,
            index,
            options: rendered.reconcile.clone(),
            ignore_project_scope_contraction: false,
            cluster_kubernetes: rendered.cluster_kubernetes.as_ref(),
            publication_base: rendered.publication_base.as_ref(),
            takeover,
            auth: &auth,
        },
        message,
        || Ok(author.clone()),
    )?;
    prepared.verify_unmoved(&auth)?;
    Ok(PreparedTarget {
        prepared,
        auth,
        destination: rendered.destination.clone(),
    })
}

/// A target publication ready to push.
pub struct PreparedTarget {
    prepared: PreparedPublication,
    auth: GitAuth,
    destination: PublicationDestination,
}

impl PreparedTarget {
    /// Whether the destination branch already holds exactly this tree, so
    /// pushing would create no commit.
    pub fn is_unchanged(&self) -> bool {
        self.prepared.commit().is_none()
    }

    /// Push the commit, if any, and describe the published tree.
    pub fn push(self) -> Result<PublishedTree> {
        let commit = self.prepared.head()?;
        self.prepared.push(&self.auth)?;
        Ok(PublishedTree {
            repository: self.destination.repository,
            branch: self.prepared.branch.clone(),
            path_prefix: self.destination.path_prefix,
            commit: commit.to_string(),
            index_digest: index_digest(&self.prepared.index)?,
        })
    }
}

/// How a rendered file differs from the published tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum FileChange {
    /// The branch holds no such file; publishing creates it.
    Added,
    /// The branch holds other bytes; publishing replaces them.
    Modified,
    /// The previous ownership index records the file and the render no
    /// longer produces it; publishing removes it.
    Removed,
}

impl FileChange {
    /// The name the `TreeDiff` document gives the change.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Modified => "modified",
            Self::Removed => "removed",
        }
    }
}

/// What publishing a rendered target would change, read from the
/// destination branch and never written to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetDiff {
    /// The branch head the render was compared with; `None` before the
    /// branch exists.
    pub base: Option<String>,
    /// The unit incarnation the published ownership index names, if any.
    pub published_owner: Option<RenderIndexOwner>,
    /// Whether the branch holds an ownership index for the prefix.
    pub published: bool,
    /// Every file that differs, by path relative to the prefix, in path order.
    pub files: BTreeMap<PathBuf, FileChange>,
}

/// Compare `rendered` with the tree published at its destination: the files
/// beneath the prefix, except the ownership index, whose content follows from
/// them, against the bytes the branch holds, and the files the published
/// index owns that the render no longer produces. Unowned files in the
/// prefix, such as state files other tools commit, are not part of the tree.
///
/// This reads the destination and writes nothing, so it applies to a prefix
/// another owner holds as well: only publishing is fenced.
pub fn diff_target_publication(
    rendered: &RenderedTarget,
    project_dir: &Path,
    variable: &dyn Fn(&str) -> Option<String>,
) -> Result<TargetDiff> {
    let auth = project_git_auth(&load_project_file(project_dir)?, variable)?;
    let branch = writable_branch_name(&rendered.destination.branch)?;
    let push_url = &rendered.destination.push_url;
    let prefix = rendered.destination.path_prefix.as_str();
    let scratch = tempfile::TempDir::new()?;
    let repository = Repository::init(scratch.path())
        .and_then(|repository| {
            repository.remote("origin", push_url)?;
            Ok(repository)
        })
        .map_err(|error| NylError::config(format!("Failed to read {}: {error}", nyl_git::sanitize_url(push_url))))?;
    fetch_branch(&repository, push_url, branch, &auth)?;
    let base = remote_branch_oid(&repository, branch);
    let desired: BTreeMap<&Path, &[u8]> = rendered
        .files
        .iter()
        .filter(|(path, _)| path.as_path() != Path::new(DEFAULT_INDEX_PATH))
        .map(|(path, bytes)| (path.as_path(), bytes.as_slice()))
        .collect();
    let mut diff = TargetDiff {
        base: base.map(|oid| oid.to_string()),
        published_owner: None,
        published: false,
        files: BTreeMap::new(),
    };
    let Some(base) = base else {
        diff.files = desired
            .keys()
            .map(|path| (path.to_path_buf(), FileChange::Added))
            .collect();
        return Ok(diff);
    };
    let tree = repository
        .find_commit(base)
        .and_then(|commit| commit.tree())
        .map_err(crate::git::GitError::from)?;
    for (path, bytes) in &desired {
        let relative =
            nyl_core::resources::relative_path_to_posix("rendered output path", path).map_err(NylError::from)?;
        match read_tree_file(&repository, &tree, prefix, &relative)? {
            None => {
                diff.files.insert(path.to_path_buf(), FileChange::Added);
            }
            Some(published) if published != *bytes => {
                diff.files.insert(path.to_path_buf(), FileChange::Modified);
            }
            Some(_) => {}
        }
    }
    if let Some(index) = read_tree_file(&repository, &tree, prefix, DEFAULT_INDEX_PATH)? {
        let index = parse_index(&index, DEFAULT_INDEX_PATH)?;
        diff.published = true;
        for owned in index.files.keys() {
            let path = PathBuf::from(owned);
            // A path the render releases from ownership stays published by design.
            if !desired.contains_key(path.as_path()) && !rendered.reconcile.release.contains(&path) {
                diff.files.insert(path, FileChange::Removed);
            }
        }
        diff.published_owner = index.owner;
    }
    Ok(diff)
}

/// What the publication branch holds of a shared catalog, compared with the
/// tree a target renders for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SharedCatalogState {
    /// The branch holds no tree at the catalog's prefix, so a target's
    /// publication creates it.
    Missing,
    /// The branch holds exactly the manifest this render would write.
    Current,
    /// The branch holds another manifest, which another Nyl version or
    /// another declaration of the catalog wrote. A target's publication never
    /// overwrites it: the manifest has one writer, `nyl build -- --shared-catalog --push`.
    Differs(Vec<PathBuf>),
}

/// Compare `shared`, a target's shared catalog tree ([`RenderedTarget::shared_catalog`]),
/// with the publication branch, reading it and writing nothing. A prefix whose
/// index names another owner than the catalog refuses with
/// [`NylError::OwnerFenced`], so nothing mistakes a unit's tree for the catalog's.
///
/// Contract: [Catalog ownership](../../../../design/orchestration-core.md#kubernetes-publication-unit).
pub fn shared_catalog_state(
    shared: &RenderedTarget,
    project_dir: &Path,
    variable: &dyn Fn(&str) -> Option<String>,
) -> Result<SharedCatalogState> {
    let diff = diff_target_publication(shared, project_dir, variable)?;
    let index_bytes = shared
        .files
        .get(Path::new(DEFAULT_INDEX_PATH))
        .ok_or_else(|| NylError::config("the shared catalog tree has no ownership index"))?;
    let expected = parse_index(index_bytes, DEFAULT_INDEX_PATH)?.owner;
    if let Some(owner) = diff
        .published_owner
        .as_ref()
        .filter(|owner| Some(*owner) != expected.as_ref())
    {
        return Err(NylError::OwnerFenced(format!(
            "NYL-KUBERNETES-OWNER-FENCED: the shared catalog prefix {} is owned by {owner}, not by {}; {}",
            shared.destination.path_prefix,
            expected.map_or_else(String::new, |owner| owner.to_string()),
            super::reconcile::OWNED_PREFIX_FIX
        )));
    }
    Ok(if !diff.published {
        SharedCatalogState::Missing
    } else if diff.files.is_empty() {
        SharedCatalogState::Current
    } else {
        SharedCatalogState::Differs(diff.files.into_keys().collect())
    })
}

/// Make `worktree`, a plain copy of a project's Git-visible files, a Git
/// worktree with those files committed, so discovery and rendering see what
/// they see in a checkout. A directory that is a worktree already is left as
/// it is.
///
/// The commit is deterministic (fixed signature, time 0) and stands for no
/// source commit, which is why a render of such a worktree takes the source
/// commit as a [`TargetRender::source_commit`] instead. The orchestration
/// shell makes a direct operation's snapshot one; drivers never write to the
/// worktree they are given.
pub fn ensure_git_worktree(worktree: &Path) -> Result<()> {
    if worktree.join(".git").exists() {
        return Ok(());
    }
    let git = |error: git2::Error| NylError::from(crate::git::GitError::from(error));
    let repository = Repository::init(worktree).map_err(git)?;
    let mut index = repository.index().map_err(git)?;
    // The snapshot holds exactly what Git showed, files it ignores but
    // tracks included.
    index.add_all(["*"], IndexAddOption::FORCE, None).map_err(git)?;
    index.write().map_err(git)?;
    let tree = repository.find_tree(index.write_tree().map_err(git)?).map_err(git)?;
    let signature = git2::Signature::new("nyl", "nyl@localhost", &git2::Time::new(0, 0)).map_err(git)?;
    repository
        .commit(
            Some("HEAD"),
            &signature,
            &signature,
            "nyl: snapshot of the worktree",
            &tree,
            &[],
        )
        .map_err(git)?;
    Ok(())
}

/// `sha256:<hex>` over the canonical JSON of an ownership index.
pub fn index_digest(index: &RenderIndex) -> Result<String> {
    Ok(format!(
        "sha256:{}",
        nyl_core::digest::sha256_hex(&nyl_core::digest::canonical_json_bytes(index)?)
    ))
}

/// The Git authentication of the project in `project_dir`: its `nyl.toml`'s
/// `[[git.credentials]]`, reading only the variables they name through
/// `variable`.
pub fn project_auth(project_dir: &Path, variable: &dyn Fn(&str) -> Option<String>) -> Result<GitAuth> {
    project_git_auth(&load_project_file(project_dir)?, variable)
}

/// The project's `nyl.toml` in `project_dir`, or the defaults without one.
fn load_project_file(project_dir: &Path) -> Result<ProjectFile> {
    let file = project_dir.join("nyl.toml");
    if file.is_file() {
        ProjectConfig::read_project_file(&file)
    } else {
        Ok(ProjectFile::default())
    }
}

/// Resolve `[[git.credentials]]` from `nyl.toml`, reading only the variables
/// its entries name through `variable`. Without entries, `git` authenticates
/// as configured for the user.
pub fn project_git_auth(config: &ProjectFile, variable: impl Fn(&str) -> Option<String>) -> Result<GitAuth> {
    GitAuth::from_settings(&config.git, variable).map_err(|error| NylError::config(error.to_string()))
}

/// The branch a publication `revision` names; tags and other refs are refused.
pub fn writable_branch_name(revision: &str) -> Result<&str> {
    let branch = revision.strip_prefix("refs/heads/").unwrap_or(revision);
    if branch.is_empty() || revision.starts_with("refs/") && !revision.starts_with("refs/heads/") {
        Err(NylError::config(format!(
            "publish-tree publication revision {revision:?} must name a branch"
        )))
    } else {
        Ok(branch)
    }
}

/// One tree publication: what [`prepare_publication`] commits and where.
pub struct TreePublication<'a> {
    /// The URL fetched from and pushed to.
    pub push_url: &'a str,
    /// The publication branch, as [`writable_branch_name`] returns it.
    pub branch: &'a str,
    pub path_prefix: &'a str,
    /// The owned files, relative to the prefix, without the index.
    pub desired: &'a DesiredFiles<'a>,
    /// The next ownership index; reconciliation records the file digests.
    pub index: RenderIndex,
    pub options: ReconcileOptions,
    /// Explicitly accept cleanup scope contractions after operator review.
    pub ignore_project_scope_contraction: bool,
    /// The target Cluster's recorded Kubernetes capabilities, which resolve
    /// the scope of resources published without a namespace.
    pub cluster_kubernetes: Option<&'a crate::resources::ClusterKubernetesCapabilities>,
    /// The branch head `fromPublication` bindings read, if any.
    pub publication_base: Option<&'a PublicationBase>,
    /// Decides whether this publication's owner (`index.owner`) may take
    /// over a prefix whose index names another owner: `Ok` when that
    /// incarnation is gone, the reason it still holds the prefix otherwise.
    /// Without it, another owner always refuses.
    pub takeover: Option<&'a OwnerTakeover<'a>>,
    pub auth: &'a GitAuth,
}

/// A decision on a prefix owned by another orchestration unit incarnation;
/// see [`TreePublication::takeover`].
pub type OwnerTakeover<'a> = dyn Fn(&RenderIndexOwner) -> std::result::Result<(), String> + 'a;

/// The reconcile options that let `next` take over the prefix beneath
/// `output_root` from another owner, if `takeover` allows it. Nothing
/// changes for a prefix without an owner or one `next` already owns.
fn takeover_options(
    output_root: &Path,
    next: Option<&RenderIndexOwner>,
    takeover: Option<&OwnerTakeover<'_>>,
    mut options: ReconcileOptions,
) -> Result<ReconcileOptions> {
    let (Some(next), Some(takeover)) = (next, takeover) else {
        return Ok(options);
    };
    if let Some(previous) = read_index_owner(output_root)?.filter(|previous| previous != next) {
        takeover(&previous).map_err(NylError::OwnerFenced)?;
        options.take_over_from = Some(previous);
    }
    Ok(options)
}

/// A publication committed in a temporary checkout of its branch, not pushed yet.
pub struct PreparedPublication {
    repository: Repository,
    push_url: String,
    branch: String,
    expected: Option<Oid>,
    commit: Option<Oid>,
    index: RenderIndex,
    // Dropped last: the repository lives inside it.
    _checkout: tempfile::TempDir,
}

/// Check out `request.branch`, reconcile the tree beneath its prefix, and
/// commit it with `message`, authored by what `author` returns. The message
/// lists the generated Applications whose files the commit changes, see
/// [`with_changed_applications`]. An unchanged tree
/// makes no commit and never calls `author`, so publishing it needs no
/// identity.
pub fn prepare_publication(
    request: &TreePublication<'_>,
    message: &str,
    author: impl FnOnce() -> Result<PublicationAuthor>,
) -> Result<PreparedPublication> {
    let checkout = tempfile::TempDir::new()?;
    let repository = clone_branch(request.push_url, request.branch, checkout.path(), request.auth)?;
    let expected = remote_branch_oid(&repository, request.branch);
    verify_publication_base(
        request.publication_base,
        expected.map(|oid| oid.to_string()).as_deref(),
        request.push_url,
        request.branch,
    )?;
    let output_root = if request.path_prefix.is_empty() {
        checkout.path().to_path_buf()
    } else {
        checkout.path().join(request.path_prefix)
    };
    let options = takeover_options(
        &output_root,
        request.index.owner.as_ref(),
        request.takeover,
        request.options.clone(),
    )?;
    let index = reconcile_rendered_tree_with_options(&output_root, request.desired, request.index.clone(), options)?;
    // Reconciliation verified the baseline's owned files, so the scope check
    // and the change summary read them once from the branch head; nothing is
    // committed if the check fails.
    let previous_files = published_owned_files(&repository, expected, request.path_prefix, &request.options.release)?;
    let previous: DesiredFiles<'_> = previous_files
        .iter()
        .map(|(path, bytes)| (path.as_path(), bytes.as_slice()))
        .collect();
    super::project_scope::check_project_scope(
        &previous,
        request.desired,
        request.path_prefix,
        request.path_prefix,
        request.cluster_kubernetes,
        request.ignore_project_scope_contraction,
    )?;
    let changed = super::changed_applications(&previous, request.desired, request.path_prefix);
    let message = with_changed_applications(message, &changed);
    let commit = commit_worktree(&repository, request.branch, &output_root, &message, || author()?.git())?;
    Ok(PreparedPublication {
        repository,
        push_url: request.push_url.to_owned(),
        branch: request.branch.to_owned(),
        expected,
        commit,
        index,
        _checkout: checkout,
    })
}

/// The bytes of `relative`, beneath `prefix`, in `tree`; `None` when the tree
/// holds no such file.
fn read_tree_file(
    repository: &Repository,
    tree: &git2::Tree<'_>,
    prefix: &str,
    relative: &str,
) -> Result<Option<Vec<u8>>> {
    let path = match prefix.trim_end_matches('/') {
        "" => relative.to_owned(),
        prefix => format!("{prefix}/{relative}"),
    };
    match tree.get_path(Path::new(&path)) {
        Ok(entry) => {
            let blob = entry
                .to_object(repository)
                .and_then(|object| object.peel_to_blob())
                .map_err(crate::git::GitError::from)?;
            Ok(Some(blob.content().to_vec()))
        }
        Err(error) if error.code() == git2::ErrorCode::NotFound => Ok(None),
        Err(error) => Err(crate::git::GitError::from(error).into()),
    }
}

/// The files the ownership index of the publication at `head` owns beneath
/// `prefix`, read from that commit, except paths released from ownership.
/// Owned files absent from the commit are skipped, as reconciliation allows.
fn published_owned_files(
    repository: &Repository,
    head: Option<Oid>,
    prefix: &str,
    released: &std::collections::BTreeSet<PathBuf>,
) -> Result<BTreeMap<PathBuf, Vec<u8>>> {
    let Some(head) = head else {
        return Ok(BTreeMap::new());
    };
    let tree = repository
        .find_commit(head)
        .and_then(|commit| commit.tree())
        .map_err(crate::git::GitError::from)?;
    let read = |relative: &str| read_tree_file(repository, &tree, prefix, relative);
    let Some(index) = read(DEFAULT_INDEX_PATH)? else {
        return Ok(BTreeMap::new());
    };
    let index = parse_index(&index, DEFAULT_INDEX_PATH)?;
    let mut files = BTreeMap::new();
    for relative in index.files.keys() {
        if released.contains(Path::new(relative)) {
            continue;
        }
        if let Some(bytes) = read(relative)? {
            files.insert(PathBuf::from(relative), bytes);
        }
    }
    Ok(files)
}

/// `message` with a `Changed applications:` paragraph listing `applications`,
/// one `- NAMESPACE/NAME` line each. It goes before a final paragraph of Git
/// trailers, such as publish-tree's `Nyl-*` provenance, so they stay
/// trailers, and otherwise at the end. No Applications leave the message as
/// it is.
pub fn with_changed_applications(message: &str, applications: &[String]) -> String {
    if applications.is_empty() {
        return message.to_owned();
    }
    let message = message.trim_end();
    let mut list = String::from("Changed applications:");
    for application in applications {
        list.push_str("\n- ");
        list.push_str(application);
    }
    match message.rsplit_once("\n\n") {
        Some((body, trailers)) if trailers.lines().all(is_trailer_line) => format!("{body}\n\n{list}\n\n{trailers}"),
        _ => format!("{message}\n\n{list}"),
    }
}

/// Whether `line` has Git's `Token: value` trailer form.
fn is_trailer_line(line: &str) -> bool {
    line.split_once(": ")
        .is_some_and(|(token, _)| !token.is_empty() && token.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
}

impl PreparedPublication {
    /// The new commit, or `None` when the tree was unchanged.
    pub fn commit(&self) -> Option<Oid> {
        self.commit
    }

    /// The published ownership index.
    pub fn index(&self) -> &RenderIndex {
        &self.index
    }

    /// The new commit, else the branch head.
    pub fn head(&self) -> Result<Oid> {
        match self.commit {
            Some(commit) => Ok(commit),
            None => Ok(self
                .repository
                .head()
                .and_then(|head| head.peel_to_commit())
                .map_err(crate::git::GitError::from)?
                .id()),
        }
    }

    /// Refuse a publication whose branch advanced since it was checked out.
    pub fn verify_unmoved(&self, auth: &GitAuth) -> Result<()> {
        fetch_branch(&self.repository, &self.push_url, &self.branch, auth)?;
        let actual = remote_branch_oid(&self.repository, &self.branch);
        if actual != self.expected {
            return Err(NylError::PublicationMoved(format!(
                "Publication {}@{} advanced from {:?} to {actual:?}; refusing stale publish",
                nyl_git::sanitize_url(&self.push_url),
                self.branch,
                self.expected,
            )));
        }
        Ok(())
    }

    /// Push the commit while the branch is still where it was checked out;
    /// without a commit, nothing is pushed.
    pub fn push(&self, auth: &GitAuth) -> Result<()> {
        if self.commit.is_none() {
            return Ok(());
        }
        crate::git::transport::push_branch_if_unchanged(
            &self.repository,
            &self.push_url,
            &self.branch,
            self.expected,
            auth,
        )
        .map_err(|error| {
            let message = format!("Failed to publish publication branch: {error}");
            match &error {
                crate::git::GitError::Remote(remote) if remote.is_stale() => NylError::PublicationMoved(message),
                _ => NylError::config(message),
            }
        })
    }
}

/// Phase 1 of a publication's teardown: `rendered` without its workload
/// Applications. The tree's `_nyl/catalog` keeps the catalog Application
/// (the one sourcing that directory, when the target has one) and every
/// other generated object, such as the AppProjects, but no other
/// Application; the workload trees stay. Publishing it with
/// [`prepare_target_publication`] lets the catalog's own sync prune the
/// workload Applications while the catalog is never empty. In a shared
/// catalog the target's catalog directory keeps its AppProjects, and the
/// shared manifest, outside the target's prefix, is untouched.
///
/// Contract: [Kubernetes publication unit](../../../../design/orchestration-core.md#kubernetes-publication-unit),
/// Teardown, phase 1.
pub fn without_workload_applications(rendered: &RenderedTarget) -> Result<RenderedTarget> {
    let catalog_source = catalog_source(&rendered.destination.path_prefix);
    let mut phase1 = rendered.clone();
    let mut removed = Vec::new();
    for (path, bytes) in &rendered.files {
        if !path.starts_with(super::CATALOG_DIRECTORY) {
            continue;
        }
        let text = std::str::from_utf8(bytes)
            .map_err(|_| NylError::config(format!("generated catalog file {} is not UTF-8", path.display())))?;
        let manifests = serde_saphyr::from_multiple::<serde_json::Value>(text).map_err(NylError::Yaml)?;
        let workload = |manifest: &serde_json::Value| {
            manifest.get("kind").and_then(serde_json::Value::as_str) == Some("Application")
                && manifest
                    .pointer("/spec/source/path")
                    .and_then(serde_json::Value::as_str)
                    != Some(catalog_source.as_str())
        };
        match manifests.iter().filter(|manifest| workload(manifest)).count() {
            0 => {}
            count if count == manifests.len() => removed.push(path.clone()),
            _ => {
                return Err(NylError::config(format!(
                    "generated catalog file {} mixes workload Applications with other objects",
                    path.display()
                )))
            }
        }
    }
    for path in removed {
        phase1.files.remove(&path);
    }
    Ok(phase1)
}

/// A target's settings that decide whether its teardown can complete, read
/// from the rendered tree ([`teardown_settings`]). A publication driver
/// records them in the unit's resolved spec, a persisted format whose
/// camelCase keys are pinned by a test, and evaluates them against its
/// teardown readiness table.
///
/// Contract: [Kubernetes publication unit](../../../../design/orchestration-core.md#kubernetes-publication-unit),
/// teardown readiness.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TeardownSettings {
    /// Whether the catalog prunes removed workload Applications on its own
    /// (`syncPolicy.automated` with `prune: true`); `None` when a parent the
    /// project writes syncs it, which Nyl cannot see (`external: not checked`).
    pub catalog_prunes: Option<bool>,
    /// The catalog's `applicationDeletionPolicy`, such as `Foreground` or `Orphan`; `None` when external.
    pub application_deletion_policy: Option<String>,
    /// Each owned Namespace's `deletePolicy` (`Automatic`, `Retain`, or `Confirm`), by name.
    pub namespace_delete_policies: BTreeMap<String, String>,
    /// Whether a parent owns the target's catalog, so phase 2 removes the
    /// catalog Application and AppProjects too.
    pub catalog_owned_by_parent: bool,
}

/// The settings that decide whether `rendered`'s teardown can complete, read
/// from the tree itself: the catalog Application and workload Applications
/// as Argo CD will see them, and the Namespaces the tree owns. A driver
/// records them at resolution, so readiness agrees with what a teardown of
/// that document would publish.
///
/// - `catalog_prunes` and `application_deletion_policy` come from the
///   catalog Application, the one sourcing `_nyl/catalog`: `syncPolicy.automated`
///   enabled with `prune: true`, and its finalizer (`Foreground`,
///   `Background`, or `Orphan` without one). Any workload Application
///   without a finalizer also makes the policy `Orphan`, because deleting
///   that Application then leaves its workloads running. A tree without a
///   catalog Application leaves a parent to sync it, which cannot be seen:
///   both settings are `None`, and `catalog_owned_by_parent` is true.
/// - Each Namespace the tree renders reports its `Delete` sync option:
///   `confirm` is `Confirm`, `false` is `Retain`, and none is `Automatic`.
///
/// Contract: [Kubernetes publication unit](../../../../design/orchestration-core.md#kubernetes-publication-unit),
/// teardown readiness.
pub fn teardown_settings(rendered: &RenderedTarget) -> Result<TeardownSettings> {
    use serde_json::Value;

    let catalog_source = catalog_source(&rendered.destination.path_prefix);
    let mut catalog: Option<Value> = None;
    let mut workload_orphaned = false;
    let mut namespaces = BTreeMap::new();
    for (path, bytes) in &rendered.files {
        let in_catalog = path.starts_with(super::CATALOG_DIRECTORY);
        if !matches!(path.extension().and_then(std::ffi::OsStr::to_str), Some("yaml" | "yml")) {
            continue;
        }
        let Ok(text) = std::str::from_utf8(bytes) else {
            continue;
        };
        // Only catalog files and Namespaces matter; leave other manifests unparsed.
        if !in_catalog && !text.contains("Namespace") {
            continue;
        }
        let manifests = serde_saphyr::from_multiple::<Value>(text).map_err(NylError::Yaml)?;
        for manifest in manifests {
            let kind = manifest.get("kind").and_then(Value::as_str);
            if in_catalog && kind == Some("Application") {
                if manifest.pointer("/spec/source/path").and_then(Value::as_str) == Some(catalog_source.as_str()) {
                    catalog = Some(manifest);
                } else if application_deletion_policy(&manifest) == "Orphan" {
                    workload_orphaned = true;
                }
            } else if !in_catalog
                && kind == Some("Namespace")
                && manifest.get("apiVersion").and_then(Value::as_str) == Some("v1")
            {
                if let Some(name) = manifest.pointer("/metadata/name").and_then(Value::as_str) {
                    namespaces.insert(name.to_owned(), namespace_delete_policy(&manifest).to_owned());
                }
            }
        }
    }
    // A shared catalog is rendered by Nyl, so its settings are read like a
    // per-target catalog's. It is no parent the project writes: it prunes this
    // target's objects as phase 1 and phase 2 remove them, and itself stays
    // until it is decommissioned, so no remnant of the target's catalog remains.
    let owned_by_parent = catalog.is_none();
    if let Some(shared) = &rendered.shared_catalog {
        catalog = shared_application(shared)?;
    }
    let catalog_prunes = catalog.as_ref().map(|application| {
        application
            .pointer("/spec/syncPolicy/automated")
            .is_some_and(|automated| {
                automated.get("enabled").and_then(Value::as_bool) != Some(false)
                    && automated.get("prune").and_then(Value::as_bool) == Some(true)
            })
    });
    let catalog_policy = catalog.as_ref().map(application_deletion_policy);
    Ok(TeardownSettings {
        catalog_prunes,
        application_deletion_policy: if workload_orphaned {
            Some("Orphan".to_owned())
        } else {
            catalog_policy.map(str::to_owned)
        },
        namespace_delete_policies: namespaces,
        catalog_owned_by_parent: owned_by_parent,
    })
}

/// The Application a shared catalog's tree holds.
fn shared_application(shared: &RenderedTarget) -> Result<Option<serde_json::Value>> {
    let Some(bytes) = shared.files.get(Path::new(super::shared_catalog::SHARED_MANIFEST)) else {
        return Ok(None);
    };
    let text = std::str::from_utf8(bytes).map_err(|_| NylError::config("the shared catalog manifest is not UTF-8"))?;
    Ok(serde_saphyr::from_multiple::<serde_json::Value>(text)
        .map_err(NylError::Yaml)?
        .into_iter()
        .next())
}

/// The `Foreground`, `Background`, or `Orphan` policy an Application's
/// finalizers express.
fn application_deletion_policy(application: &serde_json::Value) -> &'static str {
    use super::argocd::{BACKGROUND_FINALIZER, FOREGROUND_FINALIZER};
    let finalizers = application
        .pointer("/metadata/finalizers")
        .and_then(serde_json::Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let has = |finalizer: &str| finalizers.iter().any(|value| value.as_str() == Some(finalizer));
    if has(FOREGROUND_FINALIZER) {
        "Foreground"
    } else if has(BACKGROUND_FINALIZER) {
        "Background"
    } else {
        "Orphan"
    }
}

/// A Namespace's `deletePolicy` as its `Delete` Argo CD sync option renders.
fn namespace_delete_policy(namespace: &serde_json::Value) -> &'static str {
    let options = namespace
        .pointer("/metadata/annotations")
        .and_then(|annotations| annotations.get("argocd.argoproj.io/sync-options"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    match options
        .split(',')
        .map(str::trim)
        .find_map(|option| option.strip_prefix("Delete="))
    {
        Some("confirm") => "Confirm",
        Some("false") => "Retain",
        _ => "Automatic",
    }
}

/// The directory the catalog Application sources, as its `spec.source.path`.
fn catalog_source(path_prefix: &str) -> String {
    if path_prefix.is_empty() {
        super::CATALOG_DIRECTORY.to_owned()
    } else {
        format!("{path_prefix}/{}", super::CATALOG_DIRECTORY)
    }
}

/// Phase 2 of a publication's teardown: remove exactly the files the
/// target's ownership index records beneath its prefix, and the index
/// itself, in one commit pushed by compare-and-swap, as every publication
/// is. Unowned files in the prefix, such as state files other tools commit,
/// stay; an index naming another owner, or an owned file modified outside
/// Nyl, refuses the removal as reconciliation does. A prefix without an
/// index has nothing left to remove and makes no commit, so repeating an
/// interrupted removal converges.
///
/// `rendered` identifies the owner and the destination; its files are not
/// published. Contract: [Kubernetes publication unit](../../../../design/orchestration-core.md#kubernetes-publication-unit),
/// Teardown, phase 2.
pub fn prepare_target_removal(
    rendered: &RenderedTarget,
    project_dir: &Path,
    message: &str,
    author: &PublicationAuthor,
    variable: &dyn Fn(&str) -> Option<String>,
) -> Result<PreparedRemoval> {
    prepare_target_removal_fenced(rendered, project_dir, message, author, variable, None)
}

/// [`prepare_target_removal`] with the decision whether the tree's owner may
/// remove the files of a prefix whose index names another owner (owner
/// fencing, as for [`prepare_target_publication_fenced`]).
pub fn prepare_target_removal_fenced(
    rendered: &RenderedTarget,
    project_dir: &Path,
    message: &str,
    author: &PublicationAuthor,
    variable: &dyn Fn(&str) -> Option<String>,
    takeover: Option<&OwnerTakeover<'_>>,
) -> Result<PreparedRemoval> {
    let auth = project_git_auth(&load_project_file(project_dir)?, variable)?;
    let branch = writable_branch_name(&rendered.destination.branch)?;
    let index_bytes = rendered.files.get(Path::new(DEFAULT_INDEX_PATH)).ok_or_else(|| {
        NylError::config(format!(
            "Rendered target tree for {} has no {DEFAULT_INDEX_PATH}",
            rendered.destination.repository
        ))
    })?;
    let mut owner = parse_index(index_bytes, DEFAULT_INDEX_PATH)?;
    owner.files.clear();
    let checkout = tempfile::TempDir::new()?;
    let repository = clone_branch(&rendered.destination.push_url, branch, checkout.path(), &auth)?;
    let expected = remote_branch_oid(&repository, branch);
    let output_root = if rendered.destination.path_prefix.is_empty() {
        checkout.path().to_path_buf()
    } else {
        checkout.path().join(&rendered.destination.path_prefix)
    };
    let index_path = output_root.join(DEFAULT_INDEX_PATH);
    let commit = if index_path.is_file() {
        let options = takeover_options(&output_root, owner.owner.as_ref(), takeover, rendered.reconcile.clone())?;
        reconcile_rendered_tree_with_options(&output_root, &DesiredFiles::new(), owner.clone(), options)?;
        std::fs::remove_file(&index_path)?;
        commit_worktree(&repository, branch, &output_root, message, || author.git())?
    } else {
        None
    };
    let prepared = PreparedPublication {
        repository,
        push_url: rendered.destination.push_url.clone(),
        branch: branch.to_owned(),
        expected,
        commit,
        index: owner,
        _checkout: checkout,
    };
    prepared.verify_unmoved(&auth)?;
    Ok(PreparedRemoval {
        prepared,
        auth,
        destination: rendered.destination.clone(),
    })
}

/// A teardown's removal of a target's owned files, ready to push.
pub struct PreparedRemoval {
    prepared: PreparedPublication,
    auth: GitAuth,
    destination: PublicationDestination,
}

/// Where a teardown removed a target's owned files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemovedTree {
    pub repository: String,
    pub branch: String,
    pub path_prefix: String,
    /// The removal commit; `None` when the prefix held no index any more.
    pub commit: Option<String>,
}

impl PreparedRemoval {
    /// Push the removal commit, if any, while the branch is still where it
    /// was checked out.
    pub fn push(self) -> Result<RemovedTree> {
        self.prepared.push(&self.auth)?;
        Ok(RemovedTree {
            repository: self.destination.repository,
            branch: self.prepared.branch.clone(),
            path_prefix: self.destination.path_prefix,
            commit: self.prepared.commit().map(|commit| commit.to_string()),
        })
    }
}

fn clone_branch(url: &str, branch: &str, path: &Path, auth: &GitAuth) -> Result<Repository> {
    // Nothing is checked out by the fetch: the user's Git config may convert
    // line endings, and owned files must keep the exact bytes their index records.
    let repository = Repository::init(path)
        .and_then(|repository| {
            repository.remote("origin", url)?;
            Ok(repository)
        })
        .map_err(|error| NylError::config(format!("Failed to clone {}: {error}", nyl_git::sanitize_url(url))))?;
    {
        let mut config = repository.config().map_err(crate::git::GitError::from)?;
        config
            .set_bool("core.autocrlf", false)
            .and_then(|()| config.set_str("core.eol", "lf"))
            .map_err(crate::git::GitError::from)?;
    }
    fetch_branch(&repository, url, branch, auth)?;
    if let Some(oid) = remote_branch_oid(&repository, branch) {
        let commit = repository.find_commit(oid).map_err(crate::git::GitError::from)?;
        repository
            .branch(branch, &commit, true)
            .map_err(crate::git::GitError::from)?;
        repository
            .set_head(&format!("refs/heads/{branch}"))
            .map_err(crate::git::GitError::from)?;
        repository
            .reset(commit.as_object(), ResetType::Hard, None)
            .map_err(crate::git::GitError::from)?;
    } else {
        repository
            .set_head(&format!("refs/heads/{branch}"))
            .map_err(crate::git::GitError::from)?;
        let mut index = repository.index().map_err(crate::git::GitError::from)?;
        index.clear().map_err(crate::git::GitError::from)?;
        index.write().map_err(crate::git::GitError::from)?;
        let mut checkout = CheckoutBuilder::new();
        checkout.force().remove_untracked(true).remove_ignored(true);
        repository
            .checkout_index(Some(&mut index), Some(&mut checkout))
            .map_err(crate::git::GitError::from)?;
    }
    Ok(repository)
}

fn remote_branch_oid(repository: &Repository, branch: &str) -> Option<Oid> {
    repository
        .find_reference(&format!("refs/remotes/origin/{branch}"))
        .ok()
        .and_then(|reference| reference.target())
}

fn commit_worktree(
    repository: &Repository,
    branch: &str,
    output_root: &Path,
    message: &str,
    author: impl FnOnce() -> Result<git2::Signature<'static>>,
) -> Result<Option<Oid>> {
    let worktree = repository
        .workdir()
        .ok_or_else(|| NylError::config("Publication repository has no worktree"))?;
    let canonical_worktree = worktree.canonicalize()?;
    let canonical_output_root = output_root.canonicalize()?;
    let output_pathspec = canonical_output_root.strip_prefix(&canonical_worktree).map_err(|_| {
        NylError::config(format!(
            "Rendered output {} is outside publication worktree {}",
            output_root.display(),
            worktree.display()
        ))
    })?;
    let mut status_options = StatusOptions::new();
    status_options.include_untracked(true).recurse_untracked_dirs(true);
    if !output_pathspec.as_os_str().is_empty() {
        status_options.pathspec(output_pathspec);
    }
    let statuses = repository
        .statuses(Some(&mut status_options))
        .map_err(|error| NylError::config(format!("Failed to inspect rendered worktree: {error}")))?;
    if statuses.is_empty() {
        return Ok(None);
    }
    let mut index = repository.index().map_err(crate::git::GitError::from)?;
    if output_pathspec.as_os_str().is_empty() {
        index
            .add_all(["*"], IndexAddOption::DEFAULT, None)
            .map_err(crate::git::GitError::from)?;
        index.update_all(["*"], None).map_err(crate::git::GitError::from)?;
    } else {
        index
            .add_all([output_pathspec], IndexAddOption::DEFAULT, None)
            .map_err(crate::git::GitError::from)?;
        index
            .update_all([output_pathspec], None)
            .map_err(crate::git::GitError::from)?;
    }
    index.write().map_err(crate::git::GitError::from)?;
    let tree_id = index.write_tree().map_err(crate::git::GitError::from)?;
    let tree = repository.find_tree(tree_id).map_err(crate::git::GitError::from)?;
    let parent = repository.head().ok().and_then(|head| head.peel_to_commit().ok());
    let parents = parent.iter().collect::<Vec<_>>();
    let signature = author()?;
    let oid = repository
        .commit(
            Some(&format!("refs/heads/{branch}")),
            &signature,
            &signature,
            message,
            &tree,
            &parents,
        )
        .map_err(crate::git::GitError::from)?;
    Ok(Some(oid))
}

fn fetch_branch(repository: &Repository, url: &str, branch: &str, auth: &GitAuth) -> Result<()> {
    crate::git::transport::fetch_branch(repository, url, branch, &format!("refs/remotes/origin/{branch}"), auth)
        .map(drop)
        .map_err(|error| NylError::config(format!("Failed to refresh publication branch: {error}")))
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_with_changed_applications_keeps_trailers_last() {
        let applications = ["argocd/api".to_owned(), "argocd/web".to_owned()];
        assert_eq!(
            super::with_changed_applications(
                "Render deployment target production\n\nNyl-Source-Commit: 0123\nNyl-Cluster: prod",
                &applications
            ),
            "Render deployment target production\n\nChanged applications:\n- argocd/api\n- argocd/web\n\nNyl-Source-Commit: 0123\nNyl-Cluster: prod"
        );
        assert_eq!(
            super::with_changed_applications("nyl: publish dev/kubernetes\n\nExplains the change.\n", &applications),
            "nyl: publish dev/kubernetes\n\nExplains the change.\n\nChanged applications:\n- argocd/api\n- argocd/web"
        );
        assert_eq!(super::with_changed_applications("nyl: publish", &[]), "nyl: publish");
    }

    use super::*;
    use chrono::TimeZone;
    use serde_json::json;
    use std::fs;
    use tempfile::TempDir;

    const RELEASE: &str = r"apiVersion: k8s.nyl/v1
kind: Release
metadata:
  name: web
  namespace: web
spec:
  inputs:
    image: {type: string}
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: web
  namespace: web
spec:
  replicas: 1
  selector:
    matchLabels: {app: web}
  template:
    metadata:
      labels: {app: web}
    spec:
      containers:
        - name: web
          image: '{{ inputs.image }}'
";

    fn platform(publication_url: &str) -> String {
        format!(
            r"apiVersion: gitops.nyl/v1
kind: GitRepository
metadata:
  name: platform
spec:
  repoURL: '{publication_url}'
---
apiVersion: k8s.nyl/v1
kind: Cluster
metadata:
  name: dev
spec:
  destination:
    server: https://kubernetes.default.svc
  kubernetes:
    kubeVersion: 1.31.4
    apiVersions: [v1, apps/v1, v1/Namespace, apps/v1/Deployment]
    clusterScopedKinds: [core/Namespace]
---
apiVersion: k8s.gitops.nyl/v1
kind: DeploymentTarget
metadata:
  name: dev
spec:
  clusterRef: {{name: dev}}
  applicationGroupSelector:
    matchLabels: {{app: web}}
  publication:
    repositoryRef: {{name: platform}}
    revision: deploy
    pathPrefix: dev
  releaseInputs:
    web/web:
      image: {{fromUnit: {{unit: web-image, artifact: image, pointer: /reference}}}}
"
        )
    }

    const GROUP: &str = r"apiVersion: k8s.gitops.nyl/v1
kind: ApplicationGroup
metadata:
  name: web
  labels: {app: web}
spec:
  applicationNamespace: argocd
  source: {path: ../applications/web}
";

    /// A committed worktree with `nyl.toml` in `nyl/`, publishing to `publication_url`.
    fn project(publication_url: &str) -> TempDir {
        project_with_group(publication_url, GROUP)
    }

    /// [`project`] with `group` as the ApplicationGroup document.
    fn project_with_group(publication_url: &str, group: &str) -> TempDir {
        let directory = TempDir::new().unwrap();
        let root = directory.path();
        for (path, contents) in [
            ("nyl/nyl.toml", "[project]\n".to_owned()),
            ("nyl/platform.yaml", platform(publication_url)),
            ("nyl/web.yaml", group.to_owned()),
            ("applications/web/release.yaml", RELEASE.to_owned()),
        ] {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        }
        commit_all(&Repository::init(root).unwrap());
        directory
    }

    /// Commit every file of `repository`'s worktree on HEAD.
    fn commit_all(repository: &Repository) -> Oid {
        let mut index = repository.index().unwrap();
        index.add_all(["*"], IndexAddOption::DEFAULT, None).unwrap();
        index.write().unwrap();
        let tree = repository.find_tree(index.write_tree().unwrap()).unwrap();
        let signature = git2::Signature::new("Test", "test@example.invalid", &git2::Time::new(0, 0)).unwrap();
        let parent = repository.head().ok().and_then(|head| head.peel_to_commit().ok());
        repository
            .commit(
                Some("HEAD"),
                &signature,
                &signature,
                "Commit",
                &tree,
                &parent.iter().collect::<Vec<_>>(),
            )
            .unwrap()
    }

    fn publication_repository() -> (TempDir, String) {
        let directory = TempDir::new().unwrap();
        Repository::init_bare(directory.path().join("deploy.git")).unwrap();
        let url = format!("file://{}", directory.path().join("deploy.git").display());
        (directory, url)
    }

    fn snapshot(image: &str) -> InputSnapshot {
        BTreeMap::from([(
            "web/web".to_owned(),
            BTreeMap::from([("image".to_owned(), json!(image))]),
        )])
    }

    fn render(worktree: &Path, inputs: &InputSnapshot, cache_dir: Option<&Path>) -> Result<RenderedTarget> {
        render_target(&TargetRender {
            instances: &nyl_core::orchestration::instance::NO_INSTANCES,
            source_commit: None,
            inline: None,
            owner: None,
            selection: super::super::tree::partial::PublicationSelection::default(),
            publication_base: None,
            worktree,
            project_dir: &worktree.join("nyl"),
            target: "dev",
            inputs,
            cache_dir,
            resource_catalogue: crate::config::test_catalogue(),
        })
    }

    fn author(seconds: i64) -> PublicationAuthor {
        PublicationAuthor {
            identity: CommitIdentity {
                name: "Nyl Orchestration".to_owned(),
                email: "nyl@example.invalid".to_owned(),
            },
            time: chrono::Utc.timestamp_opt(seconds, 0).unwrap(),
        }
    }

    fn deployment(rendered: &RenderedTarget) -> String {
        rendered
            .files
            .iter()
            .find(|(path, bytes)| {
                path.starts_with("web") && String::from_utf8_lossy(bytes).contains("kind: Deployment")
            })
            .map(|(_, bytes)| String::from_utf8(bytes.clone()).unwrap())
            .expect("the Deployment is rendered beneath web/")
    }

    #[test]
    fn test_published_owned_files_skip_missing_and_released_paths() {
        let directory = TempDir::new().unwrap();
        let repository = Repository::init(directory.path()).unwrap();
        let mut index = RenderIndex::new(
            "production".into(),
            "production".into(),
            RenderIndexPublication {
                repository: "deploy".into(),
                revision: "deploy".into(),
                path_prefix: "production".into(),
            },
            None,
            false,
            BTreeMap::new(),
        );
        for path in ["apps/kept.yaml", "apps/deleted.yaml", "state/released.json"] {
            index.files.insert(path.into(), "digest".into());
        }
        let root = directory.path().join("production");
        fs::create_dir_all(root.join("_nyl")).unwrap();
        fs::create_dir_all(root.join("apps")).unwrap();
        fs::create_dir_all(root.join("state")).unwrap();
        fs::write(root.join("_nyl/index.json"), serde_json::to_vec(&index).unwrap()).unwrap();
        fs::write(root.join("apps/kept.yaml"), "kept\n").unwrap();
        fs::write(root.join("state/released.json"), "{}\n").unwrap();
        let mut staged = repository.index().unwrap();
        staged.add_all(["*"], IndexAddOption::DEFAULT, None).unwrap();
        let tree = repository.find_tree(staged.write_tree().unwrap()).unwrap();
        let signature = git2::Signature::now("Tests", "tests@example.invalid").unwrap();
        let head = repository
            .commit(Some("HEAD"), &signature, &signature, "publish", &tree, &[])
            .unwrap();

        // An owned file deleted outside Nyl is skipped, as reconciliation allows.
        let released = std::collections::BTreeSet::from([PathBuf::from("state/released.json")]);
        let files = published_owned_files(&repository, Some(head), "production", &released).unwrap();
        assert_eq!(
            files,
            BTreeMap::from([(PathBuf::from("apps/kept.yaml"), b"kept\n".to_vec())])
        );
        assert!(published_owned_files(&repository, None, "production", &released)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn test_render_target_renders_pinned_from_unit_values() {
        let (_publication, url) = publication_repository();
        let worktree = project(&url);
        let rendered = render(worktree.path(), &snapshot("registry.test/web@sha256:abc"), None).unwrap();

        assert!(deployment(&rendered).contains("registry.test/web@sha256:abc"));
        let index = parse_index(&rendered.files[Path::new(DEFAULT_INDEX_PATH)], "index").unwrap();
        assert_eq!(index.target, "dev");
        assert!(index.files.keys().any(|path| path.starts_with("web/")));
        assert_eq!(
            rendered.destination,
            PublicationDestination {
                repository: url.clone(),
                push_url: url,
                branch: "deploy".to_owned(),
                path_prefix: "dev".to_owned(),
            }
        );
    }

    /// A project like [`project`], whose `dev` target the unit declares inline.
    fn inline_project(publication_url: &str) -> TempDir {
        let platform = platform(publication_url);
        let platform = platform.split("---\n").take(2).collect::<Vec<_>>().join("---\n");
        let directory = project_with_group(publication_url, GROUP);
        fs::write(directory.path().join("nyl/platform.yaml"), platform).unwrap();
        commit_all(&Repository::open(directory.path()).unwrap());
        directory
    }

    fn inline_dev() -> InlineTarget {
        InlineTarget {
            name: "dev".to_owned(),
            source_file: PathBuf::from("platform.yaml"),
            spec: json!({
                "clusterRef": {"name": "dev"},
                "applicationGroupSelector": {"matchLabels": {"app": "web"}},
                "publication": {"repositoryRef": {"name": "platform"}, "revision": "deploy", "pathPrefix": "dev"},
                // What resolving the unit leaves of a `fromUnit` binding: its value.
                "releaseInputs": {"web/web": {"image": "registry.invalid/web@sha256:feed"}},
            }),
        }
    }

    fn render_inline(
        worktree: &Path,
        inline: &InlineTarget,
        owner: Option<RenderIndexOwner>,
    ) -> Result<RenderedTarget> {
        render_target(&TargetRender {
            instances: &nyl_core::orchestration::instance::NO_INSTANCES,
            source_commit: None,
            inline: Some(inline),
            owner,
            selection: super::super::tree::partial::PublicationSelection::default(),
            publication_base: None,
            worktree,
            project_dir: &worktree.join("nyl"),
            target: &inline.name,
            inputs: &snapshot("registry.invalid/web@sha256:feed"),
            cache_dir: None,
            resource_catalogue: crate::config::test_catalogue(),
        })
    }

    /// Catches an inline target that renders differently from the same
    /// declared target: it goes through the same compile and publish path,
    /// so every file is identical, and the pinned input a unit's reference
    /// resolution left in its place wins over the binding it replaced.
    #[test]
    fn test_render_target_renders_an_inline_target_like_a_declared_one() {
        let (_repository, url) = publication_repository();
        let declared = render(
            project(&url).path(),
            &snapshot("registry.invalid/web@sha256:feed"),
            None,
        )
        .unwrap();
        let worktree = inline_project(&url);
        let inline = render_inline(worktree.path(), &inline_dev(), None).unwrap();
        let files = |rendered: &RenderedTarget| {
            rendered
                .files
                .iter()
                .filter(|(path, _)| path.as_path() != Path::new(DEFAULT_INDEX_PATH))
                .map(|(path, bytes)| (path.clone(), bytes.clone()))
                .collect::<BTreeMap<_, _>>()
        };
        assert_eq!(files(&inline), files(&declared));
        assert_eq!(inline.destination, declared.destination);
        assert!(deployment(&inline).contains("registry.invalid/web@sha256:feed"));
        // The snapshot alone supplies an input the target holds no binding for.
        let mut unpinned = inline_dev();
        unpinned.spec["releaseInputs"] = json!({});
        assert!(render_inline(worktree.path(), &unpinned, None).is_ok());
    }

    const ENVIRONMENTS: &str = r"apiVersion: gitops.nyl/v1
kind: Environment
metadata: {name: dev}
spec:
  unitSelector: {matchLabels: {preview: 'true'}}
  values: {prefix: deploy}
---
apiVersion: gitops.nyl/v1
kind: Environment
metadata: {name: staging}
spec:
  unitSelector: {matchLabels: {preview: 'true'}}
  values: {prefix: STAGING_PREFIX}
";

    const PREVIEW_UNIT: &str = r"apiVersion: units.gitops.nyl/v1
kind: KubernetesPublication
metadata:
  name: kubernetes
  labels: {preview: 'true'}
spec:
  target:
    inline:
      clusterRef: {name: dev}
      applicationGroupSelector: {matchLabels: {app: web}}
      publication: {repositoryRef: {name: platform}, revision: deploy, pathPrefix: '{{ values.prefix }}'}
      releaseInputs: {web/web: {image: {fromUnit: {unit: web-image, artifact: image, pointer: /reference}}}}
";

    /// A project whose one publication unit, selected by `dev` and `staging`,
    /// declares an inline target on the same Cluster, with `staging_prefix`
    /// as staging's publication prefix and `group` as the ApplicationGroup.
    fn two_environments(publication_url: &str, staging_prefix: &str, group: &str) -> TempDir {
        let worktree = inline_project(publication_url);
        fs::write(worktree.path().join("nyl/web.yaml"), group).unwrap();
        fs::write(
            worktree.path().join("nyl/environments.yaml"),
            ENVIRONMENTS.replace("STAGING_PREFIX", staging_prefix),
        )
        .unwrap();
        fs::write(worktree.path().join("nyl/units.yaml"), PREVIEW_UNIT).unwrap();
        commit_all(&Repository::open(worktree.path()).unwrap());
        worktree
    }

    /// What `dev`'s unit renders: its inline target, named by the environment.
    fn dev_inline(prefix: &str) -> InlineTarget {
        InlineTarget {
            name: "dev-kubernetes".to_owned(),
            source_file: PathBuf::from("units.yaml"),
            spec: json!({
                "clusterRef": {"name": "dev"},
                "applicationGroupSelector": {"matchLabels": {"app": "web"}},
                "publication": {"repositoryRef": {"name": "platform"}, "revision": "deploy", "pathPrefix": prefix},
            }),
        }
    }

    /// A group whose Applications and AppProjects are named from their target.
    const PER_TARGET_GROUP: &str = r"apiVersion: k8s.gitops.nyl/v1
kind: ApplicationGroup
metadata:
  name: web
  labels: {app: web}
spec:
  applicationNamespace: argocd
  source: {path: ../applications/web}
  applicationNameTemplate: '${ target.metadata.name }-${ release.metadata.name }'
  projectTemplate:
    name: '{{ target.metadata.name }}-web'
    destinations: [{server: '*', namespace: web}]
";

    /// Catches the checks across targets missing inline targets of other
    /// environments when a unit renders its own: the one unit selected by `dev`
    /// and `staging` on one Cluster generates colliding Application names in
    /// both, which the render of either refuses, and a staging prefix nested
    /// in dev's overlaps it, which the render also refuses, while disjoint
    /// prefixes and per-target names render.
    #[test]
    fn test_render_target_checks_the_inline_targets_of_every_environment() {
        let (_repository, url) = publication_repository();
        let render_dev =
            |worktree: &TempDir, prefix: &str| render_inline(worktree.path(), &dev_inline(prefix), None).map(|_| ());

        let collision = two_environments(&url, "staging", GROUP);
        let error = render_dev(&collision, "deploy").unwrap_err().to_string();
        assert!(error.contains("default Release Application names"), "{error}");
        assert!(
            error.contains("dev-kubernetes") && error.contains("staging-kubernetes"),
            "{error}"
        );

        let nested = two_environments(&url, "deploy/staging", PER_TARGET_GROUP);
        let error = render_dev(&nested, "deploy").unwrap_err().to_string();
        assert!(error.contains("overlap") || error.contains("prefix"), "{error}");
        assert!(
            error.contains("dev-kubernetes") && error.contains("staging-kubernetes"),
            "{error}"
        );

        let disjoint = two_environments(&url, "staging", PER_TARGET_GROUP);
        render_dev(&disjoint, "deploy").unwrap();
    }

    /// Catches an owner missing from, or leaking into, the published index:
    /// a tree rendered for a unit records that owner in its version 3 index,
    /// and one rendered without records none, as `publish-tree` writes it.
    #[test]
    fn test_render_target_records_its_owner_in_the_index() {
        let (_repository, url) = publication_repository();
        let worktree = inline_project(&url);
        let owner = RenderIndexOwner::unit("dev", "kubernetes", "0192f0c4-8d0a-7c3e-9a51-3c1f0e7d2b90");
        let index = |rendered: &RenderedTarget| {
            parse_index(&rendered.files[Path::new(DEFAULT_INDEX_PATH)], DEFAULT_INDEX_PATH).unwrap()
        };
        let owned = index(&render_inline(worktree.path(), &inline_dev(), Some(owner.clone())).unwrap());
        assert_eq!((owned.version, owned.owner), (3, Some(owner)));
        let plain = index(&render_inline(worktree.path(), &inline_dev(), None).unwrap());
        assert_eq!((plain.version, plain.owner), (3, None));
    }

    #[test]
    fn test_render_target_without_a_snapshot_value_is_rejected() {
        let (_publication, url) = publication_repository();
        let worktree = project(&url);
        let error = render(worktree.path(), &InputSnapshot::new(), None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("fromUnit needs orchestrated execution"), "{error}");

        // Tree commands pass no snapshot at all and keep rejecting the binding.
        let inventory = super::super::discover_gitops_inventory(
            &worktree.path().join("nyl"),
            None,
            crate::config::test_catalogue(),
        )
        .unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let error = runtime
            .block_on(super::super::compile_target_tree_with_options(
                &inventory,
                "dev",
                TreeRenderOptions::default(),
            ))
            .unwrap_err()
            .to_string();
        assert!(error.contains("fromUnit needs orchestrated execution"), "{error}");
    }

    #[test]
    fn test_render_target_input_record_is_portable() {
        let (_publication, url) = publication_repository();
        let (first, second) = (project(&url), project(&url));
        for directory in [&first, &second] {
            let root = directory.path();
            std::fs::create_dir(root.join("shared")).unwrap();
            std::fs::write(root.join("shared/cluster.yaml"), "region: west\n").unwrap();
            std::fs::write(root.join("shared/target.yaml"), "environment: dev\n").unwrap();
            let path = root.join("nyl/platform.yaml");
            let documents =
                crate::yaml::parse_yaml_documents_k8s_compatible(&std::fs::read_to_string(&path).unwrap()).unwrap();
            let documents = documents
                .into_iter()
                .map(|mut document| {
                    if let Some(file) = match document["kind"].as_str() {
                        Some("Cluster") => Some("../shared/cluster.yaml"),
                        Some("DeploymentTarget") => Some("../shared/target.yaml"),
                        _ => None,
                    } {
                        document["spec"]["valuesFrom"] = serde_json::json!([{"file":file}]);
                    }
                    crate::yaml::serialize_yaml_document(&document).unwrap()
                })
                .collect::<Vec<_>>();
            std::fs::write(path, documents.join("---\n")).unwrap();
        }
        let (first_cache, second_cache) = (TempDir::new().unwrap(), TempDir::new().unwrap());
        let inputs = snapshot("registry.test/web@sha256:abc");
        let one = render(first.path(), &inputs, Some(first_cache.path())).unwrap();
        let two = render(second.path(), &inputs, Some(second_cache.path())).unwrap();

        assert_eq!(one.input_record, two.input_record, "{:#?}", one.input_record);
        assert!(one.cacheable);
        // Release files by worktree-rooted path, effective resources,
        // Kubernetes capabilities, resolved inputs, and renderer tools.
        for key in [
            "/applications/web/release.yaml",
            "/nyl/nyl.toml",
            "/shared/cluster.yaml",
            "/shared/target.yaml",
            "@resource:DeploymentTarget/dev",
            "@cluster-contract-capabilities:dev",
            "@input:web/web/image",
            "@tool:helm",
        ] {
            assert!(one.input_record.contains_key(key), "{key}: {:#?}", one.input_record);
        }
        for (key, digest) in &one.input_record {
            assert!(key.starts_with('/') || key.starts_with('@'), "{key}");
            assert!(!key.contains(&first.path().display().to_string()), "{key}");
            assert_eq!(digest.len(), 64, "{key}: {digest}");
        }
        // A cached second render yields the same record.
        let again = render(first.path(), &inputs, Some(first_cache.path())).unwrap();
        assert_eq!(again.input_record, one.input_record);

        // Provenance covers file content even when the effective values stay equal.
        std::fs::write(
            first.path().join("shared/cluster.yaml"),
            "# shared facts\nregion: west\n",
        )
        .unwrap();
        let changed = render(first.path(), &inputs, Some(first_cache.path())).unwrap();
        assert_ne!(
            changed.input_record["/shared/cluster.yaml"],
            one.input_record["/shared/cluster.yaml"]
        );
        assert_eq!(
            changed.input_record["@resource:Cluster/dev"],
            one.input_record["@resource:Cluster/dev"]
        );
    }

    /// The record's entries for files of remote sources.
    fn remote_entries(record: &BTreeMap<String, String>) -> BTreeMap<String, String> {
        record
            .iter()
            .filter(|(key, _)| key.starts_with("@sources/"))
            .map(|(key, digest)| (key.clone(), digest.clone()))
            .collect()
    }

    #[test]
    fn test_render_target_input_record_follows_a_remote_source() {
        let (_publication, url) = publication_repository();
        let remote = TempDir::new().unwrap();
        let remote_repository = Repository::init(remote.path()).unwrap();
        std::fs::create_dir(remote.path().join("releases")).unwrap();
        std::fs::write(remote.path().join("releases/release.yaml"), RELEASE).unwrap();
        let first_commit = commit_all(&remote_repository);
        std::fs::write(
            remote.path().join("releases/release.yaml"),
            RELEASE.replace("replicas: 1", "replicas: 2"),
        )
        .unwrap();
        let second_commit = commit_all(&remote_repository);
        let remote_url = format!("file://{}", remote.path().display());
        let group = |commit: Oid| {
            GROUP.replace(
                "  source: {path: ../applications/web}\n",
                &format!(
                    "  source:\n    repository: {{repoURL: '{remote_url}'}}\n    revision: HEAD\n    commit: '{commit}'\n    path: releases\n"
                ),
            )
        };
        let inputs = snapshot("registry.test/web:1");
        let cache = TempDir::new().unwrap();
        let worktree = project_with_group(&url, &group(first_commit));
        let one = render(worktree.path(), &inputs, Some(cache.path())).unwrap();
        assert!(one.cacheable);
        // The same commit from another worktree and cache records the same
        // remote files.
        let elsewhere = project_with_group(&url, &group(first_commit));
        let again = render(elsewhere.path(), &inputs, None).unwrap();
        assert_eq!(again.input_record, one.input_record);

        let moved = project_with_group(&url, &group(second_commit));
        let two = render(moved.path(), &inputs, Some(cache.path())).unwrap();
        assert!(deployment(&two).contains("replicas: 2"));
        let (before, after) = (remote_entries(&one.input_record), remote_entries(&two.input_record));
        assert!(!before.is_empty(), "{:#?}", one.input_record);
        assert!(!after.is_empty(), "{:#?}", two.input_record);
        assert_ne!(before, after);
    }

    fn dependency(kind: &str, digest: &str) -> crate::render::cache::RecordedDependency {
        crate::render::cache::RecordedDependency {
            kind: kind.to_owned(),
            digest: digest.to_owned(),
        }
    }

    #[test]
    fn test_portable_input_record_keeps_tool_fingerprints_and_drops_machine_state() {
        let roots = [
            (PathBuf::from("/work/tree"), "/"),
            (PathBuf::from("/cache/dir"), "@cache/"),
            (PathBuf::from("/cache/dir/sources"), "@sources/"),
        ];
        let record = |helm: &str| TargetDependencies {
            dependencies: BTreeMap::from([
                ("file:/work/tree/nyl/nyl.toml".to_owned(), dependency("file", "a")),
                (
                    "file:/cache/dir/sources/x/releases/r.yaml".to_owned(),
                    dependency("file", "b"),
                ),
                (
                    "directory:/cache/dir/helm/chart".to_owned(),
                    dependency("directory", "c"),
                ),
                ("tool:helm".to_owned(), dependency("tool", helm)),
                ("context:secrets".to_owned(), dependency("secret", "keyed")),
            ]),
            cacheable: true,
        };
        let (one, cacheable) = portable_input_record(&record("helm-3.15"), &roots);
        assert!(cacheable);
        assert_eq!(
            one,
            BTreeMap::from([
                ("/nyl/nyl.toml".to_owned(), "a".to_owned()),
                ("@sources/x/releases/r.yaml".to_owned(), "b".to_owned()),
                ("@directory:@cache/helm/chart".to_owned(), "c".to_owned()),
                ("@tool:helm".to_owned(), "helm-3.15".to_owned()),
            ])
        );
        // Upgrading a renderer tool changes the record.
        let (upgraded, _) = portable_input_record(&record("helm-3.16"), &roots);
        assert_ne!(upgraded, one);

        // A path outside every root cannot be named portably: the record no
        // longer covers the render.
        let mut outside = record("helm-3.15");
        outside
            .dependencies
            .insert("file:/etc/elsewhere".to_owned(), dependency("file", "d"));
        let (record_outside, cacheable) = portable_input_record(&outside, &roots);
        assert!(!cacheable);
        assert!(record_outside.keys().all(|key| !key.contains("/etc/")));
        // So does an uncacheable compile.
        let mut uncacheable = record("helm-3.15");
        uncacheable.cacheable = false;
        assert!(!portable_input_record(&uncacheable, &roots).1);
    }

    #[test]
    fn test_publication_credentials_are_read_only_through_the_given_lookup() {
        let (publication, url) = publication_repository();
        let worktree = project(&url);
        let project_dir = worktree.path().join("nyl");
        std::fs::write(
            project_dir.join("nyl.toml"),
            "[project]\n\n[[git.credentials]]\nurl = \"https://git.example.invalid/\"\ntoken_env = \"NYL_PUBLICATION_TEST_TOKEN\"\n",
        )
        .unwrap();
        let rendered = render(worktree.path(), &snapshot("registry.test/web:1"), None).unwrap();

        // The lookup is the only source of credential variables.
        let error = publish_target(&rendered, &project_dir, "publish", &author(1), &|_| None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("NYL_PUBLICATION_TEST_TOKEN"), "{error}");
        assert!(branch_files_or_empty(&publication.path().join("deploy.git"), "deploy").is_empty());

        let read = std::cell::RefCell::new(Vec::new());
        let lookup = |name: &str| {
            read.borrow_mut().push(name.to_owned());
            Some("token".to_owned())
        };
        publish_target(&rendered, &project_dir, "publish", &author(1), &lookup).unwrap();
        assert_eq!(read.into_inner(), ["NYL_PUBLICATION_TEST_TOKEN"]);
    }

    fn branch_files_or_empty(bare: &Path, branch: &str) -> Vec<String> {
        let repository = Repository::open_bare(bare).unwrap();
        if repository.find_reference(&format!("refs/heads/{branch}")).is_err() {
            return Vec::new();
        }
        branch_files(bare, branch)
    }

    #[tokio::test]
    async fn test_render_target_inside_an_async_runtime() {
        let (_publication, url) = publication_repository();
        let worktree = project(&url);
        let rendered = render(worktree.path(), &snapshot("registry.test/web:1"), None).unwrap();
        assert!(deployment(&rendered).contains("registry.test/web:1"));
    }

    fn branch_files(bare: &Path, branch: &str) -> Vec<String> {
        let repository = Repository::open_bare(bare).unwrap();
        let tree = repository
            .find_reference(&format!("refs/heads/{branch}"))
            .unwrap()
            .peel_to_tree()
            .unwrap();
        let mut files = Vec::new();
        tree.walk(git2::TreeWalkMode::PreOrder, |root, entry| {
            if entry.kind() == Some(git2::ObjectType::Blob) {
                files.push(format!("{root}{}", entry.name().unwrap()));
            }
            git2::TreeWalkResult::Ok
        })
        .unwrap();
        files
    }

    #[test]
    fn test_publish_target_writes_the_prefix_and_skips_an_unchanged_tree() {
        let (publication, url) = publication_repository();
        let bare = publication.path().join("deploy.git");
        let worktree = project(&url);
        let project_dir = worktree.path().join("nyl");
        let rendered = render(worktree.path(), &snapshot("registry.test/web:1"), None).unwrap();

        let published = publish_target(
            &rendered,
            &project_dir,
            "nyl: publish dev/kubernetes",
            &author(1_700_000_000),
            &|_| None,
        )
        .unwrap();
        let files = branch_files(&bare, "deploy");
        assert!(files.contains(&"dev/_nyl/index.json".to_owned()), "{files:?}");
        assert!(files.iter().any(|path| path.starts_with("dev/web/")), "{files:?}");
        assert!(files.iter().all(|path| path.starts_with("dev/")), "{files:?}");

        let repository = Repository::open_bare(&bare).unwrap();
        let head = repository
            .find_reference("refs/heads/deploy")
            .unwrap()
            .target()
            .unwrap();
        assert_eq!(published.commit, head.to_string());
        assert_eq!(published.branch, "deploy");
        assert_eq!(published.path_prefix, "dev");
        let commit = repository.find_commit(head).unwrap();
        assert_eq!(commit.author().name().unwrap(), "Nyl Orchestration");
        assert_eq!(commit.author().email().unwrap(), "nyl@example.invalid");
        assert_eq!(commit.author().when().seconds(), 1_700_000_000);
        assert_eq!(commit.committer().name().unwrap(), "Nyl Orchestration");
        assert_eq!(commit.committer().when().seconds(), 1_700_000_000);
        assert_eq!(
            commit.message().unwrap(),
            "nyl: publish dev/kubernetes\n\nChanged applications:\n- argocd/dev-catalog\n- argocd/web"
        );

        let index_blob = commit
            .tree()
            .unwrap()
            .get_path(Path::new("dev/_nyl/index.json"))
            .unwrap()
            .to_object(&repository)
            .unwrap()
            .peel_to_blob()
            .unwrap();
        let index = parse_index(index_blob.content(), "published index").unwrap();
        assert_eq!(published.index_digest, index_digest(&index).unwrap());
        assert!(published.index_digest.starts_with("sha256:"));

        // A second identical publication makes no commit.
        let again = publish_target(
            &rendered,
            &project_dir,
            "nyl: publish dev/kubernetes",
            &author(1_800_000_000),
            &|_| None,
        )
        .unwrap();
        assert_eq!(again, published);
        let head_after = repository
            .find_reference("refs/heads/deploy")
            .unwrap()
            .target()
            .unwrap();
        assert_eq!(head_after, head);
    }

    #[test]
    fn test_publication_author_is_utc_at_the_given_time() {
        let git = author(42).git().unwrap();
        assert_eq!(git.name().unwrap(), "Nyl Orchestration");
        assert_eq!(git.when().seconds(), 42);
        assert_eq!(git.when().offset_minutes(), 0);
    }

    #[test]
    fn test_writable_branch_name_accepts_only_branches() {
        assert_eq!(writable_branch_name("deploy/main").unwrap(), "deploy/main");
        assert_eq!(writable_branch_name("refs/heads/deploy/main").unwrap(), "deploy/main");
        assert!(writable_branch_name("refs/tags/v1").is_err());
    }

    fn commit_initial(repository: &Repository) -> Oid {
        let mut index = repository.index().unwrap();
        index.add_all(["*"], IndexAddOption::DEFAULT, None).unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repository.find_tree(tree_id).unwrap();
        let signature = git2::Signature::now("Initial", "initial@example.invalid").unwrap();
        repository
            .commit(Some("HEAD"), &signature, &signature, "Initial", &tree, &[])
            .unwrap()
    }

    #[test]
    fn test_publication_checkout_and_commit_are_scoped_to_the_rendered_tree() {
        let source = TempDir::new().unwrap();
        let source_repository = Repository::init(source.path()).unwrap();
        std::fs::write(source.path().join("source.txt"), "source branch\n").unwrap();
        let initial = commit_initial(&source_repository);

        std::fs::remove_file(source.path().join("source.txt")).unwrap();
        std::fs::create_dir_all(source.path().join("deploy")).unwrap();
        std::fs::write(source.path().join("deploy/old.yaml"), "old\n").unwrap();
        let mut index = source_repository.index().unwrap();
        index.clear().unwrap();
        index.add_path(Path::new("deploy/old.yaml")).unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = source_repository.find_tree(tree_id).unwrap();
        let initial_signature = git2::Signature::now("Initial", "initial@example.invalid").unwrap();
        source_repository
            .commit(
                Some("refs/heads/deploy/main"),
                &initial_signature,
                &initial_signature,
                "Deploy",
                &tree,
                &[&source_repository.find_commit(initial).unwrap()],
            )
            .unwrap();

        let checkout = TempDir::new().unwrap();
        let repository = clone_branch(
            source.path().to_str().unwrap(),
            "deploy/main",
            checkout.path(),
            &GitAuth::none(),
        )
        .unwrap();
        assert!(!checkout.path().join("source.txt").exists());
        assert!(checkout.path().join("deploy/old.yaml").is_file());
        assert!(repository.statuses(None).unwrap().is_empty());

        std::fs::write(checkout.path().join("unrelated.txt"), "do not stage\n").unwrap();
        std::fs::write(checkout.path().join("deploy/new.yaml"), "new\n").unwrap();
        let commit = commit_worktree(
            &repository,
            "deploy/main",
            &checkout.path().join("deploy"),
            "Render",
            || author(7).git(),
        )
        .unwrap()
        .unwrap();
        let commit = repository.find_commit(commit).unwrap();
        assert_eq!(commit.author().name().unwrap(), "Nyl Orchestration");
        assert!(commit.tree().unwrap().get_path(Path::new("deploy/new.yaml")).is_ok());
        assert!(commit.tree().unwrap().get_path(Path::new("unrelated.txt")).is_err());
    }

    #[test]
    fn test_absent_publication_branch_starts_with_an_empty_worktree() {
        let source = TempDir::new().unwrap();
        let source_repository = Repository::init(source.path()).unwrap();
        std::fs::write(source.path().join("source.txt"), "source branch\n").unwrap();
        commit_initial(&source_repository);

        let checkout = TempDir::new().unwrap();
        let repository = clone_branch(
            source.path().to_str().unwrap(),
            "deploy/new",
            checkout.path(),
            &GitAuth::none(),
        )
        .unwrap();
        assert!(!checkout.path().join("source.txt").exists());
        assert!(repository.statuses(None).unwrap().is_empty());
        assert!(repository.head().is_err());
    }

    /// The generated catalog's documents as `(kind, name, source path)`.
    fn catalog(files: &BTreeMap<PathBuf, Vec<u8>>) -> Vec<(String, String, Option<String>)> {
        files
            .iter()
            .filter(|(path, _)| path.starts_with(super::super::CATALOG_DIRECTORY))
            .flat_map(|(_, bytes)| {
                serde_saphyr::from_multiple::<serde_json::Value>(std::str::from_utf8(bytes).unwrap()).unwrap()
            })
            .map(|manifest| {
                (
                    manifest["kind"].as_str().unwrap().to_owned(),
                    manifest["metadata"]["name"].as_str().unwrap().to_owned(),
                    manifest
                        .pointer("/spec/source/path")
                        .and_then(serde_json::Value::as_str)
                        .map(ToOwned::to_owned),
                )
            })
            .collect()
    }

    /// Commit `path` with `contents` to `branch` of the bare repository at
    /// `url`, as another tool would.
    fn commit_unowned(url: &str, branch: &str, path: &str, contents: &str) {
        let checkout = TempDir::new().unwrap();
        let repository = clone_branch(url, branch, checkout.path(), &GitAuth::none()).unwrap();
        let expected = remote_branch_oid(&repository, branch);
        let file = checkout.path().join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, contents).unwrap();
        commit_worktree(&repository, branch, checkout.path(), "other tool", || author(5).git()).unwrap();
        crate::git::transport::push_branch_if_unchanged(&repository, url, branch, expected, &GitAuth::none()).unwrap();
    }

    /// Catches phase 1 emptying the catalog (Argo CD's empty-tree guard and
    /// the catalog's self-prune would then act) or leaving a workload
    /// Application for the catalog to keep.
    #[test]
    fn test_phase1_publishes_the_catalog_without_workload_applications() {
        let (publication, url) = publication_repository();
        let bare = publication.path().join("deploy.git");
        let worktree = project(&url);
        let project_dir = worktree.path().join("nyl");
        let rendered = render(worktree.path(), &snapshot("registry.test/web:1"), None).unwrap();
        publish_target(&rendered, &project_dir, "nyl: publish", &author(10), &|_| None).unwrap();

        let full = catalog(&rendered.files);
        let phase1 = without_workload_applications(&rendered).unwrap();
        let kept = catalog(&phase1.files);
        assert!(
            full.iter()
                .any(|(kind, _, path)| kind == "Application" && path.as_deref() != Some("dev/_nyl/catalog")),
            "the full catalog lists a workload Application: {full:?}"
        );
        assert_eq!(
            kept,
            full.iter()
                .filter(|(kind, _, path)| kind != "Application" || path.as_deref() == Some("dev/_nyl/catalog"))
                .cloned()
                .collect::<Vec<_>>()
        );
        assert!(kept.iter().any(|(kind, _, _)| kind == "AppProject"), "{kept:?}");
        assert!(
            kept.iter()
                .any(|(kind, _, path)| kind == "Application" && path.as_deref() == Some("dev/_nyl/catalog")),
            "{kept:?}"
        );
        // The workload trees stay.
        for (path, bytes) in rendered.files.iter().filter(|(path, _)| path.starts_with("web")) {
            assert_eq!(phase1.files.get(path), Some(bytes));
        }

        let published =
            prepare_target_publication(&phase1, &project_dir, "nyl: teardown phase 1", &author(20), &|_| None)
                .unwrap()
                .push()
                .unwrap();
        let files = branch_files(&bare, "deploy");
        let expected = phase1
            .files
            .keys()
            .map(|path| format!("dev/{}", path.display().to_string().replace('\\', "/")))
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(files.into_iter().collect::<std::collections::BTreeSet<_>>(), expected);
        let repository = Repository::open_bare(&bare).unwrap();
        let head = repository
            .find_reference("refs/heads/deploy")
            .unwrap()
            .target()
            .unwrap();
        assert_eq!(published.commit, head.to_string());

        // Publishing phase 1 again converges without a commit.
        let again = prepare_target_publication(&phase1, &project_dir, "nyl: teardown phase 1", &author(30), &|_| None)
            .unwrap()
            .push()
            .unwrap();
        assert_eq!(again.commit, published.commit);
    }

    /// Catches phase 2 deleting files the target does not own, or leaving
    /// an owned file or the ownership index behind.
    #[test]
    fn test_phase2_removes_exactly_the_index_owned_files() {
        let (publication, url) = publication_repository();
        let bare = publication.path().join("deploy.git");
        let worktree = project(&url);
        let project_dir = worktree.path().join("nyl");
        let rendered = render(worktree.path(), &snapshot("registry.test/web:1"), None).unwrap();
        publish_target(&rendered, &project_dir, "nyl: publish", &author(10), &|_| None).unwrap();
        commit_unowned(&url, "deploy", "dev/state/notes.yaml", "kept: true\n");

        let removed = prepare_target_removal(&rendered, &project_dir, "nyl: teardown phase 2", &author(40), &|_| None)
            .unwrap()
            .push()
            .unwrap();
        assert_eq!(branch_files(&bare, "deploy"), vec!["dev/state/notes.yaml".to_owned()]);
        let repository = Repository::open_bare(&bare).unwrap();
        let head = repository
            .find_reference("refs/heads/deploy")
            .unwrap()
            .target()
            .unwrap();
        assert_eq!(removed.commit, Some(head.to_string()));
        let commit = repository.find_commit(head).unwrap();
        assert_eq!(commit.message().unwrap(), "nyl: teardown phase 2");
        assert_eq!(commit.author().when().seconds(), 40);
        assert_eq!(
            (removed.branch.as_str(), removed.path_prefix.as_str()),
            ("deploy", "dev")
        );

        // Repeating an interrupted removal converges without a commit.
        let again = prepare_target_removal(&rendered, &project_dir, "nyl: teardown phase 2", &author(50), &|_| None)
            .unwrap()
            .push()
            .unwrap();
        assert_eq!(again.commit, None);
        assert_eq!(
            repository
                .find_reference("refs/heads/deploy")
                .unwrap()
                .target()
                .unwrap(),
            head
        );
    }

    /// Catches phase 2 deleting an owned file someone changed outside Nyl,
    /// which reconciliation refuses everywhere else.
    #[test]
    fn test_phase2_refuses_owned_files_modified_outside_nyl() {
        let (publication, url) = publication_repository();
        let bare = publication.path().join("deploy.git");
        let worktree = project(&url);
        let project_dir = worktree.path().join("nyl");
        let rendered = render(worktree.path(), &snapshot("registry.test/web:1"), None).unwrap();
        publish_target(&rendered, &project_dir, "nyl: publish", &author(10), &|_| None).unwrap();
        let owned = branch_files(&bare, "deploy")
            .into_iter()
            .find(|path| path.starts_with("dev/web/"))
            .unwrap();
        commit_unowned(&url, "deploy", &owned, "edited: true\n");

        let error = prepare_target_removal(&rendered, &project_dir, "nyl: teardown phase 2", &author(40), &|_| None)
            .err()
            .expect("the removal is refused")
            .to_string();
        assert!(error.contains("modified outside Nyl"), "{error}");
        assert!(branch_files(&bare, "deploy").contains(&owned));
    }

    /// Catches a serde rename of `TeardownSettings` breaking every stored
    /// publication: the recorded settings live in desired documents, which
    /// are read back strictly, so their camelCase keys are a persisted format.
    #[test]
    fn test_recorded_teardown_settings_have_a_pinned_json_shape() {
        let settings = TeardownSettings {
            catalog_prunes: Some(true),
            application_deletion_policy: Some("Foreground".to_owned()),
            namespace_delete_policies: BTreeMap::from([("web".to_owned(), "Confirm".to_owned())]),
            catalog_owned_by_parent: false,
        };
        let stored = serde_json::json!({
            "catalogPrunes": true,
            "applicationDeletionPolicy": "Foreground",
            "namespaceDeletePolicies": {"web": "Confirm"},
            "catalogOwnedByParent": false,
        });
        assert_eq!(serde_json::to_value(&settings).unwrap(), stored);
        assert_eq!(serde_json::from_value::<TeardownSettings>(stored).unwrap(), settings);
        // A catalog a parent owns records the settings Nyl cannot see as null.
        let external = serde_json::json!({
            "catalogPrunes": null,
            "applicationDeletionPolicy": null,
            "namespaceDeletePolicies": {},
            "catalogOwnedByParent": true,
        });
        let parent_owned = TeardownSettings {
            catalog_owned_by_parent: true,
            ..TeardownSettings::default()
        };
        assert_eq!(
            serde_json::from_value::<TeardownSettings>(external.clone()).unwrap(),
            parent_owned
        );
        assert_eq!(serde_json::to_value(parent_owned).unwrap(), external);
    }

    fn settings_of(rendered: &RenderedTarget) -> TeardownSettings {
        teardown_settings(rendered).unwrap()
    }

    /// Catches readiness judging a different catalog than the one the tree
    /// publishes: the default catalog syncs manually, cascades in the
    /// foreground, and the owned Namespaces ask for confirmation.
    #[test]
    fn test_teardown_settings_describe_the_default_rendered_tree() {
        let (_publication, url) = publication_repository();
        let worktree = project(&url);
        let rendered = render(worktree.path(), &snapshot("registry.test/web:1"), None).unwrap();
        let settings = settings_of(&rendered);
        assert_eq!(settings.catalog_prunes, Some(false));
        assert_eq!(settings.application_deletion_policy.as_deref(), Some("Foreground"));
        assert!(!settings.catalog_owned_by_parent);
        assert!(
            !settings.namespace_delete_policies.is_empty()
                && settings
                    .namespace_delete_policies
                    .values()
                    .all(|policy| policy == "Confirm"),
            "{settings:?}"
        );
    }

    fn edit_catalog(rendered: &mut RenderedTarget, edit: impl Fn(&mut serde_json::Value) -> bool) {
        let paths = rendered
            .files
            .keys()
            .filter(|path| path.starts_with(super::super::CATALOG_DIRECTORY))
            .cloned()
            .collect::<Vec<_>>();
        for path in paths {
            let text = String::from_utf8(rendered.files[&path].clone()).unwrap();
            let mut manifest = serde_saphyr::from_str::<serde_json::Value>(&text).unwrap();
            if manifest["kind"] == "Application" && edit(&mut manifest) {
                rendered
                    .files
                    .insert(path, serde_saphyr::to_string(&manifest).unwrap().into_bytes());
            }
        }
    }

    /// Catches each setting being read from the wrong Application: pruning
    /// and the catalog's finalizer come from the catalog Application, and a
    /// workload Application without a finalizer orphans its workloads.
    #[test]
    fn test_teardown_settings_follow_the_catalog_and_workload_applications() {
        let (_publication, url) = publication_repository();
        let worktree = project(&url);
        let rendered = render(worktree.path(), &snapshot("registry.test/web:1"), None).unwrap();
        let is_catalog = |manifest: &serde_json::Value| manifest["spec"]["source"]["path"] == "dev/_nyl/catalog";

        let mut pruning = rendered.clone();
        edit_catalog(&mut pruning, |manifest| {
            if !is_catalog(manifest) {
                return false;
            }
            manifest["spec"]["syncPolicy"]["automated"] = json!({"prune": true});
            manifest["metadata"]["finalizers"] = json!(["resources-finalizer.argocd.argoproj.io/background"]);
            true
        });
        let settings = settings_of(&pruning);
        assert_eq!(settings.catalog_prunes, Some(true));
        assert_eq!(settings.application_deletion_policy.as_deref(), Some("Background"));

        let mut disabled = rendered.clone();
        edit_catalog(&mut disabled, |manifest| {
            if !is_catalog(manifest) {
                return false;
            }
            manifest["spec"]["syncPolicy"]["automated"] = json!({"enabled": false, "prune": true});
            true
        });
        assert_eq!(settings_of(&disabled).catalog_prunes, Some(false));

        let mut orphaning = rendered.clone();
        edit_catalog(&mut orphaning, |manifest| {
            if is_catalog(manifest) {
                return false;
            }
            manifest["metadata"]["finalizers"] = json!([]);
            true
        });
        assert_eq!(
            settings_of(&orphaning).application_deletion_policy.as_deref(),
            Some("Orphan"),
            "the catalog's own Foreground finalizer does not make orphaned workloads safe"
        );
    }

    /// Catches a target whose catalog a parent owns being judged on settings
    /// Nyl cannot see: they are unknown, and the parent removes the catalog.
    #[test]
    fn test_teardown_settings_of_a_tree_without_a_catalog_application_are_external() {
        let (_publication, url) = publication_repository();
        let worktree = project(&url);
        let mut rendered = render(worktree.path(), &snapshot("registry.test/web:1"), None).unwrap();
        rendered.files.retain(|path, bytes| {
            !(path.starts_with(super::super::CATALOG_DIRECTORY)
                && String::from_utf8_lossy(bytes).contains("path: dev/_nyl/catalog"))
        });
        let settings = settings_of(&rendered);
        assert_eq!(settings.catalog_prunes, None);
        assert_eq!(settings.application_deletion_policy, None);
        assert!(settings.catalog_owned_by_parent);
    }

    /// Catches a lost race for the branch head being reported like a failed
    /// publication: the driver retries only a stale head, where nothing was
    /// pushed.
    #[test]
    fn test_publication_that_lost_the_branch_head_is_stale() {
        let (_publication, url) = publication_repository();
        let worktree = project(&url);
        let project_dir = worktree.path().join("nyl");
        let rendered = render(worktree.path(), &snapshot("registry.test/web:1"), None).unwrap();
        publish_target(&rendered, &project_dir, "nyl: publish", &author(10), &|_| None).unwrap();

        // The second publication changes a file, and is prepared on the branch head.
        let changed = render(worktree.path(), &snapshot("registry.test/web:2"), None).unwrap();
        let prepared =
            prepare_target_publication(&changed, &project_dir, "nyl: update", &author(20), &|_| None).unwrap();
        assert!(!prepared.is_unchanged());
        // Another writer moves the head before it pushes.
        commit_unowned(&url, "deploy", "elsewhere/state.json", "{}\n");
        let error = prepared.push().expect_err("the push loses the compare-and-swap");
        assert!(error.is_stale_publication(), "{error}");
    }

    /// An inline target of the preview kind: prefix `pr-1`, its catalog shared
    /// as `previews` with `sync_policy` (a YAML-ish JSON value).
    fn shared_inline(prefix: &str, shared: serde_json::Value) -> InlineTarget {
        let mut inline = inline_dev();
        inline.name = format!("{prefix}-kubernetes");
        inline.spec["publication"]["pathPrefix"] = json!(prefix);
        inline.spec["catalogApplication"] = json!({"shared": shared});
        inline
    }

    fn pruning_shared() -> serde_json::Value {
        json!({"name": "previews", "syncPolicy": {"automated": {"prune": true}}})
    }

    /// Catches a shared catalog leaving a catalog Application in the target's
    /// own tree (two owners for its Applications), a manifest that does not
    /// source the branch root with the contract's include pattern, an index
    /// whose owner is not `catalog:<name>`, and a teardown readiness read from
    /// a catalog that no longer exists in the tree.
    #[test]
    fn test_a_shared_catalog_replaces_the_targets_catalog_application() {
        let (_repository, url) = publication_repository();
        let worktree = inline_project(&url);
        let rendered = render_inline(worktree.path(), &shared_inline("pr-1", pruning_shared()), None).unwrap();

        let documents = catalog(&rendered.files);
        assert!(
            documents.iter().any(|(kind, _, _)| kind == "AppProject"),
            "the target keeps its AppProjects: {documents:?}"
        );
        assert!(
            documents
                .iter()
                .all(|(kind, _, path)| kind != "Application" || path.as_deref() != Some("pr-1/_nyl/catalog")),
            "the target's tree holds no catalog Application: {documents:?}"
        );

        let shared = rendered
            .shared_catalog
            .as_deref()
            .expect("the target names a shared catalog");
        assert_eq!(shared.destination.path_prefix, "_nyl/shared/previews");
        assert_eq!(shared.destination.branch, rendered.destination.branch);
        let manifest: serde_json::Value =
            serde_saphyr::from_str(std::str::from_utf8(&shared.files[Path::new("catalog.yaml")]).unwrap()).unwrap();
        assert_eq!(manifest["metadata"]["name"], "previews");
        assert_eq!(manifest["spec"]["source"]["path"], ".");
        assert_eq!(
            manifest["spec"]["source"]["directory"]["include"],
            "{_nyl/shared/previews/*.yaml,*/_nyl/catalog/*}"
        );
        assert_eq!(manifest["spec"]["source"]["directory"]["recurse"], true);
        let index = parse_index(&shared.files[Path::new(DEFAULT_INDEX_PATH)], DEFAULT_INDEX_PATH).unwrap();
        assert_eq!(index.version, 3);
        assert_eq!(index.owner, Some(RenderIndexOwner::catalog("previews")));
        let stored: serde_json::Value = serde_json::from_slice(&shared.files[Path::new(DEFAULT_INDEX_PATH)]).unwrap();
        assert_eq!(stored["owner"], "catalog:previews");

        // Readiness reads the shared Application: it prunes, and the target's
        // catalog directory is removed by it rather than left behind.
        let settings = teardown_settings(&rendered).unwrap();
        assert_eq!(settings.catalog_prunes, Some(true));
        assert_eq!(settings.application_deletion_policy.as_deref(), Some("Foreground"));
        assert!(settings.catalog_owned_by_parent);
        let unshared = render_inline(
            worktree.path(),
            &shared_inline("pr-1", json!({"name": "previews"})),
            None,
        )
        .unwrap();
        assert_eq!(teardown_settings(&unshared).unwrap().catalog_prunes, Some(false));
    }

    /// Catches the shared manifest depending on which target rendered it, or
    /// on the commit: two targets that share a catalog render byte-identical
    /// trees, so either may create it and neither can overwrite the other.
    #[test]
    fn test_targets_sharing_a_catalog_render_the_same_manifest() {
        let (_repository, url) = publication_repository();
        let worktree = inline_project(&url);
        let first = render_inline(worktree.path(), &shared_inline("pr-1", pruning_shared()), None).unwrap();
        let second = render_inline(worktree.path(), &shared_inline("pr-2", pruning_shared()), None).unwrap();
        assert_eq!(first.shared_catalog, second.shared_catalog);
        assert_ne!(first.files, second.files);
    }

    /// Catches a target's publication overwriting or ignoring the shared
    /// manifest: it is created when the branch has none, reported `Current`
    /// when equal, `Differs` (and left alone) when another Nyl version or
    /// declaration wrote it, and a prefix owned by something else is fenced.
    #[test]
    fn test_the_shared_catalog_is_created_once_and_never_overwritten_by_a_target() {
        let (publication, url) = publication_repository();
        let bare = publication.path().join("deploy.git");
        let worktree = inline_project(&url);
        let project_dir = worktree.path().join("nyl");
        let rendered = render_inline(worktree.path(), &shared_inline("pr-1", pruning_shared()), None).unwrap();
        let shared = rendered.shared_catalog.as_deref().unwrap();
        assert_eq!(
            shared_catalog_state(shared, &project_dir, &|_| None).unwrap(),
            SharedCatalogState::Missing
        );

        publish_target(
            shared,
            &project_dir,
            "nyl: create the shared catalog",
            &author(10),
            &|_| None,
        )
        .unwrap();
        let files = branch_files(&bare, "deploy");
        assert!(
            files.contains(&"_nyl/shared/previews/catalog.yaml".to_owned()),
            "{files:?}"
        );
        assert!(
            files.contains(&"_nyl/shared/previews/_nyl/index.json".to_owned()),
            "{files:?}"
        );
        assert_eq!(
            shared_catalog_state(shared, &project_dir, &|_| None).unwrap(),
            SharedCatalogState::Current
        );
        // Publishing it again is no commit.
        let again = prepare_target_publication(shared, &project_dir, "nyl: again", &author(20), &|_| None).unwrap();
        assert!(again.is_unchanged());

        let other = render_inline(
            worktree.path(),
            &shared_inline(
                "pr-2",
                json!({"name": "previews", "syncPolicy": {"automated": {"prune": false}}}),
            ),
            None,
        )
        .unwrap();
        assert_eq!(
            shared_catalog_state(other.shared_catalog.as_deref().unwrap(), &project_dir, &|_| None).unwrap(),
            SharedCatalogState::Differs(vec![PathBuf::from("catalog.yaml")])
        );

        // Another owner at the catalog's prefix fences it.
        let mut foreign = shared.clone();
        let mut index = parse_index(&foreign.files[Path::new(DEFAULT_INDEX_PATH)], DEFAULT_INDEX_PATH).unwrap();
        index.owner = Some(RenderIndexOwner::unit(
            "dev",
            "kubernetes",
            "0192f0c4-8d0a-7c3e-9a51-3c1f0e7d2b90",
        ));
        foreign.files.insert(
            PathBuf::from(DEFAULT_INDEX_PATH),
            serde_json::to_vec_pretty(&index).unwrap(),
        );
        let error = shared_catalog_state(&foreign, &project_dir, &|_| None).unwrap_err();
        assert!(error.is_owner_fenced(), "{error}");
        let error = prepare_target_publication(&foreign, &project_dir, "nyl: take over", &author(30), &|_| None)
            .err()
            .expect("another owner is refused");
        assert!(error.is_owner_fenced(), "{error}");
    }

    /// Catches a target publishing into the reserved prefix of shared
    /// catalogs: its render is refused with the stable code, so no unit
    /// reconciles `_nyl/shared/<name>/` as its own tree.
    #[test]
    fn test_a_target_cannot_publish_into_the_reserved_shared_prefix() {
        let (_publication, url) = publication_repository();
        let worktree = inline_project(&url);
        let mut inline = inline_dev();
        inline.spec["publication"]["pathPrefix"] = json!("_nyl/shared/previews");
        let error = render_inline(worktree.path(), &inline, None).unwrap_err().to_string();
        assert!(
            error.contains("NYL-KUBERNETES-SHARED-CATALOG-CONFLICT") && error.contains("_nyl/shared"),
            "{error}"
        );
    }
}
