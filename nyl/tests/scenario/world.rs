//! The test world of `design/reference-scenarios.md#test-world`: one
//! per-test temporary directory holding a bare `project.git`, a working
//! clone, and the scenario's own temporary directory.
//!
//! Pull requests are branches, merges are squash commits, and a CI job is one
//! invocation in a fresh clone at a commit, so orchestration only ever sees
//! commits, refs, and commands. Git runs as the `git` command line with its
//! identity passed per invocation, never through the process environment.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use tempfile::TempDir;

use super::format::{BranchStep, CommitStep, FileStep, SquashMergeStep};

/// The registry host tier 1 substitutes for `%REGISTRY%`; nothing listens there.
pub const FAKE_REGISTRY: &str = "registry.invalid:5000";

pub struct World {
    root: TempDir,
    /// Numbers fresh clones, so concurrent invocations never share one.
    clones: AtomicUsize,
    registry: String,
}

impl World {
    /// Create the world and commit `project` as the first commit of `main`.
    pub fn new(project: &Path) -> Self {
        Self::with_registry(project, FAKE_REGISTRY)
    }

    pub fn with_registry(project: &Path, registry: &str) -> Self {
        let world = Self {
            registry: registry.to_owned(),
            root: TempDir::new().expect("temporary directory"),
            clones: AtomicUsize::new(0),
        };
        std::fs::create_dir_all(world.scenario_tmp()).unwrap();
        git(
            world.root.path(),
            &["init", "--quiet", "--bare", "--initial-branch=main", "project.git"],
        );
        git(
            world.root.path(),
            &["clone", "--quiet", world.project_url().as_str(), "work"],
        );
        world.copy_into_work(project);
        world.commit_and_push("main", "Initial project");
        world
    }

    /// The scenario's own temporary directory, `%SCENARIO_TMP%`.
    pub fn scenario_tmp(&self) -> PathBuf {
        self.root.path().join("scenario-tmp")
    }

    /// [`Self::scenario_tmp`] as units' scripts see it: with forward
    /// slashes, because `sh` reads a Windows path's backslashes as escapes
    /// and scripts write the path into JSON outputs.
    pub fn scenario_tmp_for_scripts(&self) -> String {
        self.scenario_tmp().display().to_string().replace('\\', "/")
    }

    /// The URL of `project.git`, `%PROJECT_URL%`.
    pub fn project_url(&self) -> String {
        reqwest::Url::from_file_path(self.bare())
            .expect("the test repository has an absolute path")
            .to_string()
    }

    pub fn bare(&self) -> PathBuf {
        self.root.path().join("project.git")
    }

    fn work(&self) -> PathBuf {
        self.root.path().join("work")
    }

    /// Copy `source` over the working clone, substituting the placeholders in
    /// text files.
    fn copy_into_work(&self, source: &Path) {
        for entry in walkdir::WalkDir::new(source).sort_by_file_name() {
            let entry = entry.unwrap();
            let relative = entry.path().strip_prefix(source).unwrap();
            let target = self.work().join(relative);
            if entry.file_type().is_dir() {
                std::fs::create_dir_all(&target).unwrap();
                continue;
            }
            let bytes = std::fs::read(entry.path()).unwrap();
            let bytes = match String::from_utf8(bytes) {
                Ok(text) => self.substitute(&text).into_bytes(),
                Err(error) => error.into_bytes(),
            };
            std::fs::write(&target, bytes).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(entry.path()).unwrap().permissions().mode();
                std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode)).unwrap();
            }
        }
    }

    fn substitute(&self, text: &str) -> String {
        text.replace("%SCENARIO_TMP%", &self.scenario_tmp_for_scripts())
            .replace("%PROJECT_URL%", &self.project_url())
            .replace("%REGISTRY%", &self.registry)
            .replace(
                "['%DOCKER_PASSTHROUGH%']",
                &serde_json::to_string(&super::docker_selectors().keys().collect::<Vec<_>>()).unwrap(),
            )
    }

    fn commit_and_push(&self, branch: &str, message: &str) {
        git(&self.work(), &["add", "--all"]);
        git(
            &self.work(),
            &["commit", "--quiet", "--allow-empty", "--message", message],
        );
        git(
            &self.work(),
            &["push", "--quiet", "origin", &format!("HEAD:refs/heads/{branch}")],
        );
    }

    fn checkout(&self, branch: &str) {
        git(&self.work(), &["fetch", "--quiet", "origin"]);
        git(
            &self.work(),
            &["checkout", "--quiet", "-B", branch, &format!("origin/{branch}")],
        );
    }

    /// A `commit` step: copy files over the clone, remove paths, commit, push.
    pub fn commit(&self, step: &CommitStep, scenario_dir: &Path) {
        self.checkout(&step.branch);
        if let Some(apply) = &step.apply {
            self.copy_into_work(&scenario_dir.join(apply));
        }
        for path in &step.delete {
            let target = self.work().join(path);
            if target.is_dir() {
                std::fs::remove_dir_all(&target).unwrap();
            } else {
                std::fs::remove_file(&target)
                    .unwrap_or_else(|error| panic!("commit delete {}: {error}", target.display()));
            }
        }
        self.commit_and_push(&step.branch, &step.message);
    }

    /// Write `files` over the tip of `branch`, then commit and push them.
    pub fn commit_files(&self, branch: &str, message: &str, files: &[(&str, &str)]) {
        self.checkout(branch);
        for (path, content) in files {
            let target = self.work().join(path);
            std::fs::create_dir_all(target.parent().unwrap()).unwrap();
            std::fs::write(target, self.substitute(content)).unwrap();
        }
        self.commit_and_push(branch, message);
    }

    /// A `branch` step.
    pub fn branch(&self, step: &BranchStep) {
        git(
            &self.bare(),
            &["branch", &step.name, &format!("refs/heads/{}", step.from)],
        );
    }

    /// A `squash-merge` step, keeping the branch.
    pub fn squash_merge(&self, step: &SquashMergeStep) {
        self.checkout(&step.into);
        git(
            &self.work(),
            &["merge", "--quiet", "--squash", &format!("origin/{}", step.branch)],
        );
        self.commit_and_push(&step.into, &format!("Squash-merge {}", step.branch));
    }

    /// A `delete-branch` step.
    pub fn delete_branch(&self, name: &str) {
        git(&self.bare(), &["branch", "--quiet", "--delete", "--force", name]);
    }

    /// A `ref: {delete}` step: delete any ref in the state repository.
    /// A `file` step: a change made outside Nyl beneath `%SCENARIO_TMP%`,
    /// writing the file (and its parents) or removing it.
    pub fn file(&self, step: &FileStep) -> Result<(), String> {
        let path = self.scenario_tmp().join(&step.path);
        match &step.content {
            Some(content) => {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).map_err(|error| format!("{}: {error}", parent.display()))?;
                }
                std::fs::write(&path, content).map_err(|error| format!("{}: {error}", path.display()))
            }
            None => std::fs::remove_file(&path).map_err(|error| format!("{}: {error}", path.display())),
        }
    }

    pub fn delete_ref(&self, reference: &str) {
        git(&self.bare(), &["update-ref", "-d", reference]);
    }

    /// A fresh clone at the tip of `branch`, as one CI job checks out.
    pub fn fresh_clone(&self, branch: &str) -> PathBuf {
        let jobs = self.root.path().join("jobs");
        std::fs::create_dir_all(&jobs).unwrap();
        let count = self.clones.fetch_add(1, Ordering::Relaxed);
        let path = jobs.join(format!("job-{count}"));
        git(
            &jobs,
            &[
                "clone",
                "--quiet",
                "--branch",
                branch,
                self.project_url().as_str(),
                path.to_str().unwrap(),
            ],
        );
        path
    }

    /// The commit a revision of `project.git` names, such as `main~1`.
    pub fn rev_parse(&self, revision: &str) -> String {
        git_output(&self.bare(), &["rev-parse", revision])
    }

    /// The files of a branch in `project.git`.
    pub fn files(&self, branch: &str) -> Vec<String> {
        git_output(
            &self.bare(),
            &["ls-tree", "-r", "--name-only", &format!("refs/heads/{branch}")],
        )
        .lines()
        .map(str::to_owned)
        .collect()
    }
}

/// `dev`'s lease in the state repository `bare`, while one is held.
pub fn lease(bare: &Path) -> Option<nyl_core::orchestration::state::Lease> {
    let shown = git_command(bare, &["show", "refs/nyl/dev/lease:lease.yaml"])
        .output()
        .expect("git runs");
    shown
        .status
        .success()
        .then(|| serde_saphyr::from_str(&String::from_utf8_lossy(&shown.stdout)).unwrap())
}

fn git_command(directory: &Path, args: &[&str]) -> Command {
    let mut command = Command::new("git");
    command
        .current_dir(directory)
        .args(["-c", "user.name=Scenario", "-c", "user.email=scenario@example.invalid"])
        .args(["-c", "init.defaultBranch=main", "-c", "commit.gpgsign=false"])
        .args(args);
    command
}

pub fn git(directory: &Path, args: &[&str]) {
    let output = git_command(directory, args).output().expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?} in {} failed: {}",
        directory.display(),
        String::from_utf8_lossy(&output.stderr)
    );
}

pub fn git_output(directory: &Path, args: &[&str]) -> String {
    let output = git_command(directory, args).output().expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?} in {} failed: {}",
        directory.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}
