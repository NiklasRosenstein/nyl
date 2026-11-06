//! `nyl build` of a `KubernetesPublication` unit through the command line, with
//! the built-in driver pushing to the world's `project.git`: the render,
//! `--diff`, and `--push`, and what the generic command guarantees around
//! them: the same render path as a run, no environment without `-e`, and a
//! refusal that names its code.
//!
//! Contract: [Direct builds](../../design/orchestration-core.md#direct-builds)
//! and the [Kubernetes publication unit](../../design/orchestration-core.md#kubernetes-publication-unit).

mod scenario;

use clap::Parser;
use nyl::cli::Cli;
use nyl::orchestration::run::{CommandOutcome, Wiring};
use nyl_orchestration::fault::NoFaults;
use nyl_orchestration::shell::run::Cancellation;
use nyl_units::process::ProcessRunner;
use scenario::runner::{invoke_in, AUTHOR};
use scenario::shell::{self as harness, shared_project};
use serde_json::Value;

fn real_publication_harness(project: &std::path::Path) -> harness::Harness {
    let mut harness = harness::Harness::new(project, true);
    harness.drivers = nyl_units::registry::with_real_publication(&harness.script, nyl::resource_catalogue());
    harness
}

fn wiring<'a>(harness: &'a harness::Harness, tools: &'a ProcessRunner) -> Wiring<'a> {
    Wiring {
        clock: harness.clock.clone(),
        ids: &harness.ids,
        drivers: &harness.drivers,
        tools,
        faults: &NoFaults,
        runner: "test-runner".to_owned(),
        author: Some(AUTHOR.parse().unwrap()),
        process_env: harness.process_env.clone(),
        cancellation: Cancellation::default(),
        prompt: None,
        observer: None,
        progress: None,
    }
}

fn invoke(harness: &harness::Harness, args: &[&str]) -> CommandOutcome {
    let tools = ProcessRunner::new();
    let args = args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
    invoke_in(&harness.world, &wiring(harness, &tools), &args, "main", "").unwrap()
}

/// A copy of the shared walkthrough project whose `kubernetes` unit no
/// Environment selects and whose target needs no other unit: the target is
/// placed by nobody, so a direct build may publish it.
fn standalone_project() -> tempfile::TempDir {
    let copy = tempfile::TempDir::new().unwrap();
    for entry in walkdir::WalkDir::new(shared_project()) {
        let entry = entry.unwrap();
        let target = copy.path().join(entry.path().strip_prefix(shared_project()).unwrap());
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(target).unwrap();
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
    let edit = |path: &str, from: &str, to: &str| {
        let file = copy.path().join(path);
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains(from), "{path} holds {from:?}");
        std::fs::write(file, text.replace(from, to)).unwrap();
    };
    edit(
        "nyl/units.yaml",
        "name: kubernetes\n  labels: {dev: 'true'}",
        "name: kubernetes\n  labels: {standalone: 'true'}",
    );
    edit("nyl/units.yaml", "target: '{{ values.target }}'", "target: dev");
    edit(
        "nyl/platform.yaml",
        "      image: {fromUnit: {unit: web-image, artifact: image, pointer: /reference}}\n      databaseHost: {fromUnit: {unit: database, output: host}}",
        "      image: {value: 'registry.invalid:5000/web:direct'}\n      databaseHost: {value: db.internal}",
    );
    copy
}

/// `nyl build kubernetes <options> [PATH] [-- <driver arguments>]` in a fresh
/// clone of `main`; the project path goes before the `--` that starts the
/// driver's arguments.
fn build(harness: &harness::Harness, args: &[&str]) -> CommandOutcome {
    let clone = harness.world.fresh_clone("main");
    build_in(harness, &clone, args)
}

fn build_in(harness: &harness::Harness, clone: &std::path::Path, args: &[&str]) -> CommandOutcome {
    let (options, driver) = match args.iter().position(|arg| *arg == "--") {
        Some(at) => args.split_at(at),
        None => (args, &[][..]),
    };
    let path = clone.display().to_string();
    let argv = ["nyl", "build", "kubernetes"]
        .into_iter()
        .chain(options.iter().copied())
        .chain(std::iter::once(path.as_str()))
        .chain(driver.iter().copied());
    let tools = ProcessRunner::new();
    Cli::try_parse_from(argv)
        .unwrap()
        .execute_in_process(&wiring(harness, &tools))
        .unwrap()
}

fn deploy_branch_exists(harness: &harness::Harness) -> bool {
    !scenario::world::git_output(&harness.world.bare(), &["branch", "--list", "deploy"]).is_empty()
}

/// End to end, for a publication an environment runs: after a reconcile
/// publishes `dev`, a build with `-e dev` renders the same tree through the
/// pinned `fromUnit` inputs of the receipts and finds the published one
/// current; `--diff` names the unit that owns it, and `--push` refuses with
/// its code, the environment, and the way to publish, without touching the
/// branch. Catches a direct build that resolves inputs any other way than a
/// run, or publishes over an environment's tree.
#[test]
fn test_build_of_an_environments_publication_renders_diffs_and_refuses_to_push() {
    let harness = real_publication_harness(&shared_project());
    assert_eq!(invoke(&harness, &["state", "init", "-e", "dev"]).exit, 0);
    let reconcile = invoke(&harness, &["reconcile", "-e", "dev"]);
    assert_eq!(reconcile.exit, 0, "{}\n{}", reconcile.stderr, reconcile.stdout);
    let published = harness.tip("refs/heads/deploy");

    let rendered = build(&harness, &["-e", "dev"]);
    assert_eq!(rendered.exit, 0, "{}", rendered.stderr);
    assert_eq!(rendered.report["kind"], "RenderedTree");
    let files = rendered.report["spec"]["files"].as_object().unwrap();
    assert!(files.keys().any(|path| path.starts_with("dev/web/")), "{files:?}");

    let diff = build(&harness, &["-e", "dev", "--", "--diff"]);
    assert_eq!(diff.exit, 0, "{}", diff.stderr);
    assert_eq!(diff.report["kind"], "TreeDiff");
    assert_eq!(diff.report["spec"]["changes"], "none", "{}", diff.report);
    assert_eq!(diff.report["spec"]["base"], Value::String(published.clone()));
    assert_eq!(diff.report["spec"]["owner"]["environment"], "dev");
    assert_eq!(diff.report["spec"]["owner"]["unit"], "kubernetes");
    assert_eq!(
        diff.stdout.trim_end(),
        serde_json::to_string_pretty(&diff.report).unwrap()
    );

    let refused = build(&harness, &["-e", "dev", "--", "--push"]);
    assert_eq!(refused.exit, 3, "a driver refusal is a failed build");
    assert!(
        refused.stderr.contains("NYL-KUBERNETES-BUILD-TARGET-OWNED"),
        "{}",
        refused.stderr
    );
    assert!(refused.stderr.contains("nyl reconcile -e dev"), "{}", refused.stderr);
    assert_eq!(harness.tip("refs/heads/deploy"), published, "nothing is published");
}

/// Catches a build reading an environment's receipts without being told
/// which environment: the target's `fromUnit` bindings fail the build, naming
/// the way out, instead of rendering with whatever state is at hand.
#[test]
fn test_build_without_an_environment_fails_on_bindings_only_orchestration_resolves() {
    let harness = real_publication_harness(&shared_project());
    let refusal = build(&harness, &[]);
    assert_eq!(refusal.exit, 1);
    assert!(refusal.stderr.contains("-e <environment>"), "{}", refusal.stderr);
    assert!(!deploy_branch_exists(&harness));
}

/// A target no environment places is the one a direct build publishes: the
/// commit is the build's, from the identity the command resolved, the tree
/// records no owner, and the same build again publishes nothing new.
#[test]
fn test_push_publishes_a_target_no_environment_owns() {
    let project = standalone_project();
    let harness = real_publication_harness(project.path());

    let diff = build(&harness, &["--", "--diff"]);
    assert_eq!(diff.exit, 0, "{}", diff.stderr);
    assert_eq!(diff.report["spec"]["changes"], "create");

    let pushed = build(&harness, &["--", "--push"]);
    assert_eq!(pushed.exit, 0, "{}", pushed.stderr);
    assert_eq!(pushed.report["kind"], "PublishedTree");
    let head = harness.tip("refs/heads/deploy");
    assert_eq!(pushed.report["spec"]["commit"], Value::String(head.clone()));
    assert!(harness
        .message("refs/heads/deploy")
        .starts_with("nyl: build --push kubernetes"));
    assert!(
        harness.files("refs/heads/deploy").contains("dev/web/"),
        "the Applications are published"
    );
    let index: Value = serde_json::from_str(&harness.show("refs/heads/deploy", "dev/_nyl/index.json")).unwrap();
    assert_eq!(index["version"], 3);
    assert!(index.get("owner").is_none());

    let diff = build(&harness, &["--", "--diff"]);
    assert_eq!(diff.report["spec"]["changes"], "none", "{}", diff.report);
    assert_eq!(build(&harness, &["--", "--push"]).exit, 0);
    assert_eq!(harness.tip("refs/heads/deploy"), head);
}

/// Catches `--push` attributing a tree to a commit that does not hold it:
/// from a worktree with an uncommitted change it refuses with its code and
/// the way out, while the same worktree still renders and compares.
#[test]
fn test_push_refuses_a_worktree_with_uncommitted_changes() {
    let project = standalone_project();
    let harness = real_publication_harness(project.path());
    let clone = harness.world.fresh_clone("main");
    std::fs::write(clone.join("notes.txt"), "not committed\n").unwrap();
    let run = |extra: &[&str]| build_in(&harness, &clone, extra);
    let refused = run(&["--", "--push"]);
    assert_eq!(refused.exit, 3);
    assert!(
        refused.stderr.contains("NYL-KUBERNETES-BUILD-DIRTY-SOURCE"),
        "{}",
        refused.stderr
    );
    assert!(!deploy_branch_exists(&harness));
    assert_eq!(run(&["--", "--diff"]).exit, 0);
    assert_eq!(run(&["--source", "HEAD", "--", "--push"]).exit, 0);
    assert!(deploy_branch_exists(&harness));
}

/// The shared project with the unit's target declared inline, and no declared target.
fn inline_project() -> tempfile::TempDir {
    let project = standalone_project();
    let edit = |path: &str, from: &str, to: &str| {
        let file = project.path().join(path);
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains(from), "{path} holds {from:?}");
        std::fs::write(file, text.replace(from, to)).unwrap();
    };
    // Selected by Environment dev again, with the target named after it.
    edit(
        "nyl/units.yaml",
        "labels: {standalone: 'true'}",
        "labels: {dev: 'true'}",
    );
    edit(
        "nyl/units.yaml",
        "target: dev\n",
        "target:\n    inline:\n      clusterRef: {name: dev}\n      applicationGroupSelector: {matchLabels: {app: web}}\n      \
         publication: {repositoryRef: {name: platform}, revision: deploy, pathPrefix: preview}\n      \
         releaseInputs:\n        web/web:\n          image: {value: 'registry.invalid:5000/web:direct'}\n          databaseHost: {value: db.internal}\n",
    );
    // The declared target goes: its Applications would collide with the inline target's.
    let platform = project.path().join("nyl/platform.yaml");
    let text = std::fs::read_to_string(&platform).unwrap();
    let documents = text
        .split("---\n")
        .filter(|document| !document.contains("kind: DeploymentTarget"));
    std::fs::write(platform, documents.collect::<Vec<_>>().join("---\n")).unwrap();
    project
}

/// Catches a direct build guessing an environment for an inline target: its
/// name is `<environment>-<unit>` and its templates read the environment, so
/// without `-e` the build refuses with its code and the flag to pass. With
/// `-e` the target is the one a run renders, named after the environment, and
/// an environment that selects the unit owns it, so `--push` refuses it.
#[test]
fn test_inline_target_builds_only_with_an_environment() {
    let project = inline_project();
    let harness = real_publication_harness(project.path());

    let refused = build(&harness, &[]);
    assert_eq!(refused.exit, 1);
    assert!(
        refused.stderr.contains("NYL-KUBERNETES-BUILD-INLINE-TARGET") && refused.stderr.contains("-e <environment>"),
        "{}",
        refused.stderr
    );

    let diff = build(&harness, &["-e", "dev", "--", "--diff"]);
    assert_eq!(diff.exit, 0, "{}", diff.stderr);
    assert_eq!(diff.report["spec"]["target"], "dev-kubernetes");
    assert_eq!(diff.report["spec"]["pathPrefix"], "preview");
    assert_eq!(diff.report["spec"]["changes"], "create");

    let pushed = build(&harness, &["-e", "dev", "--", "--push"]);
    assert_eq!(pushed.exit, 3);
    assert!(
        pushed.stderr.contains("NYL-KUBERNETES-BUILD-TARGET-OWNED"),
        "{}",
        pushed.stderr
    );
    assert!(!deploy_branch_exists(&harness));
}

/// Catches the tier-1 test kind answering `nyl build` differently from the
/// built-in kind it stands in for: scenarios that build a publication must see
/// the same documents.
#[test]
fn test_the_test_kind_builds_like_the_built_in_kind() {
    let project = standalone_project();
    let harness = harness::Harness::new(project.path(), true);
    let diff = build(&harness, &["--", "--diff"]);
    assert_eq!(diff.exit, 0, "{}", diff.stderr);
    assert_eq!(diff.report["kind"], "TreeDiff");
    assert_eq!(diff.report["spec"]["changes"], "create");
    assert_eq!(build(&harness, &[]).report["kind"], "RenderedTree");
}
