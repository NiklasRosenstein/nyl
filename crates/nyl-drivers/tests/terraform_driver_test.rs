//! Native lifecycle boundaries with a deterministic tool runner: saved plans,
//! output admission, locks, state-loss fences, and content-based inputs.
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

use nyl_core::orchestration::driver::{
    Driver, DriverOperation, DriverResult, ExecutionContext, Outcome, ResolutionContext, Supported, ToolExit,
    ToolInvocation, ToolRunner,
};
use nyl_core::orchestration::ids::{EnvironmentName, ExecutionKey};
use nyl_core::orchestration::state::{DesiredUnit, FailureCategory, ObservedUnit};
use nyl_drivers::terraform::TerraformDriver;
use serde_json::{json, Value};
use tempfile::TempDir;

struct FakeTool {
    calls: Mutex<Vec<ToolInvocation>>,
    plan: Mutex<Value>,
    exit: i32,
    state: Value,
    output: Value,
    modules: Value,
    backend_metadata: Value,
    fail_plan: Option<String>,
}

impl FakeTool {
    fn new() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            plan: Mutex::new(
                json!({"format_version": "1.0", "planned_values": {"outputs": {"id": {"value": "created", "sensitive": false}}}, "resource_changes": [{"address": "terraform_data.test", "change": {"actions": ["create"], "before": null, "after": {"input": "created"}}}], "output_changes": {"id": {"actions": ["create"], "after": "created"}}}),
            ),
            exit: 2,
            state: json!({"format_version": "1.0", "values": {}}),
            output: json!({"id": {"value": "created", "sensitive": false}, "ignored": {"value": "never-record", "sensitive": true}}),
            modules: json!({"Modules": []}),
            backend_metadata: json!({"backend": {"type": "s3", "config": {"bucket": "test", "key": "dev/test"}}}),
            fail_plan: None,
        }
    }
    fn count(&self, command: &str) -> usize {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| call.args.first().is_some_and(|arg| arg == command))
            .count()
    }
}

impl ToolRunner for FakeTool {
    fn run(&self, invocation: ToolInvocation) -> DriverResult<ToolExit> {
        let command = invocation.args[0].to_string_lossy().into_owned();
        let mut code = 0;
        let mut stderr = Vec::new();
        let result = match command.as_str() {
            "version" => json!({"terraform_version": "1.9.5"}),
            "init" => {
                let data = PathBuf::from(&invocation.env["TF_DATA_DIR"]);
                std::fs::create_dir_all(data.join("modules")).unwrap();
                std::fs::write(data.join("modules/modules.json"), self.modules.to_string()).unwrap();
                let mut metadata = self.backend_metadata.clone();
                let backend = invocation
                    .args
                    .iter()
                    .find_map(|arg| arg.to_str()?.strip_prefix("-backend-config="))
                    .unwrap();
                for line in std::fs::read_to_string(backend).unwrap().lines() {
                    let (name, value) = line.split_once(" = ").unwrap();
                    metadata["backend"]["config"][name] = serde_json::from_str(value).unwrap();
                }
                std::fs::write(data.join("terraform.tfstate"), metadata.to_string()).unwrap();
                json!(null)
            }
            "plan" => {
                code = self.exit;
                if let Some(error) = &self.fail_plan {
                    code = 1;
                    stderr = error.as_bytes().to_vec();
                }
                json!(null)
            }
            "show" if invocation.args.len() > 2 => self.plan.lock().unwrap().clone(),
            "show" => self.state.clone(),
            "output" => self.output.clone(),
            "apply" => json!(null),
            _ => json!(null),
        };
        self.calls.lock().unwrap().push(invocation);
        Ok(ToolExit::Exited {
            code,
            stdout: serde_json::to_vec(&result).unwrap(),
            stderr,
        })
    }
}

struct World(TempDir);
impl World {
    fn new() -> Self {
        let world = Self(TempDir::new().unwrap());
        std::fs::create_dir_all(world.0.path().join("worktree/nyl/module")).unwrap();
        std::fs::write(world.0.path().join("worktree/nyl/module/.terraform.lock.hcl"), "").unwrap();
        std::fs::write(
            world.0.path().join("worktree/nyl/module/main.tf"),
            "resource \"terraform_data\" \"test\" {}\n",
        )
        .unwrap();
        world
    }
    fn context<'a>(&self, tool: &'a dyn ToolRunner) -> ExecutionContext<'a> {
        let now = "2026-10-28T12:00:00Z".parse().unwrap();
        ExecutionContext {
            source_roots: BTreeMap::new(),
            unit: "dev/test".parse().unwrap(),
            operation: DriverOperation::Reconcile,
            worktree: self.0.path().join("worktree"),
            project_dir: self.0.path().join("worktree/nyl"),
            temp_dir: self.0.path().join("scratch"),
            env: BTreeMap::new(),
            secrets: BTreeMap::new(),
            interactive: false,
            approved_digest: None,
            deadline: now + chrono::Duration::minutes(10),
            now,
            observed: None,
            artifacts: BTreeMap::new(),
            tools: tool,
            commit_identity: None,
            cache_dir: self.0.path().join("cache"),
            teardown_wait: None,
        }
    }
}

fn unit(extra: Value) -> DesiredUnit {
    let mut spec = json!({"source": {"path": "module"}, "backend": {"bucket": "test", "key": "dev/test"}, "outputs": {"id": {"type": "string"}}});
    for (key, value) in extra.as_object().unwrap() {
        spec[key] = value.clone();
    }
    serde_json::from_value(json!({"apiVersion": "gitops.nyl/v1", "kind": "DesiredUnit", "unit": {"environment": "dev", "name": "test", "uid": "00000000-0000-7000-8000-000000000001", "apiVersion": "units.gitops.nyl/v1", "kind": "OpenTofu"}, "lifecycle": {"state": "active"}, "sourceCommit": "a".repeat(40), "sourceRevision": "a".repeat(40), "spec": spec, "resolvedSpec": spec, "executionKey": ExecutionKey::from_digest_of(b"native"), "readiness": {"state": "ready"}})).unwrap()
}

fn outcome(driver: &TerraformDriver, context: &ExecutionContext<'_>, desired: &DesiredUnit) -> Outcome {
    let Supported::Supported(outcome) = driver.reconcile(context, desired).unwrap() else {
        panic!("native reconcile is supported")
    };
    outcome
}

#[test]
fn test_native_reconcile_uses_readonly_init_saved_plan_and_private_json_with_version_audit() {
    let world = World::new();
    let tool = FakeTool::new();
    let result = outcome(
        &TerraformDriver::opentofu(),
        &world.context(&tool),
        &unit(json!({"version": "1.9.5"})),
    );
    let Outcome::Succeeded { outputs, versions, .. } = result else {
        panic!("{result:?}")
    };
    assert_eq!(outputs, json!({"id": "created"}));
    assert_eq!(versions["tofu"], "1.9.5");
    assert_eq!(tool.count("apply"), 1);
    let calls = tool.calls.lock().unwrap();
    assert!(calls.iter().all(|call| call.private_stdout));
    assert!(calls
        .iter()
        .find(|call| call.args[0] == "init")
        .unwrap()
        .args
        .iter()
        .any(|arg| arg == "-lockfile=readonly"));
    let planned = calls
        .iter()
        .find(|call| call.args[0] == "plan")
        .unwrap()
        .args
        .iter()
        .find_map(|arg| arg.to_str().and_then(|arg| arg.strip_prefix("-out=")))
        .unwrap();
    assert_eq!(
        calls
            .iter()
            .find(|call| call.args[0] == "apply")
            .unwrap()
            .args
            .last()
            .unwrap(),
        planned
    );
}

#[test]
fn test_plan_digest_mismatch_and_sensitive_output_are_rejected_before_apply() {
    let world = World::new();
    let tool = FakeTool::new();
    let desired = unit(json!({"approval": {"mode": "manual", "bind": "plan", "requireDigest": true}}));
    let driver = TerraformDriver::opentofu();
    let mut context = world.context(&tool);
    let Outcome::AwaitingApproval { digest } = outcome(&driver, &context, &desired) else {
        panic!("unapproved native plan must wait")
    };
    assert_eq!(tool.count("apply"), 0);
    context.approved_digest = Some(digest);
    tool.plan.lock().unwrap()["resource_changes"][0]["change"]["after"]["input"] = json!("changed-after-review");
    assert!(matches!(
        outcome(&driver, &context, &desired),
        Outcome::AwaitingApproval { .. }
    ));
    assert_eq!(tool.count("apply"), 0);
    tool.plan.lock().unwrap()["planned_values"]["outputs"]["id"]["sensitive"] = json!(true);
    let rejected = outcome(&driver, &context, &desired);
    assert!(matches!(
        rejected,
        Outcome::Failed {
            category: FailureCategory::OutputRejected,
            retryable: false,
            ..
        }
    ));
    assert_eq!(tool.count("apply"), 0);
}

#[test]
fn test_nochange_skips_apply_and_state_lock_failure_is_nonretryable() {
    let world = World::new();
    let mut tool = FakeTool::new();
    tool.exit = 0;
    assert!(matches!(
        outcome(&TerraformDriver::terraform(), &world.context(&tool), &unit(json!({}))),
        Outcome::Succeeded { .. }
    ));
    assert_eq!(tool.count("apply"), 0);
    tool.fail_plan = Some("Error acquiring the state lock\nLock Info: ID: 4711".to_owned());
    let locked = outcome(&TerraformDriver::terraform(), &world.context(&tool), &unit(json!({})));
    let Outcome::Failed {
        category,
        retryable,
        message,
    } = locked
    else {
        panic!("{locked:?}")
    };
    assert_eq!(category, FailureCategory::Ownership);
    assert!(!retryable);
    assert!(message.contains("4711"));
}

#[test]
fn test_missing_state_requires_explicit_acceptance_for_the_same_scope() {
    let world = World::new();
    let mut tool = FakeTool::new();
    tool.state = json!({"format_version": "1.0"});
    let desired = unit(json!({}));
    let mut observed: ObservedUnit = serde_json::from_value(json!({"apiVersion": "gitops.nyl/v1", "kind": "ObservedUnit", "unit": desired.unit, "receipt": {"executionKey": desired.execution_key, "behaviorVersion": 1, "run": "00000000-0000-7000-8000-000000000002", "startedAt": "2026-10-28T12:00:00Z", "finishedAt": "2026-10-28T12:00:01Z"}, "condition": null})).unwrap();
    let driver = TerraformDriver::opentofu();
    let mut context = world.context(&tool);
    context.observed = Some(&observed);
    assert!(
        matches!(outcome(&driver, &context, &desired), Outcome::Failed { category: FailureCategory::Ownership, retryable: false, message } if message.starts_with("NYL-TOFU-STATE-MISSING"))
    );
    assert_eq!(tool.count("plan"), 0);
    drop(context);
    observed.accepted_empty_state = driver
        .ownership_scopes(&EnvironmentName::new("dev").unwrap(), &desired.spec)
        .into_iter()
        .next();
    let mut context = world.context(&tool);
    context.observed = Some(&observed);
    assert!(matches!(
        outcome(&driver, &context, &desired),
        Outcome::Succeeded { .. }
    ));
    let changed_scope = unit(json!({"backend": {"bucket": "other", "key": "dev/test"}}));
    assert!(matches!(
        outcome(&driver, &context, &changed_scope),
        Outcome::Failed {
            category: FailureCategory::Ownership,
            ..
        }
    ));
}

#[test]
fn test_key_inputs_ignore_native_data_but_include_varfiles_and_require_module_coverage() {
    let world = World::new();
    let tool = FakeTool::new();
    std::fs::create_dir_all(world.0.path().join("worktree/nyl/module/.terraform")).unwrap();
    std::fs::write(world.0.path().join("worktree/nyl/module/.terraform/state"), "discarded").unwrap();
    std::fs::write(world.0.path().join("worktree/nyl/vars.tfvars"), "id = 1").unwrap();
    let context = world.context(&tool);
    let resolution = ResolutionContext {
        source_roots: BTreeMap::new(),
        environment: EnvironmentName::new("dev").unwrap(),
        worktree: context.worktree.clone(),
        project_dir: context.project_dir.clone(),
        env: BTreeMap::new(),
        deadline: context.deadline,
        now: context.now,
        tools: &tool,
        cache_dir: context.cache_dir.clone(),
    };
    let desired = unit(json!({"varFiles": ["vars.tfvars"]}));
    let first = TerraformDriver::opentofu()
        .execution_key_inputs(&resolution, &desired.spec)
        .unwrap();
    assert!(first.files.keys().all(|path| !path.contains(".terraform/")));
    std::fs::write(world.0.path().join("worktree/nyl/vars.tfvars"), "id = 2").unwrap();
    assert_ne!(
        first.files,
        TerraformDriver::opentofu()
            .execution_key_inputs(&resolution, &desired.spec)
            .unwrap()
            .files
    );
    let mut tool = FakeTool::new();
    tool.modules = json!({"Modules": [{"Source": "../shared", "Dir": "../shared"}]});
    std::fs::create_dir_all(world.0.path().join("worktree/nyl/shared")).unwrap();
    std::fs::write(
        world.0.path().join("worktree/nyl/shared/main.tf"),
        "output \"x\" { value = 1 }",
    )
    .unwrap();
    assert!(
        matches!(outcome(&TerraformDriver::opentofu(), &world.context(&tool), &unit(json!({}))), Outcome::Failed { message, .. } if message.contains("outside spec.files"))
    );
    assert!(matches!(
        outcome(
            &TerraformDriver::opentofu(),
            &world.context(&tool),
            &unit(json!({"files": ["module/**", "shared/**"]}))
        ),
        Outcome::Succeeded { .. }
    ));
}

#[test]
fn test_unreviewed_plan_approval_is_allowed_unless_digest_is_required() {
    let world = World::new();
    let tool = FakeTool::new();
    let result = outcome(
        &TerraformDriver::opentofu(),
        &world.context(&tool),
        &unit(json!({"approval": {"mode": "manual", "bind": "plan"}})),
    );
    assert!(
        matches!(result, Outcome::Succeeded { .. }),
        "the shell admits explicit unreviewed approvals: {result:?}"
    );
    assert_eq!(tool.count("apply"), 1);
}

#[test]
fn test_local_backend_path_aliases_have_one_scope_and_cannot_hide_disposable_state() {
    let world = World::new();
    let driver = TerraformDriver::opentofu();
    let state = world.0.path().join("state.tfstate");
    let alias = world.0.path().join("unused/../state.tfstate");
    let canonical = unit(json!({"backend": {"path": state}}));
    let aliased = unit(json!({"backend": {"path": alias, "workspace_dir": "/ignored-for-default"}}));
    assert_eq!(
        driver.ownership_scopes(&EnvironmentName::new("dev").unwrap(), &canonical.spec),
        driver.ownership_scopes(&EnvironmentName::new("dev").unwrap(), &aliased.spec)
    );
    let mut tool = FakeTool::new();
    let disposable = world.0.path().join("outside/../worktree/state.tfstate");
    tool.backend_metadata = json!({"backend": {"type": "local", "config": {"path": disposable}}});
    let result = outcome(
        &driver,
        &world.context(&tool),
        &unit(json!({"backend": {"path": disposable}})),
    );
    assert!(
        matches!(&result, Outcome::Failed { message, .. } if message.contains("disposable")),
        "{result:?}"
    );
    assert_eq!(tool.count("plan"), 0);
    tool.backend_metadata = json!({"backend": {"type": "local", "config": {"path": state}}});
    let omitted = outcome(&driver, &world.context(&tool), &unit(json!({"backend": {}})));
    assert!(matches!(omitted, Outcome::Failed { message, .. } if message.contains("declare backend.path")));
}

#[cfg(unix)]
#[test]
fn test_local_backend_rejects_existing_parent_symlink_alias_into_disposable_checkout() {
    let world = World::new();
    let alias = world.0.path().join("checkout-alias");
    let nested = world.0.path().join("worktree/nested");
    std::fs::create_dir(&nested).unwrap();
    std::os::unix::fs::symlink(&nested, &alias).unwrap();
    // Missing leaves are supported; .. follows a symlink's physical parent.
    for state in [alias.join("new/state.tfstate"), alias.join("../new/state.tfstate")] {
        let mut tool = FakeTool::new();
        tool.backend_metadata = json!({"backend": {"type": "local", "config": {"path": state}}});
        let result = outcome(
            &TerraformDriver::opentofu(),
            &world.context(&tool),
            &unit(json!({"backend": {"path": state}})),
        );
        assert!(
            matches!(&result, Outcome::Failed { message, .. } if message.contains("disposable")),
            "{result:?}"
        );
        assert_eq!(tool.count("plan"), 0);
    }
    let dangling = world.0.path().join("dangling-alias");
    std::os::unix::fs::symlink(world.0.path().join("worktree/absent"), &dangling).unwrap();
    let mut tool = FakeTool::new();
    tool.backend_metadata = json!({"backend": {"type": "local", "config": {}}});
    let result = outcome(
        &TerraformDriver::opentofu(),
        &world.context(&tool),
        &unit(json!({"backend": {"path": dangling.join("state.tfstate")}})),
    );
    assert!(
        matches!(&result, Outcome::Failed { message, .. } if message.contains("dangling symlink")),
        "{result:?}"
    );
    assert_eq!(tool.count("plan"), 0);
}

#[test]
fn test_remote_backend_requires_effective_identity_and_normalizes_empty_settings() {
    let world = World::new();
    let driver = TerraformDriver::opentofu();
    let tool = FakeTool::new();
    let omitted = unit(json!({"backend": {"bucket": "test"}}));
    assert!(
        matches!(outcome(&driver, &world.context(&tool), &omitted), Outcome::Failed { message, .. } if message.contains("backend.key"))
    );
    assert_eq!(tool.count("plan"), 0);
    let canonical = unit(json!({}));
    let empty_settings = unit(json!({"backend": {"bucket": "test", "key": "dev/test", "encrypt": false}}));
    let env = EnvironmentName::new("dev").unwrap();
    assert_eq!(
        driver.ownership_scopes(&env, &canonical.spec),
        driver.ownership_scopes(&env, &empty_settings.spec)
    );
}

#[test]
fn test_structured_provider_secret_leaves_are_masked_and_rotation_preserves_digest() {
    let world = World::new();
    let tool = FakeTool::new();
    let desired = unit(json!({"env": {"secrets": {"TF_VAR_CONFIG": "provider-config"}}}));
    let driver = TerraformDriver::opentofu();
    let mut digests = Vec::new();
    for password in ["hidden-before", "hidden-after"] {
        tool.plan.lock().unwrap()["resource_changes"][0]["change"]["after"]["password"] = json!(password);
        let mut context = world.context(&tool);
        let raw = json!({"nested": [{"password": password}]}).to_string();
        context.env.insert("TF_VAR_CONFIG".to_owned(), raw.clone());
        let Supported::Supported(report) = driver.plan(&context, &desired).unwrap() else {
            panic!("native plan is supported")
        };
        let summary: Value = serde_json::from_str(&report.summary).unwrap();
        assert_eq!(summary["resources"][0]["change"]["after"]["password"], "***");
        assert!(!report.summary.contains(password));
        digests.push(report.change_digest);
        let calls = tool.calls.lock().unwrap();
        let call = calls.last().unwrap();
        assert!(call.masked.contains(&raw));
        assert!(call.masked.contains(&password.to_owned()));
    }
    assert_eq!(digests[0], digests[1]);
}
