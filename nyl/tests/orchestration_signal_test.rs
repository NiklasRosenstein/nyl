//! SIGINT and SIGTERM through the `nyl` binary: the command line's signal
//! handler cancels `reconcile` as the shell's cancellation does.
//!
//! Contract: "Runs and leases" item 7 in design/orchestration-core.md.

#![cfg(unix)]

mod scenario;

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use scenario::world::{git_output, World};
use tempfile::TempDir;

const AUTHOR: &str = "Scenario <scenario@example.invalid>";

const ENVIRONMENT: &str = "apiVersion: gitops.nyl/v1\nkind: Environment\nmetadata:\n  name: dev\nspec:\n  unitSelector:\n    matchLabels: {dev: 'true'}\n  allowUnprotectedSource: true\n";

/// A Command unit running `script`.
fn command(name: &str, script: &str, extra: &str) -> String {
    format!(
        "apiVersion: units.gitops.nyl/v1\nkind: Command\nmetadata:\n  name: {name}\n  labels: {{dev: 'true'}}\nspec:\n  command: [sh, -c, '{script}']\n{extra}"
    )
}

/// `nyl <args> --author <AUTHOR>` in `checkout`, with a home of its own.
fn nyl(checkout: &Path, args: &[&str]) -> Command {
    let home = checkout.with_extension("home");
    std::fs::create_dir_all(&home).unwrap();
    let mut command = Command::new(assert_cmd::cargo::cargo_bin("nyl"));
    command
        .current_dir(checkout)
        .args(args)
        .args(["--author", AUTHOR])
        .env_remove("GITHUB_RUN_ID")
        .env_remove("CI_JOB_URL")
        .env_remove("NYL_TEST_NOW")
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("NO_COLOR", "1");
    command
}

/// SIGTERM during a wave: `nyl reconcile` signals the running tool, keeps
/// its run ref with the finished unit's checkpoint, releases the lease, and
/// exits 4 with `NYL-RUN-CANCELLED` instead of dying with the lease held;
/// the next `reconcile` imports the run ref. Catches a command line that
/// installs no handler (the process dies, the lease stays until expiry) or
/// does not hand the flag to the shell and the process runner.
#[test]
fn test_reconcile_on_sigterm_keeps_its_run_ref_and_releases_the_lease() {
    let markers = TempDir::new().unwrap();
    let started = markers.path().join("started");
    let slow = format!("[ -e {0} ] && exit 0; touch {0}; sleep 30", started.display());
    let directory = TempDir::new().unwrap();
    for (path, content) in [
        ("nyl/nyl.toml", "[project]\n".to_owned()),
        ("nyl/environment.yaml", ENVIRONMENT.to_owned()),
        ("nyl/quick.yaml", command("quick", "true", "")),
        ("nyl/slow.yaml", command("slow", &slow, "  idempotent: true\n")),
    ] {
        let target = directory.path().join(path);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(target, content).unwrap();
    }
    let world = World::new(directory.path());
    let checkout = world.fresh_clone("main");
    let init = nyl(&checkout, &["state", "init", "-e", "dev"]).output().unwrap();
    assert!(init.status.success(), "{}", String::from_utf8_lossy(&init.stderr));
    let desired = git_output(&world.bare(), &["rev-parse", "refs/heads/nyl/dev/desired"]);

    let begun = Instant::now();
    let child = nyl(&checkout, &["reconcile", "-e", "dev", "-o", "json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    while !started.exists() && begun.elapsed() < Duration::from_secs(60) {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(started.exists(), "the slow unit started");
    let killed = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(killed.success());
    let output = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(begun.elapsed() < Duration::from_secs(25), "the tool was signalled");
    assert_eq!(output.status.code(), Some(4), "{stdout}\n{stderr}");
    assert!(stdout.contains("NYL-RUN-CANCELLED"), "{stdout}");
    assert_eq!(
        git_output(&world.bare(), &["rev-parse", "refs/heads/nyl/dev/desired"]),
        desired,
        "no transition commit"
    );
    let refs = git_output(&world.bare(), &["for-each-ref", "--format=%(refname)", "refs/nyl/dev/"]);
    assert!(!refs.contains("refs/nyl/dev/lease"), "the lease is released: {refs}");
    assert!(refs.contains("refs/nyl/dev/runs/"), "the run ref is kept: {refs}");

    let next = nyl(&world.fresh_clone("main"), &["reconcile", "-e", "dev", "-o", "json"])
        .output()
        .unwrap();
    let report: serde_json::Value = serde_json::from_slice(&next.stdout).unwrap();
    assert_eq!(next.status.code(), Some(0), "{report:#}");
    assert_eq!(
        report["environments"][0]["imported"],
        serde_json::json!(["quick", "slow"]),
        "{report:#}"
    );
}

/// SIGTERM while a state operation waits for a held lease: `nyl state init
/// --fresh --wait-lease` stops waiting and refuses with `NYL-RUN-CANCELLED`
/// (exit 1), having written nothing, instead of dying with the default
/// disposition. Catches a command line that installs no handler for the
/// state operations or does not hand them its flag.
#[test]
fn test_state_operation_waiting_for_the_lease_refuses_on_sigterm() {
    let markers = TempDir::new().unwrap();
    let started = markers.path().join("started");
    let slow = format!("touch {}; sleep 60", started.display());
    let directory = TempDir::new().unwrap();
    for (path, content) in [
        ("nyl/nyl.toml", "[project]\n".to_owned()),
        ("nyl/environment.yaml", ENVIRONMENT.to_owned()),
        ("nyl/slow.yaml", command("slow", &slow, "  idempotent: true\n")),
    ] {
        let target = directory.path().join(path);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(target, content).unwrap();
    }
    let world = World::new(directory.path());
    let checkout = world.fresh_clone("main");
    let init = nyl(&checkout, &["state", "init", "-e", "dev"]).output().unwrap();
    assert!(init.status.success(), "{}", String::from_utf8_lossy(&init.stderr));
    let desired = git_output(&world.bare(), &["rev-parse", "refs/heads/nyl/dev/desired"]);

    let begun = Instant::now();
    let holder = nyl(&checkout, &["reconcile", "-e", "dev", "-o", "json"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    while !started.exists() && begun.elapsed() < Duration::from_secs(60) {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(started.exists(), "the holding run started its unit");

    let waiting = nyl(
        &world.fresh_clone("main"),
        &[
            "state",
            "init",
            "-e",
            "dev",
            "--fresh",
            "--wait-lease",
            "10m",
            "-o",
            "json",
        ],
    )
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
    std::thread::sleep(Duration::from_secs(2));
    let killed = Command::new("kill")
        .args(["-TERM", &waiting.id().to_string()])
        .status()
        .unwrap();
    assert!(killed.success());
    let output = waiting.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stdout}\n{stderr}");
    assert!(stdout.contains("NYL-RUN-CANCELLED"), "{stdout}");

    let stopped = Command::new("kill")
        .args(["-TERM", &holder.id().to_string()])
        .status()
        .unwrap();
    assert!(stopped.success());
    let held = holder.wait_with_output().unwrap();
    assert_eq!(held.status.code(), Some(4), "the holding run was cancelled too");
    assert_eq!(
        git_output(&world.bare(), &["rev-parse", "refs/heads/nyl/dev/desired"]),
        desired,
        "the cancelled state operation wrote nothing"
    );
}
