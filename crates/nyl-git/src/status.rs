//! Worktree status through native Git so configured clean filters, including
//! Git LFS, determine whether working files match their committed blobs.

use std::path::{Path, PathBuf};

use crate::error::{GitError, Result};
use crate::transport::base_command;

/// A changed path relative to the worktree root.
#[derive(Debug, PartialEq, Eq)]
pub struct WorktreeStatusEntry {
    /// Git porcelain v1 index and worktree status codes, respectively.
    pub status: [u8; 2],
    pub path: PathBuf,
    /// The path is untracked, rather than a change to a tracked file.
    pub untracked: bool,
}

/// Inspect all tracked and untracked changes, excluding ignored files.
/// Paths are relative to `worktree`, including when `git_dir` belongs to a
/// linked worktree. Git's index and user configuration are left unchanged.
pub fn worktree_status(git_dir: &Path, worktree: &Path) -> Result<Vec<WorktreeStatusEntry>> {
    let output = base_command(git_dir)
        // The process working directory accepts Windows verbatim paths without
        // passing them to Git's path parser.
        .current_dir(worktree)
        .args([
            "--no-optional-locks",
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--ignored=no",
            "--ignore-submodules=none",
            // One NUL-terminated path per entry, including staged renames.
            "--no-renames",
        ])
        .output()
        .map_err(|error| GitError::Command(format!("failed to run git status: {error}")))?;
    if !output.status.success() {
        return Err(GitError::Command(format!(
            "git status failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
        .map(|record| {
            if record.len() < 4 || record[2] != b' ' {
                return Err(GitError::Command("invalid git status --porcelain=v1 -z output".into()));
            }
            #[cfg(unix)]
            let path = {
                use std::os::unix::ffi::OsStrExt;
                PathBuf::from(std::ffi::OsStr::from_bytes(&record[3..]))
            };
            #[cfg(not(unix))]
            let path = PathBuf::from(
                std::str::from_utf8(&record[3..])
                    .map_err(|error| GitError::Command(format!("invalid UTF-8 path in git status: {error}")))?,
            );
            Ok(WorktreeStatusEntry {
                status: [record[0], record[1]],
                path,
                untracked: &record[..2] == b"??",
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use git2::{IndexAddOption, Repository, Signature};
    use std::fs::{File, FileTimes};
    use std::time::{Duration, SystemTime};
    use tempfile::TempDir;

    fn commit(repository: &Repository) {
        let tree = repository
            .find_tree(repository.index().unwrap().write_tree().unwrap())
            .unwrap();
        let signature = Signature::now("Test", "test@example.invalid").unwrap();
        repository
            .commit(Some("HEAD"), &signature, &signature, "Initial", &tree, &[])
            .unwrap();
    }

    fn git(repository: &Repository, root: &Path, args: &[&str]) {
        let output = base_command(repository.path())
            .current_dir(root)
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    }

    #[test]
    fn test_worktree_status_recognizes_hydrated_lfs_content_with_stale_stat_data() {
        let directory = TempDir::new().unwrap();
        let root = directory.path();
        let repository = Repository::init(root).unwrap();
        let mut config = repository.config().unwrap();
        config.set_str("filter.lfs.clean", "git-lfs clean -- %f").unwrap();
        config.set_bool("filter.lfs.required", true).unwrap();
        std::fs::write(root.join(".gitattributes"), "*.tgz filter=lfs -text\n").unwrap();
        let chart = root.join("chart.tgz");
        let content = b"hydrated chart contents\0";
        std::fs::write(&chart, content).unwrap();
        git(&repository, root, &["add", "."]);
        commit(&repository);

        // A timestamp-only change forces Git to compare content through the
        // clean filter instead of trusting its cached index stat data.
        File::options()
            .write(true)
            .open(&chart)
            .unwrap()
            .set_times(FileTimes::new().set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1)))
            .unwrap();
        let index_path = repository.path().join("index");
        let index = std::fs::read(&index_path).unwrap();
        assert!(worktree_status(repository.path(), root).unwrap().is_empty());
        assert_eq!(std::fs::read(index_path).unwrap(), index);

        // Equal sizes still require a content comparison.
        std::fs::write(&chart, vec![b'x'; content.len()]).unwrap();
        assert_eq!(
            worktree_status(repository.path(), root).unwrap(),
            vec![WorktreeStatusEntry {
                status: *b" M",
                path: "chart.tgz".into(),
                untracked: false,
            }]
        );
        git(&repository, root, &["add", "chart.tgz"]);
        assert!(!worktree_status(repository.path(), root).unwrap().is_empty());
        std::fs::remove_file(chart).unwrap();
        assert!(!worktree_status(repository.path(), root).unwrap().is_empty());
    }

    #[test]
    fn test_worktree_status_keeps_untracked_paths_and_both_sides_of_staged_renames() {
        let directory = TempDir::new().unwrap();
        let root = directory.path();
        let repository = Repository::init(root).unwrap();
        std::fs::write(root.join("tracked"), "contents").unwrap();
        std::fs::write(root.join(".gitignore"), "ignored/\n").unwrap();
        let mut index = repository.index().unwrap();
        index.add_all(["*"], IndexAddOption::DEFAULT, None).unwrap();
        index.write().unwrap();
        commit(&repository);
        git(&repository, root, &["mv", "tracked", "renamed"]);
        std::fs::create_dir(root.join("untracked")).unwrap();
        std::fs::write(root.join("untracked/state file"), "{}").unwrap();
        std::fs::create_dir(root.join("ignored")).unwrap();
        std::fs::write(root.join("ignored/file"), "ignored").unwrap();
        // User-facing status settings must not hide changes from provenance.
        repository.config().unwrap().set_bool("status.renames", true).unwrap();
        repository
            .config()
            .unwrap()
            .set_str("status.showUntrackedFiles", "no")
            .unwrap();
        assert_eq!(
            worktree_status(repository.path(), root).unwrap(),
            vec![
                WorktreeStatusEntry {
                    status: *b"A ",
                    path: "renamed".into(),
                    untracked: false,
                },
                WorktreeStatusEntry {
                    status: *b"D ",
                    path: "tracked".into(),
                    untracked: false,
                },
                WorktreeStatusEntry {
                    status: *b"??",
                    path: "untracked/state file".into(),
                    untracked: true,
                },
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_worktree_status_preserves_raw_paths_and_newlines() {
        use std::os::unix::ffi::OsStrExt;

        let directory = TempDir::new().unwrap();
        let root = directory.path();
        let repository = Repository::init(root).unwrap();
        let names: &[&[u8]] = &[
            b"state file\n.json",
            // macOS filesystems require valid UTF-8 filenames.
            #[cfg(not(target_os = "macos"))]
            b"state\xff.json",
        ];
        for name in names {
            let path = PathBuf::from(std::ffi::OsStr::from_bytes(name));
            std::fs::write(root.join(&path), "{}").unwrap();
            assert_eq!(
                worktree_status(repository.path(), root).unwrap(),
                vec![WorktreeStatusEntry {
                    status: *b"??",
                    path: path.clone(),
                    untracked: true
                }]
            );
            std::fs::remove_file(root.join(path)).unwrap();
        }
    }

    #[test]
    fn test_worktree_status_reports_filter_failures() {
        let directory = TempDir::new().unwrap();
        let root = directory.path();
        let repository = Repository::init(root).unwrap();
        std::fs::write(root.join("tracked"), "contents").unwrap();
        let mut index = repository.index().unwrap();
        index.add_all(["*"], IndexAddOption::DEFAULT, None).unwrap();
        index.write().unwrap();
        commit(&repository);
        std::fs::write(root.join(".gitattributes"), "tracked filter=broken\n").unwrap();
        repository
            .config()
            .unwrap()
            .set_str("filter.broken.clean", "false")
            .unwrap();
        repository
            .config()
            .unwrap()
            .set_bool("filter.broken.required", true)
            .unwrap();
        std::fs::write(root.join("tracked"), "modified").unwrap();
        let error = worktree_status(repository.path(), root).unwrap_err();
        assert!(error.to_string().contains("git status failed"), "{error}");
    }

    #[test]
    fn test_worktree_status_inspects_a_linked_worktree() {
        let directory = TempDir::new().unwrap();
        let root = directory.path();
        let repository = Repository::init(root).unwrap();
        commit(&repository);
        let linked = root.join("linked");
        git(&repository, root, &["worktree", "add", "--detach", "linked"]);
        let linked_repository = Repository::open(&linked).unwrap();
        assert!(worktree_status(linked_repository.path(), &linked).unwrap().is_empty());
        std::fs::write(linked.join("input"), "contents").unwrap();
        assert_eq!(
            worktree_status(linked_repository.path(), &linked).unwrap(),
            vec![WorktreeStatusEntry {
                status: *b"??",
                path: "input".into(),
                untracked: true,
            }]
        );
    }
}
