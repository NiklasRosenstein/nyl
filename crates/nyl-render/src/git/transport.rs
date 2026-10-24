//! Remote Git transport for rendering and publication, through
//! [`nyl_git`]: the `git` command line with the user's own authentication,
//! plus the optional dedicated credentials of [`GitAuth`].

use git2::{Oid, Repository};

pub use nyl_git::GitAuth;

use super::error::Result;

/// Fetch `refspecs` from `url` into `repository`, authenticating as the user's `git` does.
pub fn fetch(repository: &Repository, url: &str, refspecs: &[&str], prune: bool) -> Result<()> {
    Ok(nyl_git::fetch(
        repository.path(),
        url,
        refspecs,
        prune,
        &GitAuth::none(),
    )?)
}

/// Fetch `branch` of `url` into the `tracking` ref and return its commit.
pub fn fetch_branch(
    repository: &Repository,
    url: &str,
    branch: &str,
    tracking: &str,
    auth: &GitAuth,
) -> Result<Option<Oid>> {
    nyl_git::fetch_branch(repository.path(), url, branch, tracking, auth)?
        .map(|oid| Oid::from_str(&oid).map_err(Into::into))
        .transpose()
}

/// Push `branch` to `url` only while the remote branch is still at
/// `expected` (`None`: absent), so a concurrent writer is never overwritten.
pub fn push_branch_if_unchanged(
    repository: &Repository,
    url: &str,
    branch: &str,
    expected: Option<Oid>,
    auth: &GitAuth,
) -> Result<()> {
    let expected = expected.map(|oid| oid.to_string());
    Ok(nyl_git::push_branch_if_unchanged(
        repository.path(),
        url,
        branch,
        expected.as_deref(),
        auth,
    )?)
}
