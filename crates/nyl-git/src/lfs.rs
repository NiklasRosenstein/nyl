//! Git LFS diagnostics for worktree status.
//!
//! Git compares a file tracked by Git LFS with its committed pointer through
//! the `lfs` clean filter. Without the `git-lfs` executable or the filter
//! configuration, Git reads a hydrated file as its full content, which differs
//! from the pointer once Git looks past its cached stat data, so an unchanged
//! file appears modified. These functions explain such a status; they never
//! change what counts as a change.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use crate::error::{GitError, Result};
use crate::transport::repository_command;

/// Why Git cannot run the Git LFS clean filter in a repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LfsFilterProblem {
    /// Git finds no `git-lfs` executable.
    ExecutableMissing,
    /// Neither `filter.lfs.process` nor `filter.lfs.clean` is configured.
    FilterNotConfigured,
}

impl LfsFilterProblem {
    /// The problem, if any, given what the repository provides. A missing
    /// executable is reported first: configuring the filter alone cannot fix it.
    pub fn from_probe(executable_available: bool, filter_configured: bool) -> Option<Self> {
        if !executable_available {
            Some(Self::ExecutableMissing)
        } else if !filter_configured {
            Some(Self::FilterNotConfigured)
        } else {
            None
        }
    }
}

/// The paths among `paths`, relative to `worktree`, that Git assigns the
/// `lfs` filter through its attributes.
pub fn lfs_tracked_paths(git_dir: &Path, worktree: &Path, paths: &[PathBuf]) -> Result<Vec<PathBuf>> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let mut child = repository_command(git_dir)
        .current_dir(worktree)
        .args(["check-attr", "--stdin", "-z", "filter"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| GitError::Command(format!("failed to run git check-attr: {error}")))?;
    let mut input = Vec::new();
    for path in paths {
        input.extend_from_slice(&path_bytes(path)?);
        input.push(0);
    }
    // Git reads all of stdin before writing, so the input cannot block on output.
    child
        .stdin
        .take()
        .expect("stdin is piped")
        .write_all(&input)
        .map_err(|error| GitError::Command(format!("failed to write to git check-attr: {error}")))?;
    let output = child
        .wait_with_output()
        .map_err(|error| GitError::Command(format!("failed to run git check-attr: {error}")))?;
    if !output.status.success() {
        return Err(GitError::Command(format!(
            "git check-attr failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    // `-z` output is a sequence of NUL-terminated path, attribute, value triples.
    let fields = output.stdout.split(|byte| *byte == 0).collect::<Vec<_>>();
    let mut tracked = Vec::new();
    for triple in fields.as_chunks::<3>().0 {
        if triple[2] == b"lfs" {
            tracked.push(bytes_path(triple[0])?);
        }
    }
    Ok(tracked)
}

/// Why the repository at `git_dir` cannot run the Git LFS clean filter, read
/// from the configuration and `PATH` that Git itself sees.
pub fn lfs_filter_problem(git_dir: &Path, worktree: &Path) -> Result<Option<LfsFilterProblem>> {
    let executable_available = repository_command(git_dir)
        .current_dir(worktree)
        .args(["lfs", "version"])
        .stdin(Stdio::null())
        .output()
        .map_err(|error| GitError::Command(format!("failed to run git lfs version: {error}")))?
        .status
        .success();
    Ok(LfsFilterProblem::from_probe(
        executable_available,
        lfs_filter_configured(git_dir, worktree)?,
    ))
}

/// Whether the repository's effective Git configuration defines the `lfs`
/// clean or process filter.
fn lfs_filter_configured(git_dir: &Path, worktree: &Path) -> Result<bool> {
    let output = repository_command(git_dir)
        .current_dir(worktree)
        .args(["config", "--get-regexp", r"^filter\.lfs\.(clean|process)$"])
        .output()
        .map_err(|error| GitError::Command(format!("failed to run git config: {error}")))?;
    match output.status.code() {
        Some(0) => Ok(true),
        // `--get-regexp` exits with 1 when no key matches.
        Some(1) => Ok(false),
        _ => Err(GitError::Command(format!(
            "git config failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))),
    }
}

#[cfg(unix)]
#[allow(clippy::unnecessary_wraps)] // Matches the fallible non-Unix variant.
fn path_bytes(path: &Path) -> Result<Vec<u8>> {
    use std::os::unix::ffi::OsStrExt;
    Ok(path.as_os_str().as_bytes().to_vec())
}

#[cfg(not(unix))]
fn path_bytes(path: &Path) -> Result<Vec<u8>> {
    path.to_str()
        .map(|path| path.replace('\\', "/").into_bytes())
        .ok_or_else(|| GitError::Command(format!("path is not valid UTF-8: {}", path.display())))
}

#[cfg(unix)]
#[allow(clippy::unnecessary_wraps)] // Matches the fallible non-Unix variant.
fn bytes_path(bytes: &[u8]) -> Result<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    Ok(PathBuf::from(std::ffi::OsStr::from_bytes(bytes)))
}

#[cfg(not(unix))]
fn bytes_path(bytes: &[u8]) -> Result<PathBuf> {
    std::str::from_utf8(bytes)
        .map(PathBuf::from)
        .map_err(|error| GitError::Command(format!("invalid UTF-8 path in git check-attr: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use git2::Repository;
    use tempfile::TempDir;

    #[test]
    fn test_lfs_tracked_paths_follows_gitattributes() {
        let directory = TempDir::new().unwrap();
        let root = directory.path();
        let repository = Repository::init(root).unwrap();
        std::fs::write(
            root.join(".gitattributes"),
            "*.tgz filter=lfs diff=lfs merge=lfs -text\n",
        )
        .unwrap();
        std::fs::create_dir(root.join("vendor")).unwrap();
        let paths = [
            PathBuf::from("vendor/chart.tgz"),
            PathBuf::from("nyl.toml"),
            PathBuf::from("vendor/a chart.tgz"),
        ];

        assert_eq!(
            lfs_tracked_paths(repository.path(), root, &paths).unwrap(),
            [PathBuf::from("vendor/chart.tgz"), PathBuf::from("vendor/a chart.tgz")]
        );
        assert!(lfs_tracked_paths(repository.path(), root, &[]).unwrap().is_empty());
    }

    #[test]
    fn test_lfs_filter_configured_reads_repository_configuration() {
        let directory = TempDir::new().unwrap();
        let root = directory.path();
        let repository = Repository::init(root).unwrap();
        repository
            .config()
            .unwrap()
            .set_str("filter.lfs.process", "git-lfs filter-process")
            .unwrap();

        assert!(lfs_filter_configured(repository.path(), root).unwrap());
    }

    #[test]
    fn test_lfs_filter_problem_reports_a_missing_executable_before_configuration() {
        assert_eq!(
            LfsFilterProblem::from_probe(false, false),
            Some(LfsFilterProblem::ExecutableMissing)
        );
        assert_eq!(
            LfsFilterProblem::from_probe(true, false),
            Some(LfsFilterProblem::FilterNotConfigured)
        );
        assert_eq!(LfsFilterProblem::from_probe(true, true), None);
    }
}
