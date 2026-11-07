//! What to print when a scenario fails: the state of the cluster, the
//! reconciler, the containers, and Nyl's own view, each a command run on a
//! best-effort basis. A failure in CI should be diagnosable from the log (and
//! the uploaded transcript) alone, without another run.

use std::time::Duration;

use crate::cmd::{truncate_tail, Cmd};
use crate::docker::docker;
use crate::env::Env;
use crate::fixture::{ENVIRONMENT, NAMESPACE};
use crate::infra::Infra;
use crate::log;
use crate::reconciler::Reconciler;

/// Lines kept of each command's output.
const KEPT_LINES: usize = 150;

fn show(title: &str, command: Cmd) {
    let command = command.timeout(Duration::from_secs(60));
    log::raw(&format!("\n--- {title}\n$ {}\n", command.display()));
    match command.run() {
        Ok(output) => {
            let text = format!("{}{}", output.stdout, output.stderr);
            log::raw(&truncate_tail(&text, KEPT_LINES));
            if !text.ends_with('\n') {
                log::raw("\n");
            }
            if !output.success() {
                log::raw(&format!("(exit {:?})\n", output.code));
            }
        }
        Err(error) => log::raw(&format!("{error}\n")),
    }
}

/// The containers and clusters of the suite, for a failure before any
/// infrastructure was saved.
pub fn overview() {
    let _group = log::Group::open("Diagnostics: containers and clusters");
    show("containers", docker().args(["ps", "--all", "--filter", "name=nyl-e2e"]));
    show("kind clusters", Cmd::new("kind").args(["get", "clusters"]));
    show("networks", docker().args(["network", "ls"]));
}

/// Everything useful about a failed run.
pub fn dump(infra: &Infra, reconciler: &dyn Reconciler, env: Option<&Env>) {
    let _group = log::Group::open("Diagnostics");
    let kubectl = infra.kubectl();
    show("nodes", kubectl.cmd(&["get", "nodes", "--output", "wide"]));
    show(
        "pods in every namespace",
        kubectl.cmd(&["get", "pods", "--all-namespaces", "--output", "wide"]),
    );
    show(
        "events, newest last",
        kubectl.cmd(&["get", "events", "--all-namespaces", "--sort-by=.lastTimestamp"]),
    );
    show(
        "the workload's objects",
        kubectl.cmd(&["get", "all", "--namespace", NAMESPACE, "--output", "wide"]),
    );
    show(
        "the workload's Deployment and pods, described",
        kubectl.cmd(&["describe", "deployment,pods", "--namespace", NAMESPACE]),
    );
    for command in reconciler.diagnostics(infra) {
        show("the reconciler", command);
    }
    show("containers", docker().args(["ps", "--all", "--filter", "name=nyl-e2e"]));
    for container in [&infra.git.container, &infra.registry.container] {
        show(
            &format!("logs of {container}"),
            docker().args(["logs", "--tail", "60", container]),
        );
    }
    show(
        "branches on the Git server",
        Cmd::new("git")
            .args(["ls-remote", &infra.git_publish_url()])
            .env("GIT_TERMINAL_PROMPT", "0"),
    );
    if let Some(env) = env {
        show("nyl status", env.nyl.command(&["status", "-e", ENVIRONMENT]));
        show("nyl plan", env.nyl.command(&["plan", "-e", ENVIRONMENT]));
    }
}
