//! Provenance of a rendered tree: the source commit it is attributed to and
//! the input digests its ownership index records.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use git2::Repository;

use super::{CompiledTargetTree, GitOpsInventory};
use crate::resources::{GitOpsResource, GitOpsResourceKind};
use crate::{NylError, Result};

/// The commit a worktree is at and whether it differs from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeSourceState {
    /// `HEAD`'s commit, or `None` in a repository without commits.
    pub commit: Option<String>,
    /// Whether tracked or untracked files differ from `commit`.
    pub dirty: bool,
}

/// Require committed-source provenance for publication. A Git commit containing
/// a worktree snapshot remains development input even after checkout or fetch.
pub fn require_publication_source(project_root: &Path, commit: &str) -> Result<()> {
    let repository = Repository::discover(project_root)
        .map_err(|error| NylError::config(format!("Cannot inspect publication source: {error}")))?;
    let source = repository
        .find_commit(git2::Oid::from_str(commit).map_err(|error| NylError::config(error.to_string()))?)
        .map_err(|error| NylError::config(error.to_string()))?;
    if source
        .message()
        .is_ok_and(nyl_core::orchestration::ports::is_worktree_snapshot_message)
    {
        return Err(NylError::config("NYL-SOURCE-WORKTREE: worktree snapshots cannot be published as committed source; commit the changes and publish that revision"));
    }
    Ok(())
}

/// Source commit and dirty state of the worktree containing `project_root`.
/// Untracked `carried` files are outputs of this run, so they never make the
/// worktree dirty, and neither does the project's `.nyl/cache`.
pub fn worktree_source_state(project_root: &Path, carried: &[PathBuf]) -> Result<WorktreeSourceState> {
    let repository = Repository::discover(project_root)
        .map_err(|error| NylError::config(format!("Failed to inspect source Git repository: {error}")))?;
    let commit = repository
        .head()
        .ok()
        .and_then(|head| head.target())
        .map(|oid| oid.to_string());
    let dirty = !dirty_worktree_changes(&repository, project_root, carried)?.is_empty();
    Ok(WorktreeSourceState { commit, dirty })
}

/// Changed files used by the source cleanliness check, with Git's two-column
/// index/worktree status codes and paths relative to the repository root.
/// Paths are quoted and escaped so whitespace and control characters stay unambiguous.
/// The diagnostic is bounded; git status can show the complete list.
///
/// When changed tracked files use Git LFS and Git cannot run its clean
/// filter, the summary explains how to install it, since such files read as
/// modified even when their content is unchanged. The files still count as changes.
pub fn dirty_worktree_summary(project_root: &Path, carried: &[PathBuf]) -> Result<String> {
    const MAX_PATHS: usize = 20;

    let repository = Repository::discover(project_root)
        .map_err(|error| NylError::config(format!("Failed to inspect source Git repository: {error}")))?;
    let changes = dirty_worktree_changes(&repository, project_root, carried)?;
    let root = repository.workdir().unwrap_or(project_root);
    let mut summary = format!("Dirty files (relative to {}):", root.display());
    for change in changes.iter().take(MAX_PATHS) {
        summary.push_str("\n  ");
        summary.push_str(&format_change(change));
    }
    if changes.len() > MAX_PATHS {
        write!(summary, "\n  ... and {} more", changes.len() - MAX_PATHS).expect("writing to a String cannot fail");
    }
    summary.push_str("\nRun `git status --short --untracked-files=all` at that repository root for the full status.");
    if let Some(hint) = lfs_filter_hint(&repository, root, &changes) {
        summary.push('\n');
        summary.push_str(&hint);
    }
    Ok(summary)
}

/// Explains changed tracked LFS files that Git cannot compare through the LFS
/// clean filter. A failure to inspect Git LFS only omits the hint, so it never
/// replaces the cleanliness error it accompanies.
fn lfs_filter_hint(
    repository: &Repository,
    worktree: &Path,
    changes: &[nyl_git::WorktreeStatusEntry],
) -> Option<String> {
    let tracked = changes
        .iter()
        .filter(|change| !change.untracked)
        .map(|change| change.path.clone())
        .collect::<Vec<_>>();
    let inspect = || -> nyl_git::Result<Option<(usize, nyl_git::LfsFilterProblem)>> {
        let lfs = nyl_git::lfs_tracked_paths(repository.path(), worktree, &tracked)?;
        if lfs.is_empty() {
            return Ok(None);
        }
        Ok(nyl_git::lfs_filter_problem(repository.path(), worktree)?.map(|problem| (lfs.len(), problem)))
    };
    let (count, problem) = match inspect() {
        Ok(found) => found?,
        Err(error) => {
            tracing::debug!(%error, "Failed to inspect Git LFS for the dirty worktree summary");
            return None;
        }
    };
    Some(lfs_filter_hint_text(count, problem))
}

fn lfs_filter_hint_text(count: usize, problem: nyl_git::LfsFilterProblem) -> String {
    let files = if count == 1 {
        "1 changed file is".to_string()
    } else {
        format!("{count} changed files are")
    };
    let reason = match problem {
        nyl_git::LfsFilterProblem::ExecutableMissing => "Git finds no `git-lfs` executable",
        nyl_git::LfsFilterProblem::FilterNotConfigured => "Git has no `filter.lfs` clean or process filter configured",
    };
    format!(
        "Hint: {files} tracked by Git LFS, but {reason}. Without the LFS clean filter, Git compares a hydrated file \
         with its committed pointer, so an unchanged file can appear modified. Install Git LFS where Nyl runs and run \
         `git lfs install --local --skip-repo` in the checkout. When CI checks the repository out in a separate \
         container, such as GitLab Runner's helper container, the job's own image needs Git LFS too."
    )
}

fn format_change(change: &nyl_git::WorktreeStatusEntry) -> String {
    format!(
        "{}{} {:?}",
        char::from(change.status[0]),
        char::from(change.status[1]),
        change.path.to_string_lossy()
    )
}

fn dirty_worktree_changes(
    repository: &Repository,
    project_root: &Path,
    carried: &[PathBuf],
) -> Result<Vec<nyl_git::WorktreeStatusEntry>> {
    let worktree = repository
        .workdir()
        .ok_or_else(|| NylError::config("Source Git repository has no worktree"))?;
    let statuses = nyl_git::worktree_status(repository.path(), worktree)
        .map_err(|error| NylError::config(format!("Failed to inspect source Git status: {error}")))?;
    let carried = carried.iter().map(|path| canonical_file_path(path)).collect::<Vec<_>>();
    let worktree = canonical_file_path(worktree);
    let mut changes = statuses
        .into_iter()
        .filter(|entry| {
            let carried_file = entry.untracked && carried.contains(&worktree.join(&entry.path));
            !carried_file && !is_project_cache_status(repository, project_root, &entry.path)
        })
        .collect::<Vec<_>>();
    changes.sort_by_cached_key(format_change);
    Ok(changes)
}

/// Canonical form of a possibly missing file: its canonical parent plus name.
fn canonical_file_path(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| {
        match (
            path.parent().and_then(|parent| parent.canonicalize().ok()),
            path.file_name(),
        ) {
            (Some(parent), Some(name)) => parent.join(name),
            _ => path.to_path_buf(),
        }
    })
}

fn is_project_cache_status(repository: &Repository, project_root: &Path, status_path: &Path) -> bool {
    let Some(worktree) = repository.workdir() else {
        return false;
    };
    let Ok(worktree) = worktree.canonicalize() else {
        return false;
    };
    let Ok(project_root) = project_root.canonicalize() else {
        return false;
    };
    let Ok(project_relative) = project_root.strip_prefix(worktree) else {
        return false;
    };
    status_path.starts_with(project_relative.join(".nyl/cache"))
}

/// Digests of every project file a compiled target read, keyed by
/// project-relative path, plus the `@`-prefixed digests of its resolved
/// Release inputs: the `inputs` map of the ownership index.
///
/// Every Cluster document is hashed without its local `live` connection
/// settings, which never affect rendered bytes (the target's Cluster always
/// through its normalized form; another one, such as the control-plane Cluster
/// of the ArgoCDInstance, when it declares `live`).
pub fn hash_inputs(inventory: &GitOpsInventory, compiled: &CompiledTargetTree) -> Result<BTreeMap<String, String>> {
    let cluster_resource = inventory
        .get(GitOpsResourceKind::Cluster, &compiled.cluster.metadata.name)
        .ok_or_else(|| NylError::config("Compiled Cluster is missing from the GitOps inventory"))?;
    let cluster_path = &cluster_resource.source_path;
    let mut render_cluster = compiled.cluster.clone();
    render_cluster.spec.live = None;
    let cluster_bytes = serde_json::to_vec(&render_cluster)?;
    let mut hashes = BTreeMap::new();
    let paths = inventory.paths();
    for relative in &compiled.inputs {
        // `@remote/…` keys name files of remote ApplicationGroup sources,
        // which are not project paths.
        if relative.starts_with("@remote") {
            continue;
        }
        let path = paths.path_for_key(relative);
        if path.is_file() {
            let mut contents = None;
            let mut replace = |raw: &str, replacement: &str, what: &str| -> Result<()> {
                let text = match contents.take() {
                    Some(text) => text,
                    None => std::fs::read_to_string(&path)?,
                };
                if text.matches(raw).count() != 1 {
                    return Err(NylError::config(format!(
                        "Cannot safely locate {what} in {} for provenance hashing",
                        path.display()
                    )));
                }
                contents = Some(text.replacen(raw, replacement, 1));
                Ok(())
            };
            if relative == cluster_path {
                replace(
                    &cluster_resource.raw_document,
                    std::str::from_utf8(&cluster_bytes).expect("JSON serialization is UTF-8"),
                    &format!("Cluster '{}'", compiled.cluster.metadata.name),
                )?;
            }
            // Every other Cluster document in the file that declares `live`
            // (the ArgoCDInstance's control-plane Cluster, for one) is hashed
            // without it too, so moving a kube context never changes the
            // index. A Cluster without `live` keeps its bytes as written.
            for discovered in inventory.resources.values().filter(|discovered| {
                &discovered.source_path == relative && discovered.identity.kind == GitOpsResourceKind::Cluster
            }) {
                if let Some(GitOpsResource::Cluster(cluster)) = &discovered.resource {
                    if cluster.spec.live.is_some() && cluster.metadata.name != compiled.cluster.metadata.name {
                        let mut render_cluster = cluster.clone();
                        render_cluster.spec.live = None;
                        replace(
                            &discovered.raw_document,
                            std::str::from_utf8(&serde_json::to_vec(&render_cluster)?)
                                .expect("JSON serialization is UTF-8"),
                            &format!("Cluster '{}'", cluster.metadata.name),
                        )?;
                    }
                }
            }
            // An ArgoCDInstance that declares `spec.access` is hashed without
            // it: how Nyl reads Argo CD never affects rendered bytes, so
            // editing it must not change the index. An instance without
            // `access` keeps its bytes as written.
            for discovered in inventory
                .resources
                .values()
                .filter(|discovered| &discovered.source_path == relative)
            {
                if let Some(GitOpsResource::ArgoCDInstance(instance)) = &discovered.resource {
                    if instance.spec.access.is_some() {
                        let mut render_instance = instance.clone();
                        render_instance.spec.access = None;
                        replace(
                            &discovered.raw_document,
                            std::str::from_utf8(&serde_json::to_vec(&render_instance)?)
                                .expect("JSON serialization is UTF-8"),
                            &format!("ArgoCDInstance '{}'", instance.metadata.name),
                        )?;
                    }
                }
            }
            let bytes = match contents {
                Some(text) => text.into_bytes(),
                None => std::fs::read(&path)?,
            };
            hashes.insert(
                relative.to_string_lossy().replace('\\', "/"),
                nyl_core::digest::sha256_hex(&bytes),
            );
        }
    }
    super::inputs::merge_input_digests(&mut hashes, &compiled.input_digests)?;
    Ok(hashes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use git2::{IndexAddOption, Signature};
    use tempfile::TempDir;

    #[test]
    fn test_publication_requires_committed_source_even_for_a_clean_snapshot_checkout() {
        let (directory, repository) = committed_worktree();
        let head = repository.head().unwrap().peel_to_commit().unwrap();
        require_publication_source(directory.path(), &head.id().to_string()).unwrap();
        let signature = Signature::now("Test", "test@example.invalid").unwrap();
        let snapshot = repository
            .commit(
                None,
                &signature,
                &signature,
                nyl_core::orchestration::ports::WORKTREE_SNAPSHOT_MARKER,
                &head.tree().unwrap(),
                &[&head],
            )
            .unwrap();
        assert!(require_publication_source(directory.path(), &snapshot.to_string()).is_err());
    }

    fn committed_worktree() -> (TempDir, Repository) {
        let directory = TempDir::new().unwrap();
        let repository = Repository::init(directory.path()).unwrap();
        std::fs::write(directory.path().join("nyl.toml"), "").unwrap();
        let mut index = repository.index().unwrap();
        index.add_all(["*"], IndexAddOption::DEFAULT, None).unwrap();
        index.write().unwrap();
        let tree = repository.find_tree(index.write_tree().unwrap()).unwrap();
        let signature = Signature::now("Test", "test@example.invalid").unwrap();
        repository
            .commit(Some("HEAD"), &signature, &signature, "Initial", &tree, &[])
            .unwrap();
        drop(tree);
        (directory, repository)
    }

    #[test]
    fn test_dirty_worktree_summary_lists_statuses_and_excludes_non_source_files() {
        let (directory, repository) = committed_worktree();
        let root = directory.path();
        let project = root.join("project");
        std::fs::create_dir_all(project.join(".nyl/cache")).unwrap();
        std::fs::write(project.join(".nyl/cache/entry"), "cached").unwrap();
        std::fs::write(project.join("carried.json"), "{}").unwrap();
        std::fs::write(root.join("nyl.toml"), "modified").unwrap();
        std::fs::write(root.join("untracked note.txt"), "untracked").unwrap();
        std::fs::write(root.join("staged.txt"), "staged").unwrap();
        let mut index = repository.index().unwrap();
        index.add_path(Path::new("staged.txt")).unwrap();
        index.write().unwrap();
        let carried = [project.join("carried.json")];

        let changes = dirty_worktree_changes(&repository, &project, &carried)
            .unwrap()
            .iter()
            .map(format_change)
            .collect::<Vec<_>>();
        assert_eq!(
            changes,
            [" M \"nyl.toml\"", "?? \"untracked note.txt\"", "A  \"staged.txt\""]
        );
        let summary = dirty_worktree_summary(&project, &carried).unwrap();
        assert!(summary.contains(&format!("relative to {}", repository.workdir().unwrap().display())));
        assert!(summary.contains("git status --short --untracked-files=all"));
    }

    #[test]
    fn test_dirty_worktree_summary_keeps_lfs_changes_and_explains_a_missing_filter() {
        let (directory, repository) = committed_worktree();
        let root = directory.path();
        std::fs::write(root.join(".gitattributes"), "*.tgz filter=lfs -text\n").unwrap();
        std::fs::write(root.join("chart.tgz"), "pointer").unwrap();
        let mut index = repository.index().unwrap();
        index.add_all(["*"], IndexAddOption::DEFAULT, None).unwrap();
        index.write().unwrap();
        let tree = repository.find_tree(index.write_tree().unwrap()).unwrap();
        let signature = Signature::now("Test", "test@example.invalid").unwrap();
        let head = repository.head().unwrap().peel_to_commit().unwrap();
        repository
            .commit(Some("HEAD"), &signature, &signature, "LFS", &tree, &[&head])
            .unwrap();
        std::fs::write(root.join("chart.tgz"), "hydrated chart").unwrap();
        std::fs::write(root.join("notes.txt"), "untracked").unwrap();

        let summary = dirty_worktree_summary(root, &[]).unwrap();
        assert!(summary.contains(r#" M "chart.tgz""#), "{summary}");
        // The hint depends on whether Git can run the LFS filter here, but it
        // counts only tracked LFS changes and always carries the fix.
        let workdir = repository.workdir().unwrap();
        match nyl_git::lfs_filter_problem(repository.path(), workdir).unwrap() {
            Some(problem) => assert!(summary.ends_with(&lfs_filter_hint_text(1, problem)), "{summary}"),
            None => assert!(!summary.contains("Git LFS"), "{summary}"),
        }
        assert!(lfs_filter_hint_text(1, nyl_git::LfsFilterProblem::ExecutableMissing)
            .contains("git lfs install --local --skip-repo"));
    }

    #[test]
    fn test_dirty_worktree_summary_bounds_and_escapes_paths() {
        let (directory, _) = committed_worktree();
        for number in 0..25 {
            std::fs::write(directory.path().join(format!("file-{number:02}")), "untracked").unwrap();
        }
        let summary = dirty_worktree_summary(directory.path(), &[]).unwrap();
        assert_eq!(summary.lines().filter(|line| line.starts_with("  ?? ")).count(), 20);
        assert!(summary.contains("... and 5 more"));
        #[cfg(unix)]
        {
            std::fs::write(directory.path().join("a\nname.txt"), "untracked").unwrap();
            let summary = dirty_worktree_summary(directory.path(), &[]).unwrap();
            assert!(summary.contains(r#"?? "a\nname.txt""#));
        }
    }

    #[test]
    fn test_worktree_source_state_excludes_carried_files_and_the_project_cache() {
        let (directory, repository) = committed_worktree();
        let root = directory.path();
        let head = repository.head().unwrap().target().unwrap().to_string();

        std::fs::create_dir_all(root.join(".nyl/cache")).unwrap();
        std::fs::write(root.join(".nyl/cache/entry"), "cached").unwrap();
        std::fs::write(root.join("state.json"), "{}").unwrap();
        let carried = [root.join("state.json")];
        assert_eq!(
            worktree_source_state(root, &carried).unwrap(),
            WorktreeSourceState {
                commit: Some(head.clone()),
                dirty: false
            }
        );

        // The same untracked file is a dirty worktree when this run does not carry it.
        assert!(worktree_source_state(root, &[]).unwrap().dirty);
        // A carried path does not excuse a modified tracked file.
        std::fs::write(root.join("nyl.toml"), "[project]\n").unwrap();
        assert!(worktree_source_state(root, &carried).unwrap().dirty);
    }

    #[test]
    fn test_worktree_source_state_checks_the_entire_worktree_from_a_nested_project() {
        let (directory, repository) = committed_worktree();
        let root = directory.path();
        let project = root.join("nested");
        std::fs::create_dir_all(project.join(".nyl/cache")).unwrap();
        std::fs::write(project.join(".nyl/cache/entry"), "cached").unwrap();
        let state = project.join("state file.json");
        std::fs::write(&state, "{}").unwrap();
        let carried = [state];
        assert!(!worktree_source_state(&project, &carried).unwrap().dirty);

        // Only the selected project's cache is excluded.
        std::fs::create_dir_all(root.join(".nyl/cache")).unwrap();
        std::fs::write(root.join(".nyl/cache/entry"), "cached").unwrap();
        assert!(worktree_source_state(&project, &carried).unwrap().dirty);
        std::fs::remove_dir_all(root.join(".nyl")).unwrap();

        std::fs::remove_file(root.join("nyl.toml")).unwrap();
        assert!(worktree_source_state(&project, &carried).unwrap().dirty);
        // Staging the deletion must not hide it either.
        let mut index = repository.index().unwrap();
        index.remove_path(Path::new("nyl.toml")).unwrap();
        index.write().unwrap();
        assert!(worktree_source_state(&project, &carried).unwrap().dirty);
    }
}
