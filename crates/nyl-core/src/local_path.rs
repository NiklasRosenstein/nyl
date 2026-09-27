//! Static form of local paths in the project's own repository.
//!
//! Resolution against the filesystem lives with the rendering code; this module
//! checks only the authored string, so resource validation can use it.

use crate::{CoreError as NylError, Result};

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
