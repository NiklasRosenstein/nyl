//! `kubectl` against the suite's cluster, always through the suite's own
//! kubeconfig and context, never the user's.

use std::path::PathBuf;
use std::time::Duration;

use serde_json::Value;

use crate::cmd::{Cmd, Output};
use crate::error::{Context, Error, Result};

#[derive(Clone, Debug)]
pub struct Kubectl {
    kubeconfig: PathBuf,
    context: String,
}

impl Kubectl {
    pub fn new(kubeconfig: PathBuf, context: String) -> Self {
        Self { kubeconfig, context }
    }

    /// `kubectl --kubeconfig <file> --context <context> <args>`; a call takes at
    /// most two minutes unless the caller sets another timeout.
    pub fn cmd(&self, args: &[&str]) -> Cmd {
        Cmd::new("kubectl")
            .arg("--kubeconfig")
            .arg(&self.kubeconfig)
            .args(["--context", &self.context])
            .args(args)
            .timeout(Duration::from_secs(120))
    }

    /// Run `kubectl <args>`, requiring success.
    pub fn run(&self, args: &[&str]) -> Result<Output> {
        self.cmd(args).run_ok()
    }

    /// `kubectl <args> -o json`, parsed.
    pub fn json(&self, args: &[&str]) -> Result<Value> {
        let mut arguments = args.to_vec();
        arguments.extend(["-o", "json"]);
        let output = self.run(&arguments)?;
        serde_json::from_str(&output.stdout).context(format!("parse the JSON of `{}`", output.command))
    }

    /// `kubectl get <args> -o json`, or `None` when the object does not exist.
    /// Any other failure is an error.
    pub fn get_optional(&self, args: &[&str]) -> Result<Option<Value>> {
        let mut arguments = vec!["get"];
        arguments.extend(args);
        arguments.extend(["-o", "json"]);
        let output = self.cmd(&arguments).run()?;
        if output.success() {
            return serde_json::from_str(&output.stdout)
                .map(Some)
                .context(format!("parse the JSON of `{}`", output.command));
        }
        if is_not_found(&output.stderr) {
            return Ok(None);
        }
        Err(Error::new(output.describe_failure(&[0])))
    }

    /// `kubectl apply -f -` with `manifest` on stdin.
    pub fn apply(&self, manifest: &str, server_side: bool) -> Result<Output> {
        let mut args = vec!["apply", "-f", "-"];
        if server_side {
            args.extend(["--server-side", "--force-conflicts"]);
        }
        self.cmd(&args).stdin(manifest).run_ok()
    }
}

/// Whether `kubectl`'s error says the object or its namespace does not exist.
pub fn is_not_found(stderr: &str) -> bool {
    stderr.contains("NotFound") || stderr.contains("not found")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_not_found_is_told_from_other_failures() {
        assert!(is_not_found(
            "Error from server (NotFound): applications.argoproj.io \"dev-web\" not found"
        ));
        assert!(is_not_found(
            "Error from server (NotFound): namespaces \"web\" not found"
        ));
        assert!(!is_not_found("The connection to the server 127.0.0.1:1 was refused"));
        // A missing CRD is not an absent object: it means Argo CD is not installed.
        assert!(!is_not_found(
            "error: the server doesn't have a resource type \"applications\""
        ));
    }

    #[test]
    fn test_every_call_names_the_suites_kubeconfig_and_context() {
        let kubectl = Kubectl::new(PathBuf::from("/work/kubeconfig"), "e2e-admin".to_owned());
        assert_eq!(
            kubectl.cmd(&["get", "pods"]).display(),
            "kubectl --kubeconfig /work/kubeconfig --context e2e-admin get pods"
        );
    }
}
