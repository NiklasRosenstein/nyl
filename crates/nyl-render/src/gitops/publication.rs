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

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use git2::build::CheckoutBuilder;
use git2::{IndexAddOption, Oid, Repository, ResetType, StatusOptions};

use nyl_core::orchestration::ports::CommitIdentity;
use nyl_core::orchestration::time::Timestamp;

use super::inputs::{verify_publication_base, InputSnapshot, OrchestratedInputs, PublicationBase};
use super::reconcile::{
    parse_index, reconcile_rendered_tree_with_options, DesiredFiles, ReconcileOptions, RenderIndex,
    RenderIndexPublication, DEFAULT_INDEX_PATH,
};
use super::{
    compile_target_tree_recorded, discover_gitops_inventory, CacheMode, CompiledTargetTree, GitOpsCache,
    GitOpsInventory, TargetDependencies, TreeRenderOptions,
};
use crate::config::{ProjectConfig, ProjectFile};
use crate::git::transport::GitAuth;
use crate::{NylError, Result};

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
    let inventory = discover_gitops_inventory(&project_dir, None)?;
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
        orchestrated_inputs: Some(OrchestratedInputs::Pinned(request.inputs.clone())),
        ..TreeRenderOptions::default()
    };
    let (compiled, dependencies) = compile_target_tree_recorded(&inventory, request.target, &cache, options).await?;

    let source_commit = Repository::discover(&inventory.worktree_root)
        .ok()
        .and_then(|repository| repository.head().ok().and_then(|head| head.target()))
        .map(|oid| oid.to_string());
    let mut index = target_render_index(&inventory, &compiled, request.target, source_commit, false)?;
    let desired = compiled.owned_files();
    index.files = desired
        .iter()
        .map(|(path, bytes)| {
            crate::resources::relative_path_to_posix("rendered output path", path)
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
    Ok(RenderedTarget {
        destination: PublicationDestination {
            repository: crate::util::sanitize_url(&compiled.repository.repo_url),
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
    })
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
    Ok(RenderIndex::new(
        target_name.to_owned(),
        compiled.cluster.metadata.name.clone(),
        RenderIndexPublication {
            repository: repository_identity,
            revision: compiled.target.spec.publication.revision.clone(),
            path_prefix: compiled.target.publication_path_prefix().to_owned(),
        },
        source_commit,
        source_dirty,
        inputs,
    ))
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
/// Index v3 (owning environment and unit uid) is deferred to the
/// KubernetesPublication driver (M5): the index is written as `publish-tree`
/// writes it, so non-orchestrated output stays byte-identical.
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
            publication_base: rendered.publication_base.as_ref(),
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

/// `sha256:<hex>` over the canonical JSON of an ownership index.
pub fn index_digest(index: &RenderIndex) -> Result<String> {
    Ok(format!(
        "sha256:{}",
        nyl_core::digest::sha256_hex(&nyl_core::digest::canonical_json_bytes(index)?)
    ))
}

/// The project's `nyl.toml` in `project_dir`, or the defaults without one.
fn load_project_file(project_dir: &Path) -> Result<ProjectFile> {
    let file = project_dir.join("nyl.toml");
    if file.is_file() {
        Ok(ProjectConfig::load_from_dir(Some(file), None)?.config)
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
    /// The branch head `fromPublication` bindings read, if any.
    pub publication_base: Option<&'a PublicationBase>,
    pub auth: &'a GitAuth,
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
/// commit it with `message`, authored by what `author` returns. An unchanged tree
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
    let index = reconcile_rendered_tree_with_options(
        &output_root,
        request.desired,
        request.index.clone(),
        request.options.clone(),
    )?;
    let commit = commit_worktree(&repository, request.branch, &output_root, message, || author()?.git())?;
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
            return Err(NylError::config(format!(
                "Publication {}@{} advanced from {:?} to {actual:?}; refusing stale publish",
                crate::util::sanitize_url(&self.push_url),
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
        .map_err(|error| NylError::config(format!("Failed to publish publication branch: {error}")))
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
        .map_err(|error| NylError::config(format!("Failed to clone {}: {error}", crate::util::sanitize_url(url))))?;
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
    use super::*;
    use chrono::TimeZone;
    use serde_json::json;
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
            worktree,
            project_dir: &worktree.join("nyl"),
            target: "dev",
            inputs,
            cache_dir,
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

    #[test]
    fn test_render_target_without_a_snapshot_value_is_rejected() {
        let (_publication, url) = publication_repository();
        let worktree = project(&url);
        let error = render(worktree.path(), &InputSnapshot::new(), None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("fromUnit needs orchestrated execution"), "{error}");

        // Tree commands pass no snapshot at all and keep rejecting the binding.
        let inventory = discover_gitops_inventory(&worktree.path().join("nyl"), None).unwrap();
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
        assert_eq!(commit.message().unwrap(), "nyl: publish dev/kubernetes");

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
}
