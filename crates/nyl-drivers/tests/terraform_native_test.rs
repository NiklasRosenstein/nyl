//! Real native tools, with builtin terraform_data and a local module: no cloud
//! account, provider download, or global tool configuration is needed.
#![cfg(feature = "native-tests")]

use std::collections::BTreeMap;

use nyl_core::orchestration::driver::{Driver, DriverOperation, ExecutionContext, Outcome, Supported};
use nyl_core::orchestration::ids::ExecutionKey;
use nyl_core::orchestration::state::{DesiredUnit, Drift, FailureCategory, ObservedUnit};
use nyl_drivers::process::ProcessRunner;
use nyl_drivers::terraform::TerraformDriver;
use serde_json::{json, Value};
use tempfile::TempDir;

fn desired(spec: &Value) -> DesiredUnit {
    serde_json::from_value(json!({"apiVersion": "gitops.nyl/v1", "kind": "DesiredUnit", "unit": {"environment": "dev", "name": "network", "uid": "00000000-0000-7000-8000-000000000001", "apiVersion": "units.gitops.nyl/v1", "kind": "OpenTofu"}, "lifecycle": {"state": "active"}, "sourceCommit": "a".repeat(40), "sourceRevision": "a".repeat(40), "spec": spec, "resolvedSpec": spec, "executionKey": ExecutionKey::from_digest_of(b"native"), "readiness": {"state": "ready"}})).unwrap()
}

fn real_native(driver: &TerraformDriver, binary: &str) {
    let root = TempDir::new().unwrap();
    let tree = root.path().join("source");
    std::fs::create_dir_all(tree.join("module")).unwrap();
    std::fs::create_dir_all(tree.join("shared")).unwrap();
    std::fs::write(tree.join("nyl.toml"), "[project]\n").unwrap();
    std::fs::write(tree.join("module/.terraform.lock.hcl"), "").unwrap();
    std::fs::write(tree.join("module/main.tf"), "terraform {\n  backend \"local\" {}\n}\nvariable \"name\" { type = string }\nmodule \"shared\" {\n  source = \"../shared\"\n  name = var.name\n}\noutput \"id\" { value = module.shared.id }\n").unwrap();
    std::fs::write(tree.join("shared/main.tf"), "variable \"name\" { type = string }\nresource \"terraform_data\" \"network\" { input = var.name }\noutput \"id\" { value = terraform_data.network.output }\n").unwrap();
    let state = root.path().join("persistent.tfstate");
    let spec = json!({"source": {"path": "module"}, "files": ["module/**", "shared/**"], "backend": {"path": state}, "variables": {"name": "network-one"}, "outputs": {"id": {"type": "string"}}});
    let tools = ProcessRunner::new();
    let now = "2026-10-28T12:00:00Z".parse().unwrap();
    let mut context = ExecutionContext {
        source_roots: BTreeMap::new(),
        unit: "dev/network".parse().unwrap(),
        operation: DriverOperation::Reconcile,
        worktree: tree.clone(),
        project_dir: tree,
        temp_dir: root.path().join("temporary"),
        env: BTreeMap::from([("PATH".to_owned(), std::env::var("PATH").unwrap())]),
        secrets: BTreeMap::new(),
        interactive: false,
        approved_digest: None,
        deadline: now + chrono::Duration::minutes(2),
        now,
        observed: None,
        artifacts: BTreeMap::new(),
        tools: &tools,
        commit_identity: None,
        cache_dir: root.path().join("cache"),
        teardown_wait: None,
    };
    let unit = desired(&spec);
    let Supported::Supported(first) = driver.reconcile(&context, &unit).unwrap() else {
        panic!("native reconcile unsupported")
    };
    let Outcome::Succeeded { outputs, versions, .. } = first else {
        panic!("{binary} must be installed for native-tests: {first:?}")
    };
    assert_eq!(outputs, json!({"id": "network-one"}));
    assert!(!versions[binary].is_empty());
    let before = std::fs::read(&state).unwrap();
    let Supported::Supported(again) = driver.reconcile(&context, &unit).unwrap() else {
        panic!("native reconcile unsupported")
    };
    assert!(matches!(again, Outcome::Succeeded { .. }), "{again:?}");
    assert_eq!(
        before,
        std::fs::read(&state).unwrap(),
        "a no-change plan does not apply or advance state"
    );
    let observed: ObservedUnit = serde_json::from_value(json!({"apiVersion": "gitops.nyl/v1", "kind": "ObservedUnit", "unit": unit.unit, "receipt": {"executionKey": unit.execution_key, "behaviorVersion": 1, "run": "00000000-0000-7000-8000-000000000002", "startedAt": "2026-10-28T12:00:00Z", "finishedAt": "2026-10-28T12:00:01Z"}, "condition": null})).unwrap();
    context.observed = Some(&observed);
    let receipt = observed.receipt.as_ref().unwrap();
    let Supported::Supported(clean) = driver.verify(&context, &unit, receipt).unwrap() else {
        panic!("native verify unsupported")
    };
    assert_eq!(clean.drift, Drift::Clean);
    let mut changed = spec.clone();
    changed["variables"]["name"] = json!("network-two");
    let Supported::Supported(drift) = driver.verify(&context, &desired(&changed), receipt).unwrap() else {
        panic!("native verify unsupported")
    };
    assert!(matches!(drift.drift, Drift::Drifted { .. }));
    let Supported::Supported(destroyed) = driver.teardown(&context, &unit).unwrap() else {
        panic!("native teardown unsupported")
    };
    assert!(matches!(destroyed, Outcome::Succeeded { .. }), "{destroyed:?}");
    assert!(
        serde_json::from_slice::<Value>(&std::fs::read(&state).unwrap()).unwrap()["resources"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    std::fs::remove_file(&state).unwrap();
    let Supported::Supported(missing) = driver.reconcile(&context, &unit).unwrap() else {
        panic!("native reconcile unsupported")
    };
    assert!(
        matches!(missing, Outcome::Failed { category: FailureCategory::Ownership, retryable: false, message } if message.starts_with("NYL-TOFU-STATE-MISSING"))
    );
}

#[test]
fn test_opentofu_real_native_lifecycle() {
    real_native(&TerraformDriver::opentofu(), "tofu");
}

#[test]
fn test_terraform_real_native_lifecycle() {
    real_native(&TerraformDriver::terraform(), "terraform");
}
