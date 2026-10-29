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
    let index = repository
        .index()
        .map_err(|cause| git_error("cannot read source index", &cause))?;
    let mut files = index
        .iter()
        .map(|entry| String::from_utf8(entry.path).map_err(|cause| error(cause.to_string())))
        .collect::<SourceResult<BTreeSet<_>>>()?;
    let mut options = git2::StatusOptions::new();
    options
        .include_untracked(true)
        .recurse_untracked_dirs(true)
        .include_ignored(false);
    let statuses = repository
        .statuses(Some(&mut options))
        .map_err(|cause| git_error("cannot inspect working files", &cause))?;
    let mut changed = Vec::new();
    for entry in statuses.iter() {
        let path = entry
            .path()
            .map_err(|cause| git_error("invalid working-file path", &cause))?;
        if entry.status().is_wt_new() {
            files.insert(path.to_owned());
        }
        changed.push(path.to_owned());
    }
    Ok((files, changed))
}

/// Whether a source commit declares worktree snapshot provenance.
pub fn is_snapshot(commit: &git2::Commit<'_>) -> bool {
    commit
        .message()
        .is_ok_and(nyl_core::orchestration::ports::is_worktree_snapshot_message)
}

/// Inspect a snapshot input without traversing symlinks in any path component.
/// Missing inputs represent deletions; every existing leaf must be an ordinary file.
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
        if metadata.file_type().is_symlink() {
            return Err(error(format!(
                "worktree snapshots cannot traverse symlinks: {}",
                path.display()
            )));
        }
        if index + 1 == components.len() {
            return if metadata.is_file() {
                Ok(Some(metadata))
            } else {
                Err(error(format!(
                    "worktree snapshots require ordinary files; {relative} is not one"
                )))
            };
        }
        if !metadata.is_dir() {
            return Err(error(format!("snapshot parent {} is not a directory", path.display())));
        }
    }
    Err(error("empty worktree snapshot path"))
}

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
    let (files, changed) = visible_files(repository)?;
    let mut index = Index::new().map_err(|cause| git_error("cannot create snapshot index", &cause))?;
    for relative in files {
        let path = root.join(&relative);
        let Some(metadata) = snapshot_file_metadata(root, &relative)? else {
            continue;
        };
        let bytes = std::fs::read(&path).map_err(|cause| error(format!("cannot read {relative}: {cause}")))?;
        let id = repository
            .blob(&bytes)
            .map_err(|cause| git_error("cannot store snapshot file", &cause))?;
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
                mode,
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
    let base_oid = Oid::from_str(base.as_str()).map_err(|cause| git_error("invalid snapshot base", &cause))?;
    let revision = format!("{WORKTREE_SNAPSHOT_PREFIX}{base}/{tree}");
    let candidates = repository
        .find_reference(&revision)
        .ok()
        .and_then(|reference| reference.target())
        .into_iter()
        .chain(previous.and_then(|commit| Oid::from_str(commit.as_str()).ok()));
    for candidate in candidates {
        if let Ok(commit) = repository.find_commit(candidate) {
            if is_snapshot(&commit) && commit.tree_id() == tree && commit.parent_id(0).ok() == Some(base_oid) {
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
    let parent = repository
        .find_commit(base_oid)
        .map_err(|cause| git_error("cannot open snapshot base", &cause))?;
    let message = format!("nyl: worktree source snapshot\n\n{WORKTREE_SNAPSHOT_MARKER}\nNyl-Worktree-Base: {base}\n\nChanged paths:\n{}\n", changed.join("\n"));
    let commit = repository
        .commit(None, &signature, &signature, &message, &tree, &[&parent])
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
    #[cfg(unix)]
    fn test_snapshot_refuses_git_visible_symlinks() {
        let (root, repository, base) = project();
        std::os::unix::fs::symlink("input", root.path().join("link")).unwrap();
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
