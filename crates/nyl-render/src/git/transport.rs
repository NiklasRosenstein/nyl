//! Remote Git transport.
//!
//! Remote operations run the `git` command line, so they authenticate exactly
//! as the user's own `git` does: OpenSSH reads `~/.ssh/config` (`Host`
//! aliases, `IdentityFile`, `IdentitiesOnly`, `ProxyJump`), and HTTPS uses the
//! configured credential helpers, `url.<base>.insteadOf` rewrites, and
//! `core.sshCommand`/`GIT_SSH_COMMAND`. libgit2's SSH transport reads none of
//! these and offers the agent's keys in agent order, so a host that accepts
//! the first key as a different account rejects a fetch that `git` performs.

use std::process::{Command, Stdio};

use git2::{Oid, Repository};

use super::error::{GitError, Result};

/// Repository-location variables that `git` would otherwise prefer over the
/// repository Nyl names, for example when Nyl runs inside a Git hook. User
/// configuration (`GIT_CONFIG_PARAMETERS`, `GIT_CONFIG_COUNT`) is kept.
const REPOSITORY_ENVIRONMENT: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_IMPLICIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_NAMESPACE",
    "GIT_GRAFT_FILE",
    "GIT_SHALLOW_FILE",
    "GIT_REPLACE_REF_BASE",
    "GIT_NO_REPLACE_OBJECTS",
    "GIT_PREFIX",
];

/// Fetch `refspecs` from `url` into `repository`.
///
/// With `prune`, destination refs matched by a glob refspec whose source no
/// longer exists are deleted.
pub fn fetch(repository: &Repository, url: &str, refspecs: &[&str], prune: bool) -> Result<()> {
    let mut args = vec!["fetch", "--quiet"];
    if prune {
        args.push("--prune");
    }
    args.extend(["--", url]);
    args.extend_from_slice(refspecs);
    run(repository, url, "git fetch", &args).map(drop)
}

/// Fetch `branch` of `url` into the `tracking` ref and return its commit.
///
/// A branch that does not exist on the remote deletes `tracking` and returns
/// `None`.
pub fn fetch_branch(repository: &Repository, url: &str, branch: &str, tracking: &str) -> Result<Option<Oid>> {
    let refspec = format!("+refs/heads/{branch}:{tracking}");
    match fetch(repository, url, &[&refspec], false) {
        Ok(()) => Ok(Some(repository.refname_to_id(tracking)?)),
        // Only an absent branch makes the fetch fail while listing works.
        Err(error) => match remote_branch(repository, url, branch) {
            Ok(None) => {
                if let Ok(mut reference) = repository.find_reference(tracking) {
                    reference.delete()?;
                }
                Ok(None)
            }
            Ok(Some(_)) | Err(_) => Err(error),
        },
    }
}

/// The commit `branch` names on `url`, or `None` when it does not exist.
pub fn remote_branch(repository: &Repository, url: &str, branch: &str) -> Result<Option<Oid>> {
    let name = format!("refs/heads/{branch}");
    let output = run(repository, url, "git ls-remote", &["ls-remote", "--", url, &name])?;
    output
        .lines()
        .find_map(|line| line.split_once('\t').filter(|(_, reference)| *reference == name))
        .map(|(oid, _)| Oid::from_str(oid).map_err(GitError::from))
        .transpose()
}

/// Push `branch` to `url` only while the remote branch is still at
/// `expected` (`None`: absent), so a concurrent writer is never overwritten.
pub fn push_branch_if_unchanged(repository: &Repository, url: &str, branch: &str, expected: Option<Oid>) -> Result<()> {
    let reference = format!("refs/heads/{branch}");
    // An empty expected value leases on the branch not existing yet.
    let lease = format!(
        "--force-with-lease={reference}:{}",
        expected.map(|oid| oid.to_string()).unwrap_or_default()
    );
    let refspec = format!("{reference}:{reference}");
    run(
        repository,
        url,
        "git push",
        &["push", "--quiet", "--porcelain", &lease, "--", url, &refspec],
    )
    .map(drop)
}

/// Run `git` against `repository` and return its standard output.
fn run(repository: &Repository, url: &str, operation: &str, args: &[&str]) -> Result<String> {
    let mut command = Command::new("git");
    command
        .arg("--git-dir")
        .arg(repository.path())
        // Fetches into the cache must not start background maintenance, and
        // a URL must never run a command through the `ext::` transport.
        .args([
            "-c",
            "gc.auto=0",
            "-c",
            "maintenance.auto=false",
            "-c",
            "protocol.ext.allow=never",
        ])
        .args(args)
        .stdin(Stdio::null())
        // Fail instead of waiting for a terminal password prompt; OpenSSH
        // can still ask on the controlling terminal, as it does for `git`.
        .env("GIT_TERMINAL_PROMPT", "0");
    for variable in REPOSITORY_ENVIRONMENT {
        command.env_remove(variable);
    }
    tracing::trace!("Running {operation} for {}", crate::util::sanitize_url(url));
    let output = command.output().map_err(|error| {
        GitError::Command(format!(
            "failed to run {operation} for {}: {error}",
            crate::util::sanitize_url(url)
        ))
    })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let message = if stderr.trim().is_empty() { stdout } else { stderr };
        return Err(remote_error(operation, url, message.trim()));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn remote_error(operation: &str, url: &str, message: &str) -> GitError {
    GitError::RemoteFailed {
        operation: operation.to_owned(),
        url: crate::util::sanitize_url(url),
        message: crate::util::redact_url_credentials(message, url),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn commit(repository: &Repository, reference: &str, message: &str, parents: &[&git2::Commit<'_>]) -> Oid {
        let signature = git2::Signature::now("Test", "test@example.invalid").unwrap();
        let tree = repository
            .find_tree(repository.index().unwrap().write_tree().unwrap())
            .unwrap();
        repository
            .commit(Some(reference), &signature, &signature, message, &tree, parents)
            .unwrap()
    }

    #[test]
    fn test_push_branch_if_unchanged_refuses_a_moved_branch() {
        let remote_dir = TempDir::new().unwrap();
        let remote = Repository::init_bare(remote_dir.path()).unwrap();
        let url = remote_dir.path().to_string_lossy().to_string();
        let local_dir = TempDir::new().unwrap();
        let local = Repository::init(local_dir.path()).unwrap();

        assert_eq!(remote_branch(&local, &url, "deploy").unwrap(), None);
        let first = commit(&local, "refs/heads/deploy", "First", &[]);
        push_branch_if_unchanged(&local, &url, "deploy", None).unwrap();
        assert_eq!(remote_branch(&local, &url, "deploy").unwrap(), Some(first));

        // Another writer advances the branch; a push leased on `first` fails.
        let concurrent = commit(
            &remote,
            "refs/heads/deploy",
            "Concurrent",
            &[&remote.find_commit(first).unwrap()],
        );
        let second = commit(
            &local,
            "refs/heads/deploy",
            "Second",
            &[&local.find_commit(first).unwrap()],
        );
        assert!(push_branch_if_unchanged(&local, &url, "deploy", Some(first)).is_err());
        assert_eq!(remote.refname_to_id("refs/heads/deploy").unwrap(), concurrent);

        fetch(&local, &url, &["+refs/heads/deploy:refs/remotes/origin/deploy"], false).unwrap();
        assert_eq!(local.refname_to_id("refs/remotes/origin/deploy").unwrap(), concurrent);
        assert!(push_branch_if_unchanged(&local, &url, "deploy", None).is_err());
        assert_ne!(remote.refname_to_id("refs/heads/deploy").unwrap(), second);
    }

    #[test]
    fn test_fetch_reports_the_remote_message_with_credentials_redacted() {
        let local_dir = TempDir::new().unwrap();
        let local = Repository::init_bare(local_dir.path()).unwrap();
        let url = format!(
            "file://user:token@localhost{}/missing",
            local_dir.path().to_string_lossy()
        );
        let error = fetch(&local, &url, &["+refs/heads/*:refs/heads/*"], true)
            .unwrap_err()
            .to_string();
        assert!(error.starts_with("git fetch failed for "), "{error}");
        assert!(!error.contains("token"), "{error}");
    }

    #[test]
    fn test_a_url_is_never_read_as_a_git_option() {
        let local_dir = TempDir::new().unwrap();
        let local = Repository::init_bare(local_dir.path()).unwrap();
        let marker = local_dir.path().join("marker");
        let url = format!("--upload-pack=touch {}", marker.display());
        assert!(fetch(&local, &url, &["+refs/heads/*:refs/heads/*"], false).is_err());
        assert!(remote_branch(&local, &url, "main").is_err());
        assert!(push_branch_if_unchanged(&local, &url, "main", None).is_err());
        assert!(!marker.exists());
    }
}
