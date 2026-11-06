//! EnvironmentTemplate instances through the command line: created with
//! `state init --template` or `reconcile --template`, found by their records
//! in a state ref shared through `state.path`, reconciled from their recorded
//! source, and removed with `state delete --teardown`.
//!
//! Contract: [Environment templates and previews](../../design/orchestration-core.md#environment-templates-and-previews).
//! Units are Command units running real scripts, so a unit's effect is a
//! directory beneath the scenario's temporary directory.

mod scenario;

use nyl::orchestration::run::{CommandOutcome, Wiring};
use nyl_orchestration::fault::NoFaults;
use nyl_orchestration::shell::run::Cancellation;
use nyl_units::process::ProcessRunner;
use scenario::runner::{invoke_in, AUTHOR};
use scenario::shell::{self as harness, project, NYL_TOML};
use scenario::world::git_output;
use serde_json::Value;

const PLATFORM: &str = "apiVersion: gitops.nyl/v1\nkind: GitRepository\nmetadata:\n  name: platform\nspec:\n  repoURL: '%PROJECT_URL%'\n  publishURL: '%PROJECT_URL%'\n";

const TEMPLATE: &str = "\
apiVersion: gitops.nyl/v1
kind: EnvironmentTemplate
metadata: {name: preview}
spec:
  parameters:
    pr: {type: integer}
    size: {type: string, default: small, enum: [small, large]}
  unitSelector: {matchLabels: {preview: 'true'}}
  values:
    dir: '%SCENARIO_TMP%/previews'
    tag: 'pr-{{ params.pr }}-{{ params.size }}'
  state:
    repositoryRef: {name: platform}
    desiredRef: nyl/previews
    observedRef: nyl/previews
    path: '{{ environment.name }}'
  allowUnprotectedSource: true
  deletionPolicy: Teardown
  allowTeardown: true
  ttl: 7d
";

/// A unit whose effect is a directory named for the environment, holding its tag.
const UNITS: &str = "\
apiVersion: units.gitops.nyl/v1
kind: Command
metadata: {name: site, labels: {preview: 'true'}}
spec:
  command: [sh, -c, 'mkdir -p {{ values.dir }}/{{ environment.name }} && printf %s {{ values.tag }} > {{ values.dir }}/{{ environment.name }}/tag && printf ''{\"tag\": \"{{ values.tag }}\"}'' > \"$NYL_OUTPUTS\"']
  teardown: [sh, -c, 'rm -rf {{ values.dir }}/{{ environment.name }}']
  idempotent: true
  outputs: {tag: {type: string}}
";

fn world(extra: &[(&str, &str)]) -> (tempfile::TempDir, harness::Harness) {
    let mut files = vec![
        NYL_TOML,
        ("nyl/platform.yaml", PLATFORM),
        ("nyl/preview.yaml", TEMPLATE),
        ("nyl/units.yaml", UNITS),
    ];
    files.extend_from_slice(extra);
    let directory = project(&files);
    let harness = harness::Harness::new(directory.path(), true);
    (directory, harness)
}

fn invoke(harness: &harness::Harness, branch: &str, args: &[&str]) -> CommandOutcome {
    let cancellation = Cancellation::default();
    let tools = ProcessRunner::new().cancelled_by(cancellation.shared());
    let wiring = Wiring {
        clock: harness.clock.clone(),
        ids: &harness.ids,
        drivers: &harness.drivers,
        tools: &tools,
        faults: &NoFaults,
        runner: "test-runner".to_owned(),
        author: Some(AUTHOR.parse().unwrap()),
        process_env: harness.process_env.clone(),
        cancellation,
        prompt: None,
        progress: None,
    };
    let args = args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
    invoke_in(&harness.world, &wiring, &args, branch, "").unwrap()
}

/// The `state.yaml` of an instance in the shared ref.
fn record(harness: &harness::Harness, instance: &str) -> Value {
    let text = git_output(
        &harness.world.bare(),
        &["show", &format!("nyl/previews:{instance}/state.yaml")],
    );
    serde_saphyr::from_str(&text).unwrap()
}

fn tag(harness: &harness::Harness, instance: &str) -> Option<String> {
    std::fs::read_to_string(harness.world.scenario_tmp().join("previews").join(instance).join("tag")).ok()
}

/// `state init --template` writes the instance's record at its own
/// directory of the shared ref, with the template, the bound parameters
/// (defaults included), the expiry, and the entry commit as source; run
/// again it changes only what differs. Catches an instance whose definition
/// is not entirely in state, or a refused or silent update.
#[test]
fn test_state_init_template_records_the_instance_in_the_shared_ref() {
    let (_directory, harness) = world(&[]);
    let created = invoke(
        &harness,
        "main",
        &[
            "state",
            "init",
            "-e",
            "pr-1",
            "--template",
            "preview",
            "--param",
            "pr=1",
        ],
    );
    assert_eq!(created.exit, 0, "{}\n{}", created.stderr, created.stdout);
    let first = record(&harness, "pr-1");
    assert_eq!(first["template"], "preview");
    assert_eq!(first["params"], serde_json::json!({"pr": 1, "size": "small"}));
    assert_eq!(first["expiresAt"], "2026-10-01T10:00:00Z");
    assert_eq!(first["sourceCommit"].as_str().unwrap(), harness.world.rev_parse("main"));
    assert_eq!(first["location"]["path"], "pr-1");

    // A second instance shares the ref and does not disturb the first.
    let second = invoke(
        &harness,
        "main",
        &[
            "state",
            "init",
            "-e",
            "pr-2",
            "--template",
            "preview",
            "--param",
            "pr=2",
            "--param",
            "size=large",
        ],
    );
    assert_eq!(second.exit, 0, "{}\n{}", second.stderr, second.stdout);
    assert_eq!(record(&harness, "pr-2")["params"]["size"], "large");
    assert_eq!(record(&harness, "pr-1")["params"]["pr"], 1);
}

fn units_unit_result(outcome: &CommandOutcome, unit: &str) -> String {
    outcome.report["environments"][0]["units"][unit]["result"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

/// A pull request job's `reconcile --template` creates and runs its instance
/// from its own checkout; a later plain `reconcile` from another checkout
/// renders the instance at the commit its record names, with the template's
/// values rendered over the recorded parameters. Catches a plain run that
/// follows the entry checkout, and values or parameters that are not the
/// instance's own.
#[test]
fn test_reconcile_runs_an_instance_from_its_recorded_source() {
    let (_directory, harness) = world(&[]);
    harness.world.branch(&scenario::format::BranchStep {
        name: "pr-7".to_owned(),
        from: "main".to_owned(),
    });
    // The pull request changes what the unit writes.
    harness.world.commit_files(
        "pr-7",
        "Tag with the pull request",
        &[(
            "nyl/units.yaml",
            &UNITS.replace("printf %s {{ values.tag }}", "printf %s pr-branch-{{ values.tag }}"),
        )],
    );
    let created = invoke(
        &harness,
        "pr-7",
        &[
            "reconcile",
            "-e",
            "pr-7",
            "--template",
            "preview",
            "--param",
            "pr=7",
            "-o",
            "json",
        ],
    );
    assert_eq!(created.exit, 0, "{}\n{}", created.stderr, created.stdout);
    assert_eq!(units_unit_result(&created, "site"), "executed", "{}", created.report);
    assert_eq!(tag(&harness, "pr-7").as_deref(), Some("pr-branch-pr-7-small"));
    assert_eq!(
        record(&harness, "pr-7")["sourceCommit"].as_str().unwrap(),
        harness.world.rev_parse("pr-7")
    );

    // From `main`, a plain reconcile finds the instance by its record, renders
    // it at the recorded commit, and has nothing to do.
    let again = invoke(&harness, "main", &["reconcile", "-e", "pr-7", "-o", "json"]);
    assert_eq!(again.exit, 0, "{}\n{}", again.stderr, again.stdout);
    assert_eq!(units_unit_result(&again, "site"), "current", "{}", again.report);
    let environment = git_output(
        &harness.world.bare(),
        &["show", "nyl/previews:pr-7/desired/environment.yaml"],
    );
    assert!(environment.contains(&harness.world.rev_parse("pr-7")), "{environment}");
    assert!(environment.contains("by: recorded"), "{environment}");

    // `status` and `get units` see the instance like a declared environment.
    let status = invoke(&harness, "main", &["status", "-e", "pr-7", "-o", "json"]);
    assert_eq!(status.exit, 0, "{}\n{}", status.stderr, status.stdout);
    assert_eq!(units_unit_result(&status, "site"), "", "{}", status.report);
}

/// An instance is removed with `state delete --teardown`: the template's
/// `deletionPolicy: Teardown` tears down every unit of a kind that supports
/// it, an explicit `Retain` included, and the instance's directory of the
/// shared ref keeps only the deleted marker while the other instance is
/// untouched. A pipeline that still runs the deleted instance fails
/// visibly. Catches a template policy that does not override a unit's own,
/// a delete that disturbs a neighbour sharing the ref, and a deleted
/// instance that silently comes back.
#[test]
fn test_state_delete_tears_an_instance_down_and_leaves_its_neighbour() {
    let retained = UNITS
        .replace("  idempotent: true\n", "  idempotent: true\n")
        .replace("  idempotent: true", "  idempotent: true\n  deletionPolicy: Retain");
    let (_directory, harness) = world(&[("nyl/units.yaml", &retained)]);
    for pr in ["1", "2"] {
        let created = invoke(
            &harness,
            "main",
            &[
                "reconcile",
                "-e",
                &format!("pr-{pr}"),
                "--template",
                "preview",
                "--param",
                &format!("pr={pr}"),
            ],
        );
        assert_eq!(created.exit, 0, "{}\n{}", created.stderr, created.stdout);
    }
    assert!(tag(&harness, "pr-1").is_some() && tag(&harness, "pr-2").is_some());

    let deleted = invoke(
        &harness,
        "main",
        &["state", "delete", "-e", "pr-1", "--teardown", "-o", "json"],
    );
    assert_eq!(deleted.exit, 0, "{}\n{}", deleted.stderr, deleted.stdout);
    assert!(
        tag(&harness, "pr-1").is_none(),
        "the unit's own Retain does not outlive the template's Teardown"
    );
    assert!(tag(&harness, "pr-2").is_some());
    let files = git_output(&harness.world.bare(), &["ls-tree", "-r", "--name-only", "nyl/previews"]);
    let files = files.lines().collect::<Vec<_>>();
    assert_eq!(
        files
            .iter()
            .copied()
            .filter(|path| path.starts_with("pr-1/"))
            .collect::<Vec<_>>(),
        ["pr-1/state.yaml"]
    );
    assert!(files.contains(&"pr-2/desired/units/site.yaml"), "{files:?}");

    let again = invoke(&harness, "main", &["reconcile", "-e", "pr-1", "-o", "json"]);
    assert_eq!(again.exit, 1);
    assert!(again.stderr.contains("NYL-STATE-DELETED"), "{}", again.stderr);
    let revived = invoke(
        &harness,
        "main",
        &[
            "state",
            "init",
            "-e",
            "pr-1",
            "--template",
            "preview",
            "--param",
            "pr=1",
        ],
    );
    assert_eq!(revived.exit, 1);
    assert!(revived.stderr.contains("NYL-STATE-DELETED"), "{}", revived.stderr);
}
