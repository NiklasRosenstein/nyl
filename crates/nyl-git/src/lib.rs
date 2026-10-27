#![warn(clippy::all, clippy::pedantic)]
#![allow(
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::must_use_candidate,
    clippy::doc_markdown,
    clippy::module_name_repetitions
)]

//! Git transport and worktree inspection shared by rendering, publication, and state.
//!
//! Remote operations run the `git` command line, so they authenticate exactly
//! as the user's own `git` does: OpenSSH reads `~/.ssh/config` (`Host`
//! aliases, `IdentityFile`, `IdentitiesOnly`, `ProxyJump`), and HTTPS uses the
//! configured credential helpers, `url.<base>.insteadOf` rewrites, and
//! `core.sshCommand`/`GIT_SSH_COMMAND`. libgit2's SSH transport reads none of
//! these and offers the agent's keys in agent order, so a host that accepts
//! the first key as a different account rejects a fetch that `git` performs.
//!
//! Dedicated credentials are optional: [`GitAuth`] supplies an SSH key or an
//! HTTPS token to the same invocations when `nyl.toml` configures one for a
//! repository URL, for runners without an agent or credential helper.

mod auth;
mod error;
mod status;
mod transport;
mod url;

pub use auth::{CredentialEntry, GitAuth, GitSettings};
pub use error::{GitError, Rejection, Result};
pub use status::{worktree_status, WorktreeStatusEntry};
pub use transport::{
    fetch, fetch_branch, ls_remote, push_atomic, push_branch_if_unchanged, remote_branch, Expect, RefUpdate,
};
pub use url::{redact_url_credentials, sanitize_url};
