//! Explicit working-file reconciliation retains exact inputs across state moves
//! and fresh clones. Contract: design/orchestration-core.md#source.

mod scenario;

use std::path::Path;

use assert_cmd::Command;
use scenario::world::{git_output, World};
use tempfile::TempDir;

fn nyl(checkout: &Path, args: &[&str]) -> Command {
    Command::from_std(scenario::nyl_as_author(checkout, args))
}

fn project() -> TempDir {
    let fixture = TempDir::new().unwrap();
    std::fs::write(
        fixture.path().join("nyl.toml"),
        "[project]\ncomponents_search_paths = []\nhelm_chart_search_paths = []\n",
    )
    .unwrap();
    std::fs::write(fixture.path().join("environment.yaml"), "apiVersion: gitops.nyl/v1\nkind: Environment\nmetadata: {name: local}\nspec:\n  state:\n    repository:\n      repoURL: '%SCENARIO_TMP%/state.git'\n").unwrap();
    std::fs::write(fixture.path().join("input.json"), r#"{"value":"committed"}"#).unwrap();
    std::fs::create_dir(fixture.path().join("context")).unwrap();
    std::fs::write(fixture.path().join("context/Dockerfile"), "FROM scratch\n").unwrap();
    fixture
}

#[test]
fn test_worktree_reconciles_untracked_units_and_retains_source_for_fresh_clone_teardown_after_a_move() {
    let fixture = project();
    let world = World::new(fixture.path());
    let checkout = world.fresh_clone("main");
    #[cfg(unix)]
    {
        std::fs::create_dir(checkout.join(".github")).unwrap();
        std::os::unix::fs::symlink("../CLAUDE.md", checkout.join(".github/copilot-instructions.md")).unwrap();
    }
    let head = git_output(&checkout, &["rev-parse", "HEAD"]);
    nyl(&checkout, &["state", "init", "-e", "local"]).assert().success();
    std::fs::write(checkout.join("input.json"), r#"{"value":"working"}"#).unwrap();
    std::fs::write(
        checkout.join("units.yaml"),
        r#"apiVersion: units.gitops.nyl/v1
kind: Command
metadata: {name: producer}
spec:
  command: [sh, -c, 'cat input.json > "$NYL_OUTPUTS"']
  teardown: [sh, -c, 'test "$(cat input.json)" = ''{"value":"working"}''']
  idempotent: true
  files: [input.json]
  outputs: {value: {type: string}}
---
apiVersion: units.gitops.nyl/v1
kind: Command
metadata: {name: consumer}
spec:
  values:
    value: {fromUnit: {unit: producer, output: value}}
  command: [sh, -c, 'grep -q working "$NYL_INPUTS"']
  idempotent: true
"#,
    )
    .unwrap();
    let first = nyl(&checkout, &["reconcile", "-e", "local", "--worktree", "-o", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let first: serde_json::Value = serde_json::from_slice(&first).unwrap();
    let environment = &first["environments"][0];
    assert_eq!(environment["units"]["producer"]["result"], "executed");
    assert_eq!(environment["units"]["consumer"]["result"], "executed");
    assert_eq!(git_output(&checkout, &["rev-parse", "HEAD"]), head);
    let again = nyl(&checkout, &["reconcile", "-e", "local", "--worktree", "-o", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let again: serde_json::Value = serde_json::from_slice(&again).unwrap();
    assert_eq!(again["environments"][0]["sourceCommit"], environment["sourceCommit"]);
    assert_eq!(again["environments"][0]["units"]["producer"]["result"], "current");
    assert_eq!(again["environments"][0]["units"]["consumer"]["result"], "current");
    let state = world.scenario_tmp().join("state.git");
    let desired = git_output(&state, &["show", "nyl/local/desired:desired/environment.yaml"]);
    assert!(desired.contains("refs/nyl-worktrees/"));

    let moved = world.scenario_tmp().join("moved.git");
    git2::Repository::init_bare(&moved).unwrap();
    let authored = std::fs::read_to_string(checkout.join("environment.yaml")).unwrap();
    let moved_environment = authored
        .replace("/state.git", "/moved.git")
        .replace("  state:\n", "  state:\n    path: copied\n");
    std::fs::write(checkout.join("environment.yaml"), &moved_environment).unwrap();
    let old_url = reqwest::Url::from_file_path(&state).unwrap().to_string();
    nyl(
        &checkout,
        &["state", "move", "-e", "local", "--from", &old_url, "--state-path", ""],
    )
    .assert()
    .success();

    let fresh = world.fresh_clone("main");
    std::fs::write(fresh.join("environment.yaml"), moved_environment).unwrap();
    assert!(!fresh.join("units.yaml").exists());
    {
        use nyl_core::orchestration::ports::{StateSide, StateStore as _};
        let reader = nyl_state::git::GitStateStore::read_only(fresh.join(".git"), nyl_git::GitAuth::none());
        let location = nyl_core::orchestration::state::StateLocation {
            repository: reqwest::Url::from_file_path(&moved).unwrap().to_string(),
            desired_ref: "nyl/local/desired".to_owned(),
            observed_ref: "nyl/local/observed".to_owned(),
            path: "copied".to_owned(),
            coordination_ref_prefix: "refs/nyl/".to_owned(),
        };
        let history = reader.log(&location, StateSide::Desired, 100).unwrap();
        assert!(!history.is_empty());
        assert!(
            history.iter().all(|entry| entry.message.contains("Nyl-Operation:")),
            "{history:?}"
        );
        assert!(history[0].message.contains("Nyl-Operation: state-move"));
    }
    nyl(
        &fresh,
        &["teardown", "-e", "local", "--all", "--reason", "finished worktree test"],
    )
    .assert()
    .success();
}

// These checks exercise source resolution without invoking a native image builder.
#[cfg(all(feature = "test-kinds", not(feature = "tier2")))]
#[test]
fn test_worktree_preserves_committed_unit_pins_and_refuses_snapshot_pins_as_clean_source() {
    use predicates::str::contains;
    let fixture = project();
    let world = World::new(fixture.path());
    let checkout = world.fresh_clone("main");
    let head = git_output(&checkout, &["rev-parse", "HEAD"]);
    nyl(&checkout, &["state", "init", "-e", "local"]).assert().success();
    let image = |revision: &str, commit: &str| {
        format!("apiVersion: units.gitops.nyl/v1\nkind: OciImage\nmetadata: {{name: image}}\nspec:\n  context: {{path: context, revision: '{revision}', commit: '{commit}'}}\n  repository: registry.invalid/worktree-test\n")
    };
    std::fs::write(checkout.join("image.yaml"), image("main", &head)).unwrap();
    std::fs::write(checkout.join("input.json"), "working").unwrap();
    nyl(&checkout, &["reconcile", "-e", "local", "--worktree"])
        .assert()
        .success();
    let state = world.scenario_tmp().join("state.git");
    let unit = git_output(&state, &["show", "nyl/local/desired:desired/units/image.yaml"]);
    let unit: serde_json::Value = serde_saphyr::from_str(&unit).unwrap();
    assert_eq!(unit["resolvedSpec"]["context"]["commit"], head);
    let record = git_output(&state, &["show", "nyl/local/desired:desired/environment.yaml"]);
    let record: serde_json::Value = serde_saphyr::from_str(&record).unwrap();
    let snapshot = record["sourceCommit"].as_str().unwrap();
    let revision = record["sourceChosenBy"]["revision"].as_str().unwrap();
    nyl(&checkout, &["reconcile", "-e", "local", "--source", snapshot])
        .assert()
        .code(1)
        .stderr(contains("a worktree snapshot requires --worktree"));
    std::fs::write(checkout.join("image.yaml"), image(revision, snapshot)).unwrap();
    nyl(&checkout, &["reconcile", "-e", "local", "--worktree"])
        .assert()
        .code(1)
        .stderr(contains(
            "explicit unit source revisions cannot select worktree snapshots",
        ));
}

#[test]
fn test_worktree_requires_one_named_environment_and_conflicts_with_source() {
    use clap::Parser as _;
    use nyl::cli::Cli;
    assert!(Cli::try_parse_from(["nyl", "reconcile", "-e", "local", "--worktree", "--source", "HEAD"]).is_err());
    assert!(Cli::try_parse_from(["nyl", "reconcile", "-l", "stage=dev", "--worktree"]).is_err());
}

#[test]
fn test_local_worktree_run_does_not_publish_its_snapshot_to_the_state_repository() {
    let fixture = project();
    let world = World::new(fixture.path());
    let checkout = world.fresh_clone("main");
    nyl(&checkout, &["state", "init", "-e", "local"]).assert().success();
    std::fs::write(checkout.join("unit.yaml"), "apiVersion: units.gitops.nyl/v1\nkind: Command\nmetadata: {name: local-check}\nspec:\n  command: [git, --version]\n  idempotent: true\n").unwrap();
    let result = nyl(
        &checkout,
        &["reconcile", "-e", "local", "--worktree", "--local", "-o", "json"],
    )
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();
    let report: serde_json::Value = serde_json::from_slice(&result).unwrap();
    let commit = git2::Oid::from_str(report["environments"][0]["sourceCommit"].as_str().unwrap()).unwrap();
    assert!(git2::Repository::open(&checkout).unwrap().find_commit(commit).is_ok());
    assert!(git2::Repository::open_bare(world.scenario_tmp().join("state.git"))
        .unwrap()
        .find_commit(commit)
        .is_err());
}
