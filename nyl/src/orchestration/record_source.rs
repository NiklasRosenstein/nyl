//! The `record: source` writer: `nyl promote` commits the PromotionRecord
//! into the target Environment's `source.fromPromotion.promoted` block.
//!
//! Contract: [Promoting an environment's source](../../../design/promotion.md#promoting-an-environments-source)
//! and decision D3 of the [M6 plan](../../../design/m6-plan.md#open-decisions):
//! without a change gate the block is one commit on the branch the entry
//! worktree has checked out, pushed to that branch's upstream (its
//! `branch.<name>.remote` and `branch.<name>.merge`) fast-forward only. This
//! module performs those effects for the shell's [`PromotionRecordWriter`];
//! what to write and when is the shell's.
//!
//! - Before anything is written, and for `--dry-run` alone
//!   ([`PromotionRecordWriter::check`]): a branch is checked out (a detached
//!   HEAD is refused), it has an upstream and no commits the upstream lacks
//!   (the push would publish them with the record), the Environment's file is
//!   tracked and has no uncommitted change, and the block can be placed.
//! - The commit carries the Environment's file and nothing else; other staged
//!   or changed files stay as they are. Its author and committer are the
//!   invocation's identity, set explicitly; the user's Git configuration
//!   otherwise applies, so a configured signature still signs it.
//! - The block is placed in the Environment's document in block style,
//!   replacing an earlier block, keeping every other line and the file's line
//!   endings. The edited document must read back as exactly the record.
//! - The push never forces. When the remote refuses it, the commit is undone
//!   only while the branch still points at it, and the file is restored only
//!   while it still holds what was written; an undo that cannot finish is
//!   reported. A push that fails without a refusal, such as a network error,
//!   may have reached the remote, so the commit is kept and reported.

use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::Arc;

use nyl_core::orchestration::ids::CommitId;
use nyl_core::orchestration::ports::{Clock, CommitIdentity, PromotionRecordWriter, RecordWriteError};
use nyl_core::orchestration::promotion::PromotionRecord;
use nyl_git::{Expect, GitAuth, RefUpdate};

/// Commits a `record: source` block in the entry worktree and pushes it.
pub struct GitRecordWriter {
    /// The `.git` directory of the entry clone.
    pub git_dir: PathBuf,
    /// The root of the entry worktree.
    pub worktree: PathBuf,
    /// The file declaring the Environment, absolute.
    pub file: PathBuf,
    /// The Environment's one-based document index in `file`.
    pub document_index: usize,
    /// The Environment's document as discovery read it.
    pub document: String,
    /// The commit's author and committer; `None` only checks (a dry run).
    pub identity: Option<CommitIdentity>,
    pub clock: Arc<dyn Clock>,
    pub auth: GitAuth,
    /// Extra environment of every local `git` the writer runs, such as an
    /// isolated configuration in tests; empty in production.
    pub git_env: Vec<(String, String)>,
}

/// What a write starts from, found without writing anything.
struct Ready {
    /// The checked-out branch, such as `refs/heads/main`.
    branch: String,
    /// The upstream's push URL and its branch on the remote.
    url: String,
    merge: String,
    /// The local ref tracking the upstream, as Git derives it from the
    /// remote's fetch refspec.
    tracking: String,
    /// The Environment's file as a literal pathspec, and as it is named.
    pathspec: String,
    relative: String,
    head: String,
    contents: String,
    edited: String,
}

impl PromotionRecordWriter for GitRecordWriter {
    fn check(&self, record: &PromotionRecord) -> Result<(), RecordWriteError> {
        self.ready(record).map(drop)
    }

    fn commit_record(&self, record: &PromotionRecord, message: &str) -> Result<CommitId, RecordWriteError> {
        let identity = self
            .identity
            .as_ref()
            .ok_or_else(|| RecordWriteError::Failed("no commit identity to write the record with".to_owned()))?;
        let ready = self.ready(record)?;
        crate::cli::resource_file::atomic_replace(&self.file, &ready.contents, &ready.edited)
            .map_err(|error| RecordWriteError::Failed(error.to_string()))?;

        let date = format!("@{} +0000", self.clock.now().timestamp());
        let committed = self
            .command()
            .env("GIT_AUTHOR_NAME", &identity.name)
            .env("GIT_AUTHOR_EMAIL", &identity.email)
            .env("GIT_COMMITTER_NAME", &identity.name)
            .env("GIT_COMMITTER_EMAIL", &identity.email)
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_DATE", &date)
            .args([
                "commit",
                "--quiet",
                "--no-verify",
                "--only",
                "--cleanup=verbatim",
                "--message",
                message,
                "--",
                &ready.pathspec,
            ])
            .output();
        let failure = match committed {
            Ok(output) if output.status.success() => None,
            Ok(output) => Some(format!(
                "git commit failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )),
            Err(error) => Some(format!("cannot run git: {error}")),
        };
        if let Some(failure) = failure {
            return Err(RecordWriteError::Failed(match self.restore_file(&ready) {
                Ok(()) => failure,
                Err(restore) => format!("{failure}; {restore}"),
            }));
        }
        let commit = self
            .git(&["rev-parse", "--verify", "HEAD"])
            .map_err(RecordWriteError::Failed)?;
        let pushed = nyl_git::push_atomic(
            &self.git_dir,
            &ready.url,
            &[RefUpdate {
                remote: ready.merge.clone(),
                new: Some(commit.clone()),
                expected: Expect::FastForward,
            }],
            &self.auth,
        );
        match pushed {
            Ok(()) => {
                // The tracking ref follows, as a `git push` to the remote would move it.
                let _ = self.git(&["update-ref", &ready.tracking, &commit, &ready.head]);
                CommitId::new(commit).map_err(|error| RecordWriteError::Failed(error.to_string()))
            }
            Err(nyl_git::GitError::Rejected { rejections, url }) => {
                let message = nyl_git::GitError::Rejected {
                    rejections: rejections.clone(),
                    url,
                }
                .to_string();
                if let Err(undo) = self.undo(&ready, &commit) {
                    return Err(RecordWriteError::UndoFailed {
                        commit,
                        message: format!("{message}; {undo}"),
                    });
                }
                let moved = rejections.iter().any(|rejection| {
                    rejection.reason.starts_with("non-fast-forward") || rejection.reason == "fetch first"
                });
                Err(if moved {
                    RecordWriteError::Rejected(message)
                } else {
                    RecordWriteError::RemoteRejected(message)
                })
            }
            Err(error) => Err(RecordWriteError::PushUnknown {
                commit,
                message: error.to_string(),
            }),
        }
    }
}

impl GitRecordWriter {
    /// Everything a write needs, checked without writing.
    fn ready(&self, record: &PromotionRecord) -> Result<Ready, RecordWriteError> {
        let failed = RecordWriteError::Failed;
        let head_ref = self.run(&["symbolic-ref", "--quiet", "HEAD"]).map_err(failed)?;
        let branch = match head_ref.status.code() {
            Some(0) => String::from_utf8_lossy(&head_ref.stdout).trim().to_owned(),
            // `--quiet` exits 1, silently, when HEAD is not a symbolic ref.
            Some(1) => return Err(RecordWriteError::Detached),
            _ => {
                return Err(failed(format!(
                    "cannot read the checked-out branch: {}",
                    String::from_utf8_lossy(&head_ref.stderr).trim()
                )))
            }
        };
        let name = branch
            .strip_prefix("refs/heads/")
            .ok_or(RecordWriteError::Detached)?
            .to_owned();
        let config = |key: &str| self.git(&["config", "--get", &format!("branch.{name}.{key}")]).ok();
        let (remote, merge) = match (config("remote"), config("merge")) {
            (Some(remote), Some(merge)) if !remote.is_empty() && remote != "." => (remote, merge),
            _ => return Err(RecordWriteError::NoUpstream { branch: name }),
        };
        let tracking = self
            .git(&["rev-parse", "--symbolic-full-name", &format!("{name}@{{upstream}}")])
            .ok()
            .filter(|tracking| !tracking.is_empty())
            .ok_or_else(|| RecordWriteError::NoUpstream { branch: name.clone() })?;
        let url = self
            .git(&["remote", "get-url", "--push", &remote])
            .map_err(|error| failed(format!("remote {remote} of branch {name} has no URL: {error}")))?;
        let file = std::fs::canonicalize(&self.file)
            .map_err(|error| failed(format!("cannot read {}: {error}", self.file.display())))?;
        let relative = file
            .strip_prefix(&self.worktree)
            .map_err(|_| failed(format!("{} is outside the worktree", self.file.display())))?
            .to_string_lossy()
            .replace('\\', "/");
        let pathspec = format!(":(literal){relative}");
        if self.git(&["ls-files", "--error-unmatch", "--", &pathspec]).is_err() {
            return Err(RecordWriteError::Untracked { file: relative });
        }
        let changed = self.git(&["status", "--porcelain", "--", &pathspec]).map_err(failed)?;
        if !changed.is_empty() {
            return Err(RecordWriteError::Dirty { file: relative });
        }
        let ahead = self
            .git(&["rev-list", "--count", &format!("{tracking}..HEAD")])
            .map_err(failed)?;
        let commits: u64 = ahead.parse().unwrap_or(0);
        if commits > 0 {
            return Err(RecordWriteError::Ahead {
                branch: name,
                upstream: tracking,
                commits,
            });
        }
        let head = self.git(&["rev-parse", "--verify", "HEAD"]).map_err(failed)?;
        let contents =
            std::fs::read_to_string(&self.file).map_err(|error| failed(format!("cannot read {relative}: {error}")))?;
        let document = place_block(&self.document, record).map_err(RecordWriteError::Unplaceable)?;
        let edited =
            crate::cli::resource_file::replace_document(&contents, self.document_index, &self.document, &document)
                .map_err(|error| RecordWriteError::Unplaceable(error.to_string()))?;
        Ok(Ready {
            branch,
            url,
            merge,
            tracking,
            pathspec,
            relative,
            head,
            contents,
            edited,
        })
    }

    /// Undo the promotion commit after a refused push: the branch moves back
    /// only while it still points at `commit`, and the index entry and the
    /// file are restored only while that branch is checked out and the file
    /// holds what was written.
    fn undo(&self, ready: &Ready, commit: &str) -> Result<(), String> {
        self.git(&[
            "update-ref",
            "-m",
            "nyl: undo a promotion commit whose push was refused",
            &ready.branch,
            &ready.head,
            commit,
        ])
        .map_err(|error| format!("the branch no longer points at the promotion commit: {error}"))?;
        if self.git(&["symbolic-ref", "--quiet", "HEAD"]).ok().as_deref() != Some(ready.branch.as_str()) {
            return Ok(());
        }
        self.git(&["reset", "--quiet", &ready.head, "--", &ready.pathspec])
            .map_err(|error| format!("cannot restore the index entry of {}: {error}", ready.relative))?;
        self.restore_file(ready)
    }

    /// Put the Environment's file back while it holds what was written.
    fn restore_file(&self, ready: &Ready) -> Result<(), String> {
        let current = std::fs::read_to_string(&self.file).map_err(|error| error.to_string())?;
        if current != ready.edited {
            return Err(format!(
                "{} changed after the record was written, so it was left as it is",
                ready.relative
            ));
        }
        crate::cli::resource_file::atomic_replace(&self.file, &ready.edited, &ready.contents)
            .map_err(|error| format!("cannot restore {}: {error}", ready.relative))
    }

    /// `git` bound to the entry clone and its worktree, without the user's hooks.
    fn command(&self) -> Command {
        let mut command = nyl_git::repository_command(&self.git_dir);
        command
            .arg("--work-tree")
            .arg(&self.worktree)
            .current_dir(&self.worktree)
            .envs(self.git_env.iter().map(|(key, value)| (key.as_str(), value.as_str())));
        command
    }

    fn run(&self, args: &[&str]) -> Result<Output, String> {
        self.command()
            .args(args)
            .output()
            .map_err(|error| format!("cannot run git: {error}"))
    }

    /// Run `git` and return its trimmed stdout, or its stderr as the error.
    fn git(&self, args: &[&str]) -> Result<String, String> {
        let output = self.run(args)?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
        } else {
            Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
        }
    }
}

/// `document` (an Environment, in block style) with `record` as its
/// `spec.source.fromPromotion.promoted` block, replacing an earlier one and
/// keeping every other line as written. The result must read back as exactly
/// `record`.
pub fn place_block(document: &str, record: &PromotionRecord) -> Result<String, String> {
    let declared = nyl_render::yaml::parse_yaml_value_k8s_compatible(document)
        .map_err(|error| format!("the Environment does not parse: {error}"))?;
    if declared.pointer("/spec/source/fromPromotion").is_none() {
        return Err(
            "the Environment's document does not declare `spec.source.fromPromotion`: it is inherited, such as from its EnvironmentGroup, and the block belongs in the Environment's own document"
                .to_owned(),
        );
    }
    let newline = if document.contains("\r\n") { "\r\n" } else { "\n" };
    let lines: Vec<&str> = document.split_inclusive('\n').collect();
    let (from_promotion, parent) = from_promotion_line(&lines)?;
    // The children of `fromPromotion`: where they end, their indentation, and an earlier block.
    let mut end = from_promotion + 1;
    let mut child = None;
    let mut promoted: Option<(usize, usize)> = None;
    let mut index = from_promotion + 1;
    while index < lines.len() {
        let line = lines[index];
        if blank(line) {
            index += 1;
            continue;
        }
        let at = indent(line);
        if at <= parent {
            break;
        }
        let child_indent = *child.get_or_insert(at);
        if at == child_indent && block_key(line.trim_start()) == Some("promoted") {
            let mut last = index + 1;
            let mut next = index + 1;
            while next < lines.len() && (blank(lines[next]) || indent(lines[next]) > child_indent) {
                if !blank(lines[next]) {
                    last = next + 1;
                }
                next += 1;
            }
            promoted = Some((index, last));
            index = next;
            end = last;
            continue;
        }
        index += 1;
        end = index;
    }
    let child = child.ok_or_else(unplaceable)?;
    let yaml = nyl_render::yaml::serialize_yaml_document(record)
        .map_err(|error| format!("cannot write the record as YAML: {error}"))?;
    let pad = " ".repeat(child + 2);
    let mut block = format!(
        "{}promoted: # written by nyl promote, never by hand{newline}",
        " ".repeat(child)
    );
    for line in yaml.lines().filter(|line| *line != "---") {
        if !line.is_empty() {
            block.push_str(&pad);
            block.push_str(line);
        }
        block.push_str(newline);
    }
    let mut output = String::with_capacity(document.len() + block.len());
    let (cut_start, cut_end) = promoted.unwrap_or((end, end));
    for line in &lines[..cut_start] {
        output.push_str(line);
    }
    if !output.is_empty() && !output.ends_with('\n') {
        output.push_str(newline);
    }
    output.push_str(&block);
    for line in &lines[cut_end..] {
        output.push_str(line);
    }
    check_placed(document, &output, record)?;
    Ok(output)
}

fn unplaceable() -> String {
    "cannot place the block: write `spec.source.fromPromotion` of the Environment in block style, one key per line"
        .to_owned()
}

fn indent(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// A line with nothing but whitespace or a comment.
fn blank(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.is_empty() || trimmed.starts_with('#')
}

/// The line of `spec.source.fromPromotion` in block style, and its indentation.
fn from_promotion_line(lines: &[&str]) -> Result<(usize, usize), String> {
    let mut start = 0;
    let mut parent: Option<usize> = None;
    let mut found = None;
    for key in ["spec", "source", "fromPromotion"] {
        let mut child = None;
        let mut hit = None;
        for (index, line) in lines.iter().enumerate().skip(start) {
            if blank(line) {
                continue;
            }
            let at = indent(line);
            if parent.is_some_and(|parent| at <= parent) {
                break;
            }
            if *child.get_or_insert(at) == at && block_key(line.trim_start()) == Some(key) {
                hit = Some((index, at));
                break;
            }
        }
        let (index, at) = hit.ok_or_else(unplaceable)?;
        // A key that holds a block mapping has nothing but a comment after its colon.
        let rest = lines[index].trim_start()[key.len()..].trim_start_matches(':').trim();
        if !(rest.is_empty() || rest.starts_with('#')) {
            return Err(unplaceable());
        }
        start = index + 1;
        parent = Some(at);
        found = Some((index, at));
    }
    found.ok_or_else(unplaceable)
}

/// `output` must read back as `document` with `record` as its block and
/// nothing else changed.
fn check_placed(document: &str, output: &str, record: &PromotionRecord) -> Result<(), String> {
    let read: serde_json::Value = nyl_render::yaml::parse_yaml_value_k8s_compatible(output)
        .map_err(|error| format!("the edited document does not parse: {error}"))?;
    let original: serde_json::Value = nyl_render::yaml::parse_yaml_value_k8s_compatible(document)
        .map_err(|error| format!("the Environment does not parse: {error}"))?;
    let placed = read
        .pointer("/spec/source/fromPromotion/promoted")
        .cloned()
        .and_then(|value| serde_json::from_value::<PromotionRecord>(value).ok());
    let mut rest = read;
    let mut before = original;
    for value in [&mut rest, &mut before] {
        if let Some(promoted) = value
            .pointer_mut("/spec/source/fromPromotion")
            .and_then(serde_json::Value::as_object_mut)
        {
            promoted.remove("promoted");
        }
    }
    if placed.as_ref() != Some(record) || rest != before {
        return Err(unplaceable());
    }
    Ok(())
}

/// The key of a block mapping line, such as `source` of `source:  # …`.
fn block_key(line: &str) -> Option<&str> {
    let (key, _) = line.split_once(':')?;
    let key = key.trim().trim_matches(|quote| quote == '"' || quote == '\'');
    (!key.is_empty() && !key.contains([' ', '{', '[', '#'])).then_some(key)
}

/// The writer for the Environment `session` works on, committing as
/// `identity` (`None` for a dry run, which only checks), or why it cannot
/// have one: an Environment no file of the project declares, such as a
/// template's instance, has no block to write.
pub fn writer_for(
    session: &super::session::Session,
    identity: Option<CommitIdentity>,
    clock: Arc<dyn Clock>,
) -> Result<GitRecordWriter, RecordWriteError> {
    let name = session.resolved.name.as_str();
    let resource = session
        .inventory
        .get(nyl_render::resources::GitOpsResourceKind::Environment, name)
        .ok_or_else(|| {
            RecordWriteError::Undeclared(format!(
                "Environment {name} is not declared in a file of this project, such as an instance of an EnvironmentTemplate, so there is no document to keep its record in"
            ))
        })?;
    let auth = crate::cli::git_auth::project_git_auth(&session.inventory.project_config.config)
        .map_err(|error| RecordWriteError::Failed(error.to_string()))?;
    Ok(GitRecordWriter {
        git_dir: session.resolved.git_dir.clone(),
        worktree: session.inventory.worktree_root.clone(),
        file: session.inventory.project_root.join(&resource.source_path),
        document_index: resource.document_index,
        document: resource.raw_document.clone(),
        identity,
        clock,
        auth,
        git_env: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use nyl_core::orchestration::ports::FrozenClock;
    use tempfile::TempDir;

    use super::*;

    const ENVIRONMENT: &str = "apiVersion: gitops.nyl/v1
kind: Environment
metadata:
  name: prod
spec:
  source:
    fromPromotion:
      path: dev-to-prod
      record: source                  # nyl promote writes the record here
  unitSelector:
    matchLabels: {prod: 'true'}      # kubernetes only
";

    fn record(sequence: u64) -> PromotionRecord {
        serde_json::from_value(serde_json::json!({
            "path": "dev-to-prod", "sequence": sequence,
            "from": {"environment": "dev", "run": "0192f0c4-8d0a-7c3e-9a51-3c1f0e7d2b90", "desiredCommit": "3".repeat(40)},
            "sourceCommit": "5".repeat(40),
            "values": {"webImage": {
                "value": "registry.invalid:5000/web@sha256:abc",
                "select": {"unit": "web-image", "artifact": "image", "pointer": "/reference"},
                "unit": {"name": "web-image", "uid": "0192f0c4-8d0a-7c3e-9a51-3c1f0e7d2b91"},
                "executionKey": format!("sha256:{}", "0123456789abcdef".repeat(4))
            }},
            "evidence": {"level": "published", "attestations": [], "override": null},
            "ignoredAttestations": [], "excludedApplications": [], "superseded": [],
            "artifactsVerified": true,
            "requested": {"by": "Nyl Test <test@example.invalid>", "identity": "local", "at": "2026-09-25T16:41:00Z", "reason": "Release: 1 # two"}
        }))
        .unwrap()
    }

    fn promoted(document: &str) -> PromotionRecord {
        let value = nyl_render::yaml::parse_yaml_value_k8s_compatible(document).unwrap();
        serde_json::from_value(value.pointer("/spec/source/fromPromotion/promoted").unwrap().clone()).unwrap()
    }

    /// The block lands under `fromPromotion`, a later promotion replaces it,
    /// and every other line (comments, flow mappings, the keys after it)
    /// stays as written. Catches a promotion that rewrites the user's
    /// Environment, or one that appends a second block.
    #[test]
    fn test_place_block_writes_and_replaces_only_the_block() {
        let first = place_block(ENVIRONMENT, &record(1)).unwrap();
        assert_eq!(promoted(&first), record(1));
        assert!(first.contains("      record: source                  # nyl promote writes the record here\n"));
        assert!(
            first.ends_with("  unitSelector:\n    matchLabels: {prod: 'true'}      # kubernetes only\n"),
            "{first}"
        );
        let second = place_block(&first, &record(2)).unwrap();
        assert_eq!(promoted(&second), record(2));
        assert_eq!(second.matches("promoted:").count(), 1, "{second}");
        assert_eq!(
            second.replace(&first, ""),
            second,
            "the block was replaced, not appended"
        );
        assert_eq!(second.lines().count(), first.lines().count());
    }

    /// A `fromPromotion` in flow style has no line to place a block under:
    /// refused rather than rewritten.
    #[test]
    fn test_place_block_refuses_a_flow_style_source() {
        let flow = ENVIRONMENT.replace(
            "    fromPromotion:\n      path: dev-to-prod\n      record: source                  # nyl promote writes the record here\n",
            "    fromPromotion: {path: dev-to-prod, record: source}\n",
        );
        let error = place_block(&flow, &record(1)).unwrap_err();
        assert!(error.contains("block style"), "{error}");
    }

    /// A file with CRLF line endings keeps them in the lines it gains.
    #[test]
    fn test_place_block_keeps_crlf_line_endings() {
        let crlf = ENVIRONMENT.replace('\n', "\r\n");
        let placed = place_block(&crlf, &record(1)).unwrap();
        assert_eq!(promoted(&placed), record(1));
        assert_eq!(
            placed.matches('\n').count(),
            placed.matches("\r\n").count(),
            "{placed:?}"
        );
    }

    /// A `fromPromotion` the Environment inherits, such as from its group,
    /// has no place in the Environment's own document: refused with a
    /// precise reason, not the block-style one.
    #[test]
    fn test_place_block_refuses_an_inherited_from_promotion() {
        let inherited =
            "apiVersion: gitops.nyl/v1\nkind: Environment\nmetadata:\n  name: prod\nspec:\n  values: {target: prod}\n";
        let error = place_block(inherited, &record(1)).unwrap_err();
        assert!(error.contains("inherited"), "{error}");
    }

    /// Git's environment for every call of these tests, the writer's
    /// included: no global or system configuration, so a developer's
    /// signing or identity settings never decide a result.
    fn isolated() -> Vec<(String, String)> {
        vec![
            ("GIT_CONFIG_GLOBAL".to_owned(), "/dev/null".to_owned()),
            ("GIT_CONFIG_NOSYSTEM".to_owned(), "1".to_owned()),
        ]
    }

    fn git(directory: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(directory)
            .envs(isolated())
            .args(["-c", "user.name=Test", "-c", "user.email=test@example.invalid"])
            .args(["-c", "init.defaultBranch=main"])
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

    /// A bare remote with `main` holding the Environment and a clone of it.
    struct World {
        _root: TempDir,
        remote: PathBuf,
        clone: PathBuf,
    }

    fn world() -> World {
        let root = TempDir::new().unwrap();
        let remote = root.path().join("remote.git");
        git(root.path(), &["init", "--quiet", "--bare", "remote.git"]);
        git(root.path(), &["clone", "--quiet", remote.to_str().unwrap(), "clone"]);
        let clone = std::fs::canonicalize(root.path().join("clone")).unwrap();
        std::fs::write(clone.join("prod.yaml"), ENVIRONMENT).unwrap();
        std::fs::write(clone.join("other.yaml"), "a: 1\n").unwrap();
        git(&clone, &["add", "."]);
        git(&clone, &["commit", "--quiet", "-m", "Initial"]);
        git(&clone, &["push", "--quiet", "origin", "HEAD:refs/heads/main"]);
        git(&clone, &["branch", "--quiet", "--set-upstream-to=origin/main"]);
        World {
            _root: root,
            remote,
            clone,
        }
    }

    fn writer(world: &World) -> GitRecordWriter {
        GitRecordWriter {
            git_dir: world.clone.join(".git"),
            worktree: world.clone.clone(),
            file: world.clone.join("prod.yaml"),
            document_index: 1,
            document: ENVIRONMENT.to_owned(),
            identity: Some("Nyl Test <test@example.invalid>".parse().unwrap()),
            clock: Arc::new(FrozenClock("2026-09-25T16:41:00Z".parse().unwrap())),
            auth: GitAuth::none(),
            git_env: isolated(),
        }
    }

    fn write(world: &World) -> Result<CommitId, RecordWriteError> {
        writer(world).commit_record(&record(1), "nyl: promote prod\n\n# kept as written\n")
    }

    /// Nothing changed in the clone or on the remote.
    fn untouched(world: &World, remote: &str, head: &str) {
        assert_eq!(git(&world.remote, &["rev-parse", "main"]), remote);
        assert_eq!(git(&world.clone, &["rev-parse", "HEAD"]), head);
        assert_eq!(
            std::fs::read_to_string(world.clone.join("prod.yaml")).unwrap(),
            ENVIRONMENT
        );
    }

    /// One commit on the checked-out branch, by the given identity (with no
    /// Git configuration of its own), carrying only the Environment's file
    /// and the message as written, pushed to the branch's upstream, whose
    /// tracking ref follows. Catches a record committed with the user's
    /// other changes, under the wrong identity (CI has none), a message
    /// Git cleaned up, or one left unpushed.
    #[test]
    fn test_commit_record_is_one_pushed_commit_with_only_the_block() {
        let world = world();
        std::fs::write(world.clone.join("other.yaml"), "a: 2\n").unwrap();
        git(&world.clone, &["add", "other.yaml"]);
        let commit = write(&world).unwrap();
        let remote = |args: &[&str]| git(&world.remote, args);
        assert_eq!(remote(&["rev-parse", "main"]), commit.as_str());
        assert_eq!(
            remote(&["log", "-1", "--format=%an <%ae>|%cn <%ce>", "main"]),
            "Nyl Test <test@example.invalid>|Nyl Test <test@example.invalid>"
        );
        assert_eq!(
            remote(&["log", "-1", "--format=%B", "main"]),
            "nyl: promote prod\n\n# kept as written"
        );
        assert_eq!(remote(&["diff", "--name-only", "main~1", "main"]), "prod.yaml");
        assert_eq!(promoted(&remote(&["show", "main:prod.yaml"])), record(1));
        assert_eq!(git(&world.clone, &["rev-parse", "origin/main"]), commit.as_str());
        // The user's staged change is still staged, and still theirs.
        assert_eq!(git(&world.clone, &["diff", "--cached", "--name-only"]), "other.yaml");
    }

    /// `check`, what a dry run asks, writes nothing, and refuses what a
    /// write would refuse. Catches a dry run that commits, or one that
    /// passes a promotion the write then refuses.
    #[test]
    fn test_check_writes_nothing_and_refuses_what_a_write_refuses() {
        let world = world();
        let (remote, head) = (
            git(&world.remote, &["rev-parse", "main"]),
            git(&world.clone, &["rev-parse", "HEAD"]),
        );
        let mut checking = writer(&world);
        checking.identity = None;
        checking.check(&record(1)).unwrap();
        untouched(&world, &remote, &head);
        git(&world.clone, &["checkout", "--quiet", "--detach"]);
        assert_eq!(checking.check(&record(1)), Err(RecordWriteError::Detached));
    }

    /// A detached HEAD has no branch to commit to: refused, nothing written.
    #[test]
    fn test_commit_record_refuses_a_detached_head() {
        let world = world();
        let (remote, head) = (
            git(&world.remote, &["rev-parse", "main"]),
            git(&world.clone, &["rev-parse", "HEAD"]),
        );
        git(&world.clone, &["checkout", "--quiet", "--detach"]);
        assert_eq!(write(&world).unwrap_err(), RecordWriteError::Detached);
        untouched(&world, &remote, &head);
    }

    /// A branch with commits its upstream lacks would publish them with the
    /// record: refused, nothing written. So is a branch without an upstream.
    #[test]
    fn test_commit_record_refuses_a_branch_ahead_of_or_without_its_upstream() {
        let world = world();
        std::fs::write(world.clone.join("other.yaml"), "a: 2\n").unwrap();
        git(&world.clone, &["commit", "--quiet", "-am", "Unpushed"]);
        let remote = git(&world.remote, &["rev-parse", "main"]);
        assert!(matches!(
            write(&world).unwrap_err(),
            RecordWriteError::Ahead { commits: 1, .. }
        ));
        assert_eq!(git(&world.remote, &["rev-parse", "main"]), remote);
        git(&world.clone, &["checkout", "--quiet", "-b", "local"]);
        assert!(matches!(
            write(&world).unwrap_err(),
            RecordWriteError::NoUpstream { .. }
        ));
    }

    /// A branch that moved on the remote refuses the fast-forward push: the
    /// push is never forced, the remote keeps the other change, and the
    /// local commit is undone so the job can update and retry. Catches a
    /// promotion that overwrites a concurrent one.
    #[test]
    fn test_commit_record_never_forces_a_rejected_push() {
        let world = world();
        let other = world.remote.parent().unwrap().join("other");
        git(
            world.remote.parent().unwrap(),
            &["clone", "--quiet", world.remote.to_str().unwrap(), "other"],
        );
        std::fs::write(other.join("other.yaml"), "a: 3\n").unwrap();
        git(&other, &["commit", "--quiet", "-am", "Concurrent"]);
        git(&other, &["push", "--quiet", "origin", "HEAD:refs/heads/main"]);
        let concurrent = git(&other, &["rev-parse", "HEAD"]);
        let head = git(&world.clone, &["rev-parse", "HEAD"]);

        let error = write(&world).unwrap_err();
        assert!(matches!(error, RecordWriteError::Rejected(_)), "{error:?}");
        untouched(&world, &concurrent, &head);
        assert_eq!(git(&world.clone, &["status", "--porcelain"]), "");
    }

    /// The remote refusing the push itself, such as a protected branch or a
    /// hook, is not a moved branch to retry: its own error, the commit undone.
    #[test]
    fn test_a_push_the_remote_rejects_is_not_a_retry() {
        let world = world();
        let hook = world.remote.join("hooks/pre-receive");
        std::fs::write(&hook, "#!/bin/sh\necho protected >&2\nexit 1\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let (remote, head) = (
            git(&world.remote, &["rev-parse", "main"]),
            git(&world.clone, &["rev-parse", "HEAD"]),
        );
        let error = write(&world).unwrap_err();
        assert!(matches!(error, RecordWriteError::RemoteRejected(_)), "{error:?}");
        untouched(&world, &remote, &head);
    }

    /// A push that fails without a refusal may have reached the remote: the
    /// commit is kept and reported, never undone.
    #[test]
    fn test_a_push_that_fails_otherwise_keeps_the_commit() {
        let world = world();
        let remote = git(&world.remote, &["rev-parse", "main"]);
        let gone = world.remote.with_extension("moved");
        std::fs::rename(&world.remote, &gone).unwrap();
        let error = write(&world).unwrap_err();
        let RecordWriteError::PushUnknown { commit, .. } = &error else {
            panic!("{error:?}");
        };
        assert_eq!(&git(&world.clone, &["rev-parse", "HEAD"]), commit);
        assert_eq!(git(&gone, &["rev-parse", "main"]), remote);
    }

    /// An uncommitted change to the Environment's file would ride along in
    /// the promotion commit, and an untracked file is no part of the branch:
    /// both refused.
    #[test]
    fn test_commit_record_refuses_a_changed_or_untracked_environment_file() {
        let world = world();
        std::fs::write(world.clone.join("prod.yaml"), format!("{ENVIRONMENT}# local edit\n")).unwrap();
        assert!(matches!(write(&world).unwrap_err(), RecordWriteError::Dirty { .. }));
        git(&world.clone, &["rm", "--quiet", "--cached", "prod.yaml"]);
        git(&world.clone, &["commit", "--quiet", "-m", "Untrack"]);
        git(&world.clone, &["push", "--quiet"]);
        assert!(matches!(write(&world).unwrap_err(), RecordWriteError::Untracked { .. }));
    }
}
