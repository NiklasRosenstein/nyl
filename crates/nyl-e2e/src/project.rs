//! The project's checkout: a local clone whose `origin` is the Git server,
//! where the suite commits each revision of the fixture and where `nyl` runs.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::cmd::Cmd;
use crate::error::{Context, Result};
use crate::fixture::RenderedFile;

/// The branch the project's source lives on.
pub const SOURCE_BRANCH: &str = "main";

pub struct Project {
    directory: PathBuf,
}

impl Project {
    /// A fresh checkout in `directory` (anything there is removed) whose
    /// `origin` is `publish_url`, with an identity Nyl commits state as.
    pub fn init(directory: &Path, publish_url: &str) -> Result<Self> {
        if directory.exists() {
            std::fs::remove_dir_all(directory).context(format!("remove the old checkout {}", directory.display()))?;
        }
        std::fs::create_dir_all(directory).context(format!("create {}", directory.display()))?;
        let project = Self {
            directory: directory.to_owned(),
        };
        project
            .git(&["init", "--quiet", "--initial-branch", SOURCE_BRANCH])
            .run_ok()?;
        project.git(&["config", "user.name", "Nyl E2E"]).run_ok()?;
        project
            .git(&["config", "user.email", "nyl-e2e@example.invalid"])
            .run_ok()?;
        project.git(&["remote", "add", "origin", publish_url]).run_ok()?;
        Ok(project)
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    fn git(&self, args: &[&str]) -> Cmd {
        Cmd::new("git")
            .args(["-c", "commit.gpgsign=false", "-c", "core.hooksPath=/dev/null"])
            .args(args)
            .cwd(&self.directory)
            .env("GIT_TERMINAL_PROMPT", "0")
            .timeout(Duration::from_secs(120))
    }

    /// Write the rendered files into the checkout.
    pub fn write(&self, files: &[RenderedFile]) -> Result<()> {
        for file in files {
            let path = self.directory.join(file.path);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).context(format!("create {}", parent.display()))?;
            }
            std::fs::write(&path, &file.content).context(format!("write {}", path.display()))?;
        }
        Ok(())
    }

    /// Commit everything and push it to the source branch; returns the commit.
    pub fn commit_and_push(&self, message: &str) -> Result<String> {
        self.git(&["add", "--all"]).run_ok()?;
        self.git(&["commit", "--quiet", "--message", message]).run_ok()?;
        self.git(&["push", "--quiet", "origin", &format!("HEAD:refs/heads/{SOURCE_BRANCH}")])
            .run_ok()?;
        Ok(self.git(&["rev-parse", "HEAD"]).run_ok()?.stdout_trimmed().to_owned())
    }

    /// Fetch every branch of `origin`, Nyl's state refs included.
    pub fn fetch(&self) -> Result<()> {
        self.git(&["fetch", "--quiet", "origin", "+refs/heads/*:refs/remotes/origin/*"])
            .run_ok()?;
        Ok(())
    }

    /// The commit `branch` points to on the server, or `None` when it does not exist.
    pub fn remote_head(&self, branch: &str) -> Result<Option<String>> {
        let reference = format!("refs/heads/{branch}");
        let output = self.git(&["ls-remote", "origin", &reference]).run_ok()?;
        Ok(parse_ls_remote(&output.stdout, &reference))
    }

    /// The files of the fetched `origin/<branch>`, or none when it does not exist.
    pub fn tree(&self, branch: &str) -> Result<Vec<String>> {
        let output = self
            .git(&["ls-tree", "-r", "--name-only", &format!("refs/remotes/origin/{branch}")])
            .run()?;
        if !output.success() {
            return Ok(Vec::new());
        }
        Ok(output.stdout.lines().map(str::to_owned).collect())
    }

    /// A file of the fetched `origin/<branch>`.
    pub fn show(&self, branch: &str, path: &str) -> Result<String> {
        Ok(self
            .git(&["show", &format!("refs/remotes/origin/{branch}:{path}")])
            .run_ok()?
            .stdout)
    }
}

/// The commit `reference` points to in the output of `git ls-remote`.
pub fn parse_ls_remote(output: &str, reference: &str) -> Option<String> {
    output.lines().find_map(|line| {
        let (commit, name) = line.split_once('\t')?;
        (name.trim() == reference).then(|| commit.trim().to_owned())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ls_remote_output_is_matched_by_the_exact_reference() {
        let output = "aaa111\trefs/heads/deploy\nbbb222\trefs/heads/deploy-old\nccc333\trefs/heads/nyl/dev/desired\n";
        assert_eq!(parse_ls_remote(output, "refs/heads/deploy").as_deref(), Some("aaa111"));
        assert_eq!(
            parse_ls_remote(output, "refs/heads/nyl/dev/desired").as_deref(),
            Some("ccc333")
        );
        assert_eq!(parse_ls_remote(output, "refs/heads/missing"), None);
        assert_eq!(parse_ls_remote("", "refs/heads/deploy"), None);
    }
}
