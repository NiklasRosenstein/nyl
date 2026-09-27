//! Resolution of local paths in the project's own repository.
//!
//! One rule serves every local path a resource names in this repository, such
//! as a local ApplicationGroup `spec.source.path`:
//!
//! - A relative path resolves against the directory that contains `nyl.toml`.
//!   It may use leading `..` segments to reach elsewhere in the Git worktree.
//! - A path with a leading `/` resolves against the Git worktree root.
//! - No path may leave the worktree or traverse a symbolic link.
//!
//! Paths inside another repository's checkout, such as a remote
//! ApplicationGroup source or a `fromGit` binding, are relative to that
//! checkout and do not use this module.
//!
//! Recorded paths, such as ownership-index inputs and rendered provenance, use
//! the matching portable key: project-relative for files beneath the
//! `nyl.toml` directory, and `/`-prefixed worktree-relative otherwise.

use crate::{NylError, Result};
use std::path::{Component, Path, PathBuf};

/// Validate the static form of a local path: slash-separated, normalized, and
/// with `..` only as leading segments of a relative path.
pub fn validate_local_path(field: &str, value: &str) -> Result<()> {
    let invalid = || {
        NylError::config(format!(
            "{field} must be a normalized path relative to the nyl.toml directory, or start with '/' for the \
             Git worktree root; got {value:?}"
        ))
    };
    if value.trim().is_empty() || value.contains('\\') {
        return Err(invalid());
    }
    if value.contains("{{") || value.contains("{%") || value.contains("{#") {
        return Err(NylError::config(format!("{field} must be a static value")));
    }
    let (rooted, rest) = match value.strip_prefix('/') {
        Some(rest) => (true, rest),
        None => (false, value),
    };
    let mut leading_parents = true;
    let mut normal_segments = 0usize;
    for segment in rest.split('/') {
        match segment {
            ".." if leading_parents && !rooted => {}
            "" | "." | ".." => return Err(invalid()),
            _ => {
                leading_parents = false;
                normal_segments += 1;
            }
        }
    }
    if normal_segments == 0 {
        return Err(invalid());
    }
    Ok(())
}

/// The two roots local paths resolve against.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectPaths {
    /// Canonical directory containing `nyl.toml`.
    pub project_root: PathBuf,
    /// Canonical root of the Git worktree containing the project.
    pub worktree_root: PathBuf,
}

impl ProjectPaths {
    pub fn new(project_root: PathBuf, worktree_root: PathBuf) -> Self {
        Self {
            project_root,
            worktree_root,
        }
    }

    /// Resolve a local path to an absolute path inside the worktree.
    ///
    /// The target need not exist; callers report missing directories with
    /// their own context. Existing prefixes must not be symbolic links.
    pub fn resolve(&self, field: &str, value: &str) -> Result<PathBuf> {
        validate_local_path(field, value)?;
        let joined = match value.strip_prefix('/') {
            Some(rest) => self.worktree_root.join(rest),
            None => self.project_root.join(value),
        };
        let resolved = normalize(&joined);
        if resolved == self.worktree_root || !resolved.starts_with(&self.worktree_root) {
            return Err(NylError::config(format!(
                "{field} {value:?} resolves outside the Git worktree {}",
                self.worktree_root.display()
            )));
        }
        let relative = resolved
            .strip_prefix(&self.worktree_root)
            .expect("resolved path was checked to be inside the worktree");
        let mut current = self.worktree_root.clone();
        for component in relative.components() {
            current.push(component);
            match std::fs::symlink_metadata(&current) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    return Err(NylError::config(format!(
                        "{field} {value:?} traverses symbolic link {}",
                        current.display()
                    )))
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
                Err(error) => return Err(error.into()),
            }
        }
        Ok(resolved)
    }

    /// The portable key of an absolute path inside the worktree: relative to
    /// the project root when beneath it, otherwise `/` plus the
    /// worktree-relative path. Returns `None` outside the worktree.
    pub fn key(&self, path: &Path) -> Option<PathBuf> {
        if let Ok(relative) = path.strip_prefix(&self.project_root) {
            return Some(relative.to_path_buf());
        }
        path.strip_prefix(&self.worktree_root)
            .ok()
            .map(|relative| Path::new("/").join(relative))
    }

    /// The absolute path a portable key names.
    pub fn path_for_key(&self, key: &Path) -> PathBuf {
        match key.strip_prefix("/") {
            Ok(relative) => self.worktree_root.join(relative),
            Err(_) => self.project_root.join(key),
        }
    }
}

/// Find the root of the Git worktree containing `path`: the nearest ancestor
/// holding a `.git` directory or file.
pub fn find_worktree_root(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .find(|directory| directory.join(".git").exists())
        .map(Path::to_path_buf)
}

fn normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn paths(temp: &TempDir) -> ProjectPaths {
        let worktree = temp.path().canonicalize().unwrap();
        std::fs::create_dir_all(worktree.join("nyl")).unwrap();
        ProjectPaths::new(worktree.join("nyl"), worktree)
    }

    #[test]
    fn test_validate_local_path_accepts_the_three_forms() {
        for value in [
            "applications/web",
            "../applications/web",
            "../../x/y",
            "/applications/web",
        ] {
            validate_local_path("path", value).unwrap_or_else(|error| panic!("{value}: {error}"));
        }
    }

    #[test]
    fn test_validate_local_path_rejects_unnormalized_forms() {
        for value in [
            "", ".", "/", "..", "a/../b", "a//b", "./a", "/../a", "a\\b", "a/", "{{ x }}",
        ] {
            assert!(validate_local_path("path", value).is_err(), "{value}");
        }
    }

    #[test]
    fn test_resolve_relative_to_project_and_rooted_to_worktree() {
        let temp = TempDir::new().unwrap();
        let paths = paths(&temp);
        let sibling = paths.worktree_root.join("applications/web");
        assert_eq!(paths.resolve("path", "../applications/web").unwrap(), sibling);
        assert_eq!(paths.resolve("path", "/applications/web").unwrap(), sibling);
        assert_eq!(
            paths.resolve("path", "applications/web").unwrap(),
            paths.project_root.join("applications/web")
        );
    }

    #[test]
    fn test_resolve_rejects_paths_leaving_the_worktree() {
        let temp = TempDir::new().unwrap();
        let paths = paths(&temp);
        let error = paths.resolve("path", "../../outside").unwrap_err().to_string();
        assert!(error.contains("outside the Git worktree"), "{error}");
        assert!(paths.resolve("path", "..").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn test_resolve_rejects_symbolic_links() {
        let temp = TempDir::new().unwrap();
        let paths = paths(&temp);
        std::fs::create_dir_all(paths.worktree_root.join("real")).unwrap();
        std::os::unix::fs::symlink(paths.worktree_root.join("real"), paths.worktree_root.join("link")).unwrap();
        let error = paths.resolve("path", "/link/web").unwrap_err().to_string();
        assert!(error.contains("symbolic link"), "{error}");
    }

    #[test]
    fn test_key_round_trips_inside_and_outside_the_project() {
        let temp = TempDir::new().unwrap();
        let paths = paths(&temp);
        let inside = paths.project_root.join("applications/web/release.yaml");
        let outside = paths.worktree_root.join("applications/web/release.yaml");
        assert_eq!(
            paths.key(&inside).unwrap(),
            PathBuf::from("applications/web/release.yaml")
        );
        assert_eq!(
            paths.key(&outside).unwrap(),
            PathBuf::from("/applications/web/release.yaml")
        );
        assert_eq!(paths.path_for_key(&paths.key(&outside).unwrap()), outside);
        assert_eq!(paths.path_for_key(&paths.key(&inside).unwrap()), inside);
        assert_eq!(paths.key(Path::new("/elsewhere/file.yaml")), None);
    }
}
