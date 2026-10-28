//! Credential admission on the same shell path used by execution and verify.
mod scenario;

use nyl_core::orchestration::ports::SecretProvider;
use nyl_drivers::process::ProcessRunner;
use nyl_orchestration::run_report::RunReport;
use nyl_orchestration::shell::run::{self, Cancellation, RunContext};
use scenario::shell::{project, unit, Harness, NYL_TOML};
use serde_json::{json, Value};

struct Secrets {
    password: &'static str,
}

impl SecretProvider for Secrets {
    fn get(&self, key: &str) -> Result<Value, String> {
        match key {
            "password" => Ok(json!(self.password)),
            "account" => Ok(json!({"id": 7})),
            _ => Err("provider error containing a private value".to_owned()),
        }
    }
}

fn reconcile(harness: &Harness, secrets: Option<&dyn SecretProvider>) -> RunReport {
    let job = harness.job(&[], Cancellation::default());
    let base = harness.context();
    let tools = ProcessRunner::new();
    let context = RunContext {
        base: &base,
        sources: &job.sources,
        renderer: &job.renderer,
        drivers: &harness.drivers,
        tools: &tools,
        workspace: job.workspace.path(),
        process_env: &harness.process_env,
        secrets,
        commit_identity: Some(&harness.identity),
        observer: None,
        prompt: None,
    };
    run::reconcile(&context, &job.request)
}

fn credentials_project() -> tempfile::TempDir {
    project(&[
        NYL_TOML,
        ("nyl/environment.yaml", "apiVersion: gitops.nyl/v1\nkind: Environment\nmetadata: {name: dev}\nspec:\n  unitSelector: {matchLabels: {dev: 'true'}}\n  allowUnprotectedSource: true\n"),
        ("nyl/units.yaml", "apiVersion: units.gitops.nyl/v1\nkind: Command\nmetadata: {name: check, labels: {dev: 'true'}}\nspec:\n  idempotent: true\n  command: [sh, -c, 'test -n \"$PASSWORD\" && test \"$ACCOUNT\" = ''{\"id\":7}'' && test -z \"${PRIVATE:-}\"']\n  env:\n    secrets: {PASSWORD: password, ACCOUNT: account}\n"),
    ])
}

#[test]
fn test_explicit_secrets_reach_the_unit_without_entering_state_or_execution_keys() {
    let project = credentials_project();
    let mut harness = Harness::new(project.path(), false);
    harness
        .process_env
        .insert("PRIVATE".to_owned(), "unadmitted".to_owned());
    harness.init();
    let first = reconcile(&harness, Some(&Secrets { password: "original" }));
    assert_eq!(first.exit().code(), 0, "{first:#?}");
    let snapshot = harness.store.snapshot(&harness.location).unwrap();
    let stored = serde_json::to_string(&snapshot.desired).unwrap();
    assert!(
        !stored.contains("original"),
        "secret values must never enter desired state"
    );
    let stored = serde_json::to_string(&snapshot.observed).unwrap();
    assert!(!stored.contains("original"), "secret values must never enter receipts");
    let second = reconcile(&harness, Some(&Secrets { password: "rotated" }));
    assert_eq!(second.exit().code(), 0, "{second:#?}");
    assert!(
        second.environments[0].commits.is_empty(),
        "rotating a credential must not execute the unit again"
    );
}

#[test]
fn test_missing_secret_stops_before_execution_and_never_records_provider_errors() {
    let project = credentials_project();
    let harness = Harness::new(project.path(), false);
    harness.init();
    let report = reconcile(&harness, None);
    assert_eq!(report.exit().code(), 3, "{report:#?}");
    let snapshot = harness.store.snapshot(&harness.location).unwrap();
    assert!(snapshot
        .observed
        .get(&unit("check"))
        .and_then(|unit| unit.receipt.as_ref())
        .is_none());
    assert!(matches!(
        snapshot.observed[&unit("check")].condition,
        Some(nyl_core::orchestration::state::Condition::Failed {
            category: nyl_core::orchestration::state::FailureCategory::Credentials,
            retryable: true,
            ..
        })
    ));
    let retried = reconcile(&harness, Some(&Secrets { password: "original" }));
    assert_eq!(retried.exit().code(), 0, "{retried:#?}");
}

#[test]
fn test_provider_error_payload_is_not_exposed() {
    struct Unavailable;
    impl SecretProvider for Unavailable {
        fn get(&self, _: &str) -> Result<Value, String> {
            Err("private provider diagnostic".to_owned())
        }
    }
    let project = credentials_project();
    let harness = Harness::new(project.path(), false);
    harness.init();
    let report = reconcile(&harness, Some(&Unavailable));
    assert_eq!(report.exit().code(), 3, "{report:#?}");
    let report = serde_json::to_string(&report).unwrap();
    assert!(!report.contains("private provider diagnostic"));
    assert!(report.contains("secret"));
}
