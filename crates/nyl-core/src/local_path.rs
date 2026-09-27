//! Static form of local paths in the project's own repository.
//!
//! Owns the authored-string half of the
//! [local path rule](https://niklasrosenstein.github.io/nyl/configuration/#local-paths):
//! slash-separated, normalized, static, and with `..` only as leading segments
//! of a relative path. Resolution against the filesystem, which rejects paths
//! leaving the worktree or traversing a symbolic link, lives in nyl-render's
//! `util::project_path`; this module checks only the string, so resource
//! validation can use it.

use crate::{CoreError, Result};

/// Validate the static form of a local path: slash-separated, normalized, and
/// with `..` only as leading segments of a relative path.
pub fn validate_local_path(field: &str, value: &str) -> Result<()> {
    let invalid = || {
        CoreError::config(format!(
            "{field} must be a normalized path relative to the nyl.toml directory, or start with '/' for the \
             Git worktree root; got {value:?}"
        ))
    };
    if value.trim().is_empty() || value.contains('\\') {
        return Err(invalid());
    }
    if value.contains("{{") || value.contains("{%") || value.contains("{#") {
        return Err(CoreError::config(format!("{field} must be a static value")));
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
