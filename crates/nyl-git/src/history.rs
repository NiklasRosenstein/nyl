//! Questions about a repository's commit history that promotion asks: which
//! commits of one history another lacks by their changes, and which commits a
//! revision range names.
//!
//! Contract: [Several paths into one environment](../../../design/promotion.md#several-paths-into-one-environment),
//! "Rule" (decision D5 of the M6 plan). A commit V *contains* a commit R when
//! every commit reachable from R and not from V, merge commits aside, has a
//! commit with the same changes (`git patch-id`) in V's history since their
//! merge base. That is `git cherry V R` listing no `+` line, so
//! [`missing_commits`] runs exactly that, once, whatever the number of
//! commits: Git computes every patch-id in the one process.

use std::path::Path;

use crate::error::{GitError, Result};
use crate::transport::repository_command;

/// A commit of the record's history whose changes the value's history lacks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingCommit {
    /// The full commit ID.
    pub commit: String,
    /// The first line of its message.
    pub subject: String,
    /// Whether it has no parent, so no `<commit>^` names the history before it.
    pub root: bool,
}

/// The commits reachable from `record` and not from `value` that have no
/// patch-equivalent commit in `value`'s history since their merge base:
/// the `+` lines of `git cherry <value> <record>`, in topological order,
/// oldest first (`git rev-list --topo-order --reverse`). Merge commits are
/// skipped, as `git cherry` skips them. Empty means `value` contains
/// `record`: an ancestor, clean cherry-picks, and a single-commit squash do;
/// a squash of several commits is equivalent to none of them.
///
/// Both commits must be present in the repository at `git_dir`. Two `git`
/// processes, whatever the number of commits.
pub fn missing_commits(git_dir: &Path, value: &str, record: &str) -> Result<Vec<MissingCommit>> {
    for commit in [value, record] {
        if commit.starts_with('-') {
            return Err(GitError::Command(format!("{commit:?} is not a commit")));
        }
    }
    let cherry = git(
        git_dir,
        &["cherry", "-v", "--end-of-options", value, record],
        "git cherry",
    )?;
    let mut subjects = std::collections::BTreeMap::new();
    for line in cherry.lines() {
        let Some(rest) = line.strip_prefix("+ ") else {
            continue;
        };
        let (commit, subject) = rest.split_once(' ').unwrap_or((rest, ""));
        subjects.insert(commit.to_owned(), subject.to_owned());
    }
    if subjects.is_empty() {
        return Ok(Vec::new());
    }
    let order = git(
        git_dir,
        &[
            "rev-list",
            "--topo-order",
            "--reverse",
            "--parents",
            "--end-of-options",
            record,
            &format!("^{value}"),
            "--",
        ],
        "git rev-list",
    )?;
    let mut missing = Vec::new();
    for line in order.lines() {
        let mut ids = line.split_whitespace();
        let Some(commit) = ids.next() else { continue };
        if let Some(subject) = subjects.remove(commit) {
            missing.push(MissingCommit {
                commit: commit.to_owned(),
                subject,
                root: ids.next().is_none(),
            });
        }
    }
    Ok(missing)
}

/// The commits a revision range names, newest first: `A..B` is every commit
/// reachable from `B` and not from `A` (`git rev-list A..B`), and any other
/// form names exactly one commit. An empty range is no commit; a revision
/// that names no commit, or an end that starts with `-`, is an error.
pub fn commits_in(git_dir: &Path, range: &str) -> Result<Vec<String>> {
    let not_revision = || GitError::Command(format!("{range:?} is not a revision or a range <from>..<to>"));
    if let Some((from, to)) = range.split_once("..") {
        if from.is_empty() || to.is_empty() || to.starts_with('.') || from.starts_with('-') || to.starts_with('-') {
            return Err(not_revision());
        }
        let from = format!("^{from}^{{commit}}");
        let to = format!("{to}^{{commit}}");
        let stdout = git(
            git_dir,
            &["rev-list", "--end-of-options", &to, &from, "--"],
            "git rev-list",
        )?;
        return Ok(stdout.lines().map(str::to_owned).collect());
    }
    if range.is_empty() || range.starts_with('-') {
        return Err(not_revision());
    }
    let stdout = git(
        git_dir,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            "--end-of-options",
            &format!("{range}^{{commit}}"),
        ],
        "git rev-parse",
    )?;
    Ok(stdout.lines().map(str::to_owned).collect())
}

fn git(git_dir: &Path, args: &[&str], what: &str) -> Result<String> {
    let output = repository_command(git_dir)
        .args(args)
        .output()
        .map_err(|error| GitError::Command(format!("failed to run {what}: {error}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let message = if stderr.trim().is_empty() {
            format!(
                "{what} found no commit for {}",
                args.last().copied().unwrap_or_default()
            )
        } else {
            format!("{what} failed: {}", stderr.trim())
        };
        return Err(GitError::Command(message));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use tempfile::TempDir;

    use super::*;

    /// A repository driven by the `git` command line with a fixed identity
    /// and no global or system configuration.
    struct Repo {
        directory: TempDir,
    }

    impl Repo {
        fn new() -> Self {
            let repo = Self {
                directory: TempDir::new().unwrap(),
            };
            repo.git(&["init", "-q", "-b", "main", "."]);
            repo.commit_file("base", "README", "base\n");
            repo
        }

        fn git(&self, args: &[&str]) -> String {
            let output = Command::new("git")
                .current_dir(self.directory.path())
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_AUTHOR_NAME", "Nyl Test")
                .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
                .env("GIT_COMMITTER_NAME", "Nyl Test")
                .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8_lossy(&output.stdout).trim().to_owned()
        }

        fn git_dir(&self) -> std::path::PathBuf {
            self.directory.path().join(".git")
        }

        fn commit_file(&self, message: &str, file: &str, content: &str) -> String {
            std::fs::write(self.directory.path().join(file), content).unwrap();
            self.git(&["add", file]);
            self.git(&["commit", "-q", "-m", message]);
            self.head()
        }

        fn head(&self) -> String {
            self.git(&["rev-parse", "HEAD"])
        }

        fn checkout(&self, args: &[&str]) {
            let mut all = vec!["checkout", "-q"];
            all.extend_from_slice(args);
            self.git(&all);
        }

        /// A hotfix branch `release` from `main` with two fix commits, and
        /// `main` moved on with work of its own. Returns the two fixes.
        fn hotfix(&self) -> (String, String) {
            self.checkout(&["-b", "release"]);
            let pool = self.commit_file("Fix pool size", "pool", "20\n");
            let timeout = self.commit_file("Bump timeout", "timeout", "30s\n");
            self.checkout(&["main"]);
            self.commit_file("Start release 2", "about", "two\n");
            (pool, timeout)
        }

        fn missing(&self, value: &str, record: &str) -> Vec<String> {
            missing_commits(&self.git_dir(), value, record)
                .unwrap()
                .into_iter()
                .map(|missing| missing.subject)
                .collect()
        }
    }

    /// A value that has the record's commit as an ancestor contains it.
    /// Catches a check that compares commit IDs for equality only.
    #[test]
    fn test_missing_commits_none_for_an_ancestor() {
        let repo = Repo::new();
        let record = repo.head();
        repo.commit_file("Later", "later", "x\n");
        assert!(repo.missing(&repo.head(), &record).is_empty());
    }

    /// Both fix commits are missing from main, oldest first with their
    /// subjects; a clean cherry-pick of one counts by its patch-id, so only
    /// the other stays missing, and cherry-picking both (in either order)
    /// leaves none. Catches containment by ancestry alone, which would refuse
    /// every cherry-picked hotfix.
    #[test]
    fn test_missing_commits_counts_cherry_picks_by_patch_id() {
        let repo = Repo::new();
        let (pool, timeout) = repo.hotfix();
        let missing = missing_commits(&repo.git_dir(), "main", "release").unwrap();
        assert_eq!(
            missing,
            vec![
                MissingCommit {
                    commit: pool.clone(),
                    subject: "Fix pool size".to_owned(),
                    root: false,
                },
                MissingCommit {
                    commit: timeout.clone(),
                    subject: "Bump timeout".to_owned(),
                    root: false,
                },
            ]
        );
        repo.git(&["cherry-pick", &timeout]);
        assert_eq!(repo.missing("main", "release"), ["Fix pool size"]);
        repo.git(&["cherry-pick", &pool]);
        assert!(repo.missing("main", "release").is_empty());
    }

    /// A squash of one commit has that commit's patch-id and contains it; a
    /// squash of two is equivalent to neither, so both stay missing. Catches
    /// a check that compares trees or diffs of whole ranges.
    #[test]
    fn test_missing_commits_single_squash_passes_and_multi_commit_squash_fails() {
        let repo = Repo::new();
        repo.checkout(&["-b", "one"]);
        repo.commit_file("Only fix", "only", "1\n");
        repo.checkout(&["main"]);
        repo.git(&["merge", "-q", "--squash", "one"]);
        repo.git(&["commit", "-q", "-m", "Squash one"]);
        assert!(repo.missing("main", "one").is_empty());

        repo.hotfix();
        repo.git(&["merge", "-q", "--squash", "release"]);
        repo.git(&["commit", "-q", "-m", "Squash release"]);
        assert_eq!(repo.missing("main", "release"), ["Fix pool size", "Bump timeout"]);
    }

    /// A merge commit makes the fix commits ancestors, so none is missing,
    /// and the merge commit itself is never listed as missing from the
    /// other side. Catches merges counted as commits with changes.
    #[test]
    fn test_missing_commits_merge_back_passes_and_skips_merge_commits() {
        let repo = Repo::new();
        repo.hotfix();
        repo.git(&["merge", "-q", "--no-ff", "--no-edit", "release"]);
        assert!(repo.missing("main", "release").is_empty());
        // The other direction lists main's own work, never the merge commit.
        assert_eq!(repo.missing("release", "main"), ["Start release 2"]);
    }

    /// The rule is symmetric: a hotfix cut before main moved on lacks main's
    /// work. Catches a check that only ever looks from main at a hotfix.
    #[test]
    fn test_missing_commits_is_symmetric() {
        let repo = Repo::new();
        repo.hotfix();
        assert_eq!(repo.missing("release", "main"), ["Start release 2"]);
        assert_eq!(repo.missing("main", "release"), ["Fix pool size", "Bump timeout"]);
    }

    /// Missing commits come in topological order, and the first commit of
    /// a history says it is a root, so a refusal never offers `<root>^`.
    /// Catches a copyable range that Git cannot resolve.
    #[test]
    fn test_missing_commits_mark_a_root_commit() {
        let repo = Repo::new();
        let root = repo.head();
        repo.git(&["checkout", "-q", "--orphan", "other"]);
        repo.git(&["rm", "-q", "-r", "--cached", "."]);
        repo.commit_file("Other root", "other", "o\n");
        let missing = missing_commits(&repo.git_dir(), "other", "main").unwrap();
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].commit, root);
        assert!(missing[0].root);
    }

    /// A commit the repository does not hold is an error, never "nothing
    /// missing". Catches a refusal silently turned into a pass.
    #[test]
    fn test_missing_commits_fails_for_an_unknown_commit() {
        let repo = Repo::new();
        assert!(missing_commits(&repo.git_dir(), "main", &"a".repeat(40)).is_err());
    }

    /// A range names the commits between its ends, newest first; a single
    /// revision names one commit; an unknown revision is an error. Catches
    /// `--supersede S1..c3d4` resolved to every ancestor of `c3d4`.
    #[test]
    fn test_commits_in_resolves_ranges_and_single_commits() {
        let repo = Repo::new();
        let base = repo.head();
        let (pool, timeout) = repo.hotfix();
        assert_eq!(
            commits_in(&repo.git_dir(), &format!("{base}..release")).unwrap(),
            [timeout.clone(), pool.clone()]
        );
        assert_eq!(commits_in(&repo.git_dir(), "release~1").unwrap(), [pool]);
        assert!(commits_in(&repo.git_dir(), "release..release").unwrap().is_empty());
        assert!(commits_in(&repo.git_dir(), "no-such-branch").is_err());
        assert!(commits_in(&repo.git_dir(), "..release").is_err());
        assert!(commits_in(&repo.git_dir(), "--all").is_err());
        assert!(commits_in(&repo.git_dir(), "--all..release").is_err());
        assert!(commits_in(&repo.git_dir(), "main..--all").is_err());
    }
}
