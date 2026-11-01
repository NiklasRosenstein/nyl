//! The project's dedicated Git credentials, resolved once per invocation.

use nyl_render::config::ProjectFile;
use nyl_render::Result;

/// Resolve `[[git.credentials]]` from `nyl.toml` against the process
/// environment. Without entries, `git` authenticates as configured for the user.
pub fn project_git_auth(config: &ProjectFile) -> Result<nyl_git::GitAuth> {
    nyl_render::gitops::publication::project_git_auth(config, |name| std::env::var(name).ok())
}
