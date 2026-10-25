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

/// The author state commits are written with: fresh clones have no Git
/// identity, and the host's must not matter.
const AUTHOR: &str = "Scenario <scenario@example.invalid>";

/// `nyl <args>` in `checkout`, with a home that holds no Git configuration,
/// so only `--author` or the clone's own configuration can name an identity.
fn bare_nyl(checkout: &Path, home: &Path, args: &[&str]) -> Command {
    let mut command = Command::cargo_bin("nyl").unwrap();
    command
        .current_dir(checkout)
        .args(args)
        .env_remove("GITHUB_RUN_ID")
        .env_remove("CI_JOB_URL")
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        // CI forces colour; the assertions read plain text.
        .env("NO_COLOR", "1");
    command
}

/// `nyl <args> --author <AUTHOR>` in `checkout`.
fn nyl(checkout: &Path, args: &[&str]) -> Command {
    let home = checkout.with_extension("home");
    std::fs::create_dir_all(&home).unwrap();
    let mut command = bare_nyl(checkout, &home, args);
    command.args(["--author", AUTHOR]);
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
    // The commit is signed by --author, as author and committer.
    let signature = git_output(
        &world.bare(),
        &[
            "log",
            "-1",
            "--format=%an <%ae>|%cn <%ce>",
            "refs/heads/nyl/dev/desired",
        ],
    );
    assert_eq!(signature, format!("{AUTHOR}|{AUTHOR}"));
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

/// State commits need an identity: `--author`, else the clone's Git
/// configuration. Without either, a writing command refuses with a code
/// whose fix names both, instead of signing with an invented identity.
#[test]
fn test_state_commands_take_their_identity_from_author_or_git_config() {
    let world = World::new(&project());
    let checkout = world.fresh_clone("main");
    let home = world.scenario_tmp().join("empty-home");
    std::fs::create_dir_all(&home).unwrap();
    bare_nyl(&checkout, &home, &["state", "init", "-e", "dev"])
        .assert()
        .code(1)
        .stderr(
            predicate::str::contains("NYL-COMMIT-IDENTITY-MISSING")
                .and(predicate::str::contains("--author"))
                .and(predicate::str::contains("user.name")),
        );
    assert!(git_output(&world.bare(), &["for-each-ref", "refs/heads/nyl/"]).is_empty());

    git_output(&checkout, &["config", "user.name", "Clone User"]);
    git_output(&checkout, &["config", "user.email", "clone@example.invalid"]);
    bare_nyl(&checkout, &home, &["state", "init", "-e", "dev"])
        .assert()
        .success();
    let signature = git_output(
        &world.bare(),
        &["log", "-1", "--format=%an <%ae>", "refs/heads/nyl/dev/desired"],
    );
    assert_eq!(signature, "Clone User <clone@example.invalid>");
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
    bare_nyl(&checkout, &world.scenario_tmp(), &["validate"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("NYL-STATE-REF-CONFLICT").and(predicate::str::contains("refs/heads/nyl/dev")));
}

/// The tier-1 harness runs state commands in-process: the wiring's author
/// signs the commit, and the outcome carries the typed report and exit
/// instead of printing and exiting.
#[test]
fn test_state_commands_run_in_process_with_the_wiring_author() {
    use clap::Parser as _;
    use nyl::cli::Cli;
    use nyl::orchestration::run::{ProcessWiring, Wiring};

    let world = World::new(&project());
    let checkout = world.fresh_clone("main");
    let owned = ProcessWiring::new();
    let mut wiring = Wiring::from_process(&owned).unwrap();
    wiring.author = Some(AUTHOR.parse().unwrap());
    let path = checkout.to_str().unwrap();

    let cli = Cli::try_parse_from(["nyl", "state", "init", "-e", "dev", "-o", "json", path]).unwrap();
    let outcome = cli.execute_in_process(&wiring).unwrap();
    assert_eq!(outcome.exit, 0, "{}", outcome.stderr);
    assert_eq!(outcome.report["outcome"], "done");
    let signature = git_output(
        &world.bare(),
        &["log", "-1", "--format=%an <%ae>", "refs/heads/nyl/dev/desired"],
    );
    assert_eq!(signature, AUTHOR);

    let cli = Cli::try_parse_from(["nyl", "state", "init", "-e", "prod", path]).unwrap();
    let outcome = cli.execute_in_process(&wiring).unwrap();
    assert_eq!(outcome.exit, 1);
    assert_eq!(outcome.report["errors"][0]["code"], "NYL-ENVIRONMENT-UNKNOWN");

    // get reads declarations in-process too, never needing an identity.
    let cli = Cli::try_parse_from(["nyl", "get", "environments", "dev", path]).unwrap();
    let outcome = cli.execute_in_process(&wiring).unwrap();
    assert_eq!(outcome.exit, 0, "{}", outcome.stderr);
    assert!(outcome.stdout.contains("dev"), "{}", outcome.stdout);
}

/// Only writing commands need a commit identity: reconcile refuses without
/// one before writing anything, while plan and status read state with none.
/// The fake kinds stand in for the shared project's OpenTofu and image units.
#[cfg(feature = "test-kinds")]
#[test]
fn test_reconcile_needs_an_identity_and_plan_and_status_do_not() {
    let world = World::new(&project());
    let checkout = world.fresh_clone("main");
    nyl(&checkout, &["state", "init", "-e", "dev"]).assert().success();
    let tips = || git_output(&world.bare(), &["for-each-ref", "refs/heads/nyl/"]);
    let before = tips();
    let home = world.scenario_tmp().join("empty-home");
    std::fs::create_dir_all(&home).unwrap();

    bare_nyl(&checkout, &home, &["reconcile", "-e", "dev"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("NYL-COMMIT-IDENTITY-MISSING").and(predicate::str::contains("--author")));
    assert_eq!(tips(), before, "reconcile without an identity wrote state");
    assert!(git_output(&world.bare(), &["for-each-ref", "refs/nyl/"]).is_empty());

    // The plan is incomplete (exit 2): database and kubernetes wait for receipts.
    bare_nyl(&checkout, &home, &["plan", "-e", "dev"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("NYL-COMMIT-IDENTITY-MISSING").not());
    bare_nyl(&checkout, &home, &["status", "-e", "dev"])
        .assert()
        .stderr(predicate::str::contains("NYL-COMMIT-IDENTITY-MISSING").not())
        .code(predicate::ne(1));
    assert_eq!(tips(), before, "plan or status wrote state");
}
