//! Local repository provisioning for explicit state initialization.
//!
//! Contract: [Initialization and moves](../../../../design/orchestration-core.md#initialization-and-moves).
//! Only a missing local destination shared by the read and write endpoints is
//! created. Existing filesystem entries must already be Git repositories.

use std::path::{Path, PathBuf};

use git2::{Repository, RepositoryInitOptions, RepositoryOpenFlags};
use nyl_core::orchestration::ports::{StateError, StateResult};

use super::refs::unavailable;

/// Absolute filesystem paths and file URLs without a remote authority are local.
fn local_path(endpoint: &str) -> Option<PathBuf> {
    if Path::new(endpoint).is_absolute() {
        return Some(PathBuf::from(endpoint));
    }
    if !endpoint.starts_with("file://") {
        return None;
    }
    let url = url::Url::parse(endpoint).ok()?;
    if url.scheme() != "file"
        || url.host_str().is_some_and(|host| host != "localhost")
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    url.to_file_path().ok()
}

pub(super) fn prepare(read: &str, write: &str) -> StateResult<()> {
    let (Some(path), Some(write_path)) = (local_path(read), local_path(write)) else {
        return Ok(());
    };
    // Separate endpoints need an operator to provision their common storage.
    if path != write_path {
        return Ok(());
    }
    match std::fs::symlink_metadata(&path) {
        Ok(_) => return existing(&path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(unavailable(error)),
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(unavailable)?;
    }
    // Claim the destination before initializing it, so an existing directory
    // never becomes a repository as a side effect of initialization.
    match std::fs::create_dir(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => return existing(&path),
        Err(error) => return Err(unavailable(error)),
    }
    Repository::init_opts(&path, RepositoryInitOptions::new().bare(true).no_reinit(true)).map_err(unavailable)?;
    Ok(())
}

fn existing(path: &Path) -> StateResult<()> {
    Repository::open_ext(path, RepositoryOpenFlags::NO_SEARCH, std::iter::empty::<&Path>()).map_err(|error| {
        StateError::Unavailable(format!(
            "local state repository path {} already exists but cannot be opened as a Git repository: {error}",
            path.display()
        ))
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_prepare_creates_a_missing_bare_repository_for_local_endpoints() {
        let root = TempDir::new().unwrap();
        for (name, file_url) in [("absolute", false), ("file url", true)] {
            let path = root.path().join(name).join("state.git");
            let endpoint = if file_url {
                url::Url::from_file_path(&path).unwrap().to_string()
            } else {
                path.display().to_string()
            };
            prepare(&endpoint, &endpoint).unwrap();
            assert!(Repository::open(&path).unwrap().is_bare());
            prepare(&endpoint, &endpoint).unwrap();
        }
    }

    #[test]
    fn test_prepare_preserves_existing_repositories() {
        let root = TempDir::new().unwrap();
        for bare in [false, true] {
            let path = root.path().join(if bare { "bare.git" } else { "worktree" });
            let repository = if bare {
                Repository::init_bare(&path).unwrap()
            } else {
                Repository::init(&path).unwrap()
            };
            repository.config().unwrap().set_str("nyl.marker", "keep").unwrap();
            let endpoint = path.display().to_string();
            prepare(&endpoint, &endpoint).unwrap();
            let reopened = Repository::open(path).unwrap();
            assert_eq!(reopened.is_bare(), bare);
            assert_eq!(reopened.config().unwrap().get_string("nyl.marker").unwrap(), "keep");
        }
    }

    #[test]
    fn test_prepare_refuses_existing_non_repository_entries_without_mutation() {
        let root = TempDir::new().unwrap();
        // A containing repository must not make an ordinary subdirectory valid.
        Repository::init(root.path()).unwrap();
        let directory = root.path().join("directory");
        std::fs::create_dir(&directory).unwrap();
        let file = root.path().join("file");
        std::fs::write(&file, "keep").unwrap();
        for path in [&directory, &file] {
            let endpoint = path.display().to_string();
            assert!(prepare(&endpoint, &endpoint).is_err());
        }
        assert_eq!(std::fs::read_dir(directory).unwrap().count(), 0);
        assert_eq!(std::fs::read_to_string(file).unwrap(), "keep");
    }

    #[test]
    fn test_prepare_does_not_provision_remote_or_separate_endpoints() {
        let root = TempDir::new().unwrap();
        let first = root.path().join("first.git");
        let second = root.path().join("second.git");
        let first_url = url::Url::from_file_path(&first).unwrap().to_string();
        let second_url = url::Url::from_file_path(&second).unwrap().to_string();
        for remote in ["https://example.invalid/state.git", "git@example.invalid:state.git"] {
            prepare(remote, &first_url).unwrap();
            prepare(&first_url, remote).unwrap();
        }
        prepare(&first_url, &second_url).unwrap();
        assert!(!first.exists());
        assert!(!second.exists());
        assert!(local_path("file://server.invalid/state.git").is_none());
        assert!(local_path("file:///state.git?query").is_none());
        assert!(local_path("file:///state.git#fragment").is_none());
        assert!(local_path("relative/state.git").is_none());
        assert!(local_path("file:state.git").is_none());
    }
}
