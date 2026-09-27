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

/// Resolve a checkout-relative path, such as a remote ApplicationGroup
/// `source.path`, to a canonical directory inside the checkout. It rejects
/// traversal, symbolic links, and anything resolving outside the checkout.
pub fn checkout_subpath(checkout: &Path, relative: &str, field: &str) -> Result<PathBuf> {
    crate::resources::validate_relative_path(field, relative, true, true)?;
    let canonical_checkout = checkout
        .canonicalize()
        .map_err(|error| NylError::config(format!("Failed to resolve checkout {}: {error}", checkout.display())))?;
    let selected = checkout.join(relative);
    let relative_path = selected
        .strip_prefix(checkout)
        .map_err(|error| NylError::config(format!("{field} {relative:?} escapes checkout: {error}")))?;
    let mut current = checkout.to_path_buf();
    for component in relative_path.components() {
        current.push(component.as_os_str());
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(NylError::config(format!(
                    "{field} {relative:?} traverses symbolic link {}",
                    current.display()
                )))
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    let canonical_selected = selected
        .canonicalize()
        .map_err(|error| NylError::config(format!("Failed to resolve {field} {relative:?}: {error}")))?;
    if !canonical_selected.starts_with(&canonical_checkout) {
        return Err(NylError::config(format!(
            "{field} {relative:?} resolves outside checkout {}",
            checkout.display()
        )));
    }
    Ok(canonical_selected)
}

/// How [`locate_checkout_project`] found a checkout's project.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectLocation {
    /// The same worktree-relative directory as the local project.
    Same,
    /// A candidate given by the invocation.
    Candidate,
    /// An entry of the local project's `[project] previous_paths`.
    PreviousPath,
}

impl std::fmt::Display for ProjectLocation {
    /// Where the project directory came from, for reports.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Same => "the current location",
            Self::Candidate => "--source-project-path",
            Self::PreviousPath => "project.previous_paths",
        })
    }
}

/// A project found in another checkout of the repository.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocatedProject {
    /// Canonical project directory inside the checkout.
    pub directory: PathBuf,
    /// Checkout-relative, slash-separated project directory; empty for the root.
    pub path: String,
    pub location: ProjectLocation,
}

/// Find the project in a checkout of this repository at another revision,
/// such as a diff baseline or a clean render of `HEAD`, where it may live
/// elsewhere because the repository was restructured in between.
///
/// Candidates are tried in order, and each is skipped when it holds no
/// `nyl.toml`, so a stale candidate is harmless once the move reached the
/// checkout:
///
/// 1. `same`: the local project's worktree-relative directory;
/// 2. `candidates`: worktree-rooted directories from the invocation, with an
///    optional leading `/`;
/// 3. `previous_paths`: `/`-rooted entries of the local `nyl.toml`.
///
/// Only locations the project itself or the invocation names are tried, so
/// another project in the same repository is never picked up by accident.
/// Every candidate is validated before the search, so a malformed one is an
/// error even when an earlier candidate matches.
pub fn locate_checkout_project(
    checkout: &Path,
    same: &Path,
    candidates: &[String],
    previous_paths: &[String],
) -> Result<LocatedProject> {
    let same = same.to_string_lossy().replace('\\', "/");
    let mut ordered = vec![(same, ProjectLocation::Same)];
    ordered.extend(candidates.iter().map(|path| (path.clone(), ProjectLocation::Candidate)));
    ordered.extend(
        previous_paths
            .iter()
            .map(|path| (path.clone(), ProjectLocation::PreviousPath)),
    );
    let field = |location| match location {
        ProjectLocation::Same => "project directory",
        ProjectLocation::Candidate => "--source-project-path",
        ProjectLocation::PreviousPath => "project.previous_paths",
    };
    let ordered = ordered
        .into_iter()
        .map(|(path, location)| {
            // Candidates are worktree-rooted; a leading `/` says so explicitly.
            let path = path.strip_prefix('/').unwrap_or(&path).to_owned();
            crate::resources::validate_relative_path(field(location), &path, true, true)?;
            Ok((path, location))
        })
        .collect::<Result<Vec<_>>>()?;
    let mut tried = Vec::new();
    for (path, location) in ordered {
        let field = field(location);
        let display = if path.is_empty() {
            "/".to_owned()
        } else {
            format!("/{path}")
        };
        if !checkout.join(&path).join("nyl.toml").is_file() {
            if !tried.contains(&display) {
                tried.push(display);
            }
            continue;
        }
        let directory = checkout_subpath(checkout, if path.is_empty() { "." } else { &path }, field)?;
        return Ok(LocatedProject {
            directory,
            path,
            location,
        });
    }
    Err(NylError::config(format!(
        "No nyl.toml found in checkout {} at {}; if the project moved, list its earlier location in \
         [project] previous_paths of nyl.toml or pass --source-project-path",
        checkout.display(),
        tried.join(", ")
    )))
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

    fn checkout_with_projects(projects: &[&str]) -> TempDir {
        let temp = TempDir::new().unwrap();
        for project in projects {
            let directory = temp.path().join(project);
            std::fs::create_dir_all(&directory).unwrap();
            std::fs::write(directory.join("nyl.toml"), "").unwrap();
        }
        temp
    }

    fn locate(checkout: &TempDir, same: &str, candidates: &[&str], previous: &[&str]) -> Result<LocatedProject> {
        let owned = |values: &[&str]| values.iter().map(|value| (*value).to_owned()).collect::<Vec<_>>();
        locate_checkout_project(checkout.path(), Path::new(same), &owned(candidates), &owned(previous))
    }

    #[test]
    fn test_locate_checkout_project_prefers_the_same_directory_over_every_fallback() {
        let checkout = checkout_with_projects(&["platform", "old", "nyl", ""]);
        let found = locate(&checkout, "platform", &["old"], &["/old"]).unwrap();
        assert_eq!(
            (found.path.as_str(), found.location),
            ("platform", ProjectLocation::Same)
        );
    }

    #[test]
    fn test_locate_checkout_project_skips_missing_candidates_in_order() {
        let checkout = checkout_with_projects(&["infra/config", "nyl"]);
        let found = locate(&checkout, "platform", &["gone", "infra/config"], &[]).unwrap();
        assert_eq!(
            (found.path.as_str(), found.location),
            ("infra/config", ProjectLocation::Candidate)
        );

        let found = locate(&checkout, "platform", &["gone"], &["/infra/config"]).unwrap();
        assert_eq!(found.location, ProjectLocation::PreviousPath);

        let found = locate(&checkout, "platform", &["/infra/config"], &[]).unwrap();
        assert_eq!(
            (found.path.as_str(), found.location),
            ("infra/config", ProjectLocation::Candidate)
        );
    }

    #[test]
    fn test_locate_checkout_project_never_falls_back_to_an_unnamed_project() {
        // A monorepo project at the root must not stand in for a new project
        // at `platform/` that the baseline does not have yet.
        let checkout = checkout_with_projects(&["", "nyl"]);
        let error = locate(&checkout, "platform", &[], &[]).unwrap_err().to_string();
        assert!(error.contains("/platform"), "{error}");
    }

    #[test]
    fn test_locate_checkout_project_validates_candidates_that_are_never_reached() {
        let checkout = checkout_with_projects(&["platform"]);
        let error = locate(&checkout, "platform", &["../outside"], &[])
            .unwrap_err()
            .to_string();
        assert!(error.contains("--source-project-path"), "{error}");
    }

    #[test]
    fn test_locate_checkout_project_uses_the_root_for_a_rooted_previous_path() {
        let checkout = checkout_with_projects(&["", "nyl"]);
        let found = locate(&checkout, "platform", &[], &["/"]).unwrap();
        assert_eq!(
            (found.path.as_str(), found.location),
            ("", ProjectLocation::PreviousPath)
        );
    }

    #[test]
    fn test_locate_checkout_project_lists_tried_paths_when_nothing_matches() {
        let checkout = checkout_with_projects(&[]);
        let error = locate(&checkout, "platform", &["old"], &["/older"])
            .unwrap_err()
            .to_string();
        for tried in ["/platform", "/old", "/older", "previous_paths", "--source-project-path"] {
            assert!(error.contains(tried), "{tried}: {error}");
        }
    }

    #[test]
    fn test_locate_checkout_project_rejects_escaping_candidates() {
        let checkout = checkout_with_projects(&["platform"]);
        assert!(locate(&checkout, "elsewhere", &["../outside"], &[]).is_err());
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
