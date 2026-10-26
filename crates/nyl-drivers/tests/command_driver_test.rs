//! The `Command` driver against real processes: outputs, failures, the
//! step environment, execution-key inputs, and deadlines.
//!
//! Contract: [Command unit](../../../design/orchestration-core.md#command-unit).
#![cfg(unix)]

use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use nyl_core::orchestration::driver::{
    Driver, DriverOperation, ExecutionContext, Outcome, ResolutionContext, Supported,
};
use nyl_core::orchestration::ids::{CommitId, EnvironmentName, ExecutionKey, Uid, UnitAddress, UnitName};
use nyl_core::orchestration::state::{
    DesiredUnit, DesiredUnitKind, FailureCategory, Lifecycle, Readiness, StateV1, UnitIdentity,
};
use nyl_core::orchestration::time::Timestamp;
use nyl_drivers::command::CommandDriver;
use nyl_drivers::process::ProcessRunner;
use serde_json::{json, Value};
use tempfile::TempDir;

/// A worktree with the project at `/nyl`, a scratch directory, and a place
/// where scripts leave evidence.
struct World {
    root: TempDir,
}

impl World {
    fn new() -> Self {
        let root = TempDir::new().unwrap();
        for directory in ["worktree/nyl", "temp", "cache", "evidence"] {
            std::fs::create_dir_all(root.path().join(directory)).unwrap();
        }
        std::fs::write(root.path().join("worktree/nyl/nyl.toml"), "").unwrap();
        Self { root }
    }

    fn worktree(&self) -> PathBuf {
        self.root.path().join("worktree")
    }

    fn project(&self) -> PathBuf {
        self.worktree().join("nyl")
    }

    fn evidence(&self, name: &str) -> PathBuf {
        self.root.path().join("evidence").join(name)
    }

    /// Write an executable `sh` script at a worktree-relative path.
    fn script(&self, path: &str, body: &str) {
        let file = self.worktree().join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, format!("#!/bin/sh\nset -e\n{body}\n")).unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn file(&self, path: &str, content: &str) {
        let file = self.worktree().join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, content).unwrap();
    }

    fn context<'a>(
        &self,
        tools: &'a ProcessRunner,
        env: BTreeMap<String, String>,
        budget_ms: i64,
    ) -> ExecutionContext<'a> {
        let now = fixed_now();
        ExecutionContext {
            unit: "dev/seed".parse::<UnitAddress>().unwrap(),
            operation: DriverOperation::Reconcile,
            worktree: self.worktree(),
            project_dir: self.project(),
            temp_dir: self.root.path().join("temp"),
            env,
            approved_digest: None,
            deadline: now + chrono::Duration::milliseconds(budget_ms),
            now,
            observed: None,
            tools,
            commit_identity: None,
            cache_dir: self.root.path().join("cache"),
            teardown_wait: None,
        }
    }

    fn resolution<'a>(&self, tools: &'a ProcessRunner) -> ResolutionContext<'a> {
        let now = fixed_now();
        ResolutionContext {
            environment: EnvironmentName::new("dev").unwrap(),
            worktree: self.worktree(),
            project_dir: self.project(),
            env: path_env(),
            deadline: now + chrono::Duration::seconds(60),
            now,
            tools,
            cache_dir: self.root.path().join("cache"),
        }
    }
}

/// An arbitrary injected time: the driver only ever subtracts it from the deadline.
fn fixed_now() -> Timestamp {
    "2026-01-01T00:00:00Z".parse().unwrap()
}

fn path_env() -> BTreeMap<String, String> {
    BTreeMap::from([("PATH".to_owned(), "/usr/bin:/bin".to_owned())])
}

fn desired(spec: Value) -> DesiredUnit {
    DesiredUnit {
        api_version: StateV1,
        kind: DesiredUnitKind,
        unit: UnitIdentity {
            environment: EnvironmentName::new("dev").unwrap(),
            name: UnitName::new("seed").unwrap(),
            uid: Some(Uid::new("00000000-0000-7000-8000-000000000001").unwrap()),
            api_version: "units.gitops.nyl/v1".to_owned(),
            kind: "Command".to_owned(),
        },
        lifecycle: Lifecycle::Active,
        source_commit: CommitId::new("a".repeat(40)).unwrap(),
        source_revision: CommitId::new("a".repeat(40)).unwrap(),
        spec: spec.clone(),
        resolved_spec: Some(spec),
        provenance: BTreeMap::new(),
        execution_key: Some(ExecutionKey::from_digest_of(b"seed")),
        fingerprint: None,
        attestations: Vec::new(),
        readiness: Readiness::Ready,
    }
}

fn reconcile(world: &World, spec: Value) -> Outcome {
    let tools = ProcessRunner::new();
    let ctx = world.context(&tools, path_env(), 60_000);
    match CommandDriver.reconcile(&ctx, &desired(spec)).unwrap() {
        Supported::Supported(outcome) => outcome,
        Supported::Unsupported => panic!("a Command reconciles"),
    }
}

#[test]
fn test_reconcile_returns_typed_outputs_and_drops_sensitive_ones() {
    let world = World::new();
    world.script(
        "nyl/run.sh",
        r#"printf '{"version": "1.2.0", "count": 3, "token": "s3cret"}' > "$NYL_OUTPUTS""#,
    );
    let outcome = reconcile(
        &world,
        json!({
            "command": ["./run.sh"],
            "outputs": {
                "version": {"type": "string"},
                "count": {"type": "integer"},
                "token": {"type": "string", "sensitive": true}
            }
        }),
    );
    let Outcome::Succeeded { outputs, .. } = outcome else {
        panic!("the command succeeds: {outcome:?}");
    };
    assert_eq!(outputs, json!({"version": "1.2.0", "count": 3}));
}

#[test]
fn test_reconcile_returns_undeclared_outputs_for_the_shell_to_filter() {
    let world = World::new();
    world.script(
        "nyl/run.sh",
        r#"printf '{"version": "1", "extra": true}' > "$NYL_OUTPUTS""#,
    );
    let outcome = reconcile(
        &world,
        json!({"command": ["./run.sh"], "outputs": {"version": {"type": "string"}}}),
    );
    let Outcome::Succeeded { outputs, .. } = outcome else {
        panic!("the command succeeds: {outcome:?}");
    };
    assert_eq!(outputs, json!({"version": "1", "extra": true}));
}

#[test]
fn test_reconcile_rejects_missing_mistyped_and_empty_outputs() {
    let world = World::new();
    let declared = json!({"version": {"type": "string"}});
    for (body, needle) in [
        (r#"printf '{}' > "$NYL_OUTPUTS""#, "missing"),
        (r#"printf '{"version": 3}' > "$NYL_OUTPUTS""#, "type string"),
        ("true", "missing"),
        (r#"printf '[1]' > "$NYL_OUTPUTS""#, "one JSON object"),
    ] {
        world.script("nyl/run.sh", body);
        let outcome = reconcile(&world, json!({"command": ["./run.sh"], "outputs": declared}));
        match outcome {
            Outcome::Failed {
                category: FailureCategory::OutputRejected,
                retryable: false,
                message,
            } => assert!(message.contains(needle), "{body}: {message}"),
            other => panic!("{body}: expected output-rejected, got {other:?}"),
        }
    }
    // Without declared outputs, an empty file is `{}`.
    world.script("nyl/run.sh", "true");
    assert!(matches!(
        reconcile(&world, json!({"command": ["./run.sh"]})),
        Outcome::Succeeded { outputs, .. } if outputs == json!({})
    ));
}

#[test]
fn test_reconcile_nonzero_exit_is_tool_error_retryable_only_when_idempotent() {
    let world = World::new();
    world.script("nyl/run.sh", "exit 4");
    for idempotent in [false, true] {
        let outcome = reconcile(&world, json!({"command": ["./run.sh"], "idempotent": idempotent}));
        assert!(
            matches!(
                &outcome,
                Outcome::Failed { category: FailureCategory::ToolError, retryable, message }
                    if *retryable == idempotent && message.contains("code 4")
            ),
            "{outcome:?}"
        );
    }
    let outcome = reconcile(&world, json!({"command": ["./missing.sh"]}));
    assert!(
        matches!(
            outcome,
            Outcome::Failed {
                category: FailureCategory::MissingTool,
                ..
            }
        ),
        "{outcome:?}"
    );
}

#[test]
fn test_reconcile_environment_is_exactly_the_admitted_variables() {
    let world = World::new();
    let env_dump = world.evidence("env");
    let inputs_copy = world.evidence("inputs.json");
    world.script(
        "nyl/run.sh",
        &format!(
            "env > {}\ncp \"$NYL_INPUTS\" {}\ntest -d \"$HOME\"\ntest -f \"$NYL_OUTPUTS\"\ntest \"$(pwd)\" = \"{}\"",
            env_dump.display(),
            inputs_copy.display(),
            world.project().canonicalize().unwrap().display()
        ),
    );
    let mut env = path_env();
    env.insert("AWS_REGION".to_owned(), "eu-west-1".to_owned());
    env.insert("DB_PASSWORD".to_owned(), "hunter2".to_owned());
    env.insert("NOT_ADMITTED".to_owned(), "x".to_owned());
    let tools = ProcessRunner::new();
    let spec = json!({
        "command": ["./run.sh"],
        "values": {"bucket": "b-1"},
        "env": {"passthrough": ["AWS_REGION", "AWS_PROFILE"], "secrets": {"DB_PASSWORD": "database-password"}}
    });
    let outcome = CommandDriver
        .reconcile(&world.context(&tools, env, 60_000), &desired(spec))
        .unwrap();
    assert!(
        matches!(outcome, Supported::Supported(Outcome::Succeeded { .. })),
        "{outcome:?}"
    );

    let dump = std::fs::read_to_string(&env_dump).unwrap();
    let variables = dump
        .lines()
        .filter_map(|line| line.split_once('='))
        .collect::<BTreeMap<_, _>>();
    let names = variables
        .keys()
        .copied()
        // `sh` itself sets these.
        .filter(|name| !["PWD", "SHLVL", "_", "OLDPWD"].contains(name))
        .collect::<BTreeSet<_>>();
    assert_eq!(
        names,
        BTreeSet::from([
            "AWS_REGION",
            "DB_PASSWORD",
            "HOME",
            "NYL_ENVIRONMENT",
            "NYL_INPUTS",
            "NYL_OPERATION",
            "NYL_OUTPUTS",
            "NYL_UNIT",
            "PATH",
        ])
    );
    assert_eq!(variables["NYL_UNIT"], "seed");
    assert_eq!(variables["NYL_ENVIRONMENT"], "dev");
    assert_eq!(variables["NYL_OPERATION"], "reconcile");
    assert_eq!(variables["PATH"], "/usr/bin:/bin");
    let inputs: Value = serde_json::from_str(&std::fs::read_to_string(&inputs_copy).unwrap()).unwrap();
    assert_eq!(inputs, json!({"values": {"bucket": "b-1"}}));
}

#[test]
fn test_reconcile_with_declared_attestations_reads_their_results() {
    let world = World::new();
    world.script("nyl/run.sh", r#"printf '{"healthy": "pass"}' > "$NYL_ATTESTATIONS""#);
    let outcome = reconcile(
        &world,
        json!({"command": ["./run.sh"], "attestations": [{"name": "healthy"}, {"name": "pending"}]}),
    );
    let Outcome::Succeeded { attestations, .. } = outcome else {
        panic!("the command succeeds: {outcome:?}");
    };
    assert_eq!(attestations.len(), 1);
    assert_eq!(attestations[0].name, "healthy");
}

#[test]
fn test_fingerprint_stdout_enters_key_inputs_and_failure_is_an_error() {
    let world = World::new();
    world.script("nyl/versions.sh", "echo \"tool 1.0 $NYL_OPERATION\"");
    let tools = ProcessRunner::new();
    let inputs = CommandDriver
        .execution_key_inputs(
            &world.resolution(&tools),
            &json!({"command": ["./run.sh"], "fingerprint": ["./versions.sh"]}),
        )
        .unwrap();
    assert_eq!(inputs.fingerprint.as_deref(), Some("tool 1.0 fingerprint\n"));
    assert!(inputs.excluded_fields.is_empty());

    world.script("nyl/versions.sh", "exit 7");
    let error = CommandDriver
        .execution_key_inputs(
            &world.resolution(&tools),
            &json!({"command": ["./run.sh"], "fingerprint": ["./versions.sh"]}),
        )
        .unwrap_err();
    assert!(!error.effects_possible());
    assert!(error.to_string().contains("code 7"), "{error}");
}

#[test]
fn test_files_globs_digest_only_matched_files_keyed_worktree_rooted() {
    let world = World::new();
    world.file("scripts/seed/run.sh", "run");
    world.file("scripts/seed/notes.txt", "notes");
    world.file("nyl/data/a/b.json", "{}");
    world.file("nyl/other.txt", "other");
    let tools = ProcessRunner::new();
    let inputs = CommandDriver
        .execution_key_inputs(
            &world.resolution(&tools),
            &json!({"command": ["./run.sh"], "files": ["../scripts/seed/*.sh", "data/**"]}),
        )
        .unwrap();
    assert_eq!(
        inputs.files.keys().map(String::as_str).collect::<Vec<_>>(),
        vec!["/nyl/data/a/b.json", "/scripts/seed/run.sh"]
    );
    assert_eq!(
        inputs.files["/scripts/seed/run.sh"],
        nyl_core::digest::sha256_hex(b"run")
    );
}

#[test]
fn test_files_glob_matching_a_symbolic_link_is_rejected() {
    let world = World::new();
    world.file("nyl/data/real.txt", "x");
    std::os::unix::fs::symlink(
        world.project().join("data/real.txt"),
        world.project().join("data/link.txt"),
    )
    .unwrap();
    let tools = ProcessRunner::new();
    let error = CommandDriver
        .execution_key_inputs(
            &world.resolution(&tools),
            &json!({"command": ["./run.sh"], "files": ["data/*"]}),
        )
        .unwrap_err();
    assert!(error.to_string().contains("symbolic link"), "{error}");
}

#[test]
fn test_deadline_minus_now_terminates_the_command_as_uncertain() {
    let world = World::new();
    world.script("nyl/run.sh", "sleep 30");
    let tools = ProcessRunner::new();
    let ctx = world.context(&tools, path_env(), 300);
    let started = std::time::Instant::now();
    let outcome = CommandDriver
        .reconcile(&ctx, &desired(json!({"command": ["./run.sh"]})))
        .unwrap();
    assert!(
        matches!(outcome, Supported::Supported(Outcome::Uncertain { .. })),
        "{outcome:?}"
    );
    assert!(started.elapsed() < std::time::Duration::from_secs(20));
}

#[test]
fn test_verify_maps_exit_codes_to_drift() {
    use nyl_core::orchestration::state::Drift;
    let world = World::new();
    let tools = ProcessRunner::new();
    let ctx = world.context(&tools, path_env(), 60_000);
    let receipt: nyl_core::orchestration::state::Receipt = serde_json::from_value(json!({
        "executionKey": format!("sha256:{}", "0".repeat(64)),
        "behaviorVersion": 1,
        "run": "00000000-0000-7000-8000-000000000002",
        "startedAt": "2026-01-01T00:00:00Z",
        "finishedAt": "2026-01-01T00:00:00Z",
        "outputs": {}
    }))
    .unwrap();
    let unit = desired(json!({"command": ["./run.sh"], "verify": ["./verify.sh"]}));
    for (code, expected) in [(0, "clean"), (2, "drifted"), (1, "unknown")] {
        world.script("nyl/verify.sh", &format!("test -z \"$NYL_OUTPUTS\"\nexit {code}"));
        let Supported::Supported(verification) = CommandDriver.verify(&ctx, &unit, &receipt).unwrap() else {
            panic!("verify is supported with a verify step");
        };
        let name = match verification.drift {
            Drift::Clean => "clean",
            Drift::Drifted { .. } => "drifted",
            Drift::Unknown { .. } => "unknown",
        };
        assert_eq!(name, expected, "exit {code}");
    }
    let without = desired(json!({"command": ["./run.sh"]}));
    assert_eq!(
        CommandDriver.verify(&ctx, &without, &receipt).unwrap(),
        Supported::Unsupported
    );
}
