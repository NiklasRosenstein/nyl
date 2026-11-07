//! The lifecycle of preview instances through the command line, with the
//! fake clock: sliding expiry (which commands extend it, which remove an
//! expired instance, `--renew` and `--no-extend`), the fleet reconcile,
//! `maxInstances` and `--make-room`, `state move --template`, and the keep
//! refs of `keepSource`.
//!
//! Contract: [Expiry](../../design/orchestration-core.md#expiry),
//! [Template changes](../../design/orchestration-core.md#template-changes), and
//! [Instances](../../design/orchestration-core.md#instances). Units are Command
//! units running real scripts, so a unit's effect is a directory beneath the
//! scenario's temporary directory, and time moves only when a test advances it.

mod scenario;

use nyl::orchestration::run::{CommandOutcome, Wiring};
use nyl_core::orchestration::ports::Clock as _;
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
  unitSelector: {matchLabels: {preview: 'true'}}
  values:
    dir: '%SCENARIO_TMP%/previews'
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

/// A unit whose effect is a directory named for the environment.
const UNITS: &str = "\
apiVersion: units.gitops.nyl/v1
kind: Command
metadata: {name: site, labels: {preview: 'true'}}
spec:
  command: [sh, -c, 'mkdir -p {{ values.dir }}/{{ environment.name }} && printf ready > {{ values.dir }}/{{ environment.name }}/tag']
  teardown: [sh, -c, 'rm -rf {{ values.dir }}/{{ environment.name }}']
  idempotent: true
";

/// The template with the opt-in to keep refs for the instances' source commits.
fn keeping(template: &str) -> String {
    format!("{template}  keepSource: true\n")
}

const DAY: u64 = 24 * 60;

fn world(template: &str, units: &str) -> (tempfile::TempDir, harness::Harness) {
    let directory = project(&[
        NYL_TOML,
        ("nyl/platform.yaml", PLATFORM),
        ("nyl/preview.yaml", template),
        ("nyl/units.yaml", units),
    ]);
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

/// The pull request job: create or update the instance and reconcile it.
fn open(harness: &harness::Harness, pr: u32) -> CommandOutcome {
    let name = format!("pr-{pr}");
    let outcome = invoke(
        harness,
        "main",
        &[
            "reconcile",
            "-e",
            &name,
            "--template",
            "preview",
            "--param",
            &format!("pr={pr}"),
            "-o",
            "json",
        ],
    );
    assert_eq!(outcome.exit, 0, "{}\n{}", outcome.stderr, outcome.stdout);
    outcome
}

fn record(harness: &harness::Harness, instance: &str) -> Value {
    let text = git_output(
        &harness.world.bare(),
        &["show", &format!("nyl/previews:{instance}/state.yaml")],
    );
    serde_saphyr::from_str(&text).unwrap()
}

fn expires_at(harness: &harness::Harness, instance: &str) -> String {
    record(harness, instance)["expiresAt"].as_str().unwrap().to_owned()
}

fn deleted(harness: &harness::Harness, instance: &str) -> bool {
    record(harness, instance)["events"]
        .as_array()
        .and_then(|events| events.last())
        .is_some_and(|event| event["type"] == "state-deleted")
}

fn exists(harness: &harness::Harness, instance: &str) -> bool {
    harness
        .world
        .scenario_tmp()
        .join("previews")
        .join(instance)
        .join("tag")
        .exists()
}

fn instance_report(outcome: &CommandOutcome, index: usize) -> &Value {
    &outcome.report["environments"][index]["instance"]
}

/// Only activity extends an instance's expiry: a plain `reconcile` does,
/// `--no-extend` reconciles it as maintenance without moving it. Catches an
/// expiry that never slides (an active preview vanishes) or one that
/// maintenance keeps alive forever.
#[test]
fn test_a_plain_reconcile_extends_the_expiry_and_no_extend_does_not() {
    let (_directory, harness) = world(TEMPLATE, UNITS);
    open(&harness, 1);
    assert_eq!(expires_at(&harness, "pr-1"), "2026-10-01T10:00:00Z");

    harness.advance(DAY);
    let plain = invoke(&harness, "main", &["reconcile", "-e", "pr-1", "-o", "json"]);
    assert_eq!(plain.exit, 0, "{}\n{}", plain.stderr, plain.stdout);
    assert_eq!(expires_at(&harness, "pr-1"), "2026-10-02T10:00:00Z");
    assert_eq!(instance_report(&plain, 0)["action"], "extended");
    assert_eq!(instance_report(&plain, 0)["expiresAt"], "2026-10-02T10:00:00Z");

    harness.advance(DAY);
    let maintenance = invoke(
        &harness,
        "main",
        &["reconcile", "-e", "pr-1", "--no-extend", "-o", "json"],
    );
    assert_eq!(maintenance.exit, 0, "{}\n{}", maintenance.stderr, maintenance.stdout);
    assert_eq!(expires_at(&harness, "pr-1"), "2026-10-02T10:00:00Z");
    assert!(instance_report(&maintenance, 0).get("action").is_none());

    // Reading an instance never extends it.
    harness.advance(DAY);
    assert_eq!(invoke(&harness, "main", &["status", "-e", "pr-1"]).exit, 0);
    assert_eq!(invoke(&harness, "main", &["plan", "-e", "pr-1"]).exit, 0);
    assert_eq!(expires_at(&harness, "pr-1"), "2026-10-02T10:00:00Z");
}

/// A reconcile that finds an instance expired removes it, tearing its
/// resources down before deleting its state, whether it is plain or
/// `--no-extend`; until then `status`, `plan`, and `get environments` show it
/// as expired. Catches an expired preview that lingers, one removed without
/// its resources, and expiry hidden from the people who look.
#[test]
fn test_a_reconcile_removes_an_expired_instance_and_its_resources() {
    let (_directory, harness) = world(TEMPLATE, UNITS);
    open(&harness, 1);
    open(&harness, 2);
    assert!(exists(&harness, "pr-1") && exists(&harness, "pr-2"));

    harness.advance(8 * DAY);
    let status = invoke(&harness, "main", &["status", "-e", "pr-1", "-o", "json"]);
    assert_eq!(instance_report(&status, 0)["expired"], true, "{}", status.report);
    assert!(status.stderr.contains("NYL-INSTANCE-EXPIRED"), "{}", status.stderr);
    // The command line appends the project path, so a listing names one environment.
    let listed = invoke(&harness, "main", &["get", "environments", "pr-1"]);
    assert!(
        listed.stdout.contains("2026-10-01T10:00:00Z (expired)"),
        "{}",
        listed.stdout
    );

    let removed = invoke(&harness, "main", &["reconcile", "-e", "pr-1", "-o", "json"]);
    assert_eq!(removed.exit, 0, "{}\n{}", removed.stderr, removed.stdout);
    assert_eq!(instance_report(&removed, 0)["action"], "removed", "{}", removed.report);
    assert_eq!(removed.report["environments"][0]["units"]["site"]["result"], "removed");
    assert!(!exists(&harness, "pr-1"), "the unit's resources are torn down");
    assert!(deleted(&harness, "pr-1"));

    // `--no-extend` is maintenance, and maintenance removes too.
    let maintained = invoke(
        &harness,
        "main",
        &["reconcile", "-e", "pr-2", "--no-extend", "-o", "json"],
    );
    assert_eq!(maintained.exit, 0, "{}\n{}", maintained.stderr, maintained.stdout);
    assert_eq!(instance_report(&maintained, 0)["action"], "removed");
    assert!(!exists(&harness, "pr-2") && deleted(&harness, "pr-2"));

    let again = invoke(&harness, "main", &["reconcile", "-e", "pr-1"]);
    assert_eq!(again.exit, 1);
    assert!(again.stderr.contains("NYL-STATE-DELETED"), "{}", again.stderr);
}

/// `--renew` and the pull request job's `reconcile --template` are the only
/// ways to keep an expired instance: both extend it and reconcile instead of
/// removing it. Catches a pull request update that removes its own preview.
#[test]
fn test_renew_and_the_pull_request_job_keep_an_expired_instance() {
    let (_directory, harness) = world(TEMPLATE, UNITS);
    open(&harness, 1);
    open(&harness, 2);

    harness.advance(8 * DAY);
    let renewed = invoke(&harness, "main", &["reconcile", "-e", "pr-1", "--renew", "-o", "json"]);
    assert_eq!(renewed.exit, 0, "{}\n{}", renewed.stderr, renewed.stdout);
    assert_eq!(instance_report(&renewed, 0)["action"], "extended");
    assert_eq!(instance_report(&renewed, 0)["expired"], false, "{}", renewed.report);
    assert_eq!(expires_at(&harness, "pr-1"), "2026-10-09T10:00:00Z");
    assert!(exists(&harness, "pr-1") && !deleted(&harness, "pr-1"));

    let updated = open(&harness, 2);
    assert_eq!(instance_report(&updated, 0)["action"], "extended");
    assert_eq!(expires_at(&harness, "pr-2"), "2026-10-09T10:00:00Z");
    assert!(exists(&harness, "pr-2") && !deleted(&harness, "pr-2"));
}

/// `--renew` and `--no-extend` apply to instances only and to one at a time:
/// rejected for a declared Environment, for the fleet reconcile, and
/// together. Catches an option that silently does nothing.
#[test]
fn test_renew_and_no_extend_are_rejected_where_they_do_not_apply() {
    let environment = "apiVersion: gitops.nyl/v1\nkind: Environment\nmetadata:\n  name: dev\nspec:\n  unitSelector: {matchLabels: {dev: 'true'}}\n  allowUnprotectedSource: true\n";
    let directory = project(&[
        NYL_TOML,
        ("nyl/platform.yaml", PLATFORM),
        ("nyl/preview.yaml", TEMPLATE),
        ("nyl/units.yaml", UNITS),
        ("nyl/dev.yaml", environment),
    ]);
    let harness = harness::Harness::new(directory.path(), true);
    assert_eq!(invoke(&harness, "main", &["state", "init", "-e", "dev"]).exit, 0);
    for option in ["--renew", "--no-extend"] {
        let declared = invoke(&harness, "main", &["reconcile", "-e", "dev", option]);
        assert_eq!(declared.exit, 1, "{}", declared.stderr);
        assert!(
            declared.stderr.contains("NYL-INSTANCE-OPTION-INVALID"),
            "{}",
            declared.stderr
        );
    }
    // Combinations that make no sense are coded findings, not usage errors.
    for args in [
        &["reconcile", "-e", "pr-1", "--renew", "--no-extend"][..],
        &[
            "reconcile",
            "-e",
            "pr-1",
            "--template",
            "preview",
            "--param",
            "pr=1",
            "--no-extend",
        ][..],
        &["reconcile", "--template", "preview", "--no-extend"][..],
    ] {
        let rejected = invoke(&harness, "main", args);
        assert_eq!(rejected.exit, 1, "{args:?}: {}", rejected.stderr);
        assert!(
            rejected.stderr.contains("NYL-INSTANCE-OPTION-INVALID"),
            "{args:?}: {}",
            rejected.stderr
        );
    }
    let fleet = invoke(&harness, "main", &["reconcile", "--template", "preview", "--renew"]);
    assert_eq!(fleet.exit, 1, "{}", fleet.stderr);
    assert!(fleet.stderr.contains("NYL-INSTANCE-OPTION-INVALID"), "{}", fleet.stderr);
    let fleet = invoke(
        &harness,
        "main",
        &["reconcile", "--template", "preview", "--param", "pr=1"],
    );
    assert_eq!(fleet.exit, 1, "{}", fleet.stderr);
    assert!(fleet.stderr.contains("NYL-INSTANCE-OPTION-INVALID"), "{}", fleet.stderr);
}

/// Without `allowTeardown` the removal of an expired instance waits: the
/// units are marked pending teardown, nothing is destroyed, the run exits 2
/// naming the setting; `--allow-teardown` completes it. Catches an unattended
/// removal that destroys resources nobody authorized.
#[test]
fn test_an_expired_instance_waits_for_allow_teardown_and_keeps_its_state() {
    let template = TEMPLATE.replace("  allowTeardown: true\n", "");
    let (_directory, harness) = world(&template, UNITS);
    open(&harness, 1);
    harness.advance(8 * DAY);

    let pending = invoke(&harness, "main", &["reconcile", "-e", "pr-1", "-o", "json"]);
    assert_eq!(pending.exit, 2, "{}\n{}", pending.stderr, pending.stdout);
    assert_eq!(instance_report(&pending, 0)["action"], "removal-pending");
    assert!(
        pending.stderr.contains("NYL-INSTANCE-REMOVAL-PENDING"),
        "{}",
        pending.stderr
    );
    assert_eq!(
        pending.report["environments"][0]["units"]["site"]["result"],
        "pending-teardown"
    );
    assert!(exists(&harness, "pr-1"), "nothing is destroyed without authorization");
    assert!(!deleted(&harness, "pr-1"));

    let allowed = invoke(
        &harness,
        "main",
        &["reconcile", "-e", "pr-1", "--allow-teardown", "-o", "json"],
    );
    assert_eq!(allowed.exit, 0, "{}\n{}", allowed.stderr, allowed.stdout);
    assert_eq!(instance_report(&allowed, 0)["action"], "removed");
    assert!(!exists(&harness, "pr-1") && deleted(&harness, "pr-1"));
}

/// Nothing is removed while resources remain: a teardown that fails keeps the
/// instance's state and its unit, and the run exits 3 so the sweep fails
/// visibly instead of forgetting a preview it could not destroy. Catches a
/// state deletion that outruns a failed teardown.
#[test]
fn test_a_failed_teardown_keeps_the_expired_instances_state() {
    let units = UNITS.replace(
        "teardown: [sh, -c, 'rm -rf {{ values.dir }}/{{ environment.name }}']",
        "teardown: [sh, -c, 'exit 7']",
    );
    let (_directory, harness) = world(TEMPLATE, &units);
    open(&harness, 1);
    harness.advance(8 * DAY);
    let failed = invoke(&harness, "main", &["reconcile", "-e", "pr-1", "-o", "json"]);
    assert_eq!(failed.exit, 3, "{}\n{}", failed.stderr, failed.stdout);
    assert_eq!(instance_report(&failed, 0)["action"], "removal-incomplete");
    assert!(exists(&harness, "pr-1") && !deleted(&harness, "pr-1"));
    let files = git_output(&harness.world.bare(), &["ls-tree", "-r", "--name-only", "nyl/previews"]);
    assert!(files.contains("pr-1/desired/units/site.yaml"), "{files}");
}

/// The fleet reconcile, `reconcile --template X` without `-e`, maintains
/// every live instance at its own recorded commit and removes the expired
/// ones, and never extends an expiry. Catches a sweep that keeps instances
/// alive by touching them, skips the expired, or reconciles from the
/// scheduler's checkout.
#[test]
fn test_the_fleet_removes_the_expired_and_maintains_the_live_without_extending() {
    let (_directory, harness) = world(TEMPLATE, UNITS);
    open(&harness, 1);
    harness.advance(5 * DAY);
    open(&harness, 2);
    harness.advance(3 * DAY);
    // pr-1 expired a day ago; pr-2 has four days left.
    let before = expires_at(&harness, "pr-2");
    let sweep = invoke(&harness, "main", &["reconcile", "--template", "preview", "-o", "json"]);
    assert_eq!(sweep.exit, 0, "{}\n{}", sweep.stderr, sweep.stdout);
    let environments = sweep.report["environments"].as_array().unwrap();
    assert_eq!(environments.len(), 2, "{}", sweep.report);
    let by_name = |name: &str| {
        environments
            .iter()
            .find(|environment| environment["name"] == name)
            .unwrap_or_else(|| panic!("{name} in {}", sweep.report))
    };
    assert_eq!(by_name("pr-1")["instance"]["action"], "removed");
    assert_eq!(by_name("pr-2")["units"]["site"]["result"], "current");
    assert_eq!(expires_at(&harness, "pr-2"), before, "maintenance never extends");
    assert!(!exists(&harness, "pr-1") && deleted(&harness, "pr-1"));
    assert!(exists(&harness, "pr-2"));

    // Nothing left to do for the deleted instance; the live one stays current.
    let again = invoke(&harness, "main", &["reconcile", "--template", "preview", "-o", "json"]);
    assert_eq!(again.exit, 0, "{}\n{}", again.stderr, again.stdout);
    assert_eq!(again.report["environments"].as_array().unwrap().len(), 1);
}

/// An instance another run holds is skipped and reported as busy, not as a
/// failure, and the exit category reflects only what the fleet run processed;
/// a later run removes it. Catches a sweep that fails (or waits) on a preview
/// whose pull request job is running.
#[test]
fn test_the_fleet_reports_an_instance_another_run_holds_as_busy() {
    use nyl_core::orchestration::ids::{EnvironmentName, RunId};
    use nyl_core::orchestration::state::{Lease, LeaseKind, Operation, StateLocation, StateV1};
    let (_directory, harness) = world(TEMPLATE, UNITS);
    open(&harness, 1);
    open(&harness, 2);
    harness.advance(8 * DAY);

    let location = StateLocation {
        repository: harness.world.project_url(),
        desired_ref: "nyl/previews".to_owned(),
        observed_ref: "nyl/previews".to_owned(),
        path: "pr-1".to_owned(),
        coordination_ref_prefix: "refs/nyl/".to_owned(),
    };
    let now = harness.clock.now();
    let lease = Lease {
        api_version: StateV1,
        kind: LeaseKind,
        run: RunId::new("0194f5a0-0000-7000-8000-000000000001").unwrap(),
        runner: "https://ci.example.com/runs/7".to_owned(),
        operation: Operation::Reconcile,
        units: Vec::new(),
        deadline: now + chrono::Duration::minutes(30),
        taken_at: now,
        broken: None,
    };
    let clone = harness.world.fresh_clone("main");
    let store = harness::git_store(&clone, &harness.clock);
    nyl_core::orchestration::ports::StateStore::take_lease(
        &store,
        &location,
        &EnvironmentName::new("pr-1").unwrap(),
        &lease,
        None,
    )
    .unwrap();

    let sweep = invoke(&harness, "main", &["reconcile", "--template", "preview", "-o", "json"]);
    assert_eq!(sweep.exit, 0, "{}\n{}", sweep.stderr, sweep.stdout);
    let environments = sweep.report["environments"].as_array().unwrap();
    let busy = environments
        .iter()
        .find(|environment| environment["name"] == "pr-1")
        .unwrap();
    assert_eq!(busy["instance"]["action"], "busy", "{}", sweep.report);
    assert!(exists(&harness, "pr-1") && !deleted(&harness, "pr-1"));
    let removed = environments
        .iter()
        .find(|environment| environment["name"] == "pr-2")
        .unwrap();
    assert_eq!(removed["instance"]["action"], "removed");

    // Once the other run is gone, a later sweep removes it.
    harness.advance(60);
    let later = invoke(&harness, "main", &["reconcile", "--template", "preview", "-o", "json"]);
    assert_eq!(later.exit, 0, "{}\n{}", later.stderr, later.stdout);
    assert!(!exists(&harness, "pr-1") && deleted(&harness, "pr-1"));
}

fn limited(max: u32) -> String {
    format!("{TEMPLATE}  maxInstances: {max}\n")
}

fn init(harness: &harness::Harness, pr: u32, extra: &[&str]) -> CommandOutcome {
    let name = format!("pr-{pr}");
    let mut args = vec![
        "state".to_owned(),
        "init".to_owned(),
        "-e".to_owned(),
        name,
        "--template".to_owned(),
        "preview".to_owned(),
        "--param".to_owned(),
        format!("pr={pr}"),
        "-o".to_owned(),
        "json".to_owned(),
    ];
    args.extend(extra.iter().map(|arg| (*arg).to_owned()));
    let args = args.iter().map(String::as_str).collect::<Vec<_>>();
    invoke(harness, "main", &args)
}

/// A template at `maxInstances` creates no further instance: `state init
/// --template` refuses with exit 2 and writes nothing, naming the expired
/// instances that could go; `--make-room` removes the oldest expired ones,
/// only as many as needed, and never a live instance; updating an existing
/// instance is never limited. Catches a limit that is not enforced, one that
/// is enforced against updates, and an eviction of a live preview.
#[test]
fn test_a_template_at_max_instances_refuses_until_room_is_made() {
    let (_directory, harness) = world(&limited(2), UNITS);
    assert_eq!(init(&harness, 1, &[]).exit, 0);
    harness.advance(DAY);
    assert_eq!(init(&harness, 2, &[]).exit, 0);
    open(&harness, 2);

    let full = init(&harness, 3, &[]);
    assert_eq!(full.exit, 2, "{}\n{}", full.stderr, full.stdout);
    assert!(full.stderr.contains("NYL-INSTANCE-LIMIT"), "{}", full.stderr);
    assert!(full.stderr.contains("no instance is expired"), "{}", full.stderr);
    let files = git_output(&harness.world.bare(), &["ls-tree", "-r", "--name-only", "nyl/previews"]);
    assert!(
        !files.contains("pr-3/"),
        "nothing is written when the limit refuses: {files}"
    );

    // An update of an existing instance at the limit is not a new instance.
    assert_eq!(init(&harness, 2, &[]).exit, 0);

    // pr-1 expires first (day 7), then pr-2 (day 8).
    harness.advance(6 * DAY + 60);
    let named = init(&harness, 3, &[]);
    assert_eq!(named.exit, 2, "{}\n{}", named.stderr, named.stdout);
    assert!(
        named.stderr.contains("the expired instances pr-1 could be removed"),
        "{}",
        named.stderr
    );
    assert!(named.stderr.contains("--make-room"), "{}", named.stderr);
    assert!(!deleted(&harness, "pr-1"), "refusing removes nothing");

    let room = init(&harness, 3, &["--make-room"]);
    assert_eq!(room.exit, 0, "{}\n{}", room.stderr, room.stdout);
    assert_eq!(room.report["details"]["evicted"], serde_json::json!(["pr-1"]));
    assert!(deleted(&harness, "pr-1"));
    assert!(!deleted(&harness, "pr-2"), "only as many as needed, the oldest first");
    assert_eq!(record(&harness, "pr-3")["template"], "preview");

    // The pull request job creating a fourth instance meets the same limit.
    let job = invoke(
        &harness,
        "main",
        &["reconcile", "-e", "pr-4", "--template", "preview", "--param", "pr=4"],
    );
    assert_eq!(job.exit, 2, "{}\n{}", job.stderr, job.stdout);
    assert!(job.stderr.contains("NYL-INSTANCE-LIMIT"), "{}", job.stderr);
}

/// `--make-room` removes an expired instance with the same gates as any
/// removal: when its teardown cannot finish, the new instance is not created
/// and the refusal says which removal has not finished. Catches a creation
/// that goes ahead over a preview that could not be torn down.
#[test]
fn test_make_room_does_not_create_an_instance_over_an_unfinished_removal() {
    let units = UNITS.replace(
        "teardown: [sh, -c, 'rm -rf {{ values.dir }}/{{ environment.name }}']",
        "teardown: [sh, -c, 'exit 7']",
    );
    let (_directory, harness) = world(&limited(1), &units);
    open(&harness, 1);
    harness.advance(8 * DAY);
    let room = init(&harness, 2, &["--make-room"]);
    assert_eq!(room.exit, 2, "{}\n{}", room.stderr, room.stdout);
    assert!(room.stderr.contains("has not finished"), "{}", room.stderr);
    let files = git_output(&harness.world.bare(), &["ls-tree", "-r", "--name-only", "nyl/previews"]);
    assert!(!files.contains("pr-2/"), "{files}");
    assert!(files.contains("pr-1/desired/units/site.yaml"), "{files}");
}

/// `maxInstances` counts the instances Nyl lists, so a template whose
/// instances cannot be listed, with a limit, is invalid before it is used and
/// refuses to create an instance after. Catches a limit that cannot be counted
/// being silently ignored.
#[test]
fn test_max_instances_needs_a_listable_state_location() {
    let template = limited(2).replace("    path: '{{ environment.name }}'\n", "");
    let (_directory, harness) = world(&template, UNITS);
    let created = init(&harness, 1, &[]);
    assert_eq!(created.exit, 1, "{}\n{}", created.stderr, created.stdout);
    assert!(
        created.stderr.contains("NYL-INSTANCE-NOT-LISTABLE"),
        "{}",
        created.stderr
    );
    assert!(created.stderr.contains("state.path"), "{}", created.stderr);

    // `nyl validate` reads no state, so it finds the same fault in the declaration.
    let root = tempfile::TempDir::new().unwrap();
    for (path, text) in [
        NYL_TOML,
        (
            "nyl/platform.yaml",
            &PLATFORM.replace("%PROJECT_URL%", "https://example.invalid/platform.git"),
        ),
        ("nyl/preview.yaml", &template.replace("%SCENARIO_TMP%", "/var/empty")),
        ("nyl/units.yaml", UNITS),
    ] {
        let target = root.path().join(path);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(target, text).unwrap();
    }
    for args in [
        &["init", "--quiet", "--initial-branch=main"][..],
        &["add", "--all"][..],
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "project",
        ][..],
    ] {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(root.path())
            .status()
            .unwrap();
        assert!(status.success(), "{args:?}");
    }
    let validate = assert_cmd::Command::new(assert_cmd::cargo::cargo_bin!("nyl"))
        .args(["validate", "nyl"])
        .current_dir(root.path())
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&validate.get_output().stderr).into_owned();
    assert!(
        stderr.contains("NYL-TEMPLATE-INVALID") && stderr.contains("maxInstances"),
        "{stderr}"
    );
}

/// The keep refs of `instance` in the source repository, by name and commit.
fn keep_refs(harness: &harness::Harness, instance: &str) -> Vec<(String, String)> {
    // The refs are named by the keep id the instance's record holds, which survives its deletion marker.
    let Some(id) = record(harness, instance)["keepId"].as_str().map(str::to_owned) else {
        return Vec::new();
    };
    let prefix = nyl_orchestration::rules::keep::keep_prefix(&nyl_core::orchestration::ids::Uid::new(id).unwrap());
    git_output(
        &harness.world.bare(),
        &[
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            &format!("{prefix}/"),
        ],
    )
    .lines()
    .filter_map(|line| line.split_once(' '))
    .map(|(name, commit)| (name.to_owned(), commit.to_owned()))
    .collect()
}

/// Whether the keep refs of `instance` are exactly the one for `commit`.
fn kept_only(harness: &harness::Harness, instance: &str, commit: &str) -> bool {
    let refs = keep_refs(harness, instance);
    refs.len() == 1 && refs[0].0.ends_with(&format!("/{commit}")) && refs[0].1 == commit
}

/// With `keepSource: true` an instance's recorded source commit stays
/// fetchable through a keep ref of its own, `refs/nyl-keep/<project>/<instance>/<commit>`
/// in the source repository: a new commit is kept before the record names it
/// and the previous one's ref goes after, a job in a fresh clone that lacks the
/// commit fetches it, and `state delete` removes the refs. Catches a teardown
/// that cannot find the source of a pull request whose branch is gone, and
/// keep refs that outlive their instances.
#[test]
fn test_the_keep_ref_follows_the_recorded_source_until_the_state_is_deleted() {
    let (_directory, harness) = world(&keeping(TEMPLATE), UNITS);
    harness.world.branch(&scenario::format::BranchStep {
        name: "pr-9".to_owned(),
        from: "main".to_owned(),
    });
    harness
        .world
        .commit_files("pr-9", "First change", &[("nyl/note.txt", "one")]);
    let reconcile = |branch: &str| {
        let created = invoke(
            &harness,
            branch,
            &[
                "reconcile",
                "-e",
                "pr-9",
                "--template",
                "preview",
                "--param",
                "pr=9",
                "-o",
                "json",
            ],
        );
        assert_eq!(created.exit, 0, "{}\n{}", created.stderr, created.stdout);
    };
    reconcile("pr-9");
    let first = harness.world.rev_parse("pr-9");
    assert!(kept_only(&harness, "pr-9", &first), "{:?}", keep_refs(&harness, "pr-9"));
    assert_eq!(record(&harness, "pr-9")["sourceCommit"], first.as_str());

    harness
        .world
        .commit_files("pr-9", "Second change", &[("nyl/note.txt", "two")]);
    reconcile("pr-9");
    let second = harness.world.rev_parse("pr-9");
    assert_ne!(first, second);
    assert!(
        kept_only(&harness, "pr-9", &second),
        "the new commit is kept and the old one's ref pruned: {:?}",
        keep_refs(&harness, "pr-9")
    );

    // The pull request is closed and its branch deleted: a job in a fresh
    // clone of `main` does not have the commit, and finds it through the keep ref,
    // which the resolver tries before fetching the id from the origin.
    harness.world.delete_branch("pr-9");
    let plain = invoke(
        &harness,
        "main",
        &["reconcile", "-e", "pr-9", "--no-extend", "-o", "json"],
    );
    assert_eq!(plain.exit, 0, "{}\n{}", plain.stderr, plain.stdout);
    assert_eq!(plain.report["environments"][0]["sourceCommit"], second.as_str());

    let closed = invoke(
        &harness,
        "main",
        &["state", "delete", "-e", "pr-9", "--teardown", "-o", "json"],
    );
    assert_eq!(closed.exit, 0, "{}\n{}", closed.stderr, closed.stdout);
    assert!(!exists(&harness, "pr-9") && deleted(&harness, "pr-9"));
    assert!(
        keep_refs(&harness, "pr-9").is_empty(),
        "state delete removes the keep refs"
    );
}

/// A keep ref that does not name the commit its name says is not this
/// instance's: `state delete` warns and leaves it, and removes the rest. Catches
/// a delete that removes another project's refs it happens to match by name.
#[test]
fn test_state_delete_leaves_a_keep_ref_that_is_not_the_instances() {
    let (_directory, harness) = world(&keeping(TEMPLATE), UNITS);
    open(&harness, 9);
    let id = record(&harness, "pr-9")["keepId"].as_str().unwrap().to_owned();
    let prefix = nyl_orchestration::rules::keep::keep_prefix(&nyl_core::orchestration::ids::Uid::new(id).unwrap());
    let foreign = format!("{prefix}/{}", "f".repeat(40));
    scenario::world::git(
        &harness.world.bare(),
        &["update-ref", &foreign, &harness.world.rev_parse("main")],
    );
    let closed = invoke(
        &harness,
        "main",
        &["state", "delete", "-e", "pr-9", "--teardown", "-o", "json"],
    );
    assert_eq!(closed.exit, 0, "{}\n{}", closed.stderr, closed.stdout);
    assert!(closed.stderr.contains("NYL-INSTANCE-KEEP-FAILED"), "{}", closed.stderr);
    let left = keep_refs(&harness, "pr-9");
    assert_eq!(left.len(), 1, "{left:?}");
    assert_eq!(left[0].0, foreign);
}

/// A source repository that refuses the keep ref refuses the job, with both
/// ways out in the fix, and writes no record: the record never names a commit
/// that is not kept. Catches a state that moves ahead of its keep ref, and a
/// failure whose fix names one option.
#[cfg(unix)]
#[test]
fn test_a_refused_keep_ref_refuses_the_job_before_the_record_is_written() {
    use std::os::unix::fs::PermissionsExt as _;
    let (_directory, harness) = world(&keeping(TEMPLATE), UNITS);
    let hook = harness.world.bare().join("hooks/pre-receive");
    std::fs::write(
        &hook,
        "#!/bin/sh\nwhile read old new ref; do case \"$ref\" in refs/nyl-keep/*) echo 'keep refs are not allowed' >&2; exit 1;; esac; done\n",
    )
    .unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    let refused = invoke(
        &harness,
        "main",
        &["reconcile", "-e", "pr-1", "--template", "preview", "--param", "pr=1"],
    );
    assert_eq!(refused.exit, 1, "{}\n{}", refused.stderr, refused.stdout);
    assert!(
        refused.stderr.contains("NYL-INSTANCE-KEEP-FAILED"),
        "{}",
        refused.stderr
    );
    assert!(refused.stderr.contains("push access"), "{}", refused.stderr);
    assert!(refused.stderr.contains("keepSource"), "{}", refused.stderr);
    assert_eq!(
        git_output(&harness.world.bare(), &["for-each-ref", "refs/heads/nyl/previews"]),
        "",
        "no state is written for a commit that is not kept"
    );
    assert!(!exists(&harness, "pr-1"), "nothing ran");
}

/// Keep refs are an opt-in: by default no ref is pushed, and an instance whose
/// branch is gone cannot be run from its recorded commit, which the error
/// says. Catches a default that pushes refs to repositories that never asked.
#[test]
fn test_keep_refs_are_off_by_default() {
    let (_directory, harness) = world(TEMPLATE, UNITS);
    harness.world.branch(&scenario::format::BranchStep {
        name: "pr-9".to_owned(),
        from: "main".to_owned(),
    });
    harness
        .world
        .commit_files("pr-9", "A change", &[("nyl/note.txt", "one")]);
    let created = invoke(
        &harness,
        "pr-9",
        &["reconcile", "-e", "pr-9", "--template", "preview", "--param", "pr=9"],
    );
    assert_eq!(created.exit, 0, "{}\n{}", created.stderr, created.stdout);
    assert!(keep_refs(&harness, "pr-9").is_empty());
    harness.world.delete_branch("pr-9");
    let plain = invoke(&harness, "main", &["reconcile", "-e", "pr-9", "--no-extend"]);
    assert_ne!(plain.exit, 0, "{}\n{}", plain.stderr, plain.stdout);
    assert!(plain.stderr.contains("does not exist"), "{}", plain.stderr);
}

/// Without keep refs, the pull request closed job still tears the instance
/// down at its recorded commit: the commit is fetched by its id from the
/// project's origin, which hosts keep for a pull request after its branch is
/// deleted (`refs/pull/<n>/head`), so a checkout of the default branch needs
/// neither `--source` nor a keep ref. A run without network access cannot
/// fetch it. Catches a close job that fails on every squash-merged pull request.
#[test]
fn test_the_closed_job_fetches_the_recorded_commit_from_the_pull_request_ref() {
    let (_directory, harness) = world(TEMPLATE, UNITS);
    harness.world.branch(&scenario::format::BranchStep {
        name: "pr-9".to_owned(),
        from: "main".to_owned(),
    });
    harness
        .world
        .commit_files("pr-9", "A change", &[("nyl/note.txt", "one")]);
    let created = invoke(
        &harness,
        "pr-9",
        &["reconcile", "-e", "pr-9", "--template", "preview", "--param", "pr=9"],
    );
    assert_eq!(created.exit, 0, "{}\n{}", created.stderr, created.stdout);
    let recorded = harness.world.rev_parse("pr-9");
    assert!(keep_refs(&harness, "pr-9").is_empty());
    scenario::world::git(&harness.world.bare(), &["update-ref", "refs/pull/9/head", &recorded]);
    harness.world.delete_branch("pr-9");

    let offline = invoke(&harness, "main", &["reconcile", "-e", "pr-9", "--no-extend", "--local"]);
    assert_ne!(offline.exit, 0, "{}\n{}", offline.stderr, offline.stdout);
    assert!(offline.stderr.contains("does not exist"), "{}", offline.stderr);
    assert!(exists(&harness, "pr-9"), "nothing ran");

    let closed = invoke(
        &harness,
        "main",
        &["state", "delete", "-e", "pr-9", "--teardown", "-o", "json"],
    );
    assert_eq!(closed.exit, 0, "{}\n{}", closed.stderr, closed.stdout);
    assert!(!exists(&harness, "pr-9") && deleted(&harness, "pr-9"));
}

/// The fallback of a close job that cannot fetch the recorded commit: `state
/// delete --teardown --source <default branch>` fails without it, and with it
/// removes the instance and says on stderr that the files came from another
/// revision. Catches a substitution that is only recorded, where the person
/// running the job never sees it.
#[test]
fn test_state_delete_with_source_warns_about_the_substitution() {
    let (_directory, harness) = world(TEMPLATE, UNITS);
    harness.world.branch(&scenario::format::BranchStep {
        name: "pr-9".to_owned(),
        from: "main".to_owned(),
    });
    harness
        .world
        .commit_files("pr-9", "A change", &[("nyl/note.txt", "one")]);
    let created = invoke(
        &harness,
        "pr-9",
        &["reconcile", "-e", "pr-9", "--template", "preview", "--param", "pr=9"],
    );
    assert_eq!(created.exit, 0, "{}\n{}", created.stderr, created.stdout);
    harness.world.delete_branch("pr-9");

    let refused = invoke(&harness, "main", &["state", "delete", "-e", "pr-9", "--teardown"]);
    assert_eq!(refused.exit, 1, "{}\n{}", refused.stderr, refused.stdout);
    assert!(refused.stderr.contains("--source <revision>"), "{}", refused.stderr);
    assert!(exists(&harness, "pr-9"), "nothing was removed");

    let closed = invoke(
        &harness,
        "main",
        &["state", "delete", "-e", "pr-9", "--teardown", "--source", "main"],
    );
    assert_eq!(closed.exit, 0, "{}\n{}", closed.stderr, closed.stdout);
    assert!(closed.stderr.contains("NYL-SOURCE-OVERRIDE"), "{}", closed.stderr);
    assert!(!exists(&harness, "pr-9") && deleted(&harness, "pr-9"));
}

/// `state move --template` moves the state of every instance of a template to
/// its configured location, found by listing the old one: after the template's
/// `state.path` changed, the instances are unknown until they move, and move
/// together. Catches instances stranded at a location nothing lists, and a
/// move of one that leaves its neighbour behind.
#[test]
fn test_state_move_template_moves_every_instance_to_the_new_location() {
    let old = TEMPLATE.replace("path: '{{ environment.name }}'", "path: 'a/{{ environment.name }}'");
    let (_directory, harness) = world(&old, UNITS);
    for pr in [1, 2] {
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
    harness.world.commit_files(
        "main",
        "Move the previews' state",
        &[(
            "nyl/preview.yaml",
            &TEMPLATE.replace("path: '{{ environment.name }}'", "path: 'b/{{ environment.name }}'"),
        )],
    );
    let stranded = invoke(&harness, "main", &["reconcile", "-e", "pr-1", "--no-extend"]);
    assert_eq!(stranded.exit, 1, "{}", stranded.stderr);
    assert!(
        stranded.stderr.contains("NYL-ENVIRONMENT-UNKNOWN"),
        "{}",
        stranded.stderr
    );

    let nothing = invoke(&harness, "main", &["state", "move", "--template", "preview"]);
    assert_eq!(nothing.exit, 1, "{}", nothing.stderr);
    assert!(
        nothing.stderr.contains("NYL-STATE-LOCATION-MISMATCH"),
        "{}",
        nothing.stderr
    );

    let moved = invoke(
        &harness,
        "main",
        &[
            "state",
            "move",
            "--template",
            "preview",
            "--state-path",
            "a/{{ environment.name }}",
            "-o",
            "json",
        ],
    );
    assert_eq!(moved.exit, 0, "{}\n{}", moved.stderr, moved.stdout);
    let instances = moved.report["instances"].as_array().unwrap();
    assert_eq!(instances.len(), 2, "{}", moved.report);
    assert!(
        instances.iter().all(|report| report["outcome"] == "done"),
        "{}",
        moved.report
    );
    let files = git_output(&harness.world.bare(), &["ls-tree", "-r", "--name-only", "nyl/previews"]);
    for instance in ["pr-1", "pr-2"] {
        assert!(
            files.contains(&format!("b/{instance}/desired/units/site.yaml")),
            "{files}"
        );
    }

    // Both are found at the new location with their units and expiry.
    for instance in ["pr-1", "pr-2"] {
        let again = invoke(
            &harness,
            "main",
            &["reconcile", "-e", instance, "--no-extend", "-o", "json"],
        );
        assert_eq!(again.exit, 0, "{}\n{}", again.stderr, again.stdout);
        assert_eq!(again.report["environments"][0]["units"]["site"]["result"], "current");
    }
    // Moving again finds the instances already there.
    let repeated = invoke(
        &harness,
        "main",
        &[
            "state",
            "move",
            "--template",
            "preview",
            "--state-path",
            "a/{{ environment.name }}",
            "-o",
            "json",
        ],
    );
    assert_eq!(repeated.exit, 0, "{}\n{}", repeated.stderr, repeated.stdout);
}

/// A renamed template leaves its instances' records naming the old one, so no
/// command finds them; `state move --template <new> --from-template <old>`
/// records them as the new template's, in place. Catches a rename that
/// orphans every open preview.
#[test]
fn test_state_move_template_renames_the_template_of_its_instances() {
    let (_directory, harness) = world(&keeping(TEMPLATE), UNITS);
    open(&harness, 1);
    harness.world.commit_files(
        "main",
        "Rename the template",
        &[(
            "nyl/preview.yaml",
            &TEMPLATE.replace("name: preview}", "name: previews}"),
        )],
    );
    let keep_id = record(&harness, "pr-1")["keepId"].clone();
    assert!(keep_id.is_string(), "the instance recorded the id of its keep refs");
    let orphaned = invoke(&harness, "main", &["reconcile", "-e", "pr-1", "--no-extend"]);
    assert_eq!(orphaned.exit, 1, "{}", orphaned.stderr);
    assert!(
        orphaned.stderr.contains("NYL-INSTANCE-TEMPLATE-UNKNOWN"),
        "{}",
        orphaned.stderr
    );

    let moved = invoke(
        &harness,
        "main",
        &[
            "state",
            "move",
            "--template",
            "previews",
            "--from-template",
            "preview",
            "-o",
            "json",
        ],
    );
    assert_eq!(moved.exit, 0, "{}\n{}", moved.stderr, moved.stdout);
    assert_eq!(record(&harness, "pr-1")["template"], "previews");
    assert_eq!(
        record(&harness, "pr-1")["keepId"],
        keep_id,
        "a rename does not strand the keep refs"
    );
    // The instance renders at its recorded commit, which predates the rename,
    // so its pull request job moves it to a commit that declares the new name.
    let job = invoke(
        &harness,
        "main",
        &[
            "reconcile",
            "-e",
            "pr-1",
            "--template",
            "previews",
            "--param",
            "pr=1",
            "-o",
            "json",
        ],
    );
    assert_eq!(job.exit, 0, "{}\n{}", job.stderr, job.stdout);
    assert_eq!(job.report["environments"][0]["units"]["site"]["result"], "current");
    // Closing it needs no source: the stored documents carry what teardown runs.
    let closed = invoke(&harness, "main", &["state", "delete", "-e", "pr-1", "--teardown"]);
    assert_eq!(closed.exit, 0, "{}\n{}", closed.stderr, closed.stdout);
}

/// Removing an expired instance renders nothing from source, so a lost source
/// commit (no keep ref, branch deleted) fails only that instance's teardown,
/// with the way forward, and the sweep still maintains the others; `--source`
/// then tears it down with another revision's files. Catches a sweep that
/// stops at one preview whose pull request branch is gone.
#[test]
fn test_a_lost_source_commit_fails_only_that_instances_removal() {
    let (_directory, harness) = world(TEMPLATE, UNITS);
    harness.world.branch(&scenario::format::BranchStep {
        name: "pr-9".to_owned(),
        from: "main".to_owned(),
    });
    harness
        .world
        .commit_files("pr-9", "A change", &[("nyl/note.txt", "one")]);
    let created = invoke(
        &harness,
        "pr-9",
        &["reconcile", "-e", "pr-9", "--template", "preview", "--param", "pr=9"],
    );
    assert_eq!(created.exit, 0, "{}\n{}", created.stderr, created.stdout);
    harness.advance(5 * DAY);
    open(&harness, 1);
    harness.advance(3 * DAY);
    harness.world.delete_branch("pr-9");

    let sweep = invoke(&harness, "main", &["reconcile", "--template", "preview", "-o", "json"]);
    assert_ne!(sweep.exit, 0, "{}\n{}", sweep.stderr, sweep.stdout);
    assert!(sweep.stderr.contains("--source <revision>"), "{}", sweep.stderr);
    let environments = sweep.report["environments"].as_array().unwrap();
    let live = environments
        .iter()
        .find(|environment| environment["name"] == "pr-1")
        .unwrap();
    assert_eq!(live["units"]["site"]["result"], "current", "{}", sweep.report);
    assert!(exists(&harness, "pr-9") && !deleted(&harness, "pr-9"));

    let removed = invoke(
        &harness,
        "main",
        &["reconcile", "-e", "pr-9", "--source", "main", "-o", "json"],
    );
    assert_eq!(removed.exit, 0, "{}\n{}", removed.stderr, removed.stdout);
    assert!(!exists(&harness, "pr-9") && deleted(&harness, "pr-9"));
}

/// A removal that waits for authorization is undone by a renewal: the units
/// are active again in the commit that renews, so the pull request job that
/// keeps the preview updates it instead of exiting 2 behind a fenced removal,
/// and a later fleet run with `--allow-teardown` has nothing to tear down.
/// Catches a live preview destroyed by a removal decided before it was renewed.
#[test]
fn test_a_renewal_undoes_a_removal_that_waited_for_authorization() {
    let template = TEMPLATE.replace("  allowTeardown: true\n", "");
    let (_directory, harness) = world(&template, UNITS);
    open(&harness, 1);
    open(&harness, 2);
    harness.advance(8 * DAY);
    for instance in ["pr-1", "pr-2"] {
        let pending = invoke(&harness, "main", &["reconcile", "-e", instance, "-o", "json"]);
        assert_eq!(pending.exit, 2, "{}\n{}", pending.stderr, pending.stdout);
    }
    let state = |instance: &str| {
        let status = invoke(&harness, "main", &["status", "-e", instance, "-o", "json"]);
        status.report["environments"][0]["units"]["site"]["state"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    };
    assert_eq!(state("pr-1"), "pending-teardown");

    // The pull request job renews pr-1 and updates it.
    let job = open(&harness, 1);
    assert_eq!(
        job.report["environments"][0]["units"]["site"]["result"], "current",
        "{}",
        job.report
    );
    assert_eq!(state("pr-1"), "current");

    // `--renew` renews pr-2, and the sweep that is allowed to tear down has nothing to remove.
    let renewed = invoke(&harness, "main", &["reconcile", "-e", "pr-2", "--renew", "-o", "json"]);
    assert_eq!(renewed.exit, 0, "{}\n{}", renewed.stderr, renewed.stdout);
    let sweep = invoke(
        &harness,
        "main",
        &["reconcile", "--template", "preview", "--allow-teardown", "-o", "json"],
    );
    assert_eq!(sweep.exit, 0, "{}\n{}", sweep.stderr, sweep.stdout);
    for instance in ["pr-1", "pr-2"] {
        assert!(exists(&harness, instance) && !deleted(&harness, instance), "{instance}");
        assert_eq!(state(instance), "current");
    }
}

/// The fleet removes an expired preview, and its pull request's next job
/// creates it again as a new instance, under `maxInstances`; `state init
/// --template` still needs `--fresh` for a deleted one. Catches a pull request
/// that cannot be updated after the sweep removed its preview, and a recreation
/// that slips past the limit.
#[test]
fn test_the_pull_request_job_recreates_an_instance_the_fleet_removed() {
    let (_directory, harness) = world(&keeping(&limited(1)), UNITS);
    open(&harness, 1);
    harness.advance(8 * DAY);
    let sweep = invoke(&harness, "main", &["reconcile", "--template", "preview", "-o", "json"]);
    assert_eq!(sweep.exit, 0, "{}\n{}", sweep.stderr, sweep.stdout);
    assert!(deleted(&harness, "pr-1") && !exists(&harness, "pr-1"));

    let plain = init(&harness, 1, &[]);
    assert_eq!(plain.exit, 1, "{}\n{}", plain.stderr, plain.stdout);
    assert!(plain.stderr.contains("NYL-STATE-DELETED"), "{}", plain.stderr);
    assert!(
        keep_refs(&harness, "pr-1").is_empty(),
        "a refused record leaves no keep ref behind"
    );

    // Another preview took the only slot: the recreation is a new instance and meets the limit.
    open(&harness, 2);
    let full = invoke(
        &harness,
        "main",
        &["reconcile", "-e", "pr-1", "--template", "preview", "--param", "pr=1"],
    );
    assert_eq!(full.exit, 2, "{}\n{}", full.stderr, full.stdout);
    assert!(full.stderr.contains("NYL-INSTANCE-LIMIT"), "{}", full.stderr);
    assert!(deleted(&harness, "pr-1"));

    harness.advance(8 * DAY);
    assert_eq!(
        invoke(&harness, "main", &["reconcile", "--template", "preview"]).exit,
        0
    );
    assert!(deleted(&harness, "pr-2"));
    let job = open(&harness, 1);
    assert_eq!(
        job.report["environments"][0]["units"]["site"]["result"], "executed",
        "{}",
        job.report
    );
    assert!(!deleted(&harness, "pr-1") && exists(&harness, "pr-1"));
}
