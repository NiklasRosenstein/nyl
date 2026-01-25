use crate::Result;
/// File system utilities for config file discovery and path resolution
use std::path::{Path, PathBuf};

/// Find a configuration file by searching upward through parent directories
///
/// Searches for config files in the following order:
/// 1. Current working directory (or provided `cwd`)
/// 2. Parent directories (upward traversal)
///
/// # Arguments
/// * `filenames` - List of filenames to search for, in priority order
/// * `cwd` - Optional starting directory (defaults to current working directory)
/// * `required` - If true, returns error when no file found; if false, returns None
///
/// # Returns
/// * `Some(PathBuf)` - First matching config file found
/// * `None` - No config file found (only when required=false)
///
/// # Examples
/// ```no_run
/// use nyl::util::fs::find_config_file;
///
/// let config = find_config_file(
///     &["nyl-project.yaml", "nyl-project.json"],
///     None,
///     false
/// );
/// ```
pub fn find_config_file(filenames: &[&str], cwd: Option<&Path>, required: bool) -> Result<Option<PathBuf>> {
    let start_dir = match cwd {
        Some(path) => path.to_path_buf(),
        None => std::env::current_dir()?,
    };

    let mut current_dir = start_dir.as_path();

    loop {
        // Check each filename in priority order in the current directory
        for filename in filenames {
            let candidate = current_dir.join(filename);
            if candidate.exists() && candidate.is_file() {
                return Ok(Some(candidate));
            }
        }

        // Move up to parent directory
        match current_dir.parent() {
            Some(parent) => current_dir = parent,
            None => {
                // Reached filesystem root without finding config
                if required {
                    return Err(crate::NylError::ConfigNotFound(
                        filenames.iter().map(|s| s.to_string()).collect::<Vec<_>>().join(", "),
                    ));
                } else {
                    return Ok(None);
                }
            }
        }
    }
}

/// Resolve a path relative to a base directory
///
/// If the path is already absolute, returns it as-is.
/// Otherwise, resolves it relative to the base directory.
///
/// # Arguments
/// * `path` - The path to resolve
/// * `base` - The base directory for relative path resolution
///
/// # Returns
/// * Absolute path
pub fn resolve_path(path: &Path, base: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

/// Resolve multiple paths relative to a base directory
///
/// # Arguments
/// * `paths` - Vector of paths to resolve
/// * `base` - The base directory for relative path resolution
///
/// # Returns
/// * Vector of absolute paths
pub fn resolve_paths(paths: &[PathBuf], base: &Path) -> Vec<PathBuf> {
    paths.iter().map(|p| resolve_path(p, base)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_find_config_in_current_dir() {
        let temp = TempDir::new().unwrap();
        let config_path = temp.path().join("nyl-project.yaml");
        fs::write(&config_path, "test: value").unwrap();

        let result = find_config_file(&["nyl-project.yaml"], Some(temp.path()), false).unwrap();

        assert!(result.is_some());
        assert_eq!(result.unwrap(), config_path);
    }

    #[test]
    fn test_find_config_in_parent_dir() {
        let temp = TempDir::new().unwrap();
        let config_path = temp.path().join("nyl-project.yaml");
        fs::write(&config_path, "test: value").unwrap();

        // Create subdirectory
        let subdir = temp.path().join("subdir");
        fs::create_dir(&subdir).unwrap();

        let result = find_config_file(&["nyl-project.yaml"], Some(&subdir), false).unwrap();

        assert!(result.is_some());
        assert_eq!(result.unwrap(), config_path);
    }

    #[test]
    fn test_priority_order() {
        let temp = TempDir::new().unwrap();
        let yaml_path = temp.path().join("nyl-project.yaml");
        let json_path = temp.path().join("nyl-project.json");

        fs::write(&yaml_path, "test: value").unwrap();
        fs::write(&json_path, r#"{"test": "value"}"#).unwrap();

        // YAML should be found first due to priority
        let result = find_config_file(&["nyl-project.yaml", "nyl-project.json"], Some(temp.path()), false).unwrap();

        assert!(result.is_some());
        assert_eq!(result.unwrap(), yaml_path);
    }

    #[test]
    fn test_no_config_found_optional() {
        let temp = TempDir::new().unwrap();

        let result = find_config_file(&["nyl-project.yaml"], Some(temp.path()), false).unwrap();

        assert!(result.is_none());
    }

    #[test]
    fn test_no_config_found_required() {
        let temp = TempDir::new().unwrap();

        let result = find_config_file(&["nyl-project.yaml"], Some(temp.path()), true);

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("nyl-project.yaml"));
    }

    #[test]
    fn test_resolve_absolute_path() {
        let base = PathBuf::from("/base/dir");
        let abs_path = PathBuf::from("/absolute/path");

        let result = resolve_path(&abs_path, &base);
        assert_eq!(result, abs_path);
    }

    #[test]
    fn test_resolve_relative_path() {
        let base = PathBuf::from("/base/dir");
        let rel_path = PathBuf::from("relative/path");

        let result = resolve_path(&rel_path, &base);
        assert_eq!(result, PathBuf::from("/base/dir/relative/path"));
    }

    #[test]
    fn test_resolve_multiple_paths() {
        let base = PathBuf::from("/base/dir");
        let paths = vec![PathBuf::from("rel1"), PathBuf::from("/abs1"), PathBuf::from("rel2")];

        let results = resolve_paths(&paths, &base);

        assert_eq!(results.len(), 3);
        assert_eq!(results[0], PathBuf::from("/base/dir/rel1"));
        assert_eq!(results[1], PathBuf::from("/abs1"));
        assert_eq!(results[2], PathBuf::from("/base/dir/rel2"));
    }
}
