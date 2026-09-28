//! Resolution of Release inputs from DeploymentTarget bindings.
//!
//! Contract: [Release inputs and bindings](../../../../design/release-inputs.md).
//!
//! - [Bindings](../../../../design/release-inputs.md#bindings): the key table and
//!   `effective input = target binding, otherwise Release default`.
//! - [Binding kinds](../../../../design/release-inputs.md#binding-kinds): `value`,
//!   `fromFile`, and `fromGit` resolve here; `fromUnit` and `fromPromotion`
//!   need orchestrated execution and are rejected. `fromGit` reads only its
//!   locked commit and never resolves `revision`. `fromPublication` reads the
//!   target's own publication branch at the base commit, or a carried
//!   working-tree file; a missing branch or file leaves the input unbound.
//! - [Provenance, caching, and validation](../../../../design/release-inputs.md#provenance-caching-and-validation):
//!   failures are reported together per target, before any Release renders,
//!   and each resolved input is digested for the ownership index.
//!
//! The declaration types and their pure rules live in
//! [`nyl_core::resources::release_inputs`].

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::resources::release_inputs::{BindingKind, InputBinding, InputDeclaration, ReleaseKey};
use crate::resources::{DeploymentTarget, InlineGitRepository};
use crate::util::project_path::ProjectPaths;
use crate::{NylError, Result};

/// Ownership-index key prefix of resolved input digests.
pub const INDEX_INPUT_PREFIX: &str = "@input/";

/// Ownership-index key prefix of `fromGit` blob digests.
pub const INDEX_GIT_PREFIX: &str = "@git/";
/// Ownership-index key prefix of state files read from the publication base commit.
pub const INDEX_PUBLICATION_PREFIX: &str = "@publication/";
/// Ownership-index key prefix of state files carried from the working tree.
pub const INDEX_CARRIED_PREFIX: &str = "@carried/";

/// How a command reads the publication branch head for `fromPublication`.
///
/// Contract: [`fromPublication`](../../../../design/release-inputs.md#binding-kinds),
/// Local commands and vendoring.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PublicationRead {
    /// Refresh the refs; a failed refresh is an error.
    #[default]
    Fresh,
    /// Use the cached refs only (`--offline`).
    Cached,
    /// Refresh when possible, otherwise use the cached refs, otherwise treat
    /// the state as unavailable so the bootstrap rule applies. `nyl vendor
    /// --check` reads this way, so it works as an offline pre-step.
    FreshOrCached,
}

impl PublicationRead {
    /// [`Self::Cached`] for `--offline`, otherwise [`Self::Fresh`].
    pub fn from_offline(offline: bool) -> Self {
        if offline {
            Self::Cached
        } else {
            Self::Fresh
        }
    }
}

/// Where a [`PublicationBase`] head came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PublicationBaseOrigin {
    /// Refreshed from the remote.
    Refreshed,
    /// Read from the cached refs, as requested.
    Cached,
    /// Read from the cached refs because the refresh failed.
    CachedAfterFailedRefresh,
    /// Neither refreshable nor cached; the state reads as not existing yet.
    Unavailable,
}

/// The publication branch head that `fromPublication` bindings read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicationBase {
    /// Repository URL the state is read from.
    pub url: String,
    /// Publication branch.
    pub branch: String,
    /// Target publication path prefix; state paths are relative to it.
    pub prefix: String,
    /// Branch head, or `None` when the branch does not exist yet or its
    /// state is [unavailable](PublicationBaseOrigin::Unavailable).
    pub commit: Option<String>,
    /// Where the head came from.
    pub origin: PublicationBaseOrigin,
}

impl PublicationBase {
    /// Resolve the head of `branch` as `read` asks.
    pub fn resolve(
        git: &dyn GitBlobSource,
        url: &str,
        branch: &str,
        prefix: &str,
        read: PublicationRead,
    ) -> Result<Self> {
        let branch = branch.strip_prefix("refs/heads/").unwrap_or(branch).to_owned();
        let error = |error: NylError, hint: &str| {
            NylError::config(format!(
                "Cannot read publication branch {branch} of {} for fromPublication Release inputs: {error}{hint}",
                crate::util::sanitize_url(url),
            ))
        };
        let (commit, origin) = match read {
            PublicationRead::Fresh => (
                git.branch_head(url, &branch, true).map_err(|e| {
                    error(
                        e,
                        "; check network access to the publication repository (render-tree and diff-tree can read the cached branch head with --offline)",
                    )
                })?,
                PublicationBaseOrigin::Refreshed,
            ),
            // Without a refresh, a branch missing from the cache cannot be
            // told apart from one that was never fetched.
            PublicationRead::Cached => match git.branch_head(url, &branch, false).map_err(|e| error(e, ""))? {
                Some(commit) => (Some(commit), PublicationBaseOrigin::Cached),
                None => (None, PublicationBaseOrigin::Unavailable),
            },
            PublicationRead::FreshOrCached => match git.branch_head(url, &branch, true) {
                Ok(commit) => (commit, PublicationBaseOrigin::Refreshed),
                Err(refresh_error) => {
                    tracing::warn!("Cannot refresh publication branch {branch}: {refresh_error}; using cached state");
                    match git.branch_head(url, &branch, false) {
                        Ok(Some(commit)) => (Some(commit), PublicationBaseOrigin::CachedAfterFailedRefresh),
                        Ok(None) | Err(_) => (None, PublicationBaseOrigin::Unavailable),
                    }
                }
            },
        };
        Ok(Self {
            url: url.to_owned(),
            branch,
            prefix: prefix.trim_matches('/').to_owned(),
            commit,
            origin,
        })
    }

    /// Human-readable description of the state this render read.
    pub fn describe(&self) -> String {
        let url = crate::util::sanitize_url(&self.url);
        match (&self.commit, self.origin) {
            (_, PublicationBaseOrigin::Unavailable) => format!(
                "Publication branch {} of {url} is not in the local Git cache (it was never fetched, or does not exist yet) and was not refreshed; fromPublication inputs are unbound",
                self.branch
            ),
            (Some(commit), origin) => format!(
                "Read publication state from {url}@{} at {commit}{}",
                self.branch,
                match origin {
                    PublicationBaseOrigin::Cached => " (cached head, --offline)",
                    PublicationBaseOrigin::CachedAfterFailedRefresh => " (cached head; the refresh failed)",
                    _ => "",
                }
            ),
            (None, _) => format!(
                "Publication branch {} of {url} does not exist yet; fromPublication inputs are unbound",
                self.branch
            ),
        }
    }

    fn repository_path(&self, path: &str) -> String {
        if self.prefix.is_empty() {
            path.to_owned()
        } else {
            format!("{}/{path}", self.prefix)
        }
    }
}

/// Where resolution may read binding sources from.
pub struct InputSources<'a> {
    /// Local path rule of this project, for `fromFile`.
    pub paths: &'a ProjectPaths,
    /// Git-visible YAML and JSON files, relative to the worktree root. A
    /// `fromFile` binding reads only these, so published output reproduces
    /// from committed source.
    pub visible_files: &'a BTreeSet<PathBuf>,
    /// Resolves a `fromGit` `repositoryRef` to the GitRepository it names and
    /// that resource's absolute source file, through
    /// [`GitOpsInventory::resolve_git_repository`](super::GitOpsInventory::resolve_git_repository).
    pub repositories: &'a RepositoryResolver<'a>,
    /// The target's own publication location. `fromGit` must not lock a file
    /// there; that state is read with `fromPublication`.
    pub publication_scope: Option<&'a PublicationScope>,
    /// Reader of files at locked commits, for `fromGit` and `fromPublication`.
    pub git: &'a dyn GitBlobSource,
    /// Publication branch head, for `fromPublication`. Resolution fails when
    /// a binding needs it and it is absent.
    pub publication: Option<&'a PublicationBase>,
    /// State bytes of another render of the same command, by path relative
    /// to the prefix. A binding with `carryFileFromWorktree` reads them instead
    /// of the worktree, so a comparison baseline sees the desired render's
    /// carried state.
    pub pinned_state: Option<&'a BTreeMap<PathBuf, Vec<u8>>>,
}

/// Resolves a GitRepository name to the repository and its source file.
pub type RepositoryResolver<'a> =
    dyn Fn(&crate::resources::LocalReference) -> Result<(InlineGitRepository, PathBuf)> + 'a;

/// Where a DeploymentTarget publishes: its repository URLs, branch, and prefix.
///
/// Contract: [`fromGit`](../../../../design/release-inputs.md#binding-kinds),
/// source locks. A lock on a file inside a target's publication moves to that
/// target's newest publication; a target cannot lock its own publication,
/// because each publication would make the lock stale again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicationScope {
    pub target: String,
    /// Read and publish URLs, normalized for equality.
    urls: Vec<String>,
    branch: String,
    prefix: String,
}

impl PublicationScope {
    /// The publication location of `target`.
    pub fn of(inventory: &super::GitOpsInventory, target: &DeploymentTarget) -> Result<Self> {
        let publication = &target.spec.publication;
        let (repository, _) =
            inventory.resolve_git_repository(publication.repository_ref.as_ref(), publication.repository.as_ref())?;
        Ok(Self {
            target: target.metadata.name.clone(),
            urls: std::iter::once(&repository.repo_url)
                .chain(repository.publish_url.as_ref())
                .map(|url| crate::git::normalize_git_url_for_equality(url))
                .collect(),
            branch: branch_name(&publication.revision).to_owned(),
            prefix: target.publication_path_prefix().trim_matches('/').to_owned(),
        })
    }

    /// Whether `path` at `revision` of `url` lies inside this publication.
    pub fn contains(&self, url: &str, revision: &str, path: &str) -> bool {
        let url = crate::git::normalize_git_url_for_equality(url);
        self.urls.contains(&url)
            && branch_name(revision) == self.branch
            && (self.prefix.is_empty()
                || path
                    .strip_prefix(&self.prefix)
                    .is_some_and(|rest| rest.starts_with('/')))
    }
}

/// The rejection of a `fromGit` lock on its own target's publication.
pub fn self_lock_error(lock: &str, target: &str) -> String {
    format!(
        "{lock} locks a file in the publication of DeploymentTarget {target} itself; every publication would make the lock stale, so read the target's own state with fromPublication"
    )
}

fn branch_name(revision: &str) -> &str {
    super::tree::normalize_branch_revision(revision)
}

/// Reads a file at an immutable commit.
pub trait GitBlobSource {
    /// The bytes of `path` at `commit` of `url`, or `None` when the commit has
    /// no such path.
    /// Locked `fromGit` files go through the vendor policy.
    fn read_blob(&self, url: &str, commit: &str, path: &str) -> Result<Option<Vec<u8>>>;

    /// A file of the publication branch at `commit`, read from Git and never
    /// vendored: publication state moves with every publication.
    fn read_publication_blob(&self, url: &str, commit: &str, path: &str) -> Result<Option<Vec<u8>>>;

    /// The commit `branch` of `url` names, or `None` when it does not exist,
    /// refreshing refs first when `refresh` is set.
    fn branch_head(&self, url: &str, branch: &str, refresh: bool) -> Result<Option<String>>;
}

/// [`GitBlobSource`] over the project's remote artifacts: the vendor snapshot,
/// then the exact source cache, then the shared bare-repository cache.
///
/// Contract: [`fromGit`](../../../../design/release-inputs.md#binding-kinds).
/// A locked file is a remote renderer input like a remote group source, so
/// `nyl vendor` captures it, `nyl vendor --check` requires it, and vendor mode
/// `required` never reads it from the network.
pub struct CachedGitBlobSource {
    manager: RefCell<Option<crate::git::GitManager>>,
    project_root: PathBuf,
    project_config: crate::config::ProjectConfig,
    cache: Option<crate::render::cache::RenderCache>,
    /// Created on first use, so targets without `fromGit` bindings never
    /// load the vendor lock.
    artifacts: std::cell::OnceCell<crate::render::artifact::ArtifactResolver>,
}

impl CachedGitBlobSource {
    /// Reuse `manager` when one exists; otherwise one is created on first use
    /// in the cache's external root, or the default Git cache.
    pub fn new(
        manager: Option<crate::git::GitManager>,
        project_root: &Path,
        project_config: &crate::config::ProjectConfig,
        cache: Option<crate::render::cache::RenderCache>,
    ) -> Self {
        Self {
            manager: RefCell::new(manager),
            project_root: project_root.to_path_buf(),
            project_config: project_config.clone(),
            cache,
            artifacts: std::cell::OnceCell::new(),
        }
    }

    fn artifacts(&self) -> Result<&crate::render::artifact::ArtifactResolver> {
        if let Some(artifacts) = self.artifacts.get() {
            return Ok(artifacts);
        }
        let artifacts = crate::render::artifact::ArtifactResolver::new(
            &self.project_root,
            &self.project_config,
            self.cache.clone(),
        )?;
        Ok(self.artifacts.get_or_init(|| artifacts))
    }

    fn with_manager<T>(
        &self,
        operation: impl FnOnce(&mut crate::git::GitManager) -> crate::git::Result<T>,
    ) -> Result<T> {
        let mut manager = self.manager.borrow_mut();
        if manager.is_none() {
            *manager = Some(crate::git::GitManager::for_cache(self.cache.as_ref())?);
        }
        Ok(operation(manager.as_mut().expect("manager was created above"))?)
    }
}

impl GitBlobSource for CachedGitBlobSource {
    fn read_blob(&self, url: &str, commit: &str, path: &str) -> Result<Option<Vec<u8>>> {
        use crate::render::artifact::{ArtifactFormat, ArtifactRequest};
        let request = ArtifactRequest::GitBlob {
            repository: credential_free_url(url),
            commit: commit.to_owned(),
            path: path.to_owned(),
        };
        let artifacts = self.artifacts()?;
        if let Some(artifact) = artifacts.lookup(&request)? {
            return Ok(Some(std::fs::read(&artifact.path)?));
        }
        let Some(bytes) = self.read_publication_blob(url, commit, path)? else {
            return Ok(None);
        };
        let staged = tempfile::NamedTempFile::new()?;
        std::fs::write(staged.path(), &bytes)?;
        artifacts.store(
            &request,
            staged.path(),
            ArtifactFormat::GitBlob,
            Some(commit.to_owned()),
        )?;
        Ok(Some(bytes))
    }

    fn read_publication_blob(&self, url: &str, commit: &str, path: &str) -> Result<Option<Vec<u8>>> {
        self.with_manager(|manager| manager.read_blob(url, commit, path))
    }

    fn branch_head(&self, url: &str, branch: &str, refresh: bool) -> Result<Option<String>> {
        self.with_manager(|manager| manager.branch_head(url, branch, refresh))
    }
}

pub use crate::util::credential_free_url;

/// A Release the target renders, with its literal input declarations.
pub struct ReleaseDeclaration<'a> {
    /// `<applicationGroup>/<release>` on the target.
    pub key: ReleaseKey,
    /// Literal `spec.inputs`; empty when the Release declares none.
    pub declarations: &'a BTreeMap<String, InputDeclaration>,
    /// Release entry file, for messages.
    pub source: &'a Path,
}

/// Where one effective input value came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputOrigin {
    /// The Release `default`.
    Default,
    /// An inline `value` binding.
    Value,
    /// A `fromFile` binding reading this project file.
    File(PathBuf),
    /// A `fromGit` binding reading `path` at `commit` of `url`.
    Git {
        /// Repository URL without credentials.
        url: String,
        commit: String,
        path: String,
        /// SHA-256 of the file bytes.
        blob_digest: String,
        /// Source file of the referenced GitRepository resource.
        repository_source: Option<PathBuf>,
    },
    /// A `fromPublication` binding reading `path` at the publication base commit.
    Publication {
        /// Path relative to the publication prefix.
        path: String,
        commit: String,
        blob_digest: String,
        /// The bytes to write back to `path`, when the binding declares `carryFileFromWorktree`.
        carried_back: Option<Vec<u8>>,
    },
    /// A `fromPublication` binding reading its `carryFileFromWorktree` file from the working tree.
    Carried {
        /// Path relative to the publication prefix that receives the bytes.
        path: String,
        /// Working-tree file the bytes came from.
        source: PathBuf,
        blob_digest: String,
        bytes: Vec<u8>,
    },
    /// A `--input` or `--inputs` override of a direct command.
    Override,
}

/// One effective input.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedInput {
    pub value: Value,
    pub origin: InputOrigin,
}

/// Effective inputs of one Release that declares inputs.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResolvedReleaseInputs {
    pub inputs: BTreeMap<String, ResolvedInput>,
}

impl ResolvedReleaseInputs {
    /// The `inputs` template variable.
    pub fn values(&self) -> serde_json::Map<String, Value> {
        self.inputs
            .iter()
            .map(|(name, input)| (name.clone(), input.value.clone()))
            .collect()
    }
}

/// Effective inputs of every Release a target renders that declares inputs.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResolvedTargetInputs {
    pub releases: BTreeMap<ReleaseKey, ResolvedReleaseInputs>,
    /// Every `fromPublication` path the target's bindings name, bound or not,
    /// to whether an enabled binding declares `carryFileFromWorktree`. Paths of
    /// disabled groups only are not carried, so their files are kept unowned.
    pub state_paths: BTreeMap<String, bool>,
}

impl ResolvedTargetInputs {
    /// Project files the bindings read: `fromFile` files and the resources of
    /// GitRepositories that `fromGit` names.
    pub fn files(&self) -> BTreeSet<PathBuf> {
        self.releases
            .values()
            .flat_map(|release| release.inputs.values())
            .filter_map(|input| match &input.origin {
                InputOrigin::File(path)
                | InputOrigin::Git {
                    repository_source: Some(path),
                    ..
                } => Some(path.clone()),
                _ => None,
            })
            .collect()
    }

    /// State files this target writes: carried bytes, or the base copy written
    /// back, keyed by path relative to the publication prefix.
    pub fn carried_files(&self) -> BTreeMap<String, Vec<u8>> {
        self.origins()
            .filter_map(|origin| match origin {
                InputOrigin::Carried { path, bytes, .. }
                | InputOrigin::Publication {
                    path,
                    carried_back: Some(bytes),
                    ..
                } => Some((path.clone(), bytes.clone())),
                _ => None,
            })
            .collect()
    }

    /// State paths other tools commit: declared without `carryFileFromWorktree`, so the
    /// target never owns them.
    pub fn committed_state_paths(&self) -> BTreeSet<String> {
        self.state_paths
            .iter()
            .filter(|(_, carry)| !**carry)
            .map(|(path, _)| path.clone())
            .collect()
    }

    fn origins(&self) -> impl Iterator<Item = &InputOrigin> {
        self.releases
            .values()
            .flat_map(|release| release.inputs.values())
            .map(|input| &input.origin)
    }

    /// Ownership-index entries: `@input/<group>/<release>/<input>` to the digest
    /// of the canonical JSON value.
    pub fn index_entries(&self) -> Result<BTreeMap<String, String>> {
        let mut entries = BTreeMap::new();
        for (key, release) in &self.releases {
            for (name, input) in &release.inputs {
                entries.insert(format!("{INDEX_INPUT_PREFIX}{key}/{name}"), value_digest(&input.value)?);
                if let InputOrigin::Git {
                    url,
                    commit,
                    path,
                    blob_digest,
                    ..
                } = &input.origin
                {
                    entries.insert(format!("{INDEX_GIT_PREFIX}{url}@{commit}/{path}"), blob_digest.clone());
                }
                match &input.origin {
                    InputOrigin::Publication { path, blob_digest, .. } => {
                        entries.insert(format!("{INDEX_PUBLICATION_PREFIX}{path}"), blob_digest.clone());
                    }
                    InputOrigin::Carried { path, blob_digest, .. } => {
                        entries.insert(format!("{INDEX_CARRIED_PREFIX}{path}"), blob_digest.clone());
                    }
                    _ => {}
                }
            }
        }
        Ok(entries)
    }
}

/// Digest of an input's canonical JSON value.
pub fn value_digest(value: &Value) -> Result<String> {
    Ok(nyl_core::digest::sha256_hex(&nyl_core::digest::canonical_json_bytes(
        value,
    )?))
}

/// Resolve the inputs of every rendered Release of `target`.
///
/// `releases` lists the Releases of selected, enabled groups; keys naming a
/// group in `disabled_groups` are ignored so `enabled` can still toggle a
/// group. Every other problem is collected and reported together.
pub fn resolve_target_inputs(
    target: &DeploymentTarget,
    releases: &[ReleaseDeclaration<'_>],
    disabled_groups: &BTreeSet<String>,
    sources: &InputSources<'_>,
) -> Result<ResolvedTargetInputs> {
    let target_name = &target.metadata.name;
    let mut issues = Vec::new();
    let mut declared = BTreeMap::new();
    for release in releases {
        if let Some(previous) = declared.insert(release.key.clone(), release) {
            if !release.declarations.is_empty() || !previous.declarations.is_empty() {
                issues.push(format!(
                    "{} is declared by both {} and {}; Releases that declare inputs need unique names within their ApplicationGroup",
                    release.key,
                    previous.source.display(),
                    release.source.display()
                ));
            }
        }
    }

    let mut bound = BTreeMap::new();
    let mut disabled_state_paths = BTreeSet::new();
    for (key_text, bindings) in &target.spec.release_inputs {
        let key = ReleaseKey::parse(key_text)?;
        if disabled_groups.contains(&key.group) {
            disabled_state_paths.extend(
                bindings
                    .values()
                    .filter_map(|binding| binding.from_publication.as_ref())
                    .map(|source| source.path.clone()),
            );
            continue;
        }
        let Some(release) = declared.get(&key) else {
            issues.push(format!(
                "spec.releaseInputs.{key_text:?} names no Release this target renders; keys are <applicationGroup>/<release> of a selected ApplicationGroup"
            ));
            continue;
        };
        if release.declarations.is_empty() {
            issues.push(format!(
                "spec.releaseInputs.{key_text:?} binds Release {key}, which declares no spec.inputs ({})",
                release.source.display()
            ));
            continue;
        }
        issues.extend(undeclared_binding_issues(&key, bindings, release.declarations));
        bound.insert(key, bindings);
    }

    let mut resolved = ResolvedTargetInputs::default();
    for bindings in bound.values() {
        for binding in bindings.values() {
            if let Some(source) = &binding.from_publication {
                resolved
                    .state_paths
                    .insert(source.path.clone(), source.carry_file_from_worktree.is_some());
            }
        }
    }
    // A disabled group's state persists: its paths are released rather than
    // deleted, and re-enabling a carried binding adopts the file again.
    for path in disabled_state_paths {
        resolved.state_paths.entry(path).or_insert(false);
    }
    for (key, release) in &declared {
        if release.declarations.is_empty() {
            continue;
        }
        let bindings = bound.get(key).copied();
        let inputs = resolve_release_inputs(
            &format!("spec.releaseInputs.{:?}", key.to_string()),
            &key.to_string(),
            release.declarations,
            bindings,
            &BTreeMap::new(),
            sources,
            &mut issues,
        );
        resolved.releases.insert(key.clone(), inputs);
    }

    if issues.is_empty() {
        Ok(resolved)
    } else {
        Err(NylError::config(format!(
            "DeploymentTarget {target_name:?} has invalid Release inputs:\n{}",
            issues
                .iter()
                .map(|issue| format!("  - {issue}"))
                .collect::<Vec<_>>()
                .join("\n")
        )))
    }
}

/// A binding must name an input its Release declares.
pub fn undeclared_binding_issues(
    key: &ReleaseKey,
    bindings: &BTreeMap<String, InputBinding>,
    declarations: &BTreeMap<String, InputDeclaration>,
) -> Vec<String> {
    bindings
        .keys()
        .filter(|name| !declarations.contains_key(*name))
        .map(|name| {
            format!(
                "spec.releaseInputs.{:?}.{name} binds an input Release {key} does not declare; declared inputs: {}",
                key.to_string(),
                declarations.keys().cloned().collect::<Vec<_>>().join(", ")
            )
        })
        .collect()
}

/// Resolve one Release's declared inputs.
///
/// `effective input = override, otherwise binding, otherwise default`.
/// Overrides come from direct commands only. Problems are appended to
/// `issues`; the returned inputs then omit the failing names.
pub fn resolve_release_inputs(
    field_prefix: &str,
    release: &str,
    declarations: &BTreeMap<String, InputDeclaration>,
    bindings: Option<&BTreeMap<String, InputBinding>>,
    overrides: &BTreeMap<String, Value>,
    sources: &InputSources<'_>,
    issues: &mut Vec<String>,
) -> ResolvedReleaseInputs {
    let mut resolved = ResolvedReleaseInputs::default();
    for (name, declaration) in declarations {
        let field = format!("{field_prefix}.{name}");
        let binding = bindings.and_then(|bindings| bindings.get(name));
        let candidate = if let Some(value) = overrides.get(name) {
            Ok(Some(ResolvedInput {
                value: value.clone(),
                origin: InputOrigin::Override,
            }))
        } else {
            // An unbound fromPublication binding (bootstrap) falls back to the
            // default like a missing binding.
            binding
                .map(|binding| resolve_binding(&field, binding, sources))
                .transpose()
                .map(Option::flatten)
                .map(|input| {
                    input.or_else(|| {
                        declaration.default.clone().map(|value| ResolvedInput {
                            value,
                            origin: InputOrigin::Default,
                        })
                    })
                })
        };
        match candidate {
            Ok(Some(input)) => match declaration.check(&input.value) {
                Ok(()) => {
                    resolved.inputs.insert(name.clone(), input);
                }
                Err(reason) => issues.push(format!(
                    "{} for input {name:?} of Release {release} is invalid: {reason}",
                    describe_origin(&field, &input.origin)
                )),
            },
            Ok(None) => {
                let description = declaration
                    .description
                    .as_deref()
                    .map(|description| format!(" ({description})"))
                    .unwrap_or_default();
                let unavailable = sources
                    .publication
                    .is_some_and(|base| base.origin == PublicationBaseOrigin::Unavailable);
                issues.push(match binding.and_then(|binding| binding.from_publication.as_ref()) {
                    Some(source) if unavailable => format!(
                        "Release {release} requires input {name:?}{description}, but {field}.fromPublication state file {} is unavailable: the publication branch was not refreshed and is not in the local Git cache, and the input has no default; run once with network access or declare a default",
                        source.path
                    ),
                    Some(source) => format!(
                        "Release {release} requires input {name:?}{description}, but {field}.fromPublication state file {} does not exist on the publication branch yet and the input has no default; commit the state file, provide its carryFileFromWorktree, or declare a default",
                        source.path
                    ),
                    None => format!(
                        "Release {release} requires input {name:?}{description}, but it has no binding and no default; bind it in {field_prefix}"
                    ),
                });
            }
            Err(error) => issues.push(error),
        }
    }
    resolved
}

fn describe_origin(field: &str, origin: &InputOrigin) -> String {
    match origin {
        InputOrigin::Default => "The default".to_owned(),
        InputOrigin::Value => format!("{field}.value"),
        InputOrigin::File(path) => format!("{field}.fromFile ({})", path.display()),
        InputOrigin::Git { url, commit, path, .. } => format!("{field}.fromGit ({url}@{commit}/{path})"),
        InputOrigin::Publication { path, commit, .. } => format!("{field}.fromPublication ({path} at {commit})"),
        InputOrigin::Carried { source, .. } => {
            format!("{field}.fromPublication carryFileFromWorktree ({})", source.display())
        }
        InputOrigin::Override => "The --input/--inputs override".to_owned(),
    }
}

/// Resolve one binding; `Ok(None)` leaves the input unbound.
fn resolve_binding(
    field: &str,
    binding: &InputBinding,
    sources: &InputSources<'_>,
) -> std::result::Result<Option<ResolvedInput>, String> {
    let kind = binding.kind(field).map_err(|error| error.to_string())?;
    if kind == BindingKind::FromPublication {
        return resolve_publication(field, binding, sources);
    }
    resolve_bound(field, kind, binding, sources).map(Some)
}

fn resolve_publication(
    field: &str,
    binding: &InputBinding,
    sources: &InputSources<'_>,
) -> std::result::Result<Option<ResolvedInput>, String> {
    let source = binding
        .from_publication
        .as_ref()
        .expect("kind agrees with the set field");
    let field = format!("{field}.fromPublication");
    if let Some(carry) = &source.carry_file_from_worktree {
        let carry_path = sources
            .paths
            .resolve(&format!("{field}.carryFileFromWorktree"), carry)
            .map_err(|error| error.to_string())?;
        if is_tracked(&carry_path) {
            return Err(format!(
                "{field}.carryFileFromWorktree {carry} is tracked by Git; a carried file is produced by this run and left uncommitted, so bind tracked files with fromFile"
            ));
        }
        // A pinned state (a comparison baseline) replaces the worktree file.
        let carried = match sources.pinned_state {
            Some(pinned) => pinned.get(Path::new(&source.path)).cloned(),
            None if carry_path.is_file() => Some(
                std::fs::read(&carry_path)
                    .map_err(|error| format!("{field}.carryFileFromWorktree: cannot read {carry}: {error}"))?,
            ),
            None => None,
        };
        if let Some(bytes) = carried {
            let document = parse_single_document(&bytes)
                .map_err(|reason| format!("{field}.carryFileFromWorktree: {carry} {reason}"))?;
            let value = select(&document, &source.pointer)
                .map_err(|reason| format!("{field}.carryFileFromWorktree: {carry} {reason}"))?;
            return Ok(Some(ResolvedInput {
                value,
                origin: InputOrigin::Carried {
                    path: source.path.clone(),
                    source: carry_path,
                    blob_digest: nyl_core::digest::sha256_hex(&bytes),
                    bytes,
                },
            }));
        }
    }
    let base = sources
        .publication
        .ok_or_else(|| format!("{field} needs the target's publication branch, which this command does not read"))?;
    let Some(commit) = &base.commit else {
        return Ok(None);
    };
    let Some(bytes) = sources
        .git
        .read_publication_blob(&base.url, commit, &base.repository_path(&source.path))
        .map_err(|error| {
            format!(
                "{field}: cannot read {} at publication commit {commit}: {error}",
                source.path
            )
        })?
    else {
        if source.carry_file_from_worktree.is_some() && owned_at(sources.git, base, commit, &source.path)? {
            return Err(format!(
                "{field}: {} is owned by this target but was deleted from the publication branch outside Nyl; restore it, or provide its carry file",
                source.path
            ));
        }
        return Ok(None);
    };
    let document = parse_single_document(&bytes).map_err(|reason| format!("{field}: {} {reason}", source.path))?;
    let value = select(&document, &source.pointer).map_err(|reason| format!("{field}: {} {reason}", source.path))?;
    Ok(Some(ResolvedInput {
        value,
        origin: InputOrigin::Publication {
            path: source.path.clone(),
            commit: commit.clone(),
            blob_digest: nyl_core::digest::sha256_hex(&bytes),
            carried_back: source.carry_file_from_worktree.is_some().then_some(bytes),
        },
    }))
}

/// Whether the ownership index at `commit` records `path` as owned.
fn owned_at(
    git: &dyn GitBlobSource,
    base: &PublicationBase,
    commit: &str,
    path: &str,
) -> std::result::Result<bool, String> {
    let index_path = base.repository_path(super::reconcile::DEFAULT_INDEX_PATH);
    let Some(bytes) = git
        .read_publication_blob(&base.url, commit, &index_path)
        .map_err(|error| format!("cannot read {index_path} at publication commit {commit}: {error}"))?
    else {
        return Ok(false);
    };
    let index = super::reconcile::parse_index(&bytes, &format!("{index_path} at publication commit {commit}"))
        .map_err(|error| error.to_string())?;
    Ok(index.files.contains_key(path))
}

/// Whether Git tracks `path` in the index of the repository containing it.
fn is_tracked(path: &Path) -> bool {
    let Some(parent) = path.parent() else {
        return false;
    };
    let Ok(repository) = git2::Repository::discover(parent) else {
        return false;
    };
    let (Some(workdir), Ok(index)) = (repository.workdir(), repository.index()) else {
        return false;
    };
    let workdir = workdir.canonicalize().unwrap_or_else(|_| workdir.to_path_buf());
    let path = parent.canonicalize().map_or_else(
        |_| path.to_path_buf(),
        |parent| parent.join(path.file_name().unwrap_or_default()),
    );
    path.strip_prefix(&workdir)
        .is_ok_and(|relative| index.get_path(relative, 0).is_some())
}

fn resolve_bound(
    field: &str,
    kind: BindingKind,
    binding: &InputBinding,
    sources: &InputSources<'_>,
) -> std::result::Result<ResolvedInput, String> {
    match kind {
        BindingKind::Value => Ok(ResolvedInput {
            value: binding.value.clone().expect("kind agrees with the set field"),
            origin: InputOrigin::Value,
        }),
        BindingKind::FromFile => {
            let source = binding.from_file.as_ref().expect("kind agrees with the set field");
            let path = sources
                .paths
                .resolve(&format!("{field}.fromFile.path"), &source.path)
                .map_err(|error| error.to_string())?;
            let visible = path
                .strip_prefix(&sources.paths.worktree_root)
                .is_ok_and(|relative| sources.visible_files.contains(relative));
            if !visible {
                return Err(format!(
                    "{field}.fromFile.path {:?} names no Git-visible YAML or JSON file of this repository; the file must exist and must not be ignored by Git or lie in the output or vendor subtree",
                    source.path
                ));
            }
            let document = read_single_document(&path).map_err(|reason| format!("{field}.fromFile: {reason}"))?;
            let value = select(&document, &source.pointer)
                .map_err(|reason| format!("{field}.fromFile: {} {reason}", source.path))?;
            Ok(ResolvedInput {
                value,
                origin: InputOrigin::File(path),
            })
        }
        BindingKind::FromGit => {
            let source = binding.from_git.as_ref().expect("kind agrees with the set field");
            let (repository, repository_source) = match (&source.repository, &source.repository_ref) {
                (Some(repository), _) => (repository.clone(), None),
                (None, Some(reference)) => {
                    let (repository, path) = (sources.repositories)(reference)
                        .map_err(|error| format!("{field}.fromGit.repositoryRef: {error}"))?;
                    (repository, Some(path))
                }
                (None, None) => unreachable!("validated fromGit names a repository"),
            };
            if let Some(scope) = sources
                .publication_scope
                .filter(|scope| scope.contains(&repository.repo_url, &source.revision, &source.path))
            {
                return Err(self_lock_error(&format!("{field}.fromGit ({})", source.path), &scope.target));
            }
            let bytes = sources
                .git
                .read_blob(&repository.repo_url, &source.commit, &source.path)
                .map_err(|error| match error {
                    NylError::Git(_) => format!(
                        "{field}.fromGit cannot read {} at locked commit {} of {}: {}. Rendering reads only the locked commit and fetches it by ID; offline, it must already be in the local Git cache",
                        source.path,
                        source.commit,
                        crate::util::sanitize_url(&repository.repo_url),
                        crate::util::redact_url_credentials(&error.to_string(), &repository.repo_url)
                    ),
                    // Vendor policy errors carry their own fix.
                    NylError::Config(message) => format!(
                        "{field}.fromGit: {}",
                        crate::util::redact_url_credentials(&message, &repository.repo_url)
                    ),
                    other => format!(
                        "{field}.fromGit: {}",
                        crate::util::redact_url_credentials(&other.to_string(), &repository.repo_url)
                    ),
                })?
                .ok_or_else(|| {
                    format!(
                        "{field}.fromGit: commit {} of {} has no file {}",
                        source.commit,
                        crate::util::sanitize_url(&repository.repo_url),
                        source.path
                    )
                })?;
            let document = parse_single_document(&bytes).map_err(|reason| format!("{field}.fromGit: {} {reason}", source.path))?;
            let value = select(&document, &source.pointer).map_err(|reason| format!("{field}.fromGit: {} {reason}", source.path))?;
            Ok(ResolvedInput {
                value,
                origin: InputOrigin::Git {
                    url: credential_free_url(&repository.repo_url),
                    commit: source.commit.clone(),
                    path: source.path.clone(),
                    blob_digest: nyl_core::digest::sha256_hex(&bytes),
                    repository_source,
                },
            })
        }
        BindingKind::FromPublication => unreachable!("resolved by resolve_publication"),
        BindingKind::FromUnit | BindingKind::FromPromotion => Err(format!(
            "{field}.{} needs orchestrated execution, which resolves it into a pinned input snapshot; render-tree, publish-tree, diff-tree, and direct commands never resolve it",
            kind.field()
        )),
    }
}

/// Working-tree files the target's `fromPublication` bindings carry.
///
/// They are excluded from the source dirty check, because a carried file is
/// an output of this run, and the clean-`HEAD` verification render receives
/// the same bytes.
pub fn carry_paths(inventory: &super::GitOpsInventory, target_name: &str) -> Result<Vec<PathBuf>> {
    let Some(super::DiscoveredGitOpsResource {
        resource: Some(crate::resources::GitOpsResource::DeploymentTarget(target)),
        ..
    }) = inventory.get(crate::resources::GitOpsResourceKind::DeploymentTarget, target_name)
    else {
        return Ok(Vec::new());
    };
    let paths = inventory.paths();
    let mut carried = Vec::new();
    for (key, bindings) in &target.spec.release_inputs {
        for (name, binding) in bindings {
            if let Some(carry) = binding
                .from_publication
                .as_ref()
                .and_then(|source| source.carry_file_from_worktree.as_ref())
            {
                carried.push(paths.resolve(
                    &format!("spec.releaseInputs.{key:?}.{name}.fromPublication.carryFileFromWorktree"),
                    carry,
                )?);
            }
        }
    }
    Ok(carried)
}

/// Check the placement of `fromPublication` state files and return the ones
/// this target writes: carried bytes, or the base copy written back.
///
/// Contract: [`fromPublication`](../../../../design/release-inputs.md#binding-kinds),
/// Placement. Every state path lies outside the directories generated Argo CD
/// Applications sync (workload Release directories and `_nyl`, which holds the
/// catalog), so Argo CD never applies a state file as a manifest. A committed
/// state file must not be owned by this target; a carried one is.
///
/// State files are owned and published like rendered files but are not
/// Kubernetes manifests, so they stay out of `files` and never reach manifest
/// provenance or validation.
pub fn place_state_files(
    resolved: &ResolvedTargetInputs,
    release_directories: &[PathBuf],
    files: &BTreeMap<PathBuf, Vec<u8>>,
) -> Result<BTreeMap<PathBuf, Vec<u8>>> {
    let carried = resolved.carried_files();
    for path in resolved.state_paths.keys() {
        let relative = Path::new(path);
        if relative.starts_with("_nyl") {
            return Err(NylError::config(format!(
                "fromPublication state file {path} lies inside _nyl, which the catalog Application syncs; choose a path outside _nyl"
            )));
        }
        if let Some(directory) = release_directories
            .iter()
            .find(|directory| relative.starts_with(directory))
        {
            return Err(NylError::config(format!(
                "fromPublication state file {path} lies inside workload Release directory {}, which its Argo CD Application syncs; choose a path outside every Release directory",
                directory.display()
            )));
        }
    }
    for path in resolved.committed_state_paths() {
        if files.contains_key(Path::new(&path)) {
            return Err(NylError::config(format!(
                "fromPublication state file {path} is a file this target renders; a committed state file must not be owned by the target"
            )));
        }
    }
    let mut state = BTreeMap::new();
    for (path, bytes) in carried {
        let path = PathBuf::from(&path);
        if files.contains_key(&path) {
            return Err(NylError::config(format!(
                "Carried fromPublication state file {} collides with a rendered file",
                path.display()
            )));
        }
        state.insert(path, bytes);
    }
    Ok(state)
}

/// After rendering, a Release's `spec.inputs` must equal its static
/// declaration: templating cannot add, remove, or change declarations.
pub fn verify_rendered_declarations(
    path: &Path,
    declared: &BTreeMap<String, InputDeclaration>,
    release: &crate::resources::Release,
) -> Result<()> {
    if &release.spec.inputs == declared {
        Ok(())
    } else {
        Err(NylError::config(format!(
            "Release {:?} in {} renders spec.inputs that differ from its literal declaration; declare spec.inputs literally, without templating",
            release.metadata.name,
            path.display()
        )))
    }
}

/// Parse a YAML or JSON file holding exactly one document.
pub fn read_single_document(path: &Path) -> std::result::Result<Value, String> {
    let bytes = std::fs::read(path).map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    parse_single_document(&bytes).map_err(|reason| format!("{} {reason}", path.display()))
}

/// Parse YAML or JSON bytes holding exactly one document.
pub fn parse_single_document(bytes: &[u8]) -> std::result::Result<Value, String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "is not valid UTF-8".to_owned())?;
    let mut documents = crate::yaml::parse_yaml_documents_k8s_compatible(text)
        .map_err(|error| format!("is not valid YAML or JSON: {error}"))?;
    match documents.len() {
        1 => Ok(documents.remove(0)),
        0 => Err("contains no document".to_owned()),
        count => Err(format!(
            "contains {count} YAML documents; an input file must hold exactly one"
        )),
    }
}

/// Select `pointer` inside `document`.
pub fn select(document: &Value, pointer: &str) -> std::result::Result<Value, String> {
    nyl_core::json_pointer::resolve(document, pointer)
        .cloned()
        .ok_or_else(|| format!("has no value at JSON Pointer {pointer:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn target(release_inputs: Value) -> DeploymentTarget {
        serde_json::from_value(json!({
            "apiVersion": "k8s.gitops.nyl/v1",
            "kind": "DeploymentTarget",
            "metadata": {"name": "dev"},
            "spec": {
                "publication": {"repository": {"repoURL": "https://example.invalid/deploy.git"}, "revision": "deploy", "pathPrefix": "dev"},
                "releaseInputs": release_inputs,
            }
        }))
        .unwrap()
    }

    fn declarations(value: Value) -> BTreeMap<String, InputDeclaration> {
        serde_json::from_value(value).unwrap()
    }

    fn resolve(
        target: &DeploymentTarget,
        declared: &[(&str, &BTreeMap<String, InputDeclaration>)],
        disabled: &[&str],
        root: &Path,
    ) -> Result<ResolvedTargetInputs> {
        let paths = ProjectPaths::new(root.to_path_buf(), root.to_path_buf());
        let visible_files = std::fs::read_dir(root)
            .unwrap()
            .map(|entry| PathBuf::from(entry.unwrap().file_name()))
            .filter(|name| !name.to_string_lossy().starts_with("ignored"))
            .collect::<BTreeSet<_>>();
        let releases = declared
            .iter()
            .map(|(key, declarations)| ReleaseDeclaration {
                key: ReleaseKey::parse(key).unwrap(),
                declarations,
                source: Path::new("release.yaml"),
            })
            .collect::<Vec<_>>();
        resolve_target_inputs(
            target,
            &releases,
            &disabled.iter().map(|name| (*name).to_owned()).collect(),
            &InputSources {
                paths: &paths,
                repositories: &|reference| {
                    Err(NylError::config(format!(
                        "GitRepository {:?} was not found",
                        reference.name
                    )))
                },
                publication_scope: None,
                git: &NoGit,
                publication: None,
                pinned_state: None,
                visible_files: &visible_files,
            },
        )
    }

    struct NoGit;

    impl GitBlobSource for NoGit {
        fn read_blob(&self, _: &str, _: &str, _: &str) -> Result<Option<Vec<u8>>> {
            Err(NylError::config("no Git in this test"))
        }

        fn read_publication_blob(&self, _: &str, _: &str, _: &str) -> Result<Option<Vec<u8>>> {
            Err(NylError::config("no Git in this test"))
        }

        fn branch_head(&self, _: &str, _: &str, _: bool) -> Result<Option<String>> {
            Err(NylError::config("no Git in this test"))
        }
    }

    #[test]
    fn test_bindings_replace_defaults_and_files_resolve_pointers() {
        let temp = TempDir::new().unwrap();
        std::fs::write(
            temp.path().join("db.yaml"),
            "database:\n  host: db.internal\n  port: 5432\n",
        )
        .unwrap();
        let declared = declarations(json!({
            "image": {"type": "string"},
            "replicas": {"type": "integer", "default": 2},
            "database": {"type": "object"},
            "host": {"type": "string"},
        }));
        let target = target(json!({"platform/web": {
            "image": {"value": "registry.example/web@sha256:1"},
            "database": {"fromFile": {"path": "db.yaml", "pointer": "/database"}},
            "host": {"fromFile": {"path": "db.yaml", "pointer": "/database/host"}},
        }}));
        let resolved = resolve(&target, &[("platform/web", &declared)], &[], temp.path()).unwrap();
        let web = &resolved.releases[&ReleaseKey::parse("platform/web").unwrap()];
        assert_eq!(
            Value::Object(web.values()),
            json!({"image": "registry.example/web@sha256:1", "replicas": 2, "database": {"host": "db.internal", "port": 5432}, "host": "db.internal"})
        );
        assert_eq!(web.inputs["replicas"].origin, InputOrigin::Default);
        assert_eq!(resolved.files(), BTreeSet::from([temp.path().join("db.yaml")]));
        let entries = resolved.index_entries().unwrap();
        assert_eq!(
            entries["@input/platform/web/replicas"],
            nyl_core::digest::sha256_hex(b"2\n")
        );
        assert_eq!(entries.len(), 4);
    }

    #[test]
    fn test_disabled_group_keys_are_ignored() {
        let temp = TempDir::new().unwrap();
        let target = target(json!({"optional/web": {"image": {"value": "x"}}}));
        let resolved = resolve(&target, &[], &["optional"], temp.path()).unwrap();
        assert!(resolved.releases.is_empty());
    }

    #[test]
    fn test_problems_are_reported_together() {
        let temp = TempDir::new().unwrap();
        std::fs::write(temp.path().join("two.yaml"), "a: 1\n---\na: 2\n").unwrap();
        let web = declarations(json!({
            "image": {"type": "string", "description": "Immutable image reference"},
            "replicas": {"type": "integer"},
            "tier": {"type": "string"},
            "seed": {"type": "string"},
        }));
        let plain = BTreeMap::new();
        let target = target(json!({
            "platform/missing": {"image": {"value": "x"}},
            "platform/plain": {"image": {"value": "x"}},
            "platform/web": {
                "unknown": {"value": 1},
                "replicas": {"value": 2.5},
                "tier": {"fromFile": {"path": "two.yaml"}},
                "seed": {"fromUnit": {"unit": "seed", "output": "version"}},
            },
        }));
        let error = resolve(
            &target,
            &[("platform/web", &web), ("platform/plain", &plain)],
            &[],
            temp.path(),
        )
        .unwrap_err()
        .to_string();
        for expected in [
            "DeploymentTarget \"dev\" has invalid Release inputs",
            "\"platform/missing\" names no Release this target renders",
            "binds Release platform/plain, which declares no spec.inputs",
            "binds an input Release platform/web does not declare",
            "requires input \"image\" (Immutable image reference)",
            "expected integer, got number",
            "contains 2 YAML documents",
            "fromUnit needs orchestrated execution",
        ] {
            assert!(error.contains(expected), "missing {expected:?} in:\n{error}");
        }
    }

    struct FakeGit(BTreeMap<(String, String, String), Vec<u8>>);

    impl GitBlobSource for FakeGit {
        fn read_blob(&self, url: &str, commit: &str, path: &str) -> Result<Option<Vec<u8>>> {
            Ok(self
                .0
                .get(&(url.to_owned(), commit.to_owned(), path.to_owned()))
                .cloned())
        }

        fn read_publication_blob(&self, url: &str, commit: &str, path: &str) -> Result<Option<Vec<u8>>> {
            self.read_blob(url, commit, path)
        }

        fn branch_head(&self, _: &str, _: &str, _: bool) -> Result<Option<String>> {
            Ok(None)
        }
    }

    #[test]
    fn test_from_git_reads_the_locked_commit_and_records_the_blob() {
        let temp = TempDir::new().unwrap();
        let commit = "3f1c9a0000000000000000000000000000000000";
        let bytes = b"web:\n  tier: large\n".to_vec();
        let git = FakeGit(BTreeMap::from([(
            (
                "https://git.example.com/state.git".to_owned(),
                commit.to_owned(),
                "staging/sizing.yaml".to_owned(),
            ),
            bytes.clone(),
        )]));
        let repositories = BTreeMap::from([(
            "state".to_owned(),
            (
                InlineGitRepository {
                    repo_url: "https://git.example.com/state.git".to_owned(),
                    publish_url: None,
                },
                PathBuf::from("config/state.yaml"),
            ),
        )]);
        let declared = declarations(json!({"tier": {"type": "string"}, "missing": {"type": "string", "default": "x"}}));
        let target = target(json!({"platform/web": {
            "tier": {"fromGit": {"repositoryRef": {"name": "state"}, "revision": "main", "commit": commit, "path": "staging/sizing.yaml", "pointer": "/web/tier"}},
            "missing": {"fromGit": {"repository": {"repoURL": "https://git.example.com/state.git"}, "revision": "main", "commit": commit, "path": "absent.yaml"}},
        }}));
        let paths = ProjectPaths::new(temp.path().to_path_buf(), temp.path().to_path_buf());
        let releases = [ReleaseDeclaration {
            key: ReleaseKey::parse("platform/web").unwrap(),
            declarations: &declared,
            source: Path::new("release.yaml"),
        }];
        let sources = InputSources {
            paths: &paths,
            visible_files: &BTreeSet::new(),
            repositories: &|reference| {
                repositories
                    .get(&reference.name)
                    .cloned()
                    .ok_or_else(|| NylError::config(format!("GitRepository {:?} was not found", reference.name)))
            },
            publication_scope: None,
            git: &git,
            publication: None,
            pinned_state: None,
        };
        let error = resolve_target_inputs(&target, &releases, &BTreeSet::new(), &sources)
            .unwrap_err()
            .to_string();
        assert!(error.contains("has no file absent.yaml"), "{error}");

        let target = super::tests::target(json!({"platform/web": {
            "tier": {"fromGit": {"repositoryRef": {"name": "state"}, "revision": "main", "commit": commit, "path": "staging/sizing.yaml", "pointer": "/web/tier"}},
        }}));
        let resolved = resolve_target_inputs(&target, &releases, &BTreeSet::new(), &sources).unwrap();
        let web = &resolved.releases[&ReleaseKey::parse("platform/web").unwrap()];
        assert_eq!(web.inputs["tier"].value, json!("large"));
        assert_eq!(resolved.files(), BTreeSet::from([PathBuf::from("config/state.yaml")]));
        let entries = resolved.index_entries().unwrap();
        assert_eq!(
            entries[&format!("@git/https://git.example.com/state.git@{commit}/staging/sizing.yaml")],
            nyl_core::digest::sha256_hex(&bytes)
        );
    }

    #[test]
    fn test_credential_free_url_drops_userinfo() {
        assert_eq!(
            credential_free_url("https://user:token@git.example.com/state.git"),
            "https://git.example.com/state.git"
        );
        assert_eq!(
            credential_free_url("https://token@Git.Example.com/state.git"),
            credential_free_url("https://Git.Example.com/state.git")
        );
        assert_eq!(
            credential_free_url("git@github.com:org/repo.git"),
            "git@github.com:org/repo.git"
        );
    }

    #[test]
    fn test_from_file_reads_only_git_visible_files() {
        let temp = TempDir::new().unwrap();
        std::fs::write(temp.path().join("ignored.yaml"), "port: 1\n").unwrap();
        let declared = declarations(json!({"port": {"type": "integer"}}));
        for path in ["ignored.yaml", "missing.yaml"] {
            let target = target(json!({"platform/web": {"port": {"fromFile": {"path": path, "pointer": "/port"}}}}));
            let error = resolve(&target, &[("platform/web", &declared)], &[], temp.path())
                .unwrap_err()
                .to_string();
            assert!(error.contains("names no Git-visible YAML or JSON file"), "{error}");
        }
    }

    #[test]
    fn test_missing_pointer_is_an_error() {
        let temp = TempDir::new().unwrap();
        std::fs::write(temp.path().join("db.json"), "{\"host\": \"db\"}").unwrap();
        let declared = declarations(json!({"port": {"type": "integer"}}));
        let target = target(json!({"platform/web": {"port": {"fromFile": {"path": "db.json", "pointer": "/port"}}}}));
        let error = resolve(&target, &[("platform/web", &declared)], &[], temp.path())
            .unwrap_err()
            .to_string();
        assert!(error.contains("has no value at JSON Pointer \"/port\""), "{error}");
    }

    #[test]
    fn test_state_placement_is_checked_before_the_state_file_exists() {
        let unbound = ResolvedTargetInputs {
            state_paths: BTreeMap::from([("_nyl/state.json".to_owned(), false)]),
            ..ResolvedTargetInputs::default()
        };
        let error = place_state_files(&unbound, &[], &BTreeMap::new())
            .unwrap_err()
            .to_string();
        assert!(error.contains("lies inside _nyl"), "{error}");

        let unbound = ResolvedTargetInputs {
            state_paths: BTreeMap::from([("workloads/api/state.json".to_owned(), true)]),
            ..ResolvedTargetInputs::default()
        };
        let error = place_state_files(&unbound, &[PathBuf::from("workloads/api")], &BTreeMap::new())
            .unwrap_err()
            .to_string();
        assert!(error.contains("inside workload Release directory"), "{error}");
    }

    #[test]
    fn test_publication_scope_matches_prefix_branch_and_credential_free_url() {
        let scope = PublicationScope {
            target: "dev".to_owned(),
            urls: vec![crate::git::normalize_git_url_for_equality(
                "https://git.example.com/deploy.git",
            )],
            branch: "deploy".to_owned(),
            prefix: "dev".to_owned(),
        };
        assert!(scope.contains(
            "https://ci-token@git.example.com/deploy",
            "refs/heads/deploy",
            "dev/state.json"
        ));
        assert!(!scope.contains("https://git.example.com/deploy.git", "deploy", "develop/state.json"));
        assert!(!scope.contains("https://git.example.com/deploy.git", "main", "dev/state.json"));
        let root = PublicationScope {
            prefix: String::new(),
            ..scope
        };
        assert!(root.contains("https://git.example.com/deploy.git", "deploy", "state.json"));
    }

    #[test]
    fn test_state_paths_of_disabled_groups_are_released_not_deleted() {
        let temp = TempDir::new().unwrap();
        let declared = declarations(json!({"image": {"type": "string", "default": "x"}}));
        let target = target(
            json!({"platform/web": {"image": {"fromPublication": {"path": "state/images.json", "carryFileFromWorktree": "build/images.json"}}}}),
        );
        let resolved = resolve(&target, &[("platform/web", &declared)], &["platform"], temp.path()).unwrap();
        assert_eq!(
            resolved.state_paths,
            BTreeMap::from([("state/images.json".to_owned(), false)])
        );
        assert!(resolved.carried_files().is_empty());
    }

    #[test]
    fn test_missing_required_publication_state_names_the_state_file() {
        let temp = TempDir::new().unwrap();
        let declared = declarations(json!({"image": {"type": "string"}}));
        let target = target(json!({"platform/web": {"image": {"fromPublication": {"path": "state/images.json"}}}}));
        let base = PublicationBase {
            url: "https://example.invalid/deploy.git".to_owned(),
            branch: "deploy".to_owned(),
            prefix: "dev".to_owned(),
            commit: None,
            origin: PublicationBaseOrigin::Refreshed,
        };
        let paths = ProjectPaths::new(temp.path().to_path_buf(), temp.path().to_path_buf());
        let releases = [ReleaseDeclaration {
            key: ReleaseKey::parse("platform/web").unwrap(),
            declarations: &declared,
            source: Path::new("release.yaml"),
        }];
        let error = resolve_target_inputs(
            &target,
            &releases,
            &BTreeSet::new(),
            &InputSources {
                paths: &paths,
                repositories: &|reference| {
                    Err(NylError::config(format!(
                        "GitRepository {:?} was not found",
                        reference.name
                    )))
                },
                publication_scope: None,
                git: &NoGit,
                publication: Some(&base),
                pinned_state: None,
                visible_files: &BTreeSet::new(),
            },
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("state file state/images.json does not exist on the publication branch yet"),
            "{error}"
        );
    }

    /// Answers `branch_head` from fixed refresh and cache outcomes.
    struct Heads {
        refreshed: std::result::Result<Option<String>, &'static str>,
        cached: std::result::Result<Option<String>, &'static str>,
    }

    impl GitBlobSource for Heads {
        fn read_blob(&self, _: &str, _: &str, _: &str) -> Result<Option<Vec<u8>>> {
            Ok(None)
        }

        fn read_publication_blob(&self, _: &str, _: &str, _: &str) -> Result<Option<Vec<u8>>> {
            Ok(None)
        }

        fn branch_head(&self, _: &str, _: &str, refresh: bool) -> Result<Option<String>> {
            let outcome = if refresh { &self.refreshed } else { &self.cached };
            outcome.clone().map_err(NylError::config)
        }
    }

    #[test]
    fn test_fresh_or_cached_falls_back_to_the_cache_then_to_unavailable() {
        let resolve = |heads: Heads| {
            let base = PublicationBase::resolve(&heads, "u", "deploy", "dev", PublicationRead::FreshOrCached).unwrap();
            (base.commit, base.origin)
        };
        let head = Some("a".repeat(40));
        assert_eq!(
            resolve(Heads {
                refreshed: Ok(head.clone()),
                cached: Err("unused")
            }),
            (head.clone(), PublicationBaseOrigin::Refreshed)
        );
        assert_eq!(
            resolve(Heads {
                refreshed: Err("offline"),
                cached: Ok(head.clone())
            }),
            (head.clone(), PublicationBaseOrigin::CachedAfterFailedRefresh)
        );
        // An empty or missing cache cannot tell a new branch from an unfetched one.
        for cached in [Ok(None), Err("not cached")] {
            assert_eq!(
                resolve(Heads {
                    refreshed: Err("offline"),
                    cached
                }),
                (None, PublicationBaseOrigin::Unavailable)
            );
        }
        let error = PublicationBase::resolve(
            &Heads {
                refreshed: Err("offline"),
                cached: Ok(head),
            },
            "u",
            "deploy",
            "dev",
            PublicationRead::Fresh,
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("can read the cached branch head with --offline"),
            "{error}"
        );
    }

    #[test]
    fn test_cached_read_of_an_empty_cache_is_unavailable() {
        let heads = Heads {
            refreshed: Err("unused"),
            cached: Ok(None),
        };
        let base = PublicationBase::resolve(&heads, "u", "deploy", "dev", PublicationRead::Cached).unwrap();
        assert_eq!((base.commit, base.origin), (None, PublicationBaseOrigin::Unavailable));
    }

    #[test]
    fn test_pinned_state_replaces_the_worktree_carry_file() {
        let temp = TempDir::new().unwrap();
        std::fs::write(temp.path().join("carried.json"), r#"{"image": "worktree"}"#).unwrap();
        let declared = declarations(json!({"image": {"type": "string"}}));
        let target = target(json!({"platform/web": {"image": {"fromPublication": {
            "path": "state.json", "pointer": "/image", "carryFileFromWorktree": "carried.json"
        }}}}));
        let paths = ProjectPaths::new(temp.path().to_path_buf(), temp.path().to_path_buf());
        let releases = [ReleaseDeclaration {
            key: ReleaseKey::parse("platform/web").unwrap(),
            declarations: &declared,
            source: Path::new("release.yaml"),
        }];
        let pinned = BTreeMap::from([(PathBuf::from("state.json"), br#"{"image": "pinned"}"#.to_vec())]);
        let resolved = resolve_target_inputs(
            &target,
            &releases,
            &BTreeSet::new(),
            &InputSources {
                paths: &paths,
                repositories: &|reference| {
                    Err(NylError::config(format!(
                        "GitRepository {:?} was not found",
                        reference.name
                    )))
                },
                publication_scope: None,
                git: &NoGit,
                publication: None,
                pinned_state: Some(&pinned),
                visible_files: &BTreeSet::new(),
            },
        )
        .unwrap();
        let web = &resolved.releases[&ReleaseKey::parse("platform/web").unwrap()];
        assert_eq!(web.inputs["image"].value, json!("pinned"));
    }
}
