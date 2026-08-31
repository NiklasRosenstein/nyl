use std::path::{Path, PathBuf};

use clap::Args;
use git2::build::RepoBuilder;
use git2::{FetchOptions, IndexAddOption, PushOptions, Repository, Signature, StatusOptions};

use crate::git::CredentialProvider;
use crate::gitops::{
    compile_target_tree, discover_gitops_inventory, reconcile_rendered_tree, RenderIndex, RenderIndexPublication,
};
use crate::{NylError, Result};

/// Render, commit, and compare-and-swap publish one target revision.
#[derive(Args, Debug)]
pub struct PublishTreeArgs {
    /// Project directory or a path beneath it.
    #[arg(default_value = ".")]
    pub path: PathBuf,
    #[arg(long)]
    pub target: String,
    /// Commit message for a changed rendered tree.
    #[arg(long)]
    pub message: Option<String>,
    /// Prepare and commit locally without pushing.
    #[arg(long)]
    pub dry_run: bool,
}

pub async fn execute(args: PublishTreeArgs) -> Result<()> {
    let inventory = discover_gitops_inventory(&args.path, None)?;
    let (source_commit, dirty) = source_state(&inventory.project_root)?;
    if dirty || source_commit.is_none() {
        return Err(NylError::config(
            "publish-tree requires a clean source worktree at a committed revision",
        ));
    }
    let compiled = compile_target_tree(&inventory, &args.target).await?;
    let publication_url = compiled
        .repository
        .publish_url
        .as_deref()
        .unwrap_or(&compiled.repository.repo_url);
    let branch = writable_branch_name(&compiled.target.spec.publication.revision)?;
    let credentials = CredentialProvider::new();
    let temp = tempfile::TempDir::new()?;
    let repository = clone_branch(publication_url, branch, temp.path(), &credentials)?;
    let expected = remote_branch_oid(&repository, branch);

    let output_root = if compiled.target.spec.publication.path_prefix.is_empty() {
        temp.path().to_path_buf()
    } else {
        temp.path().join(&compiled.target.spec.publication.path_prefix)
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
            args.target.clone(),
            compiled.cluster.metadata.name.clone(),
            RenderIndexPublication {
                repository: repository_identity,
                revision: compiled.target.spec.publication.revision.clone(),
                path_prefix: compiled.target.spec.publication.path_prefix.clone(),
            },
            source_commit,
            false,
            inputs,
        ),
    )?;

    let commit = commit_worktree(
        &repository,
        branch,
        args.message
            .as_deref()
            .unwrap_or(&format!("Render GitOps target {}", args.target)),
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
        println!("GitOps target {} is already published", args.target);
        return Ok(());
    };
    if args.dry_run {
        println!("✓ Prepared rendered commit {commit} for {} (not pushed)", args.target);
        return Ok(());
    }

    push_branch(&repository, publication_url, branch, expected, &credentials)?;
    println!("✓ Published GitOps target {} as commit {commit}", args.target);
    Ok(())
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
    let commit = if let Some(oid) = remote_branch_oid(&repository, branch) {
        repository.find_commit(oid).map_err(crate::git::GitError::from)?
    } else {
        repository
            .head()
            .and_then(|head| head.peel_to_commit())
            .map_err(crate::git::GitError::from)?
    };
    repository
        .branch(branch, &commit, true)
        .map_err(crate::git::GitError::from)?;
    repository
        .set_head(&format!("refs/heads/{branch}"))
        .map_err(crate::git::GitError::from)?;
    repository.checkout_head(None).map_err(crate::git::GitError::from)?;
    drop(commit);
    Ok(repository)
}

fn remote_branch_oid(repository: &Repository, branch: &str) -> Option<git2::Oid> {
    repository
        .find_reference(&format!("refs/remotes/origin/{branch}"))
        .ok()
        .and_then(|reference| reference.target())
}

fn commit_worktree(repository: &Repository, branch: &str, message: &str) -> Result<Option<git2::Oid>> {
    let mut status_options = StatusOptions::new();
    status_options.include_untracked(true).recurse_untracked_dirs(true);
    let statuses = repository
        .statuses(Some(&mut status_options))
        .map_err(|error| NylError::config(format!("Failed to inspect rendered worktree: {error}")))?;
    if statuses.is_empty() {
        return Ok(None);
    }
    let mut index = repository.index().map_err(crate::git::GitError::from)?;
    index
        .add_all(["*"], IndexAddOption::DEFAULT, None)
        .map_err(crate::git::GitError::from)?;
    index.update_all(["*"], None).map_err(crate::git::GitError::from)?;
    index.write().map_err(crate::git::GitError::from)?;
    let tree_id = index.write_tree().map_err(crate::git::GitError::from)?;
    let tree = repository.find_tree(tree_id).map_err(crate::git::GitError::from)?;
    let parent = repository
        .head()
        .and_then(|head| head.peel_to_commit())
        .map_err(crate::git::GitError::from)?;
    let signature = Signature::now(
        &std::env::var("NYL_GIT_AUTHOR_NAME").unwrap_or_else(|_| "Nyl GitOps".to_string()),
        &std::env::var("NYL_GIT_AUTHOR_EMAIL").unwrap_or_else(|_| "nyl@localhost".to_string()),
    )
    .map_err(crate::git::GitError::from)?;
    let oid = repository
        .commit(
            Some(&format!("refs/heads/{branch}")),
            &signature,
            &signature,
            message,
            &tree,
            &[&parent],
        )
        .map_err(crate::git::GitError::from)?;
    Ok(Some(oid))
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

fn source_state(project_root: &Path) -> Result<(Option<String>, bool)> {
    let repository = Repository::discover(project_root)
        .map_err(|error| NylError::config(format!("Failed to inspect source repository: {error}")))?;
    let commit = repository
        .head()
        .ok()
        .and_then(|head| head.target())
        .map(|oid| oid.to_string());
    let mut options = StatusOptions::new();
    options.include_untracked(true).recurse_untracked_dirs(true);
    let dirty = !repository
        .statuses(Some(&mut options))
        .map_err(|error| NylError::config(format!("Failed to inspect source status: {error}")))?
        .is_empty();
    Ok((commit, dirty))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_branch_revisions() {
        assert_eq!(writable_branch_name("deploy/main").unwrap(), "deploy/main");
        assert_eq!(writable_branch_name("refs/heads/deploy/main").unwrap(), "deploy/main");
        assert!(writable_branch_name("refs/tags/v1").is_err());
    }
}
