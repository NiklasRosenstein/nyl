use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::Args;
use git2::build::{CheckoutBuilder, RepoBuilder};
use git2::{FetchOptions, IndexAddOption, PushOptions, Repository, ResetType, Signature, StatusOptions};

use crate::git::CredentialProvider;
use crate::git::GitManager;
use crate::gitops::{
    compile_target_tree_cached_with_observer, discover_gitops_inventory, reconcile_rendered_tree,
    resolve_deployment_target_name, GitOpsCache, RenderIndex, RenderIndexPublication, TreeCacheArgs,
};
use crate::{NylError, Result};

use super::super::tree_progress::{TreeProgressArgs, TreeProgressReporter};

/// Render, commit, and compare-and-swap publish one target revision.
#[derive(Args, Debug)]
pub struct PublishTreeArgs {
    #[command(flatten)]
    pub cache: TreeCacheArgs,

    #[command(flatten)]
    pub progress: TreeProgressArgs,

    /// Project directory or a path beneath it.
    #[arg(default_value = ".")]
    pub path: PathBuf,
    /// DeploymentTarget to publish. Defaults to the sole configured target.
    #[arg(long)]
    pub target: Option<String>,
    /// Commit message for a changed rendered tree.
    #[arg(long)]
    pub message: Option<String>,
    /// Prepare and commit locally without pushing.
    #[arg(long)]
    pub dry_run: bool,
}

pub async fn execute(args: PublishTreeArgs) -> Result<()> {
    let inventory = discover_gitops_inventory(&args.path, None)?;
    let target_name = resolve_deployment_target_name(&inventory, args.target.as_deref())?;
    let (source_commit, dirty) = super::render_tree::source_state(&inventory.project_root)?;
    if dirty || source_commit.is_none() {
        return Err(NylError::config(
            "publish-tree requires a clean source worktree at a committed revision",
        ));
    }
    let cache = GitOpsCache::new(&inventory.project_root, args.cache.mode())?;
    let _cache_reporter = cache.reporter();
    let mut progress = TreeProgressReporter::new(args.progress, None);
    let compiled = compile_target_tree_cached_with_observer(&inventory, &target_name, &cache, &mut progress).await?;
    let publication_url = compiled
        .repository
        .publish_url
        .as_deref()
        .unwrap_or(&compiled.repository.repo_url);
    let branch = writable_branch_name(&compiled.target.spec.publication.revision)?;
    let credentials = Arc::new(CredentialProvider::new());
    if publication_is_current(&compiled, publication_url, &credentials, &cache)? {
        println!("deployment target {target_name} is already published");
        return Ok(());
    }
    let temp = tempfile::TempDir::new()?;
    let repository = clone_branch(publication_url, branch, temp.path(), &credentials)?;
    let expected = remote_branch_oid(&repository, branch);

    let output_root = if compiled.target.publication_path_prefix().is_empty() {
        temp.path().to_path_buf()
    } else {
        temp.path().join(compiled.target.publication_path_prefix())
    };
    let inputs = super::render_tree::hash_inputs(&inventory, &compiled)?;
    let repository_identity = compiled
        .repository_name
        .clone()
        .unwrap_or_else(|| compiled.repository.repo_url.clone());
    reconcile_rendered_tree(
        &output_root,
        &compiled.files,
        RenderIndex::new(
            target_name.clone(),
            compiled.cluster.metadata.name.clone(),
            RenderIndexPublication {
                repository: repository_identity,
                revision: compiled.target.spec.publication.revision.clone(),
                path_prefix: compiled.target.publication_path_prefix().to_owned(),
            },
            source_commit,
            false,
            inputs,
        ),
    )?;

    let author_repository = Repository::discover(&inventory.project_root)
        .map_err(|error| NylError::config(format!("Failed to inspect source Git identity: {error}")))?;
    let commit = commit_worktree(
        &repository,
        &author_repository,
        branch,
        &output_root,
        args.message
            .as_deref()
            .unwrap_or(&format!("Render deployment target {target_name}")),
    )?;
    if !args.dry_run {
        fetch_branch(&repository, publication_url, branch, &credentials)?;
        let actual = remote_branch_oid(&repository, branch);
        if actual != expected {
            return Err(NylError::config(format!(
                "Publication {publication_url}@{branch} advanced from {expected:?} to {actual:?}; refusing stale publish"
            )));
        }
    }
    let Some(commit) = commit else {
        println!("deployment target {target_name} is already published");
        return Ok(());
    };
    if args.dry_run {
        println!("✓ Prepared rendered commit {commit} for {target_name} (not pushed)");
        return Ok(());
    }

    push_branch(&repository, publication_url, branch, expected, &credentials)?;
    println!("✓ Published deployment target {target_name} as commit {commit}");
    Ok(())
}

fn publication_is_current(
    compiled: &crate::gitops::CompiledTargetTree,
    publication_url: &str,
    credentials: &Arc<CredentialProvider>,
    cache: &GitOpsCache,
) -> Result<bool> {
    let mut manager = if let Some(cache_root) = cache.external_cache_root() {
        GitManager::with_cache_dir_and_provider(cache_root, Some(Arc::clone(credentials)))
    } else {
        GitManager::with_credential_provider(Some(Arc::clone(credentials))).map_err(NylError::Git)?
    };
    let checkout =
        match manager.resolve_ref_fresh(publication_url, Some(&compiled.target.spec.publication.revision), None) {
            Ok(checkout) => checkout,
            Err(error) => {
                tracing::debug!(%error, "Published revision is unavailable for preflight comparison");
                return Ok(false);
            }
        };
    let root = super::diff_tree::checked_published_root(&checkout, compiled.target.publication_path_prefix())?;
    let published = super::diff_tree::read_rendered_tree(&root)?;
    let Some(index) = published.index else {
        return Ok(false);
    };
    let repository = compiled
        .repository_name
        .as_deref()
        .unwrap_or(&compiled.repository.repo_url);
    if index.target != compiled.target.metadata.name
        || index.cluster != compiled.cluster.metadata.name
        || index.publication.repository != repository
        || index.publication.revision != compiled.target.spec.publication.revision
        || index.publication.path_prefix != compiled.target.publication_path_prefix()
    {
        return Ok(false);
    }
    let desired = compiled
        .files
        .iter()
        .map(|(path, bytes)| {
            crate::resources::relative_path_to_posix("rendered output path", path)
                .map(|path| (path, crate::gitops::reconcile::sha256(bytes)))
        })
        .collect::<Result<std::collections::BTreeMap<_, _>>>()?;
    Ok(index.files == desired)
}

fn writable_branch_name(revision: &str) -> Result<&str> {
    let branch = revision.strip_prefix("refs/heads/").unwrap_or(revision);
    if branch.is_empty() || revision.starts_with("refs/") && !revision.starts_with("refs/heads/") {
        Err(NylError::config(format!(
            "publish-tree publication revision {revision:?} must name a branch"
        )))
    } else {
        Ok(branch)
    }
}

fn clone_branch(url: &str, branch: &str, path: &Path, credentials: &CredentialProvider) -> Result<Repository> {
    let mut fetch = FetchOptions::new();
    fetch.remote_callbacks(credentials.build_callbacks(url));
    let repository = RepoBuilder::new()
        .fetch_options(fetch)
        .clone(url, path)
        .map_err(|error| NylError::config(format!("Failed to clone {url}: {error}")))?;
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

fn remote_branch_oid(repository: &Repository, branch: &str) -> Option<git2::Oid> {
    repository
        .find_reference(&format!("refs/remotes/origin/{branch}"))
        .ok()
        .and_then(|reference| reference.target())
}

fn commit_worktree(
    repository: &Repository,
    author_repository: &Repository,
    branch: &str,
    output_root: &Path,
    message: &str,
) -> Result<Option<git2::Oid>> {
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
    let signature = publication_signature(author_repository)?;
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

fn publication_signature(repository: &Repository) -> Result<Signature<'static>> {
    configured_publication_signature(
        repository,
        std::env::var("NYL_GIT_AUTHOR_NAME").ok().as_deref(),
        std::env::var("NYL_GIT_AUTHOR_EMAIL").ok().as_deref(),
        std::env::var("GIT_AUTHOR_NAME").ok().as_deref(),
        std::env::var("GIT_AUTHOR_EMAIL").ok().as_deref(),
    )
}

fn configured_publication_signature(
    repository: &Repository,
    nyl_name: Option<&str>,
    nyl_email: Option<&str>,
    git_name: Option<&str>,
    git_email: Option<&str>,
) -> Result<Signature<'static>> {
    let configured = repository.signature().ok();
    let name = nyl_name
        .or(git_name)
        .map(ToOwned::to_owned)
        .or_else(|| {
            configured
                .as_ref()
                .and_then(|signature| signature.name().map(ToOwned::to_owned))
        })
        .ok_or_else(|| {
            NylError::config(
                "Git author name is not configured; set user.name, GIT_AUTHOR_NAME, or NYL_GIT_AUTHOR_NAME",
            )
        })?;
    let email = nyl_email
        .or(git_email)
        .map(ToOwned::to_owned)
        .or_else(|| {
            configured
                .as_ref()
                .and_then(|signature| signature.email().map(ToOwned::to_owned))
        })
        .ok_or_else(|| {
            NylError::config(
                "Git author email is not configured; set user.email, GIT_AUTHOR_EMAIL, or NYL_GIT_AUTHOR_EMAIL",
            )
        })?;
    Ok(Signature::now(&name, &email).map_err(crate::git::GitError::from)?)
}

fn fetch_branch(repository: &Repository, url: &str, branch: &str, credentials: &CredentialProvider) -> Result<()> {
    let mut remote = repository.find_remote("origin").map_err(crate::git::GitError::from)?;
    let mut options = FetchOptions::new();
    options.remote_callbacks(credentials.build_callbacks(url));
    remote
        .fetch(&[branch], Some(&mut options), None)
        .map_err(|error| NylError::config(format!("Failed to refresh publication branch: {error}")))
}

fn push_branch(
    repository: &Repository,
    url: &str,
    branch: &str,
    expected: Option<git2::Oid>,
    credentials: &CredentialProvider,
) -> Result<()> {
    let mut remote = repository.find_remote("origin").map_err(crate::git::GitError::from)?;
    let mut options = PushOptions::new();
    let publication_ref = format!("refs/heads/{branch}");
    let mut callbacks = credentials.build_callbacks(url);
    callbacks.push_negotiation({
        let publication_ref = publication_ref.clone();
        move |updates| {
            let update = updates
                .iter()
                .find(|update| update.dst_refname() == Some(publication_ref.as_str()))
                .ok_or_else(|| git2::Error::from_str("publication branch was absent from push negotiation"))?;
            let advertised = (!update.src().is_zero()).then_some(update.src());
            if advertised != expected {
                return Err(git2::Error::from_str(
                    "publication branch changed during compare-and-swap publication",
                ));
            }
            Ok(())
        }
    });
    options.remote_callbacks(callbacks);
    remote
        .push(&[format!("refs/heads/{branch}:{publication_ref}")], Some(&mut options))
        .map_err(|error| NylError::config(format!("Failed to publish publication branch: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit_initial(repository: &Repository) -> git2::Oid {
        let mut index = repository.index().unwrap();
        index.add_all(["*"], IndexAddOption::DEFAULT, None).unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repository.find_tree(tree_id).unwrap();
        let signature = Signature::now("Initial", "initial@example.invalid").unwrap();
        repository
            .commit(Some("HEAD"), &signature, &signature, "Initial", &tree, &[])
            .unwrap()
    }

    #[test]
    fn accepts_only_branch_revisions() {
        assert_eq!(writable_branch_name("deploy/main").unwrap(), "deploy/main");
        assert_eq!(writable_branch_name("refs/heads/deploy/main").unwrap(), "deploy/main");
        assert!(writable_branch_name("refs/tags/v1").is_err());
    }

    #[test]
    fn publication_checkout_and_commit_are_scoped_to_the_rendered_tree() {
        let source = tempfile::TempDir::new().unwrap();
        let source_repository = Repository::init(source.path()).unwrap();
        let mut source_config = source_repository.config().unwrap();
        source_config.set_str("user.name", "Configured Author").unwrap();
        source_config.set_str("user.email", "author@example.invalid").unwrap();
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
        let signature = Signature::now("Initial", "initial@example.invalid").unwrap();
        source_repository
            .commit(
                Some("refs/heads/deploy/main"),
                &signature,
                &signature,
                "Deploy",
                &tree,
                &[&source_repository.find_commit(initial).unwrap()],
            )
            .unwrap();

        let checkout = tempfile::TempDir::new().unwrap();
        let repository = clone_branch(
            source.path().to_str().unwrap(),
            "deploy/main",
            checkout.path(),
            &CredentialProvider::new(),
        )
        .unwrap();
        assert!(!checkout.path().join("source.txt").exists());
        assert!(checkout.path().join("deploy/old.yaml").is_file());
        assert!(repository.statuses(None).unwrap().is_empty());

        std::fs::write(checkout.path().join("unrelated.txt"), "do not stage\n").unwrap();
        std::fs::write(checkout.path().join("deploy/new.yaml"), "new\n").unwrap();
        let commit = commit_worktree(
            &repository,
            &source_repository,
            "deploy/main",
            &checkout.path().join("deploy"),
            "Render",
        )
        .unwrap()
        .unwrap();
        let commit = repository.find_commit(commit).unwrap();
        assert_eq!(commit.author().name(), Some("Configured Author"));
        assert_eq!(commit.author().email(), Some("author@example.invalid"));
        assert!(commit.tree().unwrap().get_path(Path::new("deploy/new.yaml")).is_ok());
        assert!(commit.tree().unwrap().get_path(Path::new("unrelated.txt")).is_err());
    }

    #[test]
    fn absent_publication_branch_starts_with_an_empty_worktree() {
        let source = tempfile::TempDir::new().unwrap();
        let source_repository = Repository::init(source.path()).unwrap();
        std::fs::write(source.path().join("source.txt"), "source branch\n").unwrap();
        commit_initial(&source_repository);

        let checkout = tempfile::TempDir::new().unwrap();
        let repository = clone_branch(
            source.path().to_str().unwrap(),
            "deploy/new",
            checkout.path(),
            &CredentialProvider::new(),
        )
        .unwrap();
        assert!(!checkout.path().join("source.txt").exists());
        assert!(repository.statuses(None).unwrap().is_empty());
        assert!(repository.head().is_err());
    }

    #[test]
    fn nyl_author_override_takes_precedence_over_git_identity() {
        let temporary = tempfile::TempDir::new().unwrap();
        let repository = Repository::init(temporary.path()).unwrap();
        let mut config = repository.config().unwrap();
        config.set_str("user.name", "Configured Author").unwrap();
        config.set_str("user.email", "author@example.invalid").unwrap();
        let signature = configured_publication_signature(
            &repository,
            Some("Nyl Override"),
            Some("nyl@example.invalid"),
            Some("Git Environment"),
            Some("git@example.invalid"),
        )
        .unwrap();
        assert_eq!(signature.name(), Some("Nyl Override"));
        assert_eq!(signature.email(), Some("nyl@example.invalid"));
    }
}
