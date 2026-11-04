//! Provenance of a rendered tree: the source commit it is attributed to and
//! the input digests its ownership index records.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use git2::Repository;

use super::{CompiledTargetTree, GitOpsInventory};
use crate::resources::GitOpsResourceKind;
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
pub fn dirty_worktree_summary(project_root: &Path, carried: &[PathBuf]) -> Result<String> {
    const MAX_PATHS: usize = 20;

    let repository = Repository::discover(project_root)
        .map_err(|error| NylError::config(format!("Failed to inspect source Git repository: {error}")))?;
    let changes = dirty_worktree_changes(&repository, project_root, carried)?;
    let root = repository.workdir().unwrap_or(project_root);
    let mut summary = format!("Dirty files (relative to {}):", root.display());
    for change in changes.iter().take(MAX_PATHS) {
        summary.push_str("\n  ");
        summary.push_str(change);
    }
    if changes.len() > MAX_PATHS {
        write!(summary, "\n  ... and {} more", changes.len() - MAX_PATHS).expect("writing to a String cannot fail");
    }
    summary.push_str("\nRun `git status --short --untracked-files=all` at that repository root for the full status.");
    Ok(summary)
}

fn dirty_worktree_changes(repository: &Repository, project_root: &Path, carried: &[PathBuf]) -> Result<Vec<String>> {
    let worktree = repository
        .workdir()
        .ok_or_else(|| NylError::config("Source Git repository has no worktree"))?;
    let statuses = nyl_git::worktree_status(repository.path(), worktree)
        .map_err(|error| NylError::config(format!("Failed to inspect source Git status: {error}")))?;
    let carried = carried.iter().map(|path| canonical_file_path(path)).collect::<Vec<_>>();
    let worktree = canonical_file_path(worktree);
    let mut changes = Vec::new();
    for entry in &statuses {
        let carried_file = entry.untracked && carried.contains(&worktree.join(&entry.path));
        if carried_file || is_project_cache_status(repository, project_root, &entry.path) {
            continue;
        }
        changes.push(format!(
            "{}{} {:?}",
            char::from(entry.status[0]),
            char::from(entry.status[1]),
            entry.path.to_string_lossy()
        ));
    }
    changes.sort();
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
/// The Cluster document is hashed without its local `live` connection
/// settings, which never affect rendered bytes.
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
            let bytes = if relative == cluster_path {
                let contents = std::fs::read_to_string(&path)?;
                if contents.matches(&cluster_resource.raw_document).count() != 1 {
                    return Err(NylError::config(format!(
                        "Cannot safely locate Cluster '{}' in {} for provenance hashing",
                        compiled.cluster.metadata.name,
                        path.display()
                    )));
                }
                contents
                    .replacen(
                        &cluster_resource.raw_document,
                        std::str::from_utf8(&cluster_bytes).expect("JSON serialization is UTF-8"),
                        1,
                    )
                    .into_bytes()
            } else {
                std::fs::read(path)?
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

        let changes = dirty_worktree_changes(&repository, &project, &carried).unwrap();
        assert_eq!(
            changes,
            [" M \"nyl.toml\"", "?? \"untracked note.txt\"", "A  \"staged.txt\""]
        );
        let summary = dirty_worktree_summary(&project, &carried).unwrap();
        assert!(summary.contains(&format!("relative to {}", repository.workdir().unwrap().display())));
        assert!(summary.contains("git status --short --untracked-files=all"));
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
