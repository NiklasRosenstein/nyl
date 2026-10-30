use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use clap::Args;
use git2::Repository;
use nyl_core::orchestration::ports::CommitIdentity;

use crate::git::{GitManager, WorktreeManager};
use crate::gitops::inputs::verify_publication_base;
use crate::gitops::publication::{
    prepare_publication, target_render_index, writable_branch_name, PublicationAuthor, TreePublication,
};
use crate::gitops::{
    compile_target_tree_cached_with_observer_and_options, discover_gitops_inventory, resolve_deployment_target_name,
    CompiledTargetTree, GitOpsCache, GitOpsInventory, TreeCacheArgs, TreeRenderObserver, TreeRenderOptions,
};
use crate::{NylError, Result};

use super::super::tree_progress::{TreeProgressArgs, TreeProgressReporter};

/// Render, commit, and compare-and-swap publish one target revision.
#[derive(Args, Debug)]
pub struct PublishTreeArgs {
    #[command(flatten)]
    pub validation: crate::validation::ValidationArgs,
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
    /// Replace these ApplicationGroups, retaining the rest of the published tree.
    #[arg(long = "application-group")]
    pub application_groups: Vec<String>,
    /// Replace these group-qualified Releases (<applicationGroup>/<release>).
    #[arg(long = "release")]
    pub releases: Vec<String>,
    /// Commit message for a changed rendered tree.
    #[arg(long)]
    pub message: Option<String>,
    /// Prepare and commit locally without pushing.
    #[arg(long)]
    pub dry_run: bool,

    #[command(flatten)]
    source: PublishTreeSourceArgs,
}

#[derive(Args, Debug)]
struct PublishTreeSourceArgs {
    /// Publish the working-tree render even when it differs from the committed revision.
    #[arg(long, conflicts_with = "require_clean")]
    allow_dirty: bool,

    /// Require the entire source worktree to be clean before rendering.
    #[arg(long, conflicts_with = "allow_dirty")]
    require_clean: bool,

    /// Allow project secrets and NYL_* environment variables to affect rendered output.
    #[arg(long)]
    allow_secret_inputs: bool,
}

struct PublicationProvenance<'a> {
    source_repository: Option<&'a str>,
    source_commit: &'a str,
    source_dirty: bool,
    target: &'a str,
    cluster: &'a str,
}

struct CleanHeadWorktree {
    repository_root: PathBuf,
    checkout_root: PathBuf,
    _temporary: tempfile::TempDir,
}

impl CleanHeadWorktree {
    fn create(project_root: &Path, previous_paths: &[String], source_commit: &str) -> Result<(Self, PathBuf)> {
        let repository = Repository::discover(project_root)
            .map_err(|error| NylError::config(format!("Failed to inspect source Git repository: {error}")))?;
        let repository_root = repository
            .workdir()
            .ok_or_else(|| NylError::config("Source Git repository has no worktree"))?
            .canonicalize()?;
        let project_root = project_root.canonicalize()?;
        let project_relative = project_root.strip_prefix(&repository_root).map_err(|_| {
            NylError::config(format!(
                "Nyl project {} is outside source worktree {}",
                project_root.display(),
                repository_root.display()
            ))
        })?;
        let oid = git2::Oid::from_str(source_commit)
            .map_err(|error| NylError::config(format!("Invalid source commit {source_commit}: {error}")))?;
        let temporary = tempfile::Builder::new().prefix("nyl-publish-head-").tempdir()?;
        let checkout_name = temporary
            .path()
            .file_name()
            .ok_or_else(|| NylError::config("Temporary source checkout has no name"))?;
        let checkout_root = temporary.path().join(checkout_name);
        WorktreeManager::get_or_create_worktree(&repository_root, "HEAD", oid, &checkout_root)
            .map_err(NylError::Git)?;
        // Own the worktree before anything else can fail, so dropping the guard
        // removes it on every error path.
        let worktree = Self {
            repository_root,
            checkout_root,
            _temporary: temporary,
        };
        // The committed project may still be at an earlier location when the
        // move is not committed yet.
        let clean_project_root = crate::util::project_path::locate_checkout_project(
            &worktree.checkout_root,
            project_relative,
            &[],
            previous_paths,
        )?
        .directory;
        Ok((worktree, clean_project_root))
    }
}

impl Drop for CleanHeadWorktree {
    fn drop(&mut self) {
        if let Err(error) = WorktreeManager::remove_worktree(&self.repository_root, &self.checkout_root) {
            tracing::warn!(
                path = %self.checkout_root.display(),
                %error,
                "Failed to remove temporary clean source worktree"
            );
        }
    }
}

struct CleanHeadCompilation {
    _worktree: CleanHeadWorktree,
    inventory: GitOpsInventory,
    compiled: CompiledTargetTree,
}

struct SilentTreeRenderObserver;

impl TreeRenderObserver for SilentTreeRenderObserver {}

pub async fn execute(args: PublishTreeArgs) -> Result<()> {
    args.validation.validate_outputs(false, &[], &[])?;
    let inventory = discover_gitops_inventory(&args.path, None)?;
    let target_name = resolve_deployment_target_name(&inventory, args.target.as_deref())?;
    let carried = crate::gitops::inputs::carry_paths(&inventory, &target_name)?;
    let crate::gitops::provenance::WorktreeSourceState {
        commit: source_commit,
        dirty,
    } = crate::gitops::provenance::worktree_source_state(&inventory.project_root, &carried)?;
    let Some(source_commit) = source_commit else {
        return Err(NylError::config(
            "publish-tree requires the source worktree to have a committed revision",
        ));
    };
    if dirty && args.source.require_clean {
        let summary = crate::gitops::provenance::dirty_worktree_summary(&inventory.project_root, &carried)?;
        return Err(NylError::config(format!(
            "publish-tree --require-clean requires a clean source worktree\n{summary}\nCommit, stash, or remove the changes before publishing."
        )));
    }
    let cache = GitOpsCache::new(&inventory.project_root, args.cache.mode())?;
    let _cache_reporter = cache.reporter();
    let mut progress = TreeProgressReporter::new(args.progress, None);
    let render_options = TreeRenderOptions {
        selection: crate::gitops::tree::partial::PublicationSelection {
            application_groups: args.application_groups.iter().cloned().collect(),
            releases: args.releases.iter().cloned().collect(),
        },
        allow_secret_inputs: args.source.allow_secret_inputs,
        ..TreeRenderOptions::default()
    };
    let compiled = compile_target_tree_cached_with_observer_and_options(
        &inventory,
        &target_name,
        &cache,
        &mut progress,
        render_options.clone(),
    )
    .await?;
    let clean = if dirty && !args.source.allow_dirty {
        Some(
            compile_clean_head(
                &inventory,
                &compiled,
                &target_name,
                &source_commit,
                &cache,
                // The clean render reads the same publication state, so only
                // the source difference can make the two compiles disagree.
                TreeRenderOptions {
                    publication_base: compiled.publication_base.clone(),
                    ..render_options
                },
            )
            .await?,
        )
    } else {
        None
    };
    let (publication_inventory, compiled, source_dirty) = if let Some(clean) = &clean {
        tracing::info!(
            target = target_name,
            commit = source_commit,
            "Source worktree is dirty but the rendered deployment target matches HEAD"
        );
        (&clean.inventory, &clean.compiled, false)
    } else {
        if dirty {
            tracing::warn!(
                target = target_name,
                commit = source_commit,
                "Publishing a rendered deployment target from a dirty source worktree because --allow-dirty was specified"
            );
        }
        (&inventory, &compiled, dirty)
    };
    crate::validation::validate_tree(&args.validation, publication_inventory, compiled).await?;
    if !compiled.publication_report.selection.is_full() {
        let report = &compiled.publication_report;
        println!(
            "Partial publication at {}: {} rendered, {} retained, {} removed",
            report.baseline_commit.as_deref().unwrap_or("none"),
            report.rendered_releases.len(),
            report.retained_releases.len(),
            report.removed_releases.len()
        );
        for path in &report.changed_shared_files {
            println!("  Shared artifact: {}", path.display());
        }
    }
    publish_compiled(
        &args,
        publication_inventory,
        compiled,
        &target_name,
        &source_commit,
        source_dirty,
        &cache,
    )
}

fn publish_compiled(
    args: &PublishTreeArgs,
    inventory: &GitOpsInventory,
    compiled: &CompiledTargetTree,
    target_name: &str,
    source_commit: &str,
    source_dirty: bool,
    cache: &GitOpsCache,
) -> Result<()> {
    let publication_url = compiled
        .repository
        .publish_url
        .as_deref()
        .unwrap_or(&compiled.repository.repo_url);
    let branch = writable_branch_name(&compiled.target.spec.publication.revision)?;
    if let Some(commit) = publication_current_commit(compiled, publication_url, cache)? {
        // The published tree matching is not enough: state read at an older
        // base means the branch moved while rendering.
        verify_publication_base(
            compiled.publication_base.as_ref(),
            Some(&commit.to_string()),
            publication_url,
            branch,
        )?;
        print_publication_result(
            &format!("Deployment target {target_name} is already published"),
            publication_url,
            branch,
            commit,
        );
        return Ok(());
    }
    let auth = crate::cli::git_auth::project_git_auth(&inventory.project_config.config)?;
    let author_repository = Repository::discover(&inventory.project_root)
        .map_err(|error| NylError::config(format!("Failed to inspect source Git identity: {error}")))?;
    let subject = args
        .message
        .as_deref()
        .map_or_else(|| format!("Render deployment target {target_name}"), ToOwned::to_owned);
    let source_repository = source_repository_url(&author_repository);
    let message = publication_commit_message(
        &subject,
        &PublicationProvenance {
            source_repository: source_repository.as_deref(),
            source_commit,
            source_dirty,
            target: target_name,
            cluster: &compiled.cluster.metadata.name,
        },
        &compiled.publication_report,
    );
    let desired = compiled.owned_files();
    let prepared = prepare_publication(
        &TreePublication {
            push_url: publication_url,
            branch,
            path_prefix: compiled.target.publication_path_prefix(),
            desired: &desired,
            index: target_render_index(
                inventory,
                compiled,
                target_name,
                Some(source_commit.to_owned()),
                source_dirty,
            )?,
            options: compiled.reconcile_options(false),
            publication_base: compiled.publication_base.as_ref(),
            auth: &auth,
        },
        &message,
        || publication_author(&author_repository),
    )?;
    if !args.dry_run {
        prepared.verify_unmoved(&auth)?;
    }
    let Some(commit) = prepared.commit() else {
        let commit = prepared.head()?;
        print_publication_result(
            &format!("Deployment target {target_name} is already published"),
            publication_url,
            branch,
            commit,
        );
        return Ok(());
    };
    if args.dry_run {
        print_publication_result(
            &format!("Prepared publication for deployment target {target_name} without pushing"),
            publication_url,
            branch,
            commit,
        );
        return Ok(());
    }

    prepared.push(&auth)?;
    print_publication_result(
        &format!("Published deployment target {target_name}"),
        publication_url,
        branch,
        commit,
    );
    Ok(())
}

async fn compile_clean_head(
    working_inventory: &GitOpsInventory,
    working: &CompiledTargetTree,
    target_name: &str,
    source_commit: &str,
    cache: &GitOpsCache,
    render_options: TreeRenderOptions,
) -> Result<CleanHeadCompilation> {
    let (worktree, project_root) = CleanHeadWorktree::create(
        &working_inventory.project_root,
        &working_inventory.project_config.config.project.previous_paths,
        source_commit,
    )
    .map_err(|error| {
        dirty_verification_failure(
            target_name,
            source_commit,
            format!("could not locate the committed project: {error}"),
        )
    })?;
    let inventory = discover_gitops_inventory(&project_root, None).map_err(|error| {
        dirty_verification_failure(
            target_name,
            source_commit,
            format!("could not discover the committed project: {error}"),
        )
    })?;
    resolve_deployment_target_name(&inventory, Some(target_name)).map_err(|error| {
        dirty_verification_failure(
            target_name,
            source_commit,
            format!("could not select the committed target: {error}"),
        )
    })?;
    copy_carried_files(working_inventory, &inventory, target_name).map_err(|error| {
        dirty_verification_failure(
            target_name,
            source_commit,
            format!("could not provide carried files to the committed project: {error}"),
        )
    })?;
    let mut observer = SilentTreeRenderObserver;
    let compiled = compile_target_tree_cached_with_observer_and_options(
        &inventory,
        target_name,
        cache,
        &mut observer,
        render_options,
    )
    .await
    .map_err(|error| {
        dirty_verification_failure(
            target_name,
            source_commit,
            format!("could not render the committed target: {error}"),
        )
    })?;
    let differences = compilation_differences(working, &compiled)?;
    if !differences.is_empty() {
        return Err(NylError::config(format!(
            "Local changes affect deployment target {target_name:?} relative to source commit {source_commit}:\n- {}\nCommit or stash the relevant changes, or pass --allow-dirty to publish them explicitly.",
            differences.join("\n- ")
        )));
    }
    Ok(CleanHeadCompilation {
        _worktree: worktree,
        inventory,
        compiled,
    })
}

fn dirty_verification_failure(target_name: &str, source_commit: &str, detail: String) -> NylError {
    NylError::config(format!(
        "Could not verify dirty source worktree for deployment target {target_name:?} against source commit {source_commit}: {detail}\nCommit or stash the relevant changes, or pass --allow-dirty to publish them explicitly."
    ))
}

fn compilation_differences(working: &CompiledTargetTree, committed: &CompiledTargetTree) -> Result<Vec<String>> {
    const MAX_DIFFERENCES: usize = 12;

    let mut differences = Vec::new();
    if working.target.spec.publication != committed.target.spec.publication
        || working.repository_name != committed.repository_name
        || working.repository != committed.repository
    {
        differences.push(format!(
            "publication destination changed; moving a publication needs manual steps, see {}",
            super::diff_tree::PUBLICATION_MOVE_DOCS
        ));
    }
    if working.cluster.metadata.name != committed.cluster.metadata.name {
        differences.push(format!(
            "destination cluster changed from {:?} to {:?}",
            committed.cluster.metadata.name, working.cluster.metadata.name
        ));
    }
    let (working_files, committed_files) = (working.owned_files(), committed.owned_files());
    for path in working_files
        .keys()
        .chain(committed_files.keys())
        .collect::<std::collections::BTreeSet<_>>()
    {
        match (working_files.get(path), committed_files.get(path)) {
            (Some(_), None) => differences.push(format!(
                "added {}",
                crate::resources::relative_path_to_posix("rendered output path", path)?
            )),
            (None, Some(_)) => differences.push(format!(
                "removed {}",
                crate::resources::relative_path_to_posix("rendered output path", path)?
            )),
            (Some(working), Some(committed)) if working != committed => differences.push(format!(
                "modified {}",
                crate::resources::relative_path_to_posix("rendered output path", path)?
            )),
            _ => {}
        }
    }
    if differences.len() > MAX_DIFFERENCES {
        let omitted = differences.len() - MAX_DIFFERENCES;
        differences.truncate(MAX_DIFFERENCES);
        differences.push(format!("and {omitted} more difference(s)"));
    }
    Ok(differences)
}

/// Give the clean-`HEAD` render the same carried bytes as the working render.
///
/// Carried files are untracked outputs of this run, so a checkout of `HEAD`
/// never contains them. Each `carryFileFromWorktree` path of the committed target is copied
/// from the same worktree-relative location of the working tree, when present.
fn copy_carried_files(working: &GitOpsInventory, committed: &GitOpsInventory, target_name: &str) -> Result<()> {
    for destination in crate::gitops::inputs::carry_paths(committed, target_name)? {
        let relative = destination.strip_prefix(&committed.worktree_root).map_err(|_| {
            NylError::config(format!(
                "Carried file {} is outside the committed worktree",
                destination.display()
            ))
        })?;
        let source = working.worktree_root.join(relative);
        if std::fs::symlink_metadata(&source).is_ok_and(|metadata| metadata.is_file()) {
            if let Some(parent) = destination.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(&source, &destination)?;
        }
    }
    Ok(())
}

fn publication_current_commit(
    compiled: &crate::gitops::CompiledTargetTree,
    publication_url: &str,
    cache: &GitOpsCache,
) -> Result<Option<git2::Oid>> {
    let mut manager = if let Some(cache_root) = cache.external_cache_root() {
        GitManager::with_cache_dir(cache_root)
    } else {
        GitManager::new().map_err(NylError::Git)?
    }
    .with_render_cache(Some(cache.clone()));
    let checkout =
        match manager.resolve_ref_fresh(publication_url, Some(&compiled.target.spec.publication.revision), None) {
            Ok(checkout) => checkout,
            Err(error) => {
                tracing::debug!(%error, "Published revision is unavailable for preflight comparison");
                return Ok(None);
            }
        };
    let published_repository = Repository::open(&checkout).map_err(crate::git::GitError::from)?;
    let published_commit = published_repository
        .head()
        .and_then(|head| head.peel_to_commit())
        .map_err(crate::git::GitError::from)?
        .id();
    let root = super::diff_tree::checked_published_root(&checkout, compiled.target.publication_path_prefix())?;
    let published = super::diff_tree::read_rendered_tree(&root, &compiled.declared_state_paths())?;
    let Some(index) = published.index else {
        return Ok(None);
    };
    let repository = compiled
        .repository_name
        .as_deref()
        .unwrap_or(&compiled.repository.repo_url);
    if index.compilation.is_none()
        || index.target != compiled.target.metadata.name
        || index.cluster != compiled.cluster.metadata.name
        || index.publication.repository != repository
        || index.publication.revision != compiled.target.spec.publication.revision
        || index.publication.path_prefix != compiled.target.publication_path_prefix()
    {
        return Ok(None);
    }
    let desired = compiled
        .owned_files()
        .iter()
        .map(|(path, bytes)| {
            crate::resources::relative_path_to_posix("rendered output path", path)
                .map(|path| (path, nyl_core::digest::sha256_hex(bytes)))
                .map_err(NylError::from)
        })
        .collect::<Result<std::collections::BTreeMap<_, _>>>()?;
    Ok((index.files == desired).then_some(published_commit))
}

fn source_repository_url(repository: &Repository) -> Option<String> {
    repository
        .find_remote("origin")
        .ok()
        .and_then(|remote| remote.url().ok().map(crate::util::sanitize_url))
}

fn publication_commit_message(
    subject: &str,
    provenance: &PublicationProvenance<'_>,
    report: &crate::gitops::tree::partial::PublicationReport,
) -> String {
    let mut message = subject.trim_end().to_owned();
    message.push_str("\n\n");
    if let Some(source_repository) = provenance.source_repository {
        writeln!(message, "Nyl-Source-Repository: {source_repository}").expect("writing to a String cannot fail");
    }
    writeln!(message, "Nyl-Source-Commit: {}", provenance.source_commit).expect("writing to a String cannot fail");
    if provenance.source_dirty {
        writeln!(message, "Nyl-Source-Dirty: true").expect("writing to a String cannot fail");
    }
    writeln!(message, "Nyl-Deployment-Target: {}", provenance.target).expect("writing to a String cannot fail");
    write!(message, "Nyl-Cluster: {}", provenance.cluster).expect("writing to a String cannot fail");
    if !report.selection.is_full() {
        write!(
            message,
            "\nNyl-Publication-Base: {}\nNyl-Publication-Selection: {}",
            report.baseline_commit.as_deref().unwrap_or("none"),
            report
                .selection
                .application_groups
                .iter()
                .map(|group| format!("group:{group}"))
                .chain(
                    report
                        .selection
                        .releases
                        .iter()
                        .map(|release| format!("release:{release}"))
                )
                .collect::<Vec<_>>()
                .join(", ")
        )
        .expect("writing to a String cannot fail");
    }
    message
}

fn print_publication_result(status: &str, repository: &str, branch: &str, commit: git2::Oid) {
    println!("✓ {status}");
    println!("  Repository: {}", crate::util::sanitize_url(repository));
    println!("  Branch: {branch}");
    println!("  Commit: {commit}");
}

/// The author and committer of a publish-tree commit: the identity from
/// `NYL_GIT_AUTHOR_*`, `GIT_AUTHOR_*`, or the source repository's Git
/// configuration, at the current time.
fn publication_author(repository: &Repository) -> Result<PublicationAuthor> {
    Ok(PublicationAuthor {
        identity: configured_publication_identity(
            repository,
            std::env::var("NYL_GIT_AUTHOR_NAME").ok().as_deref(),
            std::env::var("NYL_GIT_AUTHOR_EMAIL").ok().as_deref(),
            std::env::var("GIT_AUTHOR_NAME").ok().as_deref(),
            std::env::var("GIT_AUTHOR_EMAIL").ok().as_deref(),
        )?,
        time: chrono::Utc::now(),
    })
}

fn configured_publication_identity(
    repository: &Repository,
    nyl_name: Option<&str>,
    nyl_email: Option<&str>,
    git_name: Option<&str>,
    git_email: Option<&str>,
) -> Result<CommitIdentity> {
    let configured = repository.signature().ok();
    let name = nyl_name
        .or(git_name)
        .map(ToOwned::to_owned)
        .or_else(|| {
            configured
                .as_ref()
                .and_then(|signature| signature.name().ok().map(ToOwned::to_owned))
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
                .and_then(|signature| signature.email().ok().map(ToOwned::to_owned))
        })
        .ok_or_else(|| {
            NylError::config(
                "Git author email is not configured; set user.email, GIT_AUTHOR_EMAIL, or NYL_GIT_AUTHOR_EMAIL",
            )
        })?;
    Ok(CommitIdentity { name, email })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_publication_subject_keeps_provenance_trailers() {
        let message = publication_commit_message(
            "Custom rendered update\n",
            &PublicationProvenance {
                source_repository: Some("https://example.invalid/source.git"),
                source_commit: "0123456789abcdef",
                source_dirty: false,
                target: "production",
                cluster: "prod-eu-1",
            },
            &crate::gitops::tree::partial::PublicationReport::default(),
        );

        assert_eq!(
            message,
            "Custom rendered update\n\nNyl-Source-Repository: https://example.invalid/source.git\nNyl-Source-Commit: 0123456789abcdef\nNyl-Deployment-Target: production\nNyl-Cluster: prod-eu-1"
        );
    }

    fn configured_repository() -> (tempfile::TempDir, Repository) {
        let temporary = tempfile::TempDir::new().unwrap();
        let repository = Repository::init(temporary.path()).unwrap();
        let mut config = repository.config().unwrap();
        config.set_str("user.name", "Configured Author").unwrap();
        config.set_str("user.email", "author@example.invalid").unwrap();
        (temporary, repository)
    }

    #[test]
    fn nyl_author_override_takes_precedence_over_git_identity() {
        let (_temporary, repository) = configured_repository();
        let identity = configured_publication_identity(
            &repository,
            Some("Nyl Override"),
            Some("nyl@example.invalid"),
            Some("Git Environment"),
            Some("git@example.invalid"),
        )
        .unwrap();
        assert_eq!(identity.name, "Nyl Override");
        assert_eq!(identity.email, "nyl@example.invalid");
    }

    #[test]
    fn source_repository_identity_authors_without_overrides() {
        let (_temporary, repository) = configured_repository();
        let identity = configured_publication_identity(&repository, None, None, None, None).unwrap();
        assert_eq!(identity.name, "Configured Author");
        assert_eq!(identity.email, "author@example.invalid");
    }
}
