//! `nyl state` and `nyl lease` through the binary, in the scenario test world
//! with the walkthroughs' shared project: state lives in `project.git`, as
//! its Environment's GitRepository names.

mod scenario;

use std::path::Path;

use assert_cmd::Command;
use predicates::prelude::*;
use scenario::world::{git_output, World};

fn project() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/scenarios/walkthroughs/_project")
}

fn nyl(checkout: &Path, args: &[&str]) -> Command {
    let mut command = Command::cargo_bin("nyl").unwrap();
    command
        .current_dir(checkout)
        .args(args)
        .env_remove("GITHUB_RUN_ID")
        .env_remove("CI_JOB_URL")
        .env("GIT_AUTHOR_NAME", "Scenario")
        .env("GIT_COMMITTER_NAME", "Scenario")
        .env("GIT_AUTHOR_EMAIL", "scenario@example.invalid")
        .env("GIT_COMMITTER_EMAIL", "scenario@example.invalid");
    command
}

#[test]
fn test_state_init_writes_state_once_to_the_configured_repository() {
    let world = World::new(&project());
    let checkout = world.fresh_clone("main");
    nyl(&checkout, &["state", "init", "-e", "dev"])
        .assert()
        .success()
        .stdout(predicate::str::contains("state-init dev: done"));
    let record = git_output(&world.bare(), &["show", "refs/heads/nyl/dev/desired:state.yaml"]);
    assert!(
        record.contains("kind: StateRecord") && record.contains("environment: dev"),
        "{record}"
    );
    assert!(git_output(&world.bare(), &["show", "refs/heads/nyl/dev/observed:state.yaml"]).contains("StateRecord"));
    let message = git_output(
        &world.bare(),
        &["log", "-1", "--format=%B", "refs/heads/nyl/dev/desired"],
    );
    assert!(message.contains("Nyl-Operation: state-init"), "{message}");
    // The lease is released and the clone's own state branches follow the remote.
    assert!(git_output(&world.bare(), &["for-each-ref", "refs/nyl/"]).is_empty());
    assert!(!git_output(&checkout, &["rev-parse", "--verify", "nyl/dev/desired"]).is_empty());

    // From another job, state that exists stays as it is.
    let again = world.fresh_clone("main");
    nyl(&again, &["state", "init", "-e", "dev", "-o", "json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"outcome\": \"unchanged\""));
}

#[test]
fn test_state_commands_refuse_with_stable_codes_and_exit_categories() {
    let world = World::new(&project());
    let checkout = world.fresh_clone("main");
    // No state yet: exit 1, naming state init.
    nyl(&checkout, &["state", "forget", "-e", "dev", "--unit", "database"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("NYL-STATE-MISSING").and(predicate::str::contains("nyl state init -e dev")));
    nyl(&checkout, &["state", "init", "-e", "prod"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("NYL-ENVIRONMENT-UNKNOWN"));

    nyl(&checkout, &["state", "init", "-e", "dev"]).assert().success();
    // A --from name no GitRepository declares fails instead of passing as a URL.
    nyl(&checkout, &["state", "move", "-e", "dev", "--from", "statee"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("GitRepository \"statee\" was not found"));
    nyl(&checkout, &["state", "forget", "-e", "dev", "--unit", "database"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("NYL-UNIT-UNKNOWN"));

    // A lease another runner holds makes every state command wait: exit 2.
    let lease = "apiVersion: gitops.nyl/v1\nkind: Lease\nrun: 0192f0c4-8d0a-7c3e-9a51-3c1f0e7d2b90\nrunner: https://ci.example.com/runs/1\noperation: reconcile\ndeadline: '2999-01-01T00:00:00Z'\ntakenAt: '2026-09-24T10:00:00Z'\n";
    let blob = {
        let path = world.scenario_tmp().join("lease.yaml");
        std::fs::write(&path, lease).unwrap();
        git_output(&world.bare(), &["hash-object", "-w", path.to_str().unwrap()])
    };
    let tree = {
        let mut command = std::process::Command::new("git");
        command
            .current_dir(world.bare())
            .args(["mktree"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped());
        let mut child = command.spawn().unwrap();
        use std::io::Write as _;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(format!("100644 blob {blob}\tlease.yaml\n").as_bytes())
            .unwrap();
        String::from_utf8(child.wait_with_output().unwrap().stdout)
            .unwrap()
            .trim()
            .to_owned()
    };
    let commit = git_output(
        &world.bare(),
        &[
            "-c",
            "user.name=CI",
            "-c",
            "user.email=ci@example.invalid",
            "commit-tree",
            &tree,
            "-m",
            "lease",
        ],
    );
    git_output(&world.bare(), &["update-ref", "refs/nyl/dev/lease", &commit]);
    nyl(&checkout, &["state", "init", "-e", "dev", "--fresh"])
        .assert()
        .code(2)
        .stderr(
            predicate::str::contains("NYL-LEASE-HELD").and(predicate::str::contains("https://ci.example.com/runs/1")),
        );

    // Breaking the lease lets the next command take over at once.
    nyl(&checkout, &["lease", "break", "-e", "dev", "--reason", "runner lost"])
        .assert()
        .success()
        .stdout(predicate::str::contains("lease-break dev: done"));
    nyl(&checkout, &["state", "init", "-e", "dev", "--fresh"])
        .assert()
        .success();
    let message = git_output(
        &world.bare(),
        &["log", "-1", "--format=%B", "refs/heads/nyl/dev/desired"],
    );
    assert!(
        message.contains("leaseBroken:") && message.contains("runner lost"),
        "{message}"
    );
}

#[test]
fn test_validate_rejects_state_refs_git_cannot_hold_together() {
    let world = World::new(&project());
    let checkout = world.fresh_clone("main");
    // A shared branch nyl/dev would be the directory of dev's nyl/dev/desired.
    std::fs::write(
        checkout.join("nyl/previews.yaml"),
        "apiVersion: gitops.nyl/v1\nkind: Environment\nmetadata:\n  name: previews\nspec:\n  unitSelector:\n    matchLabels:\n      preview: 'true'\n  state:\n    repositoryRef:\n      name: platform\n    desiredRef: nyl/dev\n    observedRef: nyl/dev\n    path: previews\n",
    )
    .unwrap();
    nyl(&checkout, &["validate"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("NYL-STATE-REF-CONFLICT").and(predicate::str::contains("refs/heads/nyl/dev")));
}
