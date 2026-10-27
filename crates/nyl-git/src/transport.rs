//! The hardened `git` command line for remote operations.
//!
//! Every invocation names its repository explicitly, ignores the user's hooks
//! and repository-location variables, never prompts, never runs a command
//! through the `ext::` transport, and redacts credentials embedded in URLs
//! from its messages.

use std::path::Path;
use std::process::{Command, Stdio};

use crate::auth::GitAuth;
use crate::error::{GitError, Rejection, Result};
use crate::url::{redact_url_credentials, sanitize_url};

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

/// Fetch `refspecs` from `url` into the repository at `git_dir`.
///
/// With `prune`, destination refs matched by a glob refspec whose source no
/// longer exists are deleted.
pub fn fetch(git_dir: &Path, url: &str, refspecs: &[&str], prune: bool, auth: &GitAuth) -> Result<()> {
    let mut args = vec!["fetch", "--quiet", "--no-tags"];
    if prune {
        args.push("--prune");
    }
    args.extend(["--", url]);
    args.extend_from_slice(refspecs);
    run(git_dir, url, "git fetch", &args, auth).map(drop)
}

/// Fetch `branch` of `url` into the `tracking` ref and return its commit.
///
/// A branch that does not exist on the remote deletes `tracking` and returns
/// `None`.
pub fn fetch_branch(git_dir: &Path, url: &str, branch: &str, tracking: &str, auth: &GitAuth) -> Result<Option<String>> {
    let refspec = format!("+refs/heads/{branch}:{tracking}");
    match fetch(git_dir, url, &[&refspec], false, auth) {
        Ok(()) => local_ref(git_dir, tracking),
        // Only an absent branch makes the fetch fail while listing works.
        Err(error) => match remote_branch(git_dir, url, branch, auth) {
            Ok(None) => {
                run_local(git_dir, &["update-ref", "-d", tracking])?;
                Ok(None)
            }
            Ok(Some(_)) | Err(_) => Err(error),
        },
    }
}

/// The commit `branch` names on `url`, or `None` when it does not exist.
pub fn remote_branch(git_dir: &Path, url: &str, branch: &str, auth: &GitAuth) -> Result<Option<String>> {
    let name = format!("refs/heads/{branch}");
    Ok(ls_remote(git_dir, url, &[&name], auth)?.remove(&name))
}

/// The commits that `patterns` (full ref names or `refs/…/*` globs) name on
/// `url`, by ref name.
pub fn ls_remote(
    git_dir: &Path,
    url: &str,
    patterns: &[&str],
    auth: &GitAuth,
) -> Result<std::collections::BTreeMap<String, String>> {
    let mut args = vec!["ls-remote", "--", url];
    args.extend_from_slice(patterns);
    let output = run(git_dir, url, "git ls-remote", &args, auth)?;
    Ok(output
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .filter(|(_, reference)| !reference.ends_with("^{}"))
        .map(|(oid, reference)| (reference.to_owned(), oid.to_owned()))
        .collect())
}

/// Push `branch` to `url` only while the remote branch is still at
/// `expected` (`None`: absent), so a concurrent writer is never overwritten.
pub fn push_branch_if_unchanged(
    git_dir: &Path,
    url: &str,
    branch: &str,
    expected: Option<&str>,
    auth: &GitAuth,
) -> Result<()> {
    let reference = format!("refs/heads/{branch}");
    push_atomic(
        git_dir,
        url,
        &[RefUpdate {
            remote: reference.clone(),
            new: Some(reference.clone()),
            expected: Expect::at(expected),
        }],
        auth,
    )
}

/// What a ref of an atomic push must be before the push.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expect {
    /// Nothing but a fast-forward.
    FastForward,
    /// The ref must not exist.
    Absent,
    /// The ref must be at this commit.
    At(String),
}

impl Expect {
    /// `Absent` for `None`, otherwise `At`.
    pub fn at(commit: Option<&str>) -> Self {
        commit.map_or(Self::Absent, |commit| Self::At(commit.to_owned()))
    }
}

/// One ref of an atomic push.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefUpdate {
    /// The full name of the ref on the remote.
    pub remote: String,
    /// The local ref or commit to push; `None` deletes the remote ref.
    pub new: Option<String>,
    /// The compare-and-swap.
    pub expected: Expect,
}

/// Push every update or none: `git push --atomic` with a lease per ref, so a
/// transition and the lease check land together.
///
/// Git checks a lease only on refs the push changes: a ref already at its
/// new value counts as up to date and is never compared with its expected
/// value. A fence therefore moves the fencing ref to a new commit, and an
/// update whose new and expected commits are equal is refused here.
pub fn push_atomic(git_dir: &Path, url: &str, updates: &[RefUpdate], auth: &GitAuth) -> Result<()> {
    if let Some(update) = updates.iter().find(|update| {
        update.new.is_some()
            && matches!(&update.expected, Expect::At(expected) if Some(expected) == update.new.as_ref())
    }) {
        return Err(GitError::Command(format!(
            "a push that leaves {} at its expected commit is never checked by git; move it to a new commit to fence",
            update.remote
        )));
    }
    let mut args = vec!["push".to_owned(), "--porcelain".to_owned(), "--no-verify".to_owned()];
    // One ref is atomic by itself; asking for more would require servers
    // that advertise the capability even for single-branch publication.
    if updates.len() > 1 {
        args.push("--atomic".to_owned());
    }
    for update in updates {
        let expected = match &update.expected {
            Expect::FastForward => continue,
            Expect::Absent => "",
            Expect::At(commit) => commit.as_str(),
        };
        args.push(format!("--force-with-lease={}:{expected}", update.remote));
    }
    args.push("--".to_owned());
    args.push(url.to_owned());
    for update in updates {
        args.push(format!(
            "{}:{}",
            update.new.as_deref().unwrap_or_default(),
            update.remote
        ));
    }
    let args = args.iter().map(String::as_str).collect::<Vec<_>>();
    let output = command(git_dir, url, &args, auth)?;
    let (stdout, stderr, success) = output;
    if success {
        return Ok(());
    }
    let rejections = porcelain_rejections(&stdout);
    if rejections.is_empty() {
        let message = if stderr.trim().is_empty() { stdout } else { stderr };
        return Err(remote_error("git push", url, message.trim()));
    }
    Err(GitError::Rejected {
        url: sanitize_url(url),
        rejections,
    })
}

/// The refs `git push --porcelain` reports as rejected, with their reasons.
fn porcelain_rejections(stdout: &str) -> Vec<Rejection> {
    stdout
        .lines()
        .filter_map(|line| line.strip_prefix("!\t"))
        .filter_map(|line| {
            let (refs, summary) = line.split_once('\t')?;
            let reference = refs.rsplit_once(':').map_or(refs, |(_, remote)| remote);
            let reason = summary
                .split_once('(')
                .and_then(|(_, rest)| rest.strip_suffix(')'))
                .unwrap_or(summary);
            Some(Rejection {
                reference: reference.to_owned(),
                reason: reason.to_owned(),
            })
        })
        .collect()
}

fn local_ref(git_dir: &Path, reference: &str) -> Result<Option<String>> {
    let mut command = base_command(git_dir);
    let output = command
        .args(["rev-parse", "--verify", "--quiet", reference])
        .output()
        .map_err(|error| GitError::Command(format!("failed to run git rev-parse: {error}")))?;
    Ok(output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned()))
}

fn run_local(git_dir: &Path, args: &[&str]) -> Result<()> {
    let output = base_command(git_dir)
        .args(args)
        .output()
        .map_err(|error| GitError::Command(format!("failed to run git: {error}")))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(GitError::Command(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ))
    }
}

/// Run `git` against `git_dir` and return its standard output.
fn run(git_dir: &Path, url: &str, operation: &str, args: &[&str], auth: &GitAuth) -> Result<String> {
    let (stdout, stderr, success) = command(git_dir, url, args, auth)?;
    if !success {
        let message = if stderr.trim().is_empty() { &stdout } else { &stderr };
        return Err(remote_error(operation, url, message.trim()));
    }
    Ok(stdout)
}

pub(super) fn base_command(git_dir: &Path) -> Command {
    // A hooks directory that never exists: the user's hooks, such as a
    // global `core.hooksPath` pre-push hook, must not judge Nyl's own
    // fetches and pushes.
    let mut hooks_path = std::ffi::OsString::from("core.hooksPath=");
    hooks_path.push(git_dir.join("nyl-no-hooks"));
    let mut command = Command::new("git");
    command
        .arg("--git-dir")
        .arg(git_dir)
        // Fetches must not start background maintenance, a URL must never run
        // a command through the `ext::` transport, and push negotiation, which
        // some configurations enable, only adds noise to atomic pushes.
        .args([
            "-c",
            "gc.auto=0",
            "-c",
            "maintenance.auto=false",
            "-c",
            "protocol.ext.allow=never",
            "-c",
            "push.negotiate=false",
            "-c",
        ])
        .arg(hooks_path)
        .stdin(Stdio::null())
        // Fail instead of waiting for a terminal password prompt; OpenSSH
        // can still ask on the controlling terminal, as it does for `git`.
        .env("GIT_TERMINAL_PROMPT", "0");
    for variable in REPOSITORY_ENVIRONMENT {
        command.env_remove(variable);
    }
    command
}

/// Run `git` with the credentials for `url`, returning stdout, stderr, and success.
fn command(git_dir: &Path, url: &str, args: &[&str], auth: &GitAuth) -> Result<(String, String, bool)> {
    let mut command = base_command(git_dir);
    let _applied = auth.apply(url, &mut command)?;
    command.args(args);
    tracing::trace!("Running git {} for {}", args.first().unwrap_or(&""), sanitize_url(url));
    let output = command
        .output()
        .map_err(|error| GitError::Command(format!("failed to run git for {}: {error}", sanitize_url(url))))?;
    Ok((
        redact_url_credentials(&String::from_utf8_lossy(&output.stdout), url),
        String::from_utf8_lossy(&output.stderr).into_owned(),
        output.status.success(),
    ))
}

fn remote_error(operation: &str, url: &str, message: &str) -> GitError {
    GitError::RemoteFailed {
        operation: operation.to_owned(),
        url: sanitize_url(url),
        message: redact_url_credentials(message, url),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use git2::{Oid, Repository};
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

    fn none() -> GitAuth {
        GitAuth::none()
    }

    #[test]
    fn test_push_branch_if_unchanged_refuses_a_moved_branch() {
        let remote_dir = TempDir::new().unwrap();
        let remote = Repository::init_bare(remote_dir.path()).unwrap();
        let url = remote_dir.path().to_string_lossy().to_string();
        let local_dir = TempDir::new().unwrap();
        let local = Repository::init(local_dir.path()).unwrap();

        assert_eq!(remote_branch(local.path(), &url, "deploy", &none()).unwrap(), None);
        let first = commit(&local, "refs/heads/deploy", "First", &[]);
        push_branch_if_unchanged(local.path(), &url, "deploy", None, &none()).unwrap();
        assert_eq!(
            remote_branch(local.path(), &url, "deploy", &none()).unwrap(),
            Some(first.to_string())
        );

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
        let error =
            push_branch_if_unchanged(local.path(), &url, "deploy", Some(&first.to_string()), &none()).unwrap_err();
        assert!(error.is_stale(), "{error}");
        assert_eq!(remote.refname_to_id("refs/heads/deploy").unwrap(), concurrent);

        fetch(
            local.path(),
            &url,
            &["+refs/heads/deploy:refs/remotes/origin/deploy"],
            false,
            &none(),
        )
        .unwrap();
        assert_eq!(local.refname_to_id("refs/remotes/origin/deploy").unwrap(), concurrent);
        assert!(push_branch_if_unchanged(local.path(), &url, "deploy", None, &none()).is_err());
        assert_ne!(remote.refname_to_id("refs/heads/deploy").unwrap(), second);
    }

    #[test]
    fn test_atomic_push_lands_every_ref_or_none() {
        let remote_dir = TempDir::new().unwrap();
        let remote = Repository::init_bare(remote_dir.path()).unwrap();
        let url = remote_dir.path().to_string_lossy().to_string();
        let local_dir = TempDir::new().unwrap();
        let local = Repository::init_bare(local_dir.path()).unwrap();
        let state = commit(&local, "refs/heads/nyl/dev/desired", "state", &[]);
        let lease = commit(&local, "refs/nyl/dev/lease", "lease", &[]);
        let create = |reference: &str| RefUpdate {
            remote: reference.to_owned(),
            new: Some(reference.to_owned()),
            expected: Expect::Absent,
        };
        push_atomic(
            local.path(),
            &url,
            &[create("refs/heads/nyl/dev/desired"), create("refs/nyl/dev/lease")],
            &none(),
        )
        .unwrap();
        assert_eq!(remote.refname_to_id("refs/nyl/dev/lease").unwrap(), lease);

        // The state ref advances only together with a matching lease, which
        // moves to a new commit so that git checks it.
        let renewed = commit(
            &local,
            "refs/nyl/dev/lease",
            "lease renewed",
            &[&local.find_commit(lease).unwrap()],
        );
        let next = commit(
            &local,
            "refs/heads/nyl/dev/desired",
            "next",
            &[&local.find_commit(state).unwrap()],
        );
        let leased = |expected: Oid| {
            vec![
                RefUpdate {
                    remote: "refs/heads/nyl/dev/desired".to_owned(),
                    new: Some(next.to_string()),
                    expected: Expect::At(state.to_string()),
                },
                RefUpdate {
                    remote: "refs/nyl/dev/lease".to_owned(),
                    new: Some(renewed.to_string()),
                    expected: Expect::At(expected.to_string()),
                },
            ]
        };
        let error = push_atomic(local.path(), &url, &leased(state), &none()).unwrap_err();
        assert!(error.is_stale(), "{error}");
        assert_eq!(remote.refname_to_id("refs/heads/nyl/dev/desired").unwrap(), state);
        assert_eq!(remote.refname_to_id("refs/nyl/dev/lease").unwrap(), lease);
        // A lease that would not move is refused rather than silently unchecked.
        let unchecked = RefUpdate {
            remote: "refs/nyl/dev/lease".to_owned(),
            new: Some(lease.to_string()),
            expected: Expect::At(lease.to_string()),
        };
        assert!(push_atomic(local.path(), &url, &[unchecked], &none()).is_err());
        push_atomic(local.path(), &url, &leased(lease), &none()).unwrap();
        assert_eq!(remote.refname_to_id("refs/heads/nyl/dev/desired").unwrap(), next);

        let ls = ls_remote(local.path(), &url, &["refs/nyl/dev/*"], &none()).unwrap();
        assert_eq!(ls.get("refs/nyl/dev/lease"), Some(&renewed.to_string()));

        // Deleting is a compare-and-swap too.
        push_atomic(
            local.path(),
            &url,
            &[RefUpdate {
                remote: "refs/nyl/dev/lease".to_owned(),
                new: None,
                expected: Expect::At(renewed.to_string()),
            }],
            &none(),
        )
        .unwrap();
        assert!(remote.refname_to_id("refs/nyl/dev/lease").is_err());
    }

    #[test]
    fn test_fetch_reports_the_remote_message_with_credentials_redacted() {
        let local_dir = TempDir::new().unwrap();
        let local = Repository::init_bare(local_dir.path()).unwrap();
        let url = format!(
            "file://user:token@localhost{}/missing",
            local_dir.path().to_string_lossy()
        );
        let error = fetch(local.path(), &url, &["+refs/heads/*:refs/heads/*"], true, &none())
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
        assert!(fetch(local.path(), &url, &["+refs/heads/*:refs/heads/*"], false, &none()).is_err());
        assert!(remote_branch(local.path(), &url, "main", &none()).is_err());
        assert!(push_branch_if_unchanged(local.path(), &url, "main", None, &none()).is_err());
        assert!(!marker.exists());
    }

    #[cfg(unix)]
    #[test]
    fn test_push_ignores_the_users_git_hooks() {
        use std::os::unix::fs::PermissionsExt;

        let remote_dir = TempDir::new().unwrap();
        Repository::init_bare(remote_dir.path()).unwrap();
        let url = remote_dir.path().to_string_lossy().to_string();
        let local_dir = TempDir::new().unwrap();
        let local = Repository::init(local_dir.path()).unwrap();
        let hook = local.path().join("hooks/pre-push");
        std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
        std::fs::write(&hook, "#!/bin/sh\nexit 1\n").unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();

        let commit = commit(&local, "refs/heads/deploy", "First", &[]);
        push_branch_if_unchanged(local.path(), &url, "deploy", None, &none()).unwrap();
        assert_eq!(
            remote_branch(local.path(), &url, "deploy", &none()).unwrap(),
            Some(commit.to_string())
        );
    }
}
