//! `nyl state` and `nyl lease` through the binary, in the scenario test world
//! with the walkthroughs' shared project: state lives in `project.git`, as
//! its Environment's GitRepository names.

mod scenario;

use std::path::Path;
use std::sync::Arc;

use assert_cmd::Command;
use nyl_core::orchestration::ids::RunId;
use nyl_core::orchestration::ports::StateStore as _;
use nyl_core::orchestration::state::{Lease, LeaseKind, Operation, StateV1};
use nyl_orchestration::testing::FakeClock;
use predicates::prelude::*;
use scenario::runner::AUTHOR;
use scenario::shell;
use scenario::world::{git_output, World};

fn project() -> std::path::PathBuf {
    scenario::shell::shared_project()
}

/// `nyl <args>` in `checkout`, with a home that holds no Git configuration.
fn bare_nyl(checkout: &Path, home: &Path, args: &[&str]) -> Command {
    Command::from_std(scenario::nyl_binary(checkout, home, args))
}

/// `nyl <args> --author <AUTHOR>` in `checkout`.
fn nyl(checkout: &Path, args: &[&str]) -> Command {
    Command::from_std(scenario::nyl_as_author(checkout, args))
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
    // The commit is written by --author, as author and committer.
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
/// whose fix names both, instead of committing as an invented identity.
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
    let lease = Lease {
        api_version: StateV1,
        kind: LeaseKind,
        run: RunId::new("0192f0c4-8d0a-7c3e-9a51-3c1f0e7d2b90").unwrap(),
        runner: "https://ci.example.com/runs/1".to_owned(),
        operation: Operation::Reconcile,
        units: Vec::new(),
        deadline: "2999-01-01T00:00:00Z".parse().unwrap(),
        taken_at: shell::start(),
        broken: None,
    };
    let clock = Arc::new(FakeClock::new(shell::start()));
    shell::git_store(&world.fresh_clone("main"), &clock)
        .take_lease(&shell::location(&world), &shell::dev(), &lease, None)
        .unwrap();
    nyl(&checkout, &["state", "init", "-e", "dev", "--fresh"])
        .assert()
        .code(2)
        .stderr(
            predicate::str::contains("NYL-LEASE-HELD").and(predicate::str::contains("https://ci.example.com/runs/1")),
        );
}

/// "Runs and leases" items 1 and 8: a broken lease refuses every state
/// command (exit 2, naming `nyl recover -e dev`) until an import records
/// the break; `nyl recover -e dev` is that import. The fake kinds stand in
/// for the shared project's units, which the import resolves.
#[cfg(feature = "test-kinds")]
#[test]
fn test_a_broken_lease_refuses_state_commands_until_recover_imports_it() {
    let world = World::new(&project());
    let checkout = world.fresh_clone("main");
    nyl(&checkout, &["state", "init", "-e", "dev"]).assert().success();
    let lease = Lease {
        api_version: StateV1,
        kind: LeaseKind,
        run: RunId::new("0192f0c4-8d0a-7c3e-9a51-3c1f0e7d2b90").unwrap(),
        runner: "https://ci.example.com/runs/1".to_owned(),
        operation: Operation::StateInit,
        units: Vec::new(),
        deadline: "2999-01-01T00:00:00Z".parse().unwrap(),
        taken_at: shell::start(),
        broken: None,
    };
    let clock = Arc::new(FakeClock::new(shell::start()));
    shell::git_store(&world.fresh_clone("main"), &clock)
        .take_lease(&shell::location(&world), &shell::dev(), &lease, None)
        .unwrap();
    nyl(&checkout, &["lease", "break", "-e", "dev", "--reason", "runner lost"])
        .assert()
        .success()
        .stdout(predicate::str::contains("lease-break dev: done"));
    nyl(&checkout, &["state", "init", "-e", "dev", "--fresh"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("NYL-LEASE-BROKEN").and(predicate::str::contains("nyl recover -e dev")));
    // The broken holder left no run ref: recover imports nothing and
    // records the break once, in its transition commit.
    nyl(&checkout, &["recover", "-e", "dev", "--reason", "runner gone"])
        .assert()
        .success()
        .stdout(predicate::str::contains("recover dev: done"));
    let message = ["desired", "observed"]
        .iter()
        .map(|side| {
            git_output(
                &world.bare(),
                &["log", "-1", "--format=%B", &format!("refs/heads/nyl/dev/{side}")],
            )
        })
        .find(|message| message.starts_with("nyl: recover dev"))
        .expect("recover wrote a transition commit");
    assert!(
        message.starts_with("nyl: recover dev")
            && message.contains("leaseBroken:")
            && message.contains("runner lost")
            && message.contains("Nyl-Reason: runner gone"),
        "{message}"
    );
    assert!(git_output(&world.bare(), &["for-each-ref", "refs/nyl/"]).is_empty());
    nyl(&checkout, &["state", "init", "-e", "dev", "--fresh"])
        .assert()
        .success();
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

/// Commands run in-process over the production wiring: the wiring's author
/// writes the commit, and the outcome carries the typed report and exit
/// instead of printing and exiting. Refusals and the other in-process
/// commands are covered through the binary and by the tier-1 scenarios.
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

/// The local-state options through the binary, in one workstation clone:
/// `--approve` needs `--local`, `reconcile --local` leaves remote state
/// alone, `status` and `get` take `--local`, `state reset --local
/// --discard-all` and `state restore` reach their operations. What those
/// operations do is `orchestration_local_test`'s.
#[cfg(feature = "test-kinds")]
#[test]
fn test_local_state_commands_through_the_binary() {
    let world = World::new(&project());
    let checkout = world.fresh_clone("main");
    nyl(&checkout, &["state", "init", "-e", "dev"]).assert().success();
    let remote = || git_output(&world.bare(), &["for-each-ref", "refs/heads/nyl/", "refs/nyl/"]);
    let before = remote();
    // Reading commands take no --author.
    let home = world.scenario_tmp().join("reader-home");
    std::fs::create_dir_all(&home).unwrap();
    let read = |args: &[&str]| bare_nyl(&checkout, &home, args);

    nyl(&checkout, &["reconcile", "-e", "dev", "--approve", "network"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("--local"));
    nyl(&checkout, &["reconcile", "-e", "dev", "--local"])
        .assert()
        .success()
        .stderr(predicate::str::contains("NYL-LOCAL-RUN-UNRECORDED"));
    assert_eq!(remote(), before, "a --local run wrote remote state");

    read(&["status", "-e", "dev", "--local"]).assert().success();
    read(&["get", "units", "-e", "dev", "--local", "-o", "name"])
        .assert()
        .success()
        .stdout(predicate::str::contains("network"));
    read(&["status", "-e", "dev"])
        .assert()
        .stderr(predicate::str::contains("NYL-STATE-LOCAL-DIVERGED"));

    nyl(&checkout, &["state", "reset", "-e", "dev", "--local", "--discard-all"])
        .assert()
        .success()
        .stdout(predicate::str::contains("state-reset dev: done"));

    nyl(&checkout, &["state", "restore", "-e", "dev", "--to", &"0".repeat(40)])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("NYL-STATE-RESTORE-INVALID"));
}
