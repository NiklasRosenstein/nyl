//! The Git implementation of [`SourceResolver`]: revisions, reachability
//! from protected refs, and pinned worktrees of source commits.
//!
//! Contract: [Source](../../../design/orchestration-core.md#source) and
//! [Pinned commits](../../../design/orchestration-core.md#pinned-commits).
//! Every execution gets its own detached checkout of the commit it runs, so
//! no unit reads another commit's files or the entry worktree's uncommitted
//! changes.
//!
//! - [`SourceRepository::This`] is the entry clone at `git_dir`: a branch is
//!   a local branch or its `origin/<branch>` remote-tracking ref, and
//!   reachability checks both.
//! - A repository named by URL is fetched through `nyl-git` (the user's own
//!   `git`, as for state repositories) into a private namespace of the entry
//!   clone, `refs/nyl-sources/<url digest>/{heads,tags}/…`, once per
//!   resolver, and resolved there.
//! - [`SourceResolver::worktree`] creates an independent repository at the
//!   caller's path that borrows the entry clone's objects through Git's
//!   alternates mechanism, the form of `git clone --shared --no-checkout`
//!   followed by a detached checkout. Nothing is registered in the entry
//!   clone, so removing the directory ([`GitSourceResolver::remove_worktree`])
//!   is the whole cleanup.
//! - [`SourceResolver::reset_worktree`] reuses such a checkout for another
//!   commit, as a run's concurrency slots do: it moves the detached HEAD,
//!   rewrites every tracked file, removes untracked and ignored files and
//!   empty directories, and starts over from a fresh checkout when anything
//!   is left, so each unit still sees exactly a fresh checkout. Contract:
//!   [Execution](../../../design/orchestration-core.md#execution).
//! - Symbolic links are checked once per tree and resolver, whichever of
//!   the two creates the checkout; a rejected tree stays rejected.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

use git2::{ObjectType, Oid, Repository, TreeWalkMode, TreeWalkResult};
use nyl_core::orchestration::ids::CommitId;
use nyl_core::orchestration::ports::{SourceError, SourceRepository, SourceResolver, SourceResult};
use nyl_git::GitAuth;

/// The ref prefix under which repositories named by URL are fetched.
const SOURCES_NAMESPACE: &str = "refs/nyl-sources";

/// Git's mode for a symbolic link entry.
const SYMLINK_MODE: i32 = 0o120_000;

/// Sources in the clone at `git_dir` and, by URL, in other repositories,
/// fetched into it.
pub struct GitSourceResolver {
    git_dir: PathBuf,
    auth: GitAuth,
    /// Read URLs of repositories named by credential-free URL, as for
    /// [`crate::git::Endpoint`].
    endpoints: BTreeMap<String, String>,
    /// Repositories already fetched by this resolver, by identity.
    fetched: Mutex<BTreeSet<String>>,
    /// The symbolic-link check of each tree this resolver checked out, so
    /// a slot reused for many units walks each tree once.
    symlinks: Mutex<BTreeMap<Oid, Result<(), String>>>,
}

fn error(message: impl Into<String>) -> SourceError {
    SourceError(message.into())
}

fn git_error(context: &str, cause: &git2::Error) -> SourceError {
    error(format!("{context}: {}", cause.message()))
}

/// Where a repository's branches and tags live in the entry clone.
struct RefSpace {
    /// Prefixes a branch name is appended to, in lookup order.
    branches: Vec<String>,
    /// Prefix a tag name is appended to.
    tags: String,
    /// Prefix that a full ref name outside heads and tags maps under, for a
    /// fetched repository; empty for the entry clone.
    other: Option<String>,
}

impl RefSpace {
    /// The candidate refs a revision name may denote, in lookup order.
    fn candidates(&self, revision: &str) -> Vec<String> {
        if let Some(rest) = revision.strip_prefix("refs/") {
            return vec![self.map_full(revision, rest)];
        }
        let mut candidates = self
            .branches
            .iter()
            .map(|prefix| format!("{prefix}{revision}"))
            .collect::<Vec<_>>();
        candidates.push(format!("{}{revision}", self.tags));
        candidates
    }

    /// The refs a protected ref name or glob covers: branches only.
    fn branch_patterns(&self, name: &str) -> Vec<String> {
        if let Some(rest) = name.strip_prefix("refs/") {
            return vec![self.map_full(name, rest)];
        }
        self.branches.iter().map(|prefix| format!("{prefix}{name}")).collect()
    }

    fn map_full(&self, full: &str, rest: &str) -> String {
        let Some(other) = &self.other else {
            return full.to_owned();
        };
        if let Some(branch) = rest.strip_prefix("heads/") {
            format!("{}{branch}", self.branches[0])
        } else if let Some(tag) = rest.strip_prefix("tags/") {
            format!("{}{tag}", self.tags)
        } else {
            format!("{other}{rest}")
        }
    }
}

impl GitSourceResolver {
    /// Resolve sources through the repository at `git_dir`, the entry clone,
    /// which [`SourceRepository::This`] names.
    pub fn new(git_dir: impl Into<PathBuf>, auth: GitAuth) -> Self {
        Self {
            git_dir: git_dir.into(),
            auth,
            endpoints: BTreeMap::new(),
            fetched: Mutex::new(BTreeSet::new()),
            symlinks: Mutex::new(BTreeMap::new()),
        }
    }

    /// Read the repository that sources name by `identity` from `url`.
    #[must_use]
    pub fn with_endpoint(mut self, identity: impl Into<String>, url: impl Into<String>) -> Self {
        self.endpoints.insert(identity.into(), url.into());
        self
    }

    /// Remove a directory [`SourceResolver::worktree`] created. The checkout
    /// is a standalone repository borrowing the entry clone's objects, so
    /// nothing in the entry clone needs pruning; removing the directory is
    /// the whole cleanup. A missing directory is not an error.
    pub fn remove_worktree(path: &Path) -> SourceResult<()> {
        match std::fs::remove_dir_all(path) {
            Ok(()) => Ok(()),
            Err(cause) if cause.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(cause) => Err(error(format!("cannot remove worktree {}: {cause}", path.display()))),
        }
    }

    fn open(&self) -> SourceResult<Repository> {
        Repository::open(&self.git_dir)
            .map_err(|cause| git_error(&format!("cannot open repository {}", self.git_dir.display()), &cause))
    }

    fn read_url<'a>(&'a self, identity: &'a str) -> &'a str {
        self.endpoints.get(identity).map_or(identity, String::as_str)
    }

    fn describe(repository: &SourceRepository) -> String {
        match repository {
            SourceRepository::This => "this repository".to_owned(),
            SourceRepository::Url(url) => nyl_git::sanitize_url(url),
        }
    }

    fn space(repository: &SourceRepository) -> RefSpace {
        match repository {
            SourceRepository::This => RefSpace {
                branches: vec!["refs/heads/".to_owned(), "refs/remotes/origin/".to_owned()],
                tags: "refs/tags/".to_owned(),
                other: None,
            },
            SourceRepository::Url(url) => {
                let base = Self::namespace(url);
                RefSpace {
                    branches: vec![format!("{base}/heads/")],
                    tags: format!("{base}/tags/"),
                    other: Some(format!("{base}/refs/")),
                }
            }
        }
    }

    fn namespace(identity: &str) -> String {
        let digest = nyl_core::digest::sha256_hex(identity.as_bytes());
        format!("{SOURCES_NAMESPACE}/{}", &digest[..16])
    }

    /// Fetch a URL repository's branches and tags into its namespace, once.
    fn ensure_fetched(&self, repository: &Repository, source: &SourceRepository) -> SourceResult<()> {
        let SourceRepository::Url(identity) = source else {
            return Ok(());
        };
        let mut fetched = self.fetched.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if fetched.contains(identity) {
            return Ok(());
        }
        let base = Self::namespace(identity);
        let heads = format!("+refs/heads/*:{base}/heads/*");
        let tags = format!("+refs/tags/*:{base}/tags/*");
        nyl_git::fetch(
            repository.path(),
            self.read_url(identity),
            &[&heads, &tags],
            true,
            &self.auth,
        )
        .map_err(|cause| error(cause.to_string()))?;
        fetched.insert(identity.clone());
        Ok(())
    }

    /// Fetch one ref or commit of a URL repository on demand, such as
    /// `refs/pull/1/merge` or a commit no branch carries; failures leave the
    /// lookup to report the revision as missing.
    fn fetch_one(&self, repository: &Repository, source: &SourceRepository, what: &str, destination: Option<&str>) {
        let SourceRepository::Url(identity) = source else {
            return;
        };
        let refspec = match destination {
            Some(destination) => format!("+{what}:{destination}"),
            None => what.to_owned(),
        };
        let _ = nyl_git::fetch(
            repository.path(),
            self.read_url(identity),
            &[&refspec],
            false,
            &self.auth,
        );
    }

    /// The entry clone and `commit`, fetched when missing, after checking
    /// its tree's symbolic links (once per tree and resolver).
    fn checked_commit(&self, repository: &SourceRepository, commit: &CommitId) -> SourceResult<(Repository, Oid)> {
        let source = self.open()?;
        let oid = Oid::from_str(commit.as_str()).map_err(|cause| git_error("invalid commit", &cause))?;
        if !Self::commit_exists(&source, oid) {
            self.ensure_fetched(&source, repository)?;
            self.fetch_one(&source, repository, commit.as_str(), None);
        }
        let checked = {
            let found = source.find_commit(oid).map_err(|_| {
                error(format!(
                    "commit {commit} does not exist in {}",
                    Self::describe(repository)
                ))
            })?;
            let tree = found
                .tree()
                .map_err(|cause| git_error(&format!("cannot read the tree of {commit}"), &cause))?;
            let mut cache = self.symlinks.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            cache
                .entry(tree.id())
                .or_insert_with(|| check_symlinks(&source, &tree).map_err(|cause| cause.0))
                .clone()
        };
        checked.map_err(SourceError)?;
        Ok((source, oid))
    }

    /// The canonical objects directory of the entry clone, which every
    /// checkout this resolver makes borrows through its alternates file.
    fn objects_dir(source: &Repository) -> SourceResult<PathBuf> {
        let objects = source.commondir().join("objects");
        objects
            .canonicalize()
            .map_err(|cause| error(format!("cannot resolve {}: {cause}", objects.display())))
    }

    /// Create a detached checkout of `oid` at the missing or empty `path`.
    fn create_checkout(source: &Repository, oid: Oid, path: &Path) -> SourceResult<()> {
        let objects = Self::objects_dir(source)?;
        let created = Repository::init(path)
            .map_err(|cause| git_error(&format!("cannot create worktree {}", path.display()), &cause))?;
        let alternates = created.path().join("objects/info/alternates");
        drop(created);
        std::fs::create_dir_all(alternates.parent().expect("alternates has a parent"))
            .and_then(|()| std::fs::write(&alternates, format!("{}\n", objects.display())))
            .map_err(|cause| error(format!("cannot write {}: {cause}", alternates.display())))?;
        let checkout = Repository::open(path)
            .map_err(|cause| git_error(&format!("cannot open worktree {}", path.display()), &cause))?;
        checkout
            .set_head_detached(oid)
            .map_err(|cause| git_error(&format!("cannot detach the worktree at {oid}"), &cause))?;
        checkout
            .checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .map_err(|cause| git_error(&format!("cannot check out {oid}"), &cause))?;
        Ok(())
    }

    /// Whether `path` holds a checkout this resolver made: its
    /// `.git/objects/info/alternates` names the entry clone's objects.
    fn made_here(source: &Repository, path: &Path) -> SourceResult<bool> {
        let alternates = path.join(".git/objects/info/alternates");
        let Ok(content) = std::fs::read_to_string(&alternates) else {
            return Ok(false);
        };
        let objects = Self::objects_dir(source)?;
        Ok(content.trim_end_matches(['\n', '\r']) == objects.display().to_string())
    }

    /// Reset a checkout this resolver made to `oid`. Returns whether the
    /// working tree is exactly a fresh checkout afterwards; `false` asks
    /// the caller to start over.
    fn reset_checkout(path: &Path, oid: Oid) -> Result<bool, Box<dyn std::error::Error>> {
        // No symbolic link a unit left may redirect the checkout's writes,
        // and without the index every tracked file is rewritten, so a file
        // changed within the index's timestamp granularity cannot pass as
        // unchanged.
        remove_symlinks(path)?;
        match std::fs::remove_file(path.join(".git/index")) {
            Ok(()) => {}
            Err(cause) if cause.kind() == std::io::ErrorKind::NotFound => {}
            Err(cause) => return Err(cause.into()),
        }
        let checkout = Repository::open(path)?;
        if checkout.workdir().map(Path::canonicalize).transpose()? != Some(path.canonicalize()?) {
            return Ok(false);
        }
        checkout.set_head_detached(oid)?;
        checkout.checkout_head(Some(
            git2::build::CheckoutBuilder::new()
                .force()
                .remove_untracked(true)
                .remove_ignored(true),
        ))?;
        remove_empty_directories(path, true)?;
        let mut options = git2::StatusOptions::new();
        options
            .include_untracked(true)
            .include_ignored(true)
            .recurse_untracked_dirs(true)
            .recurse_ignored_dirs(true)
            .include_unmodified(false);
        let clean = checkout.statuses(Some(&mut options))?.is_empty();
        Ok(clean)
    }

    fn commit_exists(repository: &Repository, oid: Oid) -> bool {
        repository.find_commit(oid).is_ok()
    }

    /// The commit a ref names, peeled through tags; `None` when it is absent.
    fn ref_commit(repository: &Repository, name: &str) -> SourceResult<Option<Oid>> {
        match repository.find_reference(name) {
            Ok(reference) => {
                let commit = reference
                    .peel_to_commit()
                    .map_err(|cause| git_error(&format!("{name} does not name a commit"), &cause))?;
                Ok(Some(commit.id()))
            }
            Err(cause) if cause.code() == git2::ErrorCode::NotFound => Ok(None),
            Err(cause) if cause.code() == git2::ErrorCode::InvalidSpec => Ok(None),
            Err(cause) => Err(git_error(&format!("cannot read {name}"), &cause)),
        }
    }

    /// The commits of every ref a name or glob covers.
    fn pattern_commits(repository: &Repository, pattern: &str) -> SourceResult<Vec<Oid>> {
        if !pattern.contains(['*', '?', '[']) {
            return Ok(Self::ref_commit(repository, pattern)?.into_iter().collect());
        }
        let references = repository
            .references_glob(pattern)
            .map_err(|cause| git_error(&format!("cannot list {pattern}"), &cause))?;
        let mut commits = Vec::new();
        for reference in references {
            let reference = reference.map_err(|cause| git_error(&format!("cannot list {pattern}"), &cause))?;
            if let Ok(commit) = reference.peel_to_commit() {
                commits.push(commit.id());
            }
        }
        Ok(commits)
    }
}

fn is_full_commit(revision: &str) -> bool {
    revision.len() == 40 && revision.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn commit_id(oid: Oid) -> SourceResult<CommitId> {
    CommitId::new(oid.to_string()).map_err(|cause| error(cause.to_string()))
}

/// Whether `path` is missing or an empty directory.
fn is_missing_or_empty(path: &Path) -> SourceResult<bool> {
    if !path.exists() {
        return Ok(true);
    }
    let mut entries =
        std::fs::read_dir(path).map_err(|cause| error(format!("cannot read {}: {cause}", path.display())))?;
    Ok(entries.next().is_none())
}

/// Remove every symbolic link in a working tree, outside `.git`, without
/// following any.
fn remove_symlinks(directory: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            std::fs::remove_file(&path)?;
        } else if kind.is_dir() && entry.file_name() != ".git" {
            remove_symlinks(&path)?;
        }
    }
    Ok(())
}

/// Remove the empty directories beneath `directory` (and `directory` itself
/// unless it is the root), outside the root's `.git`. Returns whether it is
/// empty.
fn remove_empty_directories(directory: &Path, root: bool) -> std::io::Result<bool> {
    let mut empty = true;
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() && !(root && entry.file_name() == ".git") {
            if !remove_empty_directories(&entry.path(), false)? {
                empty = false;
            }
        } else {
            empty = false;
        }
    }
    if empty && !root {
        std::fs::remove_dir(directory)?;
    }
    Ok(empty)
}

/// Reject a symbolic link whose target leaves the tree: absolute, or with
/// more `..` segments than its depth.
fn check_symlinks(repository: &Repository, tree: &git2::Tree<'_>) -> SourceResult<()> {
    let mut problem = None;
    tree.walk(TreeWalkMode::PreOrder, |directory, entry| {
        if entry.filemode() != SYMLINK_MODE || entry.kind() != Some(ObjectType::Blob) {
            return TreeWalkResult::Ok;
        }
        let name = format!("{directory}{}", entry.name().unwrap_or_default());
        let target = repository
            .find_blob(entry.id())
            .map(|blob| String::from_utf8_lossy(blob.content()).into_owned())
            .unwrap_or_default();
        if escapes(directory, &target) {
            problem = Some(format!("symbolic link {name} points outside the tree: {target:?}"));
            return TreeWalkResult::Abort;
        }
        TreeWalkResult::Ok
    })
    .or_else(|cause| {
        if problem.is_some() {
            Ok(())
        } else {
            Err(git_error("cannot read the tree", &cause))
        }
    })?;
    match problem {
        Some(message) => Err(error(message)),
        None => Ok(()),
    }
}

/// Whether `target`, relative to the tree directory `directory` (with a
/// trailing `/`, or empty at the root), leaves the tree.
fn escapes(directory: &str, target: &str) -> bool {
    let target_path = Path::new(target);
    if target.starts_with('/') || target.starts_with('\\') || target_path.has_root() {
        return true;
    }
    let mut depth = directory.split('/').filter(|segment| !segment.is_empty()).count();
    for component in target_path.components() {
        match component {
            Component::ParentDir => {
                if depth == 0 {
                    return true;
                }
                depth -= 1;
            }
            Component::Normal(_) => depth += 1,
            Component::CurDir => {}
            Component::RootDir | Component::Prefix(_) => return true,
        }
    }
    false
}

impl SourceResolver for GitSourceResolver {
    fn resolve_revision(&self, repository: &SourceRepository, revision: &str) -> SourceResult<CommitId> {
        let repo = self.open()?;
        self.ensure_fetched(&repo, repository)?;
        let missing = || {
            error(format!(
                "revision {revision:?} does not exist in {}",
                Self::describe(repository)
            ))
        };
        if is_full_commit(revision) {
            let oid = Oid::from_str(revision).map_err(|_| missing())?;
            if !Self::commit_exists(&repo, oid) {
                self.fetch_one(&repo, repository, revision, None);
            }
            return if Self::commit_exists(&repo, oid) {
                commit_id(oid)
            } else {
                Err(missing())
            };
        }
        let space = Self::space(repository);
        let candidates = space.candidates(revision);
        for candidate in &candidates {
            if let Some(oid) = Self::ref_commit(&repo, candidate)? {
                return commit_id(oid);
            }
        }
        // A full ref outside branches and tags, such as a pull request's
        // merge ref, is fetched on demand.
        if revision.starts_with("refs/") && space.other.is_some() {
            self.fetch_one(&repo, repository, revision, Some(&candidates[0]));
            if let Some(oid) = Self::ref_commit(&repo, &candidates[0])? {
                return commit_id(oid);
            }
        }
        Err(missing())
    }

    fn reachable_from(
        &self,
        repository: &SourceRepository,
        commit: &CommitId,
        refs: &[String],
    ) -> SourceResult<Option<String>> {
        let repo = self.open()?;
        self.ensure_fetched(&repo, repository)?;
        let Ok(oid) = Oid::from_str(commit.as_str()) else {
            return Ok(None);
        };
        if !Self::commit_exists(&repo, oid) {
            return Ok(None);
        }
        let space = Self::space(repository);
        for name in refs {
            for pattern in space.branch_patterns(name) {
                for tip in Self::pattern_commits(&repo, &pattern)? {
                    let contains = tip == oid
                        || repo
                            .graph_descendant_of(tip, oid)
                            .map_err(|cause| git_error("cannot compare commits", &cause))?;
                    if contains {
                        return Ok(Some(name.clone()));
                    }
                }
            }
        }
        Ok(None)
    }

    fn worktree(&self, repository: &SourceRepository, commit: &CommitId, path: &Path) -> SourceResult<()> {
        let (source, oid) = self.checked_commit(repository, commit)?;
        if !is_missing_or_empty(path)? {
            return Err(error(format!("worktree path {} is not empty", path.display())));
        }
        Self::create_checkout(&source, oid, path)
    }

    fn reset_worktree(&self, repository: &SourceRepository, commit: &CommitId, path: &Path) -> SourceResult<()> {
        let (source, oid) = self.checked_commit(repository, commit)?;
        if is_missing_or_empty(path)? {
            return Self::create_checkout(&source, oid, path);
        }
        if !Self::made_here(&source, path)? {
            return Err(error(format!(
                "worktree path {} is not empty and is not a checkout Nyl made",
                path.display()
            )));
        }
        if Self::reset_checkout(path, oid).unwrap_or(false) {
            return Ok(());
        }
        // Something survived the reset, such as a nested repository or a
        // changed file mode: start over from a fresh checkout.
        Self::remove_worktree(path)?;
        Self::create_checkout(&source, oid, path)
    }

    fn default_branch(&self, repository: &SourceRepository) -> SourceResult<String> {
        let repo = self.open()?;
        match repository {
            SourceRepository::This => {
                if let Ok(reference) = repo.find_reference("refs/remotes/origin/HEAD") {
                    if let Some(branch) = reference
                        .symbolic_target()
                        .ok()
                        .flatten()
                        .and_then(|target| target.strip_prefix("refs/remotes/origin/"))
                    {
                        return Ok(branch.to_owned());
                    }
                }
                let head = repo
                    .find_reference("HEAD")
                    .map_err(|cause| git_error("cannot read HEAD", &cause))?;
                head.symbolic_target()
                        .ok()
                        .flatten()
                    .and_then(|target| target.strip_prefix("refs/heads/"))
                    .map(str::to_owned)
                    .ok_or_else(|| {
                        error("this repository has no origin/HEAD and its HEAD is detached; name protectedRefs explicitly")
                    })
            }
            SourceRepository::Url(identity) => {
                let heads = nyl_git::ls_remote(
                    repo.path(),
                    self.read_url(identity),
                    &["HEAD", "refs/heads/*"],
                    &self.auth,
                )
                .map_err(|cause| error(cause.to_string()))?;
                let describe = Self::describe(repository);
                let head = heads
                    .get("HEAD")
                    .ok_or_else(|| error(format!("{describe} has no HEAD")))?;
                // `ls-remote` reports HEAD's commit, not its symbolic target:
                // the default branch is the branch at that commit, preferring
                // `main` and `master` when several are.
                let mut branches = heads
                    .iter()
                    .filter(|(name, oid)| *oid == head && name.starts_with("refs/heads/"))
                    .filter_map(|(name, _)| name.strip_prefix("refs/heads/"))
                    .collect::<Vec<_>>();
                if let Some(preferred) = ["main", "master"].into_iter().find(|name| branches.contains(name)) {
                    return Ok(preferred.to_owned());
                }
                match branches.len() {
                    1 => Ok(branches.remove(0).to_owned()),
                    0 => Err(error(format!("{describe} has no branch at its HEAD"))),
                    _ => Err(error(format!(
                        "the default branch of {describe} is ambiguous ({}); name protectedRefs explicitly",
                        branches.join(", ")
                    ))),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_escapes_detects_links_leaving_the_tree() {
        assert!(!escapes("", "file"));
        assert!(!escapes("a/b/", "../c"));
        assert!(!escapes("a/", "../c"));
        assert!(escapes("", "../c"));
        assert!(escapes("a/", "../../c"));
        assert!(escapes("a/", "/etc/passwd"));
        assert!(!escapes("a/", "x/../../b"));
        assert!(escapes("a/", "x/../../../b"));
    }
}
