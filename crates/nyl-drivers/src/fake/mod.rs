//! Test-only fake kinds in `units.test.nyl/v1`.
//!
//! Contract: [Fake kinds](../../../../design/reference-scenarios.md#fake-kinds).
//! Each fake implements exactly the driver trait and capabilities of the
//! built-in kind it stands in for, so every milestone runs the whole
//! platform scenario before the real kind exists, and tier 1 never needs the
//! real tools. Scenario files name the real kinds;
//! [`with_test_kinds`](crate::registry::with_test_kinds) registers each fake
//! under the real kinds it stands in for.
//!
//! A [`Script`] lets a scenario's `driver` steps decide the outcome of a
//! fake's next calls, such as a retryable failure. For recovery tests it
//! can also give `FakeInfra` the `inspect` recovery policy
//! ([`Script::recover_by`]) and script its `inspect` results: a scripted
//! success reports the effects applied, a failure not applied, and an
//! uncertain result unknown. A scripted `verify` decides the fake's next
//! drift verdict ([`Script::verification`]): `drifted`, `clean` for a
//! success, and `unknown` for an uncertain result.

mod image;
mod infra;
mod observer;
mod publication;

use std::collections::{BTreeMap, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex};

use nyl_core::orchestration::driver::{DriverError, DriverOperation, DriverResult, Outcome, RecoveryPolicy};
use nyl_core::orchestration::ids::UnitName;
use nyl_core::orchestration::state::{Drift, FailureCategory};
use serde_json::Value;

use crate::command::resolved_spec;

pub use image::FakeImage;
pub use infra::FakeInfra;
pub use observer::FakeObserver;
pub use publication::FakePublication;

/// API version of the fake kinds.
pub const API_VERSION_TEST_UNITS: &str = "units.test.nyl/v1";

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
    /// `verify` only: the resources no longer match the receipt.
    Drifted,
}

/// Queued results by unit and operation name.
type ScriptEntries = BTreeMap<(UnitName, &'static str), VecDeque<Scripted>>;

/// Scripted results shared by every fake of one test world.
#[derive(Debug, Clone, Default)]
pub struct Script {
    entries: Arc<Mutex<ScriptEntries>>,
    /// `FakeInfra`'s execution recovery policy, when a test sets one.
    execution_policy: Arc<Mutex<Option<RecoveryPolicy>>>,
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

    /// Give `FakeInfra` this execution recovery policy instead of
    /// `converge`, such as `inspect` to exercise recovery by inspection.
    pub fn recover_by(&self, policy: RecoveryPolicy) {
        *self.execution_policy.lock().expect("script lock") = Some(policy);
    }

    /// The execution recovery policy a test set, else `default`.
    fn execution_policy(&self, default: RecoveryPolicy) -> RecoveryPolicy {
        self.execution_policy.lock().expect("script lock").unwrap_or(default)
    }

    fn take(&self, unit: &UnitName, operation: DriverOperation) -> Option<Scripted> {
        self.entries
            .lock()
            .expect("script lock")
            .get_mut(&(unit.clone(), operation.as_str()))
            .and_then(VecDeque::pop_front)
    }

    /// The scripted drift verdict of the fake's next `verify` of `unit`, or
    /// `None` when none is queued and the fake decides itself. A scripted
    /// failure is a verification error, which records no observation.
    fn verification(&self, unit: &UnitName) -> DriverResult<Option<Drift>> {
        Ok(match self.take(unit, DriverOperation::Verify) {
            None => None,
            Some(Scripted::Drifted) => Some(Drift::Drifted {
                summary: "scripted drift".to_owned(),
            }),
            Some(Scripted::Succeeded { .. }) => Some(Drift::Clean),
            Some(Scripted::Uncertain) => Some(Drift::Unknown {
                reason: "scripted unknown verify".to_owned(),
            }),
            Some(Scripted::Failed { .. }) => {
                return Err(DriverError::before_effects("scripted verify failure"));
            }
        })
    }

    /// The scripted outcome of a reconcile or teardown, when one is queued.
    /// A scripted success falls through to the fake's own behavior with
    /// replaced outputs, reported through `outputs`; a `drifted` script
    /// only applies to `verify`, so it falls through unchanged.
    fn outcome(&self, unit: &UnitName, operation: DriverOperation) -> ScriptedOutcome {
        match self.take(unit, operation) {
            None | Some(Scripted::Drifted) => ScriptedOutcome::Run { outputs: None },
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

/// The digests of the directory a unit's local path field names, as the
/// execution key's files: `pointer` in `spec`, `field` in findings.
fn source_files(
    ctx: &nyl_core::orchestration::driver::ResolutionContext<'_>,
    spec: &Value,
    pointer: &str,
    field: &str,
) -> DriverResult<BTreeMap<String, String>> {
    let path = required_str(spec, pointer)?;
    let source_field = pointer.strip_suffix("/path").unwrap_or(pointer);
    let root = ctx.source_roots.get(source_field);
    let paths = crate::command::roots(
        root.map_or(&ctx.worktree, |root| &root.worktree),
        root.map_or(&ctx.project_dir, |root| &root.project_dir),
    )?;
    let directory = paths
        .resolve(field, path)
        .map_err(|error| DriverError::before_effects(error.to_string()))?;
    directory_digests(&paths.worktree_root, &directory)
}

/// A field of a resolved spec, or a driver error naming it.
fn required_str<'a>(spec: &'a Value, pointer: &str) -> DriverResult<&'a str> {
    spec.pointer(pointer)
        .and_then(Value::as_str)
        .ok_or_else(|| DriverError::before_effects(format!("spec{} must be a string", pointer.replace('/', "."))))
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
            source_commit: Some(CommitId::new("a".repeat(40)).unwrap()),
            source_revision: Some(CommitId::new("a".repeat(40)).unwrap()),
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
            source_roots: std::collections::BTreeMap::default(),
            secrets: std::collections::BTreeMap::default(),
            artifacts: std::collections::BTreeMap::default(),
            interactive: false,
            unit: "dev/database".parse::<UnitAddress>().unwrap(),
            operation: DriverOperation::Reconcile,
            worktree: directory.path().to_path_buf(),
            project_dir: directory.path().to_path_buf(),
            temp_dir: directory.path().to_path_buf(),
            env: BTreeMap::new(),
            approved_digest: approved,
            deadline: chrono::Utc::now(),
            now: chrono::Utc::now(),
            observed: None,
            tools,
            commit_identity: None,
            cache_dir: directory.path().join("cache"),
            teardown_wait: None,
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
                hold: false,
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

    fn receipt(unit: &DesiredUnit) -> nyl_core::orchestration::state::Receipt {
        nyl_core::orchestration::state::Receipt {
            execution_key: unit.execution_key.clone().unwrap(),
            behavior_version: 1,
            run: nyl_core::orchestration::ids::RunId::new("00000000-0000-7000-8000-0000000000aa").unwrap(),
            read_desired: None,
            started_at: chrono::Utc::now(),
            finished_at: chrono::Utc::now(),
            versions: BTreeMap::new(),
            approval: None,
            provenance: BTreeMap::new(),
            outputs: json!({}),
            artifacts: Vec::new(),
        }
    }

    fn drift_of(verification: DriverResult<Supported<nyl_core::orchestration::driver::Verification>>) -> Drift {
        match verification.unwrap() {
            Supported::Supported(verification) => verification.drift,
            Supported::Unsupported => panic!("the fakes verify"),
        }
    }

    /// A scenario's `driver: {operation: verify, outcome: drifted}` step is
    /// how tier 1 makes a fake unit drift; if the verdict never reached the
    /// fakes' `verify`, drift scenarios would only ever see clean units.
    #[test]
    fn test_scripted_drift_reaches_the_fakes_verify_once() {
        let directory = TempDir::new().unwrap();
        let script = Script::new();
        let image = desired(
            "web-image",
            json!({"context": {"path": "web"}, "repository": "registry.test/web"}),
        );
        let backend = directory.path().join("tofu/dev-network.tfstate");
        let network = desired(
            "network",
            json!({"source": {"path": "infra"}, "backend": {"path": backend.to_str().unwrap()},
                   "variables": {"cidr": "10.0.0.0/16"}, "outputs": {"vpcId": {"type": "string"}}}),
        );
        let infra = FakeInfra::new(script.clone());
        let Supported::Supported(Outcome::Succeeded { .. }) =
            infra.reconcile(&context(&directory, &NoTools, None), &network).unwrap()
        else {
            panic!("the network applies");
        };
        for name in ["web-image", "network"] {
            script.push(
                UnitName::new(name).unwrap(),
                DriverOperation::Verify,
                &Scripted::Drifted,
                1,
            );
        }
        let image_driver = FakeImage::new(script.clone());
        let verify_image = || image_driver.verify(&context(&directory, &NoTools, None), &image, &receipt(&image));
        let verify_network = || infra.verify(&context(&directory, &NoTools, None), &network, &receipt(&network));
        assert!(matches!(drift_of(verify_image()), Drift::Drifted { .. }));
        assert!(matches!(drift_of(verify_network()), Drift::Drifted { .. }));
        // Only the next call is scripted: afterwards each fake decides itself,
        // and the applied network still matches its state file.
        assert_eq!(drift_of(verify_image()), Drift::Clean);
        assert_eq!(drift_of(verify_network()), Drift::Clean);
        // A drift script applies to verify only; a reconcile runs as usual.
        script.push(
            UnitName::new("web-image").unwrap(),
            DriverOperation::Reconcile,
            &Scripted::Drifted,
            1,
        );
        assert!(matches!(
            image_driver
                .reconcile(&context(&directory, &NoTools, None), &image)
                .unwrap(),
            Supported::Supported(Outcome::Succeeded { .. })
        ));
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
}
