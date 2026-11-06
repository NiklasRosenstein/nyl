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
    try_invoke(harness, branch, args).unwrap()
}

/// [`invoke`] for a command that may fail with an error rather than a report.
fn try_invoke(harness: &harness::Harness, branch: &str, args: &[&str]) -> Result<CommandOutcome, String> {
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
    invoke_in(&harness.world, &wiring, &args, branch, "")
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
    let retained = UNITS.replace("  idempotent: true", "  idempotent: true\n  deletionPolicy: Retain");
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

fn first_error_code(outcome: &CommandOutcome) -> String {
    [
        "NYL-INSTANCE-TEMPLATE-UNKNOWN",
        "NYL-INSTANCE-PARAMETER-INVALID",
        "NYL-INSTANCE-NAME-TAKEN",
        "NYL-INSTANCE-TEMPLATE-REQUIRED",
        "NYL-ENVIRONMENT-UNKNOWN",
        "NYL-TEMPLATE-INVALID",
    ]
    .into_iter()
    .find(|code| outcome.stderr.contains(code))
    .unwrap_or_default()
    .to_owned()
}

/// Every way to name an instance wrongly is refused with its own code before
/// anything is written, and says what to do: an unknown template, a
/// parameter of the wrong type, a missing or undeclared one, `--param`
/// without `--template`, a name a declared Environment or another
/// instance already uses, and a name nothing declares. Catches an instance
/// created from a guess, or a refusal that is not actionable.
#[test]
fn test_naming_an_instance_wrongly_is_refused_with_a_code_and_writes_nothing() {
    let environment = "apiVersion: gitops.nyl/v1\nkind: Environment\nmetadata:\n  name: dev\nspec:\n  unitSelector: {matchLabels: {dev: 'true'}}\n  allowUnprotectedSource: true\n";
    let other = TEMPLATE
        .replace("name: preview", "name: other")
        .replace("nyl/previews", "nyl/others");
    let (_directory, harness) = world(&[("nyl/dev.yaml", environment), ("nyl/other.yaml", &other)]);
    let init = |args: &[&str]| {
        let mut command = vec!["state", "init"];
        command.extend_from_slice(args);
        invoke(&harness, "main", &command)
    };
    let refused = [
        (
            vec!["-e", "pr-1", "--template", "nope", "--param", "pr=1"],
            "NYL-INSTANCE-TEMPLATE-UNKNOWN",
        ),
        (
            vec!["-e", "pr-1", "--template", "preview", "--param", "pr=one"],
            "NYL-INSTANCE-PARAMETER-INVALID",
        ),
        (
            vec!["-e", "pr-1", "--template", "preview"],
            "NYL-INSTANCE-PARAMETER-INVALID",
        ),
        (
            vec![
                "-e",
                "pr-1",
                "--template",
                "preview",
                "--param",
                "pr=1",
                "--param",
                "x=1",
            ],
            "NYL-INSTANCE-PARAMETER-INVALID",
        ),
        (vec!["-e", "pr-1", "--param", "pr=1"], "NYL-INSTANCE-TEMPLATE-REQUIRED"),
        (
            vec!["-e", "dev", "--template", "preview", "--param", "pr=1"],
            "NYL-INSTANCE-NAME-TAKEN",
        ),
        (
            vec![
                "-e",
                "pr-1",
                "--template",
                "preview",
                "--param",
                "size=huge",
                "--param",
                "pr=1",
            ],
            "NYL-INSTANCE-PARAMETER-INVALID",
        ),
    ];
    for (args, code) in refused {
        let outcome = init(&args);
        assert_eq!(outcome.exit, 1, "{args:?}: {}", outcome.stderr);
        assert_eq!(first_error_code(&outcome), code, "{args:?}: {}", outcome.stderr);
    }
    let unknown = invoke(&harness, "main", &["reconcile", "-e", "pr-9"]);
    assert_eq!(unknown.exit, 1);
    assert!(unknown.stderr.contains("NYL-ENVIRONMENT-UNKNOWN"), "{}", unknown.stderr);
    assert!(
        unknown.stderr.contains("--template"),
        "the fix names how to create an instance: {}",
        unknown.stderr
    );

    // A name another template's instance uses is taken.
    assert_eq!(
        init(&["-e", "pr-1", "--template", "preview", "--param", "pr=1"]).exit,
        0
    );
    let taken = init(&["-e", "pr-1", "--template", "other", "--param", "pr=1"]);
    assert_eq!(taken.exit, 1);
    assert_eq!(first_error_code(&taken), "NYL-INSTANCE-NAME-TAKEN", "{}", taken.stderr);
    assert!(!git_output(&harness.world.bare(), &["branch", "--list", "nyl/others"]).contains("nyl/others"));
}

/// `get environments` lists the declared environments and the live
/// instances found by listing the template's state location, with their
/// expiry; a deleted instance is gone. Catches an instance that needs a
/// central index to be found, and a deleted one that lingers.
#[test]
fn test_get_environments_lists_instances_by_listing_their_state() {
    let environment = "apiVersion: gitops.nyl/v1\nkind: Environment\nmetadata:\n  name: dev\nspec:\n  unitSelector: {matchLabels: {dev: 'true'}}\n  allowUnprotectedSource: true\n";
    let (_directory, harness) = world(&[("nyl/dev.yaml", environment)]);
    for pr in ["1", "2"] {
        let created = invoke(
            &harness,
            "main",
            &[
                "state",
                "init",
                "-e",
                &format!("pr-{pr}"),
                "--template",
                "preview",
                "--param",
                &format!("pr={pr}"),
            ],
        );
        assert_eq!(created.exit, 0, "{}", created.stderr);
    }
    // The command line appends the project path, so each listing names one environment.
    for (name, template) in [("dev", "-"), ("pr-1", "preview"), ("pr-2", "preview")] {
        let listed = invoke(&harness, "main", &["get", "environments", name]);
        assert_eq!(listed.exit, 0, "{}", listed.stderr);
        let row = listed
            .stdout
            .lines()
            .nth(1)
            .unwrap()
            .split_whitespace()
            .take(2)
            .collect::<Vec<_>>();
        assert_eq!(row, [name, template], "{}", listed.stdout);
        assert_eq!(
            listed.stdout.contains("2026-10-01T10:00:00Z"),
            template == "preview",
            "{}",
            listed.stdout
        );
    }

    assert_eq!(invoke(&harness, "main", &["state", "delete", "-e", "pr-1"]).exit, 0);
    let gone = invoke(&harness, "main", &["get", "environments", "pr-1"]);
    assert_eq!(gone.exit, 1);
    assert!(gone.stderr.contains("was not found"), "{}", gone.stderr);
    assert_eq!(invoke(&harness, "main", &["get", "environments", "pr-2"]).exit, 0);
}

/// A unit the pull request removes from the template's selection is torn
/// down by the next reconcile of the instance without `--allow-teardown`,
/// because the template sets `allowTeardown`. Catches omission that stalls
/// as pending teardown on an instance, which no operator would be there to
/// approve.
#[test]
fn test_a_unit_left_by_the_template_is_torn_down_unattended() {
    let extra = UNITS
        .replace("name: site", "name: extra")
        .replace("{{ values.dir }}", "{{ values.dir }}-extra");
    let both = format!("{UNITS}---\n{extra}");
    let (_directory, harness) = world(&[("nyl/units.yaml", &both)]);
    let reconcile = |branch: &str| {
        invoke(
            &harness,
            branch,
            &[
                "reconcile",
                "-e",
                "pr-4",
                "--template",
                "preview",
                "--param",
                "pr=4",
                "-o",
                "json",
            ],
        )
    };
    harness.world.branch(&scenario::format::BranchStep {
        name: "pr-4".to_owned(),
        from: "main".to_owned(),
    });
    let created = reconcile("pr-4");
    assert_eq!(created.exit, 0, "{}\n{}", created.stderr, created.stdout);
    let units = created.report["environments"][0]["units"].as_object().unwrap().clone();
    assert_eq!(
        units.keys().collect::<Vec<_>>(),
        ["extra", "site"],
        "{}",
        created.report
    );

    let left = extra.replace("preview: 'true'", "other: 'true'");
    harness.world.commit_files(
        "pr-4",
        "Drop the extra unit from the preview",
        &[("nyl/units.yaml", &format!("{UNITS}---\n{left}"))],
    );
    let leaving = reconcile("pr-4");
    assert_eq!(leaving.exit, 0, "{}\n{}", leaving.stderr, leaving.stdout);
    let result = |unit: &str| leaving.report["environments"][0]["units"][unit]["result"].clone();
    assert_eq!(result("extra"), "removed", "{}", leaving.report);
    assert_eq!(result("site"), "current", "{}", leaving.report);
    assert!(!harness
        .world
        .scenario_tmp()
        .join("previews-extra")
        .join("pr-4")
        .exists());
    assert!(tag(&harness, "pr-4").is_some());
}

/// Templates that share a state ref (what the contract recommends: one ref,
/// a directory per instance) must not make each other's instances
/// ambiguous: every command still finds `pr-1` as the instance of the template
/// its record names, the other template cannot take the name, and each
/// template's instances are listed with their own template. Catches every
/// command for an instance failing with `NYL-INSTANCE-AMBIGUOUS` as soon as a
/// second template exists.
#[test]
fn test_two_templates_sharing_a_state_ref_keep_their_instances_apart() {
    let other = TEMPLATE.replace("name: preview", "name: e2e");
    let (_directory, harness) = world(&[("nyl/e2e.yaml", &other)]);
    let init = |name: &str, template: &str, pr: &str| {
        invoke(
            &harness,
            "main",
            &["state", "init", "-e", name, "--template", template, "--param", pr],
        )
    };
    assert_eq!(init("pr-1", "preview", "pr=1").exit, 0);
    assert_eq!(init("pr-2", "e2e", "pr=2").exit, 0);

    for name in ["pr-1", "pr-2"] {
        let status = invoke(&harness, "main", &["status", "-e", name, "-o", "json"]);
        assert_eq!(status.exit, 0, "{name}: {}", status.stderr);
        assert!(!status.stderr.contains("AMBIGUOUS"), "{}", status.stderr);
    }
    let taken = init("pr-1", "e2e", "pr=1");
    assert_eq!(taken.exit, 1);
    assert_eq!(first_error_code(&taken), "NYL-INSTANCE-NAME-TAKEN", "{}", taken.stderr);
    for (name, template) in [("pr-1", "preview"), ("pr-2", "e2e")] {
        let listed = invoke(&harness, "main", &["get", "environments", name]);
        let row = listed
            .stdout
            .lines()
            .nth(1)
            .unwrap()
            .split_whitespace()
            .take(2)
            .collect::<Vec<_>>();
        assert_eq!(row, [name, template], "{}", listed.stdout);
    }
    let deleted = invoke(&harness, "main", &["state", "delete", "-e", "pr-1", "--teardown"]);
    assert_eq!(deleted.exit, 0, "{}", deleted.stderr);
}

/// An instance is an environment of every check across environments: the run
/// request lists the state location of every live instance, the session's own
/// included, because owner fencing, `exec`'s ownership check, and the
/// protection of producers other environments read are all answered from that
/// list. Catches an instance whose claims, consumers, or own incarnations
/// are invisible to them.
#[test]
fn test_every_live_instance_is_in_the_run_requests_environments() {
    let (_directory, harness) = world(&[]);
    for pr in ["1", "2"] {
        let created = invoke(
            &harness,
            "main",
            &[
                "state",
                "init",
                "-e",
                &format!("pr-{pr}"),
                "--template",
                "preview",
                "--param",
                &format!("pr={pr}"),
            ],
        );
        assert_eq!(created.exit, 0, "{}", created.stderr);
    }
    let clone = harness.world.fresh_clone("main");
    let session = nyl::orchestration::session::Session::open(&clone, "pr-1").unwrap_or_else(|_| panic!("opens"));
    let request = session
        .request(&nyl::orchestration::session::RunOptions {
            units: &[],
            concurrency: 1,
            source: None,
            wait: None,
            local: None,
            cancellation: Cancellation::default(),
            mode: nyl_orchestration::shell::run::RunMode::Reconcile,
            lifecycle: nyl_orchestration::shell::run::LifecycleOptions::default(),
            reapply: false,
            fail_fast: false,
            reason: None,
        })
        .unwrap();
    let names = request.environments.keys().map(ToString::to_string).collect::<Vec<_>>();
    assert_eq!(names, ["pr-1", "pr-2"]);
    assert_eq!(request.environments[&request.environment].path, "pr-1");
    assert_eq!(
        request
            .environments
            .values()
            .map(|l| l.path.as_str())
            .collect::<Vec<_>>(),
        ["pr-1", "pr-2"]
    );
    assert!(
        request.environment_errors.is_empty(),
        "{:?}",
        request.environment_errors
    );
}

/// `--local` reads an instance's record without network access too. The
/// template's state is in a remote nothing can reach here, so a read that
/// went to the network fails as unavailable; offline, the instance is simply
/// not in the clone's branches. Catches `--local` commands that still need the
/// network to find an instance.
#[test]
fn test_local_reads_do_not_reach_the_remote_to_find_an_instance() {
    let remote = TEMPLATE.replace(
        "    repositoryRef: {name: platform}\n",
        "    repository: {repoURL: 'https://example.invalid/nyl/state.git'}\n",
    );
    let (_directory, harness) = world(&[("nyl/preview.yaml", &remote)]);
    // Online, finding the instance asks the remote; offline, it asks only the
    // clone's cache, which has nothing yet.
    let commands: [(&[&str], &str); 2] = [
        (&["status", "-e", "pr-1"], "git ls-remote failed"),
        (&["status", "-e", "pr-1", "--local"], "no local state repository"),
    ];
    for (args, reason) in commands {
        let outcome = invoke(&harness, "main", args);
        assert_eq!(outcome.exit, 1, "{args:?}: {}", outcome.stdout);
        assert!(
            outcome.stderr.contains("NYL-STATE-UNAVAILABLE"),
            "{args:?}: {}",
            outcome.stderr
        );
        assert!(outcome.stderr.contains(reason), "{args:?}: {}", outcome.stderr);
    }
}

/// A template whose state cannot be listed leaves its instances unknown, so
/// anything that needs every environment's state fails closed instead of
/// checking without them: a teardown of dev refuses and names the template,
/// and dev's own commands that do not need them still work. Catches a failed
/// listing quietly shrinking the checks to the instances that could be read.
#[test]
fn test_a_template_that_cannot_be_listed_fails_checks_across_environments_closed() {
    let environment = "apiVersion: gitops.nyl/v1\nkind: Environment\nmetadata:\n  name: dev\nspec:\n  unitSelector: {matchLabels: {dev: 'true'}}\n  allowUnprotectedSource: true\n  state: {repositoryRef: {name: platform}}\n";
    let broken = TEMPLATE.replace(
        "    repositoryRef: {name: platform}\n",
        "    repository: {repoURL: 'file:///nonexistent/nyl/state.git'}\n",
    );
    let unit = UNITS.replace("preview: 'true'", "dev: 'true'");
    let (_directory, harness) = world(&[
        ("nyl/dev.yaml", environment),
        ("nyl/preview.yaml", &broken),
        ("nyl/units.yaml", &unit),
    ]);
    assert_eq!(invoke(&harness, "main", &["state", "init", "-e", "dev"]).exit, 0);
    let status = invoke(&harness, "main", &["status", "-e", "dev"]);
    assert_eq!(status.exit, 0, "{}", status.stderr);
    assert!(
        status
            .stderr
            .contains("cannot list the instances of EnvironmentTemplate preview"),
        "{}",
        status.stderr
    );

    assert_eq!(invoke(&harness, "main", &["reconcile", "-e", "dev"]).exit, 0);
    let teardown = invoke(&harness, "main", &["teardown", "-e", "dev", "--all", "-o", "json"]);
    // Nothing is torn down: the unit waits, with the reason.
    assert_eq!(teardown.exit, 2, "{}\n{}", teardown.stderr, teardown.stdout);
    assert!(
        teardown
            .stderr
            .contains("the instances of EnvironmentTemplate preview could not be listed"),
        "{}",
        teardown.stderr
    );
    assert!(teardown.report["environments"][0]["units"]["site"]["result"] != "removed");
}

/// A declared Environment that takes the name of an existing instance wins:
/// listings show the Environment once, and warn that the instance is ignored
/// instead of shadowing it silently. Catches two environments of one name
/// answering to every command.
#[test]
fn test_a_declared_environment_takes_precedence_over_an_instance_of_its_name() {
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
    assert_eq!(created.exit, 0, "{}", created.stderr);
    let environment = "apiVersion: gitops.nyl/v1\nkind: Environment\nmetadata:\n  name: pr-1\nspec:\n  unitSelector: {matchLabels: {other: 'true'}}\n  allowUnprotectedSource: true\n";
    harness
        .world
        .commit_files("main", "Declare pr-1", &[("nyl/pr-1.yaml", environment)]);
    let listed = invoke(&harness, "main", &["get", "environments", "pr-1"]);
    assert_eq!(listed.exit, 0, "{}", listed.stderr);
    assert_eq!(
        listed.stdout.lines().count(),
        2,
        "one row beside the header: {}",
        listed.stdout
    );
    assert!(listed.stderr.contains("NYL-INSTANCE-NAME-TAKEN"), "{}", listed.stderr);
}
