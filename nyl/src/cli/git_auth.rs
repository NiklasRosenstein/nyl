//! The project's dedicated Git credentials, resolved once per invocation.

use crate::config::ProjectFile;
use crate::{NylError, Result};

/// Resolve `[[git.credentials]]` from `nyl.toml` against the process
/// environment. Without entries, `git` authenticates as configured for the user.
pub fn project_git_auth(config: &ProjectFile) -> Result<nyl_git::GitAuth> {
    nyl_git::GitAuth::from_settings(&config.git, |name| std::env::var(name).ok())
        .map_err(|error| NylError::config(error.to_string()))
}
