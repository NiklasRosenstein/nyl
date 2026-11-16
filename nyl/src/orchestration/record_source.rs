//! The `record: source` writer: `nyl promote` commits the PromotionRecord
//! into the target Environment's `source.fromPromotion.promoted` block.
//!
//! Contract: [Promoting an environment's source](../../../design/promotion.md#promoting-an-environments-source)
//! and decision D3 of the [M6 plan](../../../design/m6-plan.md#open-decisions):
//! without a change gate the block is one commit on the branch the entry
//! worktree has checked out, pushed to that branch's upstream fast-forward
//! only. This module performs those effects for the shell's
//! [`PromotionRecordWriter`]; what to write and when is the shell's.
//!
//! - A detached HEAD is refused: there is no branch to commit to.
//! - The Environment's file must have no uncommitted change, so the commit
//!   carries the block and nothing else; other staged or changed files stay
//!   as they are and out of the commit.
//! - The block is placed in the Environment's document in block style,
//!   replacing an earlier block, and every other line is kept as written.
//!   The edited document must read back as exactly the record.
//! - The push never forces. When the remote refuses it, such as because the
//!   branch moved since the checkout, the commit is undone in the entry
//!   worktree, so nothing was written and the job can update and retry.

use std::path::PathBuf;
use std::process::Command;
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
    pub identity: CommitIdentity,
    pub clock: Arc<dyn Clock>,
    pub auth: GitAuth,
}

impl PromotionRecordWriter for GitRecordWriter {
    fn commit_record(&self, record: &PromotionRecord, message: &str) -> Result<CommitId, RecordWriteError> {
        let unwritable = RecordWriteError::Unwritable;
        let Ok(branch) = self.git(&["symbolic-ref", "--quiet", "HEAD"]) else {
            return Err(RecordWriteError::Detached);
        };
        let name = branch.strip_prefix("refs/heads/").ok_or(RecordWriteError::Detached)?;
        let (remote, merge) = self.upstream(name)?;
        let url = self
            .git(&["remote", "get-url", "--push", &remote])
            .map_err(|error| unwritable(format!("remote {remote} of branch {name} has no URL: {error}")))?;
        let file = std::fs::canonicalize(&self.file)
            .map_err(|error| unwritable(format!("cannot read {}: {error}", self.file.display())))?;
        let relative = file
            .strip_prefix(&self.worktree)
            .map_err(|_| unwritable(format!("{} is outside the worktree", self.file.display())))?
            .to_string_lossy()
            .replace('\\', "/");
        let changed = self
            .git(&["status", "--porcelain", "--", &relative])
            .map_err(unwritable)?;
        if !changed.is_empty() {
            return Err(unwritable(format!(
                "{relative} has uncommitted changes; commit or discard them, so the promotion commit carries only its record"
            )));
        }
        let before = self.git(&["rev-parse", "--verify", "HEAD"]).map_err(unwritable)?;

        let contents = std::fs::read_to_string(&self.file)
            .map_err(|error| unwritable(format!("cannot read {relative}: {error}")))?;
        let document = place_block(&self.document, record).map_err(unwritable)?;
        let edited =
            crate::cli::resource_file::replace_document(&contents, self.document_index, &self.document, &document)
                .map_err(|error| unwritable(error.to_string()))?;
        crate::cli::resource_file::atomic_replace(&self.file, &contents, &edited)
            .map_err(|error| unwritable(error.to_string()))?;

        let date = format!("@{} +0000", self.clock.now().timestamp());
        let committed = self
            .command()
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_DATE", &date)
            .args(["-c", &format!("user.name={}", self.identity.name)])
            .args(["-c", &format!("user.email={}", self.identity.email)])
            .args([
                "commit",
                "--quiet",
                "--no-verify",
                "--only",
                "--message",
                message,
                "--",
                &relative,
            ])
            .output();
        let restore = || {
            let _ = self.git(&["reset", "--quiet", "--soft", &before]);
            let _ = self.git(&["checkout", &before, "--", &relative]);
        };
        match committed {
            Ok(output) if output.status.success() => {}
            Ok(output) => {
                restore();
                return Err(unwritable(format!(
                    "git commit failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                )));
            }
            Err(error) => {
                restore();
                return Err(unwritable(format!("cannot run git: {error}")));
            }
        }
        let commit = self.git(&["rev-parse", "--verify", "HEAD"]).map_err(unwritable)?;
        let pushed = nyl_git::push_atomic(
            &self.git_dir,
            &url,
            &[RefUpdate {
                remote: merge.clone(),
                new: Some(commit.clone()),
                expected: Expect::FastForward,
            }],
            &self.auth,
        );
        match pushed {
            Ok(()) => {
                // The upstream's tracking ref follows, as a `git push` to the remote would move it.
                if let Some(tracked) = merge.strip_prefix("refs/heads/") {
                    let _ = self.git(&["update-ref", &format!("refs/remotes/{remote}/{tracked}"), &commit]);
                }
                CommitId::new(commit).map_err(|error| unwritable(error.to_string()))
            }
            Err(error) => {
                restore();
                match error {
                    nyl_git::GitError::Rejected { .. } => Err(RecordWriteError::Rejected(error.to_string())),
                    other => Err(unwritable(other.to_string())),
                }
            }
        }
    }
}

impl GitRecordWriter {
    /// `git` bound to the entry clone and its worktree, without the user's hooks.
    fn command(&self) -> Command {
        let mut command = nyl_git::repository_command(&self.git_dir);
        command
            .arg("--work-tree")
            .arg(&self.worktree)
            .current_dir(&self.worktree);
        command
    }

    /// Run `git` and return its trimmed stdout, or its stderr as the error.
    fn git(&self, args: &[&str]) -> Result<String, String> {
        let output = self
            .command()
            .args(args)
            .output()
            .map_err(|error| format!("cannot run git: {error}"))?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
        } else {
            Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
        }
    }

    /// The remote and the remote ref `branch` pushes to: its configured upstream.
    fn upstream(&self, branch: &str) -> Result<(String, String), RecordWriteError> {
        let config = |key: &str| self.git(&["config", "--get", &format!("branch.{branch}.{key}")]).ok();
        match (config("remote"), config("merge")) {
            (Some(remote), Some(merge)) if !remote.is_empty() && remote != "." => Ok((remote, merge)),
            _ => Err(RecordWriteError::Unwritable(format!(
                "branch {branch} has no upstream on a remote to push the record to; set one with `git branch --set-upstream-to`"
            ))),
        }
    }
}

/// `document` (an Environment, in block style) with `record` as its
/// `spec.source.fromPromotion.promoted` block, replacing an earlier one and
/// keeping every other line as written. The result must read back as exactly
/// `record`.
pub fn place_block(document: &str, record: &PromotionRecord) -> Result<String, String> {
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
        "{}promoted: # written by nyl promote, never by hand\n",
        " ".repeat(child)
    );
    for line in yaml.lines().filter(|line| *line != "---") {
        if line.is_empty() {
            block.push('\n');
        } else {
            block.push_str(&pad);
            block.push_str(line);
            block.push('\n');
        }
    }
    let mut output = String::with_capacity(document.len() + block.len());
    let (cut_start, cut_end) = promoted.unwrap_or((end, end));
    for line in &lines[..cut_start] {
        output.push_str(line);
    }
    if !output.is_empty() && !output.ends_with('\n') {
        output.push('\n');
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
/// `identity`, or why it cannot have one: an Environment no file of the
/// project declares, such as a template's instance, has no block to write.
pub fn writer_for(
    session: &super::session::Session,
    identity: CommitIdentity,
    clock: Arc<dyn Clock>,
) -> Result<GitRecordWriter, String> {
    let name = session.resolved.name.as_str();
    let resource = session
        .inventory
        .get(nyl_render::resources::GitOpsResourceKind::Environment, name)
        .ok_or_else(|| format!("Environment {name} is not declared in a file of this project, such as an instance of a template; declare it to keep its record in source"))?;
    let auth = crate::cli::git_auth::project_git_auth(&session.inventory.project_config.config)
        .map_err(|error| error.to_string())?;
    Ok(GitRecordWriter {
        git_dir: session.resolved.git_dir.clone(),
        worktree: session.inventory.worktree_root.clone(),
        file: session.inventory.project_root.join(&resource.source_path),
        document_index: resource.document_index,
        document: resource.raw_document.clone(),
        identity,
        clock,
        auth,
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

    fn git(directory: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(directory)
            .args(["-c", "user.name=Test", "-c", "user.email=test@example.invalid"])
            .args(["-c", "commit.gpgsign=false", "-c", "init.defaultBranch=main"])
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
            identity: "Nyl Test <test@example.invalid>".parse().unwrap(),
            clock: Arc::new(FrozenClock("2026-09-25T16:41:00Z".parse().unwrap())),
            auth: GitAuth::none(),
        }
    }

    /// One commit on the checked-out branch, by the given author, carrying
    /// only the Environment's file, pushed to the branch's upstream. Catches
    /// a record committed with the user's other changes, under the wrong
    /// identity (CI has none), or left unpushed.
    #[test]
    fn test_commit_record_is_one_pushed_commit_with_only_the_block() {
        let world = world();
        std::fs::write(world.clone.join("other.yaml"), "a: 2\n").unwrap();
        git(&world.clone, &["add", "other.yaml"]);
        let commit = writer(&world).commit_record(&record(1), "nyl: promote prod\n").unwrap();
        let remote = |args: &[&str]| git(&world.remote, args);
        assert_eq!(remote(&["rev-parse", "main"]), commit.as_str());
        assert_eq!(
            remote(&["log", "-1", "--format=%an <%ae>|%s", "main"]),
            "Nyl Test <test@example.invalid>|nyl: promote prod"
        );
        assert_eq!(remote(&["diff", "--name-only", "main~1", "main"]), "prod.yaml");
        assert_eq!(promoted(&remote(&["show", "main:prod.yaml"])), record(1));
        // The user's staged change is still staged, and still theirs.
        assert_eq!(git(&world.clone, &["diff", "--cached", "--name-only"]), "other.yaml");
    }

    /// A detached HEAD has no branch to commit to: refused, nothing written.
    #[test]
    fn test_commit_record_refuses_a_detached_head() {
        let world = world();
        git(&world.clone, &["checkout", "--quiet", "--detach"]);
        let before = git(&world.remote, &["rev-parse", "main"]);
        let error = writer(&world)
            .commit_record(&record(1), "nyl: promote prod\n")
            .unwrap_err();
        assert_eq!(error, RecordWriteError::Detached);
        assert_eq!(git(&world.remote, &["rev-parse", "main"]), before);
        assert_eq!(
            std::fs::read_to_string(world.clone.join("prod.yaml")).unwrap(),
            ENVIRONMENT
        );
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

        let error = writer(&world)
            .commit_record(&record(1), "nyl: promote prod\n")
            .unwrap_err();
        assert!(matches!(error, RecordWriteError::Rejected(_)), "{error:?}");
        assert_eq!(git(&world.remote, &["rev-parse", "main"]), concurrent);
        assert_eq!(git(&world.clone, &["rev-parse", "HEAD"]), head);
        assert_eq!(
            std::fs::read_to_string(world.clone.join("prod.yaml")).unwrap(),
            ENVIRONMENT
        );
        assert_eq!(git(&world.clone, &["status", "--porcelain"]), "");
    }

    /// An uncommitted change to the Environment's file would ride along in
    /// the promotion commit: refused.
    #[test]
    fn test_commit_record_refuses_a_changed_environment_file() {
        let world = world();
        std::fs::write(world.clone.join("prod.yaml"), format!("{ENVIRONMENT}# local edit\n")).unwrap();
        let error = writer(&world)
            .commit_record(&record(1), "nyl: promote prod\n")
            .unwrap_err();
        assert!(
            matches!(&error, RecordWriteError::Unwritable(message) if message.contains("uncommitted")),
            "{error:?}"
        );
    }
}
