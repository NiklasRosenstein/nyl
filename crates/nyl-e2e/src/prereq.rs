//! Prerequisites, checked up front with one message that lists everything
//! missing, so a runner without `kind` does not fail ten minutes in.

use crate::cmd::Cmd;
use crate::error::{Error, Result};

/// A program the suite shells out to, and the command that proves it works.
pub struct Tool {
    pub name: &'static str,
    pub program: &'static str,
    pub args: &'static [&'static str],
    pub hint: &'static str,
}

/// Everything the cluster scenarios need besides the `nyl` binary.
pub const TOOLS: [Tool; 7] = [
    Tool {
        name: "docker (daemon)",
        program: "docker",
        args: &["info", "--format", "{{.ServerVersion}}"],
        hint: "install Docker and start the daemon; kind and the registry run as containers",
    },
    Tool {
        name: "docker buildx",
        program: "docker",
        args: &["buildx", "version"],
        hint: "install the Docker Buildx plugin; the OciImage unit builds with it",
    },
    Tool {
        name: "kind",
        program: "kind",
        args: &["version"],
        hint: "https://kind.sigs.k8s.io/docs/user/quick-start/#installation",
    },
    Tool {
        name: "kubectl",
        program: "kubectl",
        args: &["version", "--client"],
        hint: "https://kubernetes.io/docs/tasks/tools/",
    },
    Tool {
        name: "OpenTofu",
        program: "tofu",
        args: &["version"],
        hint: "https://opentofu.org/docs/intro/install/; the OpenTofu unit runs it",
    },
    Tool {
        name: "git",
        program: "git",
        args: &["--version"],
        hint: "install Git",
    },
    Tool {
        name: "curl",
        program: "curl",
        args: &["--version"],
        hint: "install curl; the suite uses it for the registry and the Argo CD API",
    },
];

/// Runs a tool's proof command; swapped in tests.
pub trait Probe {
    /// `Err` carries why the tool does not work.
    fn probe(&self, tool: &Tool) -> std::result::Result<(), String>;
}

/// Runs the real programs.
pub struct SystemProbe;

impl Probe for SystemProbe {
    fn probe(&self, tool: &Tool) -> std::result::Result<(), String> {
        let output = Cmd::new(tool.program)
            .args(tool.args)
            .timeout(std::time::Duration::from_secs(30))
            .run()
            .map_err(|error| error.to_string())?;
        if output.success() {
            Ok(())
        } else {
            let detail = output.stderr.trim();
            Err(format!(
                "`{} {}` failed{}",
                tool.program,
                tool.args.join(" "),
                if detail.is_empty() {
                    String::new()
                } else {
                    format!(": {}", detail.lines().last().unwrap_or(detail))
                }
            ))
        }
    }
}

/// Check every tool and fail with all that are missing, each with a hint.
pub fn check(tools: &[Tool], probe: &dyn Probe) -> Result<()> {
    let missing = tools
        .iter()
        .filter_map(|tool| {
            probe
                .probe(tool)
                .err()
                .map(|reason| format!("  - {}: {reason}\n    {}", tool.name, tool.hint))
        })
        .collect::<Vec<_>>();
    if missing.is_empty() {
        return Ok(());
    }
    Err(Error::new(format!(
        "the e2e suite needs {} tool(s) that are not usable:\n{}",
        missing.len(),
        missing.join("\n")
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Broken(&'static [&'static str]);

    impl Probe for Broken {
        fn probe(&self, tool: &Tool) -> std::result::Result<(), String> {
            if self.0.contains(&tool.program) {
                Err("not found".to_owned())
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn test_every_missing_tool_is_reported_at_once_with_its_hint() {
        let error = check(&TOOLS, &Broken(&["kind", "tofu"])).unwrap_err().to_string();
        assert!(error.starts_with("the e2e suite needs 2 tool(s)"), "{error}");
        assert!(error.contains("- kind: not found"), "{error}");
        assert!(error.contains("kind.sigs.k8s.io"), "{error}");
        assert!(error.contains("- OpenTofu: not found"), "{error}");
        assert!(!error.contains("kubectl"), "{error}");
    }

    #[test]
    fn test_a_shared_program_is_reported_once_per_failed_check() {
        // `docker` backs two checks; a missing binary fails both, each named.
        let error = check(&TOOLS, &Broken(&["docker"])).unwrap_err().to_string();
        assert!(
            error.contains("docker (daemon)") && error.contains("docker buildx"),
            "{error}"
        );
    }

    #[test]
    fn test_nothing_missing_is_ok() {
        assert!(check(&TOOLS, &Broken(&[])).is_ok());
    }
}
