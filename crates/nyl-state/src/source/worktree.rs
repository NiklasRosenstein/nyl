//! Immutable working-file inputs for explicit local development.
//!
//! Contract: [Source](../../../../design/orchestration-core.md#source).
//! Git-visible files are read once into an independent index. HEAD and the
//! operator's index stay untouched; state and run commits retain the snapshot
//! as an additional parent before effects can start.

use std::collections::BTreeSet;

use git2::{Index, IndexEntry, IndexTime, Oid, Repository, Signature};
use nyl_core::orchestration::ids::CommitId;
use nyl_core::orchestration::ports::{
    CommitIdentity, SourceResult, WorktreeSnapshot, WORKTREE_SNAPSHOT_MARKER, WORKTREE_SNAPSHOT_PREFIX,
};
use nyl_core::orchestration::time::Timestamp;

use super::{error, git_error};

/// Git-visible tracked and untracked paths, and paths whose working bytes changed.
/// Deleted files remain candidates so callers can omit them from the snapshot.
pub fn visible_files(repository: &Repository) -> SourceResult<(BTreeSet<String>, Vec<String>)> {
    let (files, changed, _) = visible_files_status(repository)?;
    Ok((files, changed))
}

fn visible_files_status(
    repository: &Repository,
) -> SourceResult<(BTreeSet<String>, Vec<String>, BTreeSet<std::path::PathBuf>)> {
    let index = repository
        .index()
        .map_err(|cause| git_error("cannot read source index", &cause))?;
    let mut files = index
        .iter()
        .map(|entry| String::from_utf8(entry.path).map_err(|cause| error(cause.to_string())))
        .collect::<SourceResult<BTreeSet<_>>>()?;
    let root = repository
        .workdir()
        .ok_or_else(|| error("worktree snapshots require a working checkout"))?;
    let statuses = nyl_git::worktree_status(repository.path(), root)
        .map_err(|cause| error(format!("cannot inspect working files: {cause}")))?;
    let mut changed = Vec::new();
    let mut working_changes = BTreeSet::new();
    for entry in statuses {
        let path = entry
            .path
            .to_str()
            .ok_or_else(|| error("non-UTF-8 working-file path"))?;
        if entry.untracked {
            files.insert(path.to_owned());
        }
        changed.push(path.to_owned());
        if entry.untracked || entry.status[1] != b' ' {
            working_changes.insert(entry.path);
        }
    }
    Ok((files, changed, working_changes))
}

/// Whether a worktree path is consumed by a declared path or glob.
/// A directory matches when a glob can traverse it to reach an input.
pub fn source_path_matches(path: &str, relative: &str, recursive: bool, directory: bool) -> SourceResult<bool> {
    let path = path.trim_matches('/');
    if path.contains(['*', '?', '[']) {
        let matches = |pattern: &str, value: &str| -> SourceResult<bool> {
            Ok(glob::Pattern::new(pattern)
                .map_err(|cause| error(format!("invalid source path {path}: {cause}")))?
                .matches_with(
                    value,
                    glob::MatchOptions {
                        require_literal_separator: true,
                        ..Default::default()
                    },
                ))
        };
        if !directory {
            return matches(path, relative);
        }
        for (index, _) in path.match_indices('/') {
            if matches(&path[..index], relative)? {
                return Ok(true);
            }
        }
        return if path.ends_with("**") {
            matches(path, &format!("{relative}/input"))
        } else {
            Ok(false)
        };
    }
    Ok((recursive && path.is_empty())
        || relative == path
        || (recursive && relative.starts_with(&format!("{path}/")))
        || (directory && path.starts_with(&format!("{relative}/"))))
}

/// Whether bytes are a Git LFS pointer rather than hydrated file content.
pub fn is_lfs_pointer(bytes: &[u8]) -> bool {
    bytes
        .strip_prefix(b"version https://git-lfs.github.com/spec/v1")
        .is_some_and(|tail| tail.starts_with(b"\n") || tail.starts_with(b"\r\n"))
}

/// Whether a source commit declares worktree snapshot provenance.
pub fn is_snapshot(commit: &git2::Commit<'_>) -> bool {
    commit
        .message()
        .is_ok_and(nyl_core::orchestration::ports::is_worktree_snapshot_message)
}

/// Inspect a snapshot input without traversing symlinks in any path component.
/// Missing inputs represent deletions; leaves may be files or symbolic links.
pub fn snapshot_file_metadata(root: &std::path::Path, relative: &str) -> SourceResult<Option<std::fs::Metadata>> {
    use std::path::Component;
    let mut path = root.to_path_buf();
    let components = std::path::Path::new(relative).components().collect::<Vec<_>>();
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(name) = component else {
            return Err(error("unsafe worktree snapshot path"));
        };
        if *name == ".git" {
            return Err(error("unsafe worktree snapshot path"));
        }
        path.push(name);
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(cause) if cause.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(cause) => return Err(error(format!("cannot inspect {relative}: {cause}"))),
        };
        if metadata.file_type().is_symlink() && index + 1 != components.len() {
            return Err(error(format!(
                "worktree snapshots cannot traverse symlinks: {}",
                path.display()
            )));
        }
        if index + 1 == components.len() {
            return if metadata.is_file() || metadata.file_type().is_symlink() {
                Ok(Some(metadata))
            } else {
                Err(error(format!(
                    "worktree snapshot input {relative} is neither a file nor a symbolic link"
                )))
            };
        }
        if !metadata.is_dir() {
            return Err(error(format!("snapshot parent {} is not a directory", path.display())));
        }
    }
    Err(error("empty worktree snapshot path"))
}

/// Read file bytes or a symbolic link's target without following it.
/// Link targets must stay within the worktree, including dangling targets.
pub fn snapshot_file_contents(
    root: &std::path::Path,
    relative: &str,
    metadata: &std::fs::Metadata,
) -> SourceResult<Vec<u8>> {
    let path = root.join(relative);
    if metadata.file_type().is_symlink() {
        let target =
            std::fs::read_link(&path).map_err(|cause| error(format!("cannot read link {relative}: {cause}")))?;
        let target = target
            .to_str()
            .ok_or_else(|| error(format!("symbolic link {relative} has a non-UTF-8 target")))?;
        let directory = relative.rsplit_once('/').map_or("", |(parent, _)| parent);
        if super::escapes(directory, target) {
            return Err(error(format!(
                "symbolic link {relative} points outside the tree: {target:?}"
            )));
        }
        Ok(target.as_bytes().to_vec())
    } else {
        std::fs::read(path).map_err(|cause| error(format!("cannot read {relative}: {cause}")))
    }
}

#[allow(clippy::too_many_lines)] // Capture owns one immutable tree and its retained commit.
pub(super) fn capture(
    repository: &Repository,
    base: &CommitId,
    identity: &CommitIdentity,
    now: Timestamp,
    previous: Option<&CommitId>,
) -> SourceResult<WorktreeSnapshot> {
    let root = repository
        .workdir()
        .ok_or_else(|| error("worktree snapshots require a working checkout"))?;
    let (files, changed, working_changes) = visible_files_status(repository)?;
    let source_index = repository
        .index()
        .map_err(|cause| git_error("cannot read source index", &cause))?;
    let attributes_changed = changed
        .iter()
        .any(|path| path == ".gitattributes" || path.ends_with("/.gitattributes"));
    let mut index = Index::new().map_err(|cause| git_error("cannot create snapshot index", &cause))?;
    for relative in files {
        let original = source_index.get_path(std::path::Path::new(&relative), 0);
        if let Some(entry) = original.as_ref().filter(|entry| entry.mode == 0o160_000) {
            index
                .add(entry)
                .map_err(|cause| git_error("cannot index submodule", &cause))?;
            continue;
        }
        let Some(metadata) = snapshot_file_metadata(root, &relative)? else {
            continue;
        };
        let id = if !attributes_changed
            && !working_changes.contains(std::path::Path::new(&relative))
            && !metadata.file_type().is_symlink()
        {
            original.as_ref().map(|entry| entry.id)
        } else {
            None
        };
        let id = match id {
            Some(id) => id,
            None if metadata.file_type().is_symlink() => {
                let bytes = snapshot_file_contents(root, &relative, &metadata)?;
                repository
                    .blob(&bytes)
                    .map_err(|cause| git_error("cannot store snapshot link", &cause))?
            }
            None => {
                let output = nyl_git::repository_command(repository.path())
                    .current_dir(root)
                    .args(["hash-object", "-w", "--path", &relative, "--", &relative])
                    .output()
                    .map_err(|cause| error(format!("cannot run Git clean filters for {relative}: {cause}")))?;
                if !output.status.success() {
                    return Err(error(format!(
                        "cannot clean {relative}: {}",
                        String::from_utf8_lossy(&output.stderr).trim()
                    )));
                }
                Oid::from_str(String::from_utf8_lossy(&output.stdout).trim())
                    .map_err(|cause| git_error("invalid cleaned blob", &cause))?
            }
        };
        #[cfg(unix)]
        let mode = {
            use std::os::unix::fs::PermissionsExt as _;
            if metadata.permissions().mode() & 0o111 != 0 {
                0o100_755
            } else {
                0o100_644
            }
        };
        #[cfg(not(unix))]
        let mode = repository
            .index()
            .ok()
            .and_then(|index| index.get_path(std::path::Path::new(&relative), 0))
            .map_or(0o100_644, |entry| entry.mode);
        index
            .add(&IndexEntry {
                ctime: IndexTime::new(0, 0),
                mtime: IndexTime::new(0, 0),
                dev: 0,
                ino: 0,
                mode: if metadata.file_type().is_symlink() {
                    0o120_000
                } else {
                    mode
                },
                uid: 0,
                gid: 0,
                file_size: u32::try_from(metadata.len()).unwrap_or(u32::MAX),
                id,
                flags: 0,
                flags_extended: 0,
                path: relative.into_bytes(),
            })
            .map_err(|cause| git_error("cannot index snapshot file", &cause))?;
    }
    let tree = index
        .write_tree_to(repository)
        .map_err(|cause| git_error("cannot store snapshot tree", &cause))?;
    let revision = format!("{WORKTREE_SNAPSHOT_PREFIX}{base}/{tree}");
    let candidates = repository
        .find_reference(&revision)
        .ok()
        .and_then(|reference| reference.target())
        .into_iter()
        .chain(previous.and_then(|commit| Oid::from_str(commit.as_str()).ok()));
    for candidate in candidates {
        if let Ok(commit) = repository.find_commit(candidate) {
            if is_snapshot(&commit)
                && commit.tree_id() == tree
                && commit.parent_count() == 0
                && commit
                    .message()
                    .is_ok_and(|message| message.lines().any(|line| line == format!("Nyl-Worktree-Base: {base}")))
            {
                repository
                    .reference(&revision, candidate, true, "retain worktree snapshot")
                    .map_err(|cause| git_error("cannot retain snapshot", &cause))?;
                return Ok(WorktreeSnapshot {
                    base: base.clone(),
                    commit: CommitId::new(candidate.to_string()).map_err(|cause| error(cause.to_string()))?,
                    revision,
                });
            }
        }
    }
    let signature = Signature::new(&identity.name, &identity.email, &git2::Time::new(now.timestamp(), 0))
        .map_err(|cause| git_error("invalid snapshot identity", &cause))?;
    let tree = repository
        .find_tree(tree)
        .map_err(|cause| git_error("cannot open snapshot tree", &cause))?;
    let message = format!("nyl: worktree source snapshot\n\n{WORKTREE_SNAPSHOT_MARKER}\nNyl-Worktree-Base: {base}\n\nChanged paths:\n{}\n", changed.join("\n"));
    let commit = repository
        .commit(None, &signature, &signature, &message, &tree, &[])
        .map_err(|cause| git_error("cannot commit worktree snapshot", &cause))?;
    repository
        .reference(&revision, commit, true, "retain worktree snapshot")
        .map_err(|cause| git_error("cannot retain snapshot", &cause))?;
    Ok(WorktreeSnapshot {
        base: base.clone(),
        commit: CommitId::new(commit.to_string()).map_err(|cause| error(cause.to_string()))?,
        revision,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use nyl_core::orchestration::ports::{SourceRepository, SourceResolver};
    use tempfile::TempDir;

    fn project() -> (TempDir, Repository, CommitId) {
        let root = TempDir::new().unwrap();
        let repository = Repository::init(root.path()).unwrap();
        for (path, bytes) in [
            ("input", "committed"),
            ("deleted", "remove"),
            ("untracked-later", "committed"),
            (".gitignore", "ignored\n"),
        ] {
            std::fs::write(root.path().join(path), bytes).unwrap();
        }
        let mut index = repository.index().unwrap();
        index.add_all(["*"], git2::IndexAddOption::DEFAULT, None).unwrap();
        index.write().unwrap();
        let tree = index.write_tree().unwrap();
        let signature = Signature::now("Test", "test@example.invalid").unwrap();
        let commit = repository
            .commit(
                Some("HEAD"),
                &signature,
                &signature,
                "source",
                &repository.find_tree(tree).unwrap(),
                &[],
            )
            .unwrap();
        (root, repository, CommitId::new(commit.to_string()).unwrap())
    }

    #[test]
    fn test_snapshot_freezes_working_bytes_and_modes_without_changing_head_or_index() {
        let (root, repository, base) = project();
        std::fs::write(root.path().join("input"), "staged").unwrap();
        let mut index = repository.index().unwrap();
        index.add_path(std::path::Path::new("input")).unwrap();
        index.remove_path(std::path::Path::new("untracked-later")).unwrap();
        index.write().unwrap();
        let original_index = std::fs::read(repository.path().join("index")).unwrap();
        std::fs::write(root.path().join("input"), "working").unwrap();
        std::fs::remove_file(root.path().join("deleted")).unwrap();
        std::fs::write(root.path().join("new"), "untracked").unwrap();
        std::fs::write(root.path().join("ignored"), "private").unwrap();
        std::fs::write(root.path().join(".gitignore"), "ignored\nuntracked-later\n").unwrap();
        std::fs::write(root.path().join("untracked-later"), "private working bytes").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(root.path().join("new"), std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let identity = "Test <test@example.invalid>".parse().unwrap();
        let now: Timestamp = "2026-10-28T12:00:00Z".parse().unwrap();
        let snapshot = capture(&repository, &base, &identity, now, None).unwrap();
        let commit = repository
            .find_commit(Oid::from_str(snapshot.commit.as_str()).unwrap())
            .unwrap();
        let tree = commit.tree().unwrap();
        let input = tree.get_name("input").unwrap().id();
        assert_eq!(repository.find_blob(input).unwrap().content(), b"working");
        assert!(tree.get_name("new").is_some());
        assert!(tree.get_name("ignored").is_none());
        assert!(tree.get_name("untracked-later").is_none());
        assert!(tree.get_name("deleted").is_none());
        #[cfg(unix)]
        assert_eq!(tree.get_name("new").unwrap().filemode(), 0o100_755);
        assert_eq!(repository.head().unwrap().target().unwrap().to_string(), base.as_str());
        assert_eq!(std::fs::read(repository.path().join("index")).unwrap(), original_index);
        assert!(is_snapshot(&commit));
        assert_eq!(commit.parent_count(), 0);
        assert!(commit
            .message()
            .unwrap()
            .contains(&format!("Nyl-Worktree-Base: {base}")));
        assert_eq!(
            capture(&repository, &base, &identity, now + chrono::Duration::hours(1), None).unwrap(),
            snapshot
        );
        repository.find_reference(&snapshot.revision).unwrap().delete().unwrap();
        assert_eq!(
            capture(
                &repository,
                &base,
                &identity,
                now + chrono::Duration::hours(2),
                Some(&snapshot.commit)
            )
            .unwrap(),
            snapshot
        );
        std::fs::write(root.path().join("input"), "later").unwrap();
        assert_eq!(repository.find_blob(input).unwrap().content(), b"working");
        assert_ne!(
            capture(&repository, &base, &identity, now, None).unwrap().commit,
            snapshot.commit
        );
    }

    #[test]
    fn test_source_globs_detect_submodules_they_can_traverse() {
        assert!(source_path_matches("/nyl/*/entry.sh", "nyl/module", false, true).unwrap());
        assert!(!source_path_matches("/nyl/*/entry.sh", "other/module", false, true).unwrap());
        assert!(!source_path_matches("/nyl/*", "nyl/module", false, true).unwrap());
        assert!(source_path_matches("/nyl/**", "nyl/module", false, true).unwrap());
        assert!(source_path_matches("/nyl/*/entry.sh", "nyl/module/entry.sh", false, false).unwrap());
        assert!(!source_path_matches("/nyl/*/entry.sh", "nyl/module/other", false, false).unwrap());
    }

    #[test]
    fn test_snapshot_retains_gitlink_and_uses_git_clean_filter() {
        let (root, repository, base) = project();
        repository
            .config()
            .unwrap()
            .set_str("filter.upper.clean", "tr a-z A-Z")
            .unwrap();
        std::fs::write(root.path().join(".gitattributes"), "input filter=upper\n").unwrap();
        std::fs::write(root.path().join("input"), "new content").unwrap();
        let mut index = repository.index().unwrap();
        let mut entry = index.get_path(std::path::Path::new("deleted"), 0).unwrap();
        entry.path = b"submodule".to_vec();
        entry.mode = 0o160_000;
        entry.id = Oid::from_str(base.as_str()).unwrap();
        index.add(&entry).unwrap();
        index.write().unwrap();
        std::fs::create_dir(root.path().join("submodule")).unwrap();
        let snapshot = capture(
            &repository,
            &base,
            &"Test <test@example.invalid>".parse().unwrap(),
            "2026-10-28T12:00:00Z".parse().unwrap(),
            None,
        )
        .unwrap();
        let commit = repository
            .find_commit(Oid::from_str(snapshot.commit.as_str()).unwrap())
            .unwrap();
        let tree = commit.tree().unwrap();
        let submodule = tree.get_name("submodule").unwrap();
        assert_eq!(submodule.filemode(), 0o160_000);
        assert_eq!(submodule.id(), Oid::from_str(base.as_str()).unwrap());
        assert_eq!(
            repository
                .find_blob(tree.get_name("input").unwrap().id())
                .unwrap()
                .content(),
            b"NEW CONTENT"
        );
        let resolver = crate::source::GitSourceResolver::new(repository.path(), nyl_git::GitAuth::default());
        assert!(resolver
            .validate_source_path(&SourceRepository::This, &snapshot.commit, "/input", true)
            .is_ok());
        let error = resolver
            .validate_source_path(&SourceRepository::This, &snapshot.commit, "/submodule", true)
            .unwrap_err();
        assert!(error.to_string().contains("requires submodule submodule"));
    }

    #[test]
    fn test_source_path_rejects_required_lfs_pointer() {
        assert!(is_lfs_pointer(
            b"version https://git-lfs.github.com/spec/v1\r\noid sha256:123\r\n"
        ));
        let (root, repository, base) = project();
        std::fs::write(
            root.path().join("asset"),
            "version https://git-lfs.github.com/spec/v1\noid sha256:123\nsize 1\n",
        )
        .unwrap();
        let snapshot = capture(
            &repository,
            &base,
            &"Test <test@example.invalid>".parse().unwrap(),
            "2026-10-28T12:00:00Z".parse().unwrap(),
            None,
        )
        .unwrap();
        let resolver = crate::source::GitSourceResolver::new(repository.path(), nyl_git::GitAuth::default());
        assert!(resolver
            .validate_source_path(&SourceRepository::This, &snapshot.commit, "/input", true)
            .is_ok());
        assert!(resolver
            .validate_source_path(&SourceRepository::This, &snapshot.commit, "/asset", true)
            .unwrap_err()
            .to_string()
            .contains("requires Git LFS file asset"));
    }

    #[test]
    #[cfg(unix)]
    fn test_snapshot_preserves_internal_and_dangling_links_without_reading_targets() {
        let (root, repository, base) = project();
        std::os::unix::fs::symlink("input", root.path().join("link")).unwrap();
        std::fs::create_dir(root.path().join(".github")).unwrap();
        std::os::unix::fs::symlink("../CLAUDE.md", root.path().join(".github/copilot-instructions.md")).unwrap();
        let snapshot = capture(
            &repository,
            &base,
            &"Test <test@example.invalid>".parse().unwrap(),
            "2026-10-28T12:00:00Z".parse().unwrap(),
            None,
        )
        .unwrap();
        let commit = repository
            .find_commit(Oid::from_str(snapshot.commit.as_str()).unwrap())
            .unwrap();
        let tree = commit.tree().unwrap();
        for (name, target) in [("link", "input"), (".github/copilot-instructions.md", "../CLAUDE.md")] {
            let entry = tree.get_path(std::path::Path::new(name)).unwrap();
            assert_eq!(entry.filemode(), 0o120_000);
            assert_eq!(repository.find_blob(entry.id()).unwrap().content(), target.as_bytes());
        }
        std::os::unix::fs::symlink("../outside", root.path().join("escape")).unwrap();
        assert!(capture(
            &repository,
            &base,
            &"Test <test@example.invalid>".parse().unwrap(),
            "2026-10-28T12:00:00Z".parse().unwrap(),
            None
        )
        .is_err());
    }

    #[test]
    #[cfg(unix)]
    fn test_snapshot_refuses_an_ignored_symlink_parent_of_a_tracked_file() {
        let (root, repository, base) = project();
        std::fs::create_dir(root.path().join("context")).unwrap();
        std::fs::write(root.path().join("context/input"), "tracked").unwrap();
        let mut index = repository.index().unwrap();
        index.add_path(std::path::Path::new("context/input")).unwrap();
        index.write().unwrap();
        std::fs::write(root.path().join(".gitignore"), "context\n").unwrap();
        let outside = TempDir::new().unwrap();
        std::fs::write(outside.path().join("input"), "outside").unwrap();
        std::fs::remove_dir_all(root.path().join("context")).unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("context")).unwrap();
        assert!(capture(
            &repository,
            &base,
            &"Test <test@example.invalid>".parse().unwrap(),
            "2026-10-28T12:00:00Z".parse().unwrap(),
            None
        )
        .is_err());
    }
}
