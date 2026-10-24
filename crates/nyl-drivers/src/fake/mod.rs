//! Test-only fake kinds in `units.test.nyl/v1`.
//!
//! Contract: [Fake kinds](../../../../design/reference-scenarios.md#fake-kinds).
//! Each fake implements exactly the driver trait and capabilities of the
//! built-in kind it stands in for, so every milestone runs the whole
//! platform scenario before the real kind exists, and tier 1 never needs the
//! real tools. Scenario files name the real kinds; [`stand_in_for`] maps
//! them.
//!
//! A [`Script`] lets a scenario's `driver` steps decide the outcome of a
//! fake's next calls, such as a retryable failure.

mod image;
mod infra;
mod publication;

use std::collections::{BTreeMap, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex};

use nyl_core::orchestration::driver::{DriverError, DriverOperation, DriverResult, Outcome};
use nyl_core::orchestration::ids::UnitName;
use nyl_core::orchestration::state::FailureCategory;
use serde_json::Value;

pub use image::FakeImage;
pub use infra::FakeInfra;
pub use publication::FakePublication;

/// API version of the fake kinds.
pub const API_VERSION_TEST_UNITS: &str = "units.test.nyl/v1";

/// The fake kind standing in for a built-in kind, by its `apiVersion` and
/// `kind`, for the kinds a milestone has not built yet.
pub fn stand_in_for(api_version: &str, kind: &str) -> Option<&'static str> {
    if api_version != nyl_core::constants::API_VERSION_UNITS {
        return None;
    }
    match kind {
        "OciImage" => Some(image::KIND),
        "OpenTofu" | "Terraform" => Some(infra::KIND),
        "KubernetesPublication" => Some(publication::KIND),
        _ => None,
    }
}

/// A scripted result for a fake's next call of one operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scripted {
    Failed {
        category: FailureCategory,
        retryable: bool,
    },
    Uncertain,
    /// Succeed with these outputs instead of the derived ones.
    Succeeded {
        outputs: Option<Value>,
    },
}

/// Queued results by unit and operation name.
type ScriptEntries = BTreeMap<(UnitName, &'static str), VecDeque<Scripted>>;

/// Scripted results shared by every fake of one test world.
#[derive(Debug, Clone, Default)]
pub struct Script {
    entries: Arc<Mutex<ScriptEntries>>,
}

impl Script {
    pub fn new() -> Self {
        Self::default()
    }

    /// Script the next `times` calls of `operation` for `unit`.
    pub fn push(&self, unit: UnitName, operation: DriverOperation, result: &Scripted, times: usize) {
        let mut entries = self.entries.lock().expect("script lock");
        let queue = entries.entry((unit, operation.as_str())).or_default();
        for _ in 0..times {
            queue.push_back(result.clone());
        }
    }

    fn take(&self, unit: &UnitName, operation: DriverOperation) -> Option<Scripted> {
        self.entries
            .lock()
            .expect("script lock")
            .get_mut(&(unit.clone(), operation.as_str()))
            .and_then(VecDeque::pop_front)
    }

    /// The scripted outcome of a reconcile or teardown, when one is queued.
    /// A scripted success falls through to the fake's own behavior with
    /// replaced outputs, reported through `outputs`.
    fn outcome(&self, unit: &UnitName, operation: DriverOperation) -> ScriptedOutcome {
        match self.take(unit, operation) {
            None => ScriptedOutcome::Run { outputs: None },
            Some(Scripted::Succeeded { outputs }) => ScriptedOutcome::Run { outputs },
            Some(Scripted::Uncertain) => ScriptedOutcome::Return(Outcome::Uncertain {
                reason: format!("scripted uncertain {}", operation.as_str()),
            }),
            Some(Scripted::Failed { category, retryable }) => ScriptedOutcome::Return(Outcome::Failed {
                category,
                retryable,
                message: format!("scripted {} failure", operation.as_str()),
            }),
        }
    }
}

enum ScriptedOutcome {
    Run { outputs: Option<Value> },
    Return(Outcome),
}

/// Digests of every regular file beneath `directory`, keyed by
/// worktree-rooted path, such as `/infra/network/main.tf`.
fn directory_digests(worktree: &Path, directory: &Path) -> DriverResult<BTreeMap<String, String>> {
    let mut digests = BTreeMap::new();
    let mut pending = vec![directory.to_path_buf()];
    while let Some(current) = pending.pop() {
        let entries = std::fs::read_dir(&current)
            .map_err(|error| DriverError::before_effects(format!("cannot read {}: {error}", current.display())))?;
        for entry in entries {
            let entry = entry.map_err(|error| DriverError::before_effects(error.to_string()))?;
            let path = entry.path();
            let file_type = entry
                .file_type()
                .map_err(|error| DriverError::before_effects(error.to_string()))?;
            if file_type.is_dir() {
                pending.push(path);
            } else if file_type.is_file() {
                let bytes = std::fs::read(&path)
                    .map_err(|error| DriverError::before_effects(format!("cannot read {}: {error}", path.display())))?;
                let relative = path
                    .strip_prefix(worktree)
                    .map_err(|_| DriverError::before_effects(format!("{} is outside the worktree", path.display())))?;
                digests.insert(
                    format!("/{}", relative.to_string_lossy().replace('\\', "/")),
                    nyl_core::digest::sha256_hex(&bytes),
                );
            }
        }
    }
    Ok(digests)
}

/// Resolve a unit's local `path` under the local path rule.
fn local_directory(worktree: &Path, project_dir: &Path, field: &str, path: &str) -> DriverResult<std::path::PathBuf> {
    let worktree = worktree
        .canonicalize()
        .map_err(|error| DriverError::before_effects(format!("cannot resolve the worktree: {error}")))?;
    let project_dir = project_dir
        .canonicalize()
        .map_err(|error| DriverError::before_effects(format!("cannot resolve the project directory: {error}")))?;
    nyl_render::util::project_path::ProjectPaths::new(project_dir, worktree)
        .resolve(field, path)
        .map_err(|error| DriverError::before_effects(error.to_string()))
}

/// A field of a resolved spec, or a driver error naming it.
fn required_str<'a>(spec: &'a Value, pointer: &str) -> DriverResult<&'a str> {
    spec.pointer(pointer)
        .and_then(Value::as_str)
        .ok_or_else(|| DriverError::before_effects(format!("spec{} must be a string", pointer.replace('/', "."))))
}

/// The resolved spec of a desired document, which execution requires.
fn resolved_spec(unit: &nyl_core::orchestration::state::DesiredUnit) -> DriverResult<&Value> {
    unit.resolved_spec
        .as_ref()
        .ok_or_else(|| DriverError::before_effects(format!("unit {} has no resolved spec", unit.unit.name)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use nyl_core::orchestration::driver::{
        Driver, ExecutionContext, Outcome, Supported, ToolExit, ToolInvocation, ToolRunner,
    };
    use nyl_core::orchestration::ids::{CommitId, EnvironmentName, ExecutionKey, Uid, UnitAddress};
    use nyl_core::orchestration::state::{
        DeletionIntent, DeletionReason, DesiredUnit, DesiredUnitKind, Lifecycle, Readiness, StateV1, UnitIdentity,
    };
    use serde_json::json;
    use tempfile::TempDir;

    struct NoTools;

    impl ToolRunner for NoTools {
        fn run(&self, _invocation: ToolInvocation) -> DriverResult<ToolExit> {
            panic!("fakes run no tools")
        }
    }

    fn desired(name: &str, spec: Value) -> DesiredUnit {
        DesiredUnit {
            api_version: StateV1,
            kind: DesiredUnitKind,
            unit: UnitIdentity {
                environment: EnvironmentName::new("dev").unwrap(),
                name: UnitName::new(name).unwrap(),
                uid: Some(Uid::new("00000000-0000-7000-8000-000000000001").unwrap()),
                api_version: "units.gitops.nyl/v1".to_owned(),
                kind: "OpenTofu".to_owned(),
            },
            lifecycle: Lifecycle::Active,
            source_commit: CommitId::new("a".repeat(40)).unwrap(),
            source_revision: CommitId::new("a".repeat(40)).unwrap(),
            spec: spec.clone(),
            resolved_spec: Some(spec),
            provenance: BTreeMap::new(),
            execution_key: Some(ExecutionKey::from_digest_of(name.as_bytes())),
            fingerprint: None,
            attestations: Vec::new(),
            readiness: Readiness::Ready,
        }
    }

    fn context<'a>(directory: &TempDir, tools: &'a NoTools, approved: Option<ChangeDigestArg>) -> ExecutionContext<'a> {
        ExecutionContext {
            unit: "dev/database".parse::<UnitAddress>().unwrap(),
            operation: DriverOperation::Reconcile,
            worktree: directory.path().to_path_buf(),
            project_dir: directory.path().to_path_buf(),
            temp_dir: directory.path().to_path_buf(),
            env: BTreeMap::new(),
            approved_digest: approved,
            deadline: chrono::Utc::now(),
            observed: None,
            tools,
        }
    }

    type ChangeDigestArg = nyl_core::orchestration::ids::ChangeDigest;

    fn plan_digest(driver: &FakeInfra, directory: &TempDir, unit: &DesiredUnit) -> ChangeDigestArg {
        match driver.plan(&context(directory, &NoTools, None), unit).unwrap() {
            Supported::Supported(report) => report.change_digest.unwrap(),
            Supported::Unsupported => panic!("FakeInfra plans"),
        }
    }

    #[test]
    fn test_fake_infra_applies_converges_and_tears_down_under_digest_approval() {
        let directory = TempDir::new().unwrap();
        let backend = directory.path().join("tofu/dev-database.tfstate");
        let spec = json!({
            "source": {"path": "infra"},
            "backend": {"path": backend.to_str().unwrap()},
            "variables": {"vpc_id": "vpc-1"},
            "outputs": {"host": {"type": "string"}, "password": {"type": "string", "sensitive": true}}
        });
        let driver = FakeInfra::new(Script::new());
        let unit = desired("database", spec);

        // An approval of another plan applies nothing.
        let stale = nyl_core::orchestration::ids::ChangeDigest::from_digest_of(b"other plan");
        let outcome = driver
            .reconcile(&context(&directory, &NoTools, Some(stale)), &unit)
            .unwrap();
        assert!(matches!(
            outcome,
            Supported::Supported(Outcome::AwaitingApproval { .. })
        ));
        assert!(!backend.exists());

        let digest = plan_digest(&driver, &directory, &unit);
        let Supported::Supported(Outcome::Succeeded { outputs, .. }) = driver
            .reconcile(&context(&directory, &NoTools, Some(digest)), &unit)
            .unwrap()
        else {
            panic!("the approved plan applies");
        };
        // Sensitive outputs are never returned.
        assert!(outputs.get("host").is_some() && outputs.get("password").is_none());
        // Re-executing after an uncertain execution converges: the plan finds no changes.
        let report = driver.plan(&context(&directory, &NoTools, None), &unit).unwrap();
        assert!(
            matches!(report, Supported::Supported(report) if report.changes == nyl_core::orchestration::driver::Change::None)
        );

        let mut deleting = unit.clone();
        deleting.lifecycle = Lifecycle::Deleting {
            deletion: nyl_core::orchestration::state::Deletion {
                reason: DeletionReason::Omission,
                intent: DeletionIntent::Teardown,
                teardown: None,
                requested: None,
            },
        };
        let destroy = plan_digest(&driver, &directory, &deleting);
        assert_ne!(destroy, plan_digest(&driver, &directory, &unit));
        let outcome = driver
            .teardown(&context(&directory, &NoTools, Some(destroy)), &deleting)
            .unwrap();
        assert!(matches!(outcome, Supported::Supported(Outcome::Succeeded { .. })));
        assert!(!backend.exists());
    }

    #[test]
    fn test_fake_infra_outputs_depend_on_variables_only() {
        let outputs = |cidr: &str| {
            let unit = desired(
                "network",
                json!({"source": {"path": "infra"}, "backend": {"path": "/unused"}, "variables": {"cidr": cidr},
                       "outputs": {"vpcId": {"type": "string"}}}),
            );
            FakeInfra::outputs(&unit, unit.resolved_spec.as_ref().unwrap()).unwrap()
        };
        // Equal inputs keep consumers current; changed inputs change the outputs.
        assert_eq!(outputs("10.0.0.0/16"), outputs("10.0.0.0/16"));
        assert_ne!(outputs("10.0.0.0/16"), outputs("10.1.0.0/16"));
    }

    #[test]
    fn test_script_overrides_the_next_calls_only() {
        let directory = TempDir::new().unwrap();
        let script = Script::new();
        let unit = desired(
            "web-image",
            json!({"context": {"path": "web"}, "repository": "registry.test/web"}),
        );
        script.push(
            unit.unit.name.clone(),
            DriverOperation::Reconcile,
            &Scripted::Failed {
                category: FailureCategory::ToolError,
                retryable: true,
            },
            1,
        );
        let driver = FakeImage::new(script);
        let first = driver.reconcile(&context(&directory, &NoTools, None), &unit).unwrap();
        assert!(matches!(
            first,
            Supported::Supported(Outcome::Failed { retryable: true, .. })
        ));
        let Supported::Supported(Outcome::Succeeded { artifacts, .. }) =
            driver.reconcile(&context(&directory, &NoTools, None), &unit).unwrap()
        else {
            panic!("the second call is not scripted");
        };
        let reference = artifacts[0].spec["reference"].as_str().unwrap();
        assert!(reference.starts_with("registry.test/web@sha256:"), "{reference}");
        assert!(artifacts[0].spec["tags"][0].as_str().unwrap().starts_with("nyl-dev-"));
    }

    #[test]
    fn test_stand_ins_cover_the_kinds_m3_lacks() {
        assert_eq!(stand_in_for("units.gitops.nyl/v1", "OciImage"), Some("FakeImage"));
        assert_eq!(stand_in_for("units.gitops.nyl/v1", "OpenTofu"), Some("FakeInfra"));
        assert_eq!(
            stand_in_for("units.gitops.nyl/v1", "KubernetesPublication"),
            Some("FakePublication")
        );
        assert_eq!(stand_in_for("units.gitops.nyl/v1", "Command"), None);
    }
}
