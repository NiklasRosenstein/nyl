//! Direct builds share unit rendering and reference resolution, but never write state.
mod scenario;
use clap::Parser;
use nyl::cli::Cli;
use nyl::orchestration::direct;
use nyl::orchestration::run::Wiring;
use nyl_drivers::process::ProcessRunner;
use nyl_orchestration::fault::NoFaults;
use nyl_orchestration::shell::run::Cancellation;
use scenario::shell::{project, Harness, NYL_TOML};
use scenario::world::git_output;

const IMAGE:&str="apiVersion: units.gitops.nyl/v1\nkind: OciImage\nmetadata: {name: image}\nspec:\n  context: {path: /app}\n  repository: registry.invalid/web\n  tags: [try]\n";
#[test]
fn test_direct_build_uses_dirty_snapshot_and_source_override_without_state() {
    let project = project(&[
        NYL_TOML,
        ("nyl/image.yaml", IMAGE),
        ("app/Dockerfile", "FROM scratch\nCOPY input /input\n"),
        ("app/input", "committed"),
    ]);
    let harness = Harness::new(project.path(), true);
    let clone = harness.world.fresh_clone("main");
    std::fs::write(clone.join("app/input"), "uncommitted").unwrap();
    let tools = ProcessRunner::new();
    let wiring = Wiring {
        clock: harness.clock.clone(),
        ids: &harness.ids,
        drivers: &harness.drivers,
        tools: &tools,
        faults: &NoFaults,
        runner: "test".into(),
        author: None,
        process_env: harness.process_env.clone(),
        cancellation: Cancellation::default(),
        prompt: None,
        observer: None,
        progress: None,
    };
    let before = git_output(
        &harness.world.bare(),
        &["for-each-ref", "--format=%(refname):%(objectname)"],
    );
    let prepared = direct::prepare(&clone, "image", None, None, &wiring).unwrap();
    assert!(prepared.dirty);
    assert_eq!(
        std::fs::read_to_string(prepared.worktree.join("app/input")).unwrap(),
        "uncommitted"
    );
    assert!(prepared.changed_paths.contains(&"app/input".into()));
    assert!(prepared.audit["snapshotDigest"]
        .as_str()
        .unwrap()
        .starts_with("sha256:"));
    std::fs::write(prepared.worktree.join("app/input"), "tool mutation").unwrap();
    assert_eq!(std::fs::read_to_string(clone.join("app/input")).unwrap(), "uncommitted");
    let run = |extra: &[&str]| {
        let mut args = vec!["nyl".to_owned(), "build".into(), "image".into()];
        args.extend(extra.iter().map(|arg| (*arg).to_owned()));
        args.push(clone.display().to_string());
        Cli::try_parse_from(args).unwrap().execute_in_process(&wiring).unwrap()
    };
    let dirty = run(&[]);
    assert_eq!(dirty.exit, 0, "{}", dirty.stderr);
    assert_eq!(dirty.report["spec"]["tags"], serde_json::json!(["try"]));
    let clean = run(&["--source", "HEAD"]);
    assert_eq!(clean.exit, 0, "{}", clean.stderr);
    assert_ne!(dirty.report["spec"]["reference"], clean.report["spec"]["reference"]);
    let after = git_output(
        &harness.world.bare(),
        &["for-each-ref", "--format=%(refname):%(objectname)"],
    );
    assert_eq!(before, after);
}
#[test]
fn test_build_rejects_environment_inputs_without_environment_and_effectful_kinds() {
    let image = IMAGE.replace("registry.invalid/web", "registry.invalid/{{ environment.name }}");
    let project=project(&[NYL_TOML,("nyl/image.yaml",&image),("app/Dockerfile","FROM scratch"),("nyl/job.yaml","apiVersion: units.gitops.nyl/v1\nkind: Command\nmetadata: {name: job}\nspec:\n  command: [sh, -c, 'true']\n")]);
    let harness = Harness::new(project.path(), true);
    let refusal = harness.invoke(&["build", "image"]);
    assert_eq!(refusal.exit, 1);
    assert!(refusal.stderr.contains("-e"), "{}", refusal.stderr);
    let refusal = harness.invoke(&["build", "job"]);
    assert_eq!(refusal.exit, 1);
    assert!(refusal.stderr.contains("build capability"), "{}", refusal.stderr);
}

#[test]
fn test_direct_build_refuses_a_locked_commit_outside_protected_and_source_refs() {
    let project = project(&[
        NYL_TOML,
        ("nyl/image.yaml", IMAGE),
        ("app/Dockerfile", "FROM scratch\n"),
        ("app/input", "committed"),
    ]);
    let harness = Harness::new(project.path(), true);
    let clone = harness.world.fresh_clone("main");
    let repository = git2::Repository::open(&clone).unwrap();
    let head = repository.head().unwrap().peel_to_commit().unwrap();
    let signature = git2::Signature::now("Test", "test@example.invalid").unwrap();
    let dangling = repository
        .commit(
            None,
            &signature,
            &signature,
            "unprotected source",
            &head.tree().unwrap(),
            &[&head],
        )
        .unwrap();
    std::fs::write(
        clone.join("nyl/image.yaml"),
        IMAGE.replace(
            "context: {path: /app}",
            &format!("context: {{path: /app, revision: main, commit: {dangling}}}"),
        ),
    )
    .unwrap();
    let tools = ProcessRunner::new();
    let wiring = Wiring {
        clock: harness.clock.clone(),
        ids: &harness.ids,
        drivers: &harness.drivers,
        tools: &tools,
        faults: &NoFaults,
        runner: "test".into(),
        author: None,
        process_env: harness.process_env.clone(),
        cancellation: Cancellation::default(),
        prompt: None,
        observer: None,
        progress: None,
    };
    let error = direct::prepare(&clone, "image", None, None, &wiring)
        .err()
        .expect("unprotected lock must be refused");
    assert!(error.to_string().contains("not reachable"), "{error}");
}

#[test]
fn test_build_accepts_driver_arguments_only_after_separator() {
    let error = Cli::try_parse_from(["nyl", "build", "image", "--push"]).unwrap_err();
    assert_eq!(error.kind(), clap::error::ErrorKind::UnknownArgument);
    Cli::try_parse_from(["nyl", "build", "image", "--", "--push", "--tag", "experiment"])
        .expect("image options belong to the driver argument list");
    Cli::try_parse_from(["nyl", "build", "image", "--", "--driver-specific", "arbitrary-value"])
        .expect("the generic CLI must not enumerate driver options");
}

#[test]
fn test_build_preserves_driver_argument_contents() {
    #[derive(Parser)]
    struct DriverBuildCli {
        #[command(flatten)]
        args: nyl::cli::commands::build::BuildArgs,
    }
    let parsed = DriverBuildCli::try_parse_from([
        "build",
        "image",
        "--",
        "--driver-specific",
        "value with spaces",
        "--tag=literal",
    ])
    .unwrap();
    assert_eq!(parsed.args.unit, "image");
    assert_eq!(parsed.args.path, std::path::PathBuf::from("."));
    assert_eq!(
        parsed.args.arguments,
        ["--driver-specific", "value with spaces", "--tag=literal"].map(std::ffi::OsString::from)
    );
}
