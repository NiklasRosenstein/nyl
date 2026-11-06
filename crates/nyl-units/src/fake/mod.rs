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
//! uncertain result unknown. A scripted `detect-drift` decides the fake's next
//! drift result ([`Script::drift`]): `drifted`, `clean` for a
//! success, and `unknown` for an uncertain result. A scripted `observe`
//! decides what the publication's next read of its observed values finds
//! ([`Script::observed`]): the values of a success, every observation pending
//! or failed, and nothing scripted reads as pending.

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

/// The real publication driver reading observed values through a [`Script`]
/// instead of Argo CD and the workload Cluster, so tier-1 scenarios can
/// script what the live system holds (`driver: {operation: observe}`) while
/// the driver's own declaration, validation, and resolution run as built.
#[derive(Debug, Clone)]
pub struct ScriptedLive {
    script: Script,
}

impl ScriptedLive {
    pub fn new(script: Script) -> Self {
        Self { script }
    }
}

impl crate::kubernetes_publication::LiveReader for ScriptedLive {
    fn read(
        &self,
        request: &crate::kubernetes_publication::LiveRequest<'_>,
    ) -> BTreeMap<String, nyl_core::orchestration::driver::ObservedValue> {
        let names: Vec<String> = request.sources.keys().cloned().collect();
        match self.script.observed(&request.unit.unit.name, &names) {
            Ok(values) => values.values,
            Err(error) => names
                .into_iter()
                .map(|name| {
                    (
                        name,
                        nyl_core::orchestration::driver::ObservedValue::Failed {
                            reason: error.to_string(),
                        },
                    )
                })
                .collect(),
        }
    }
}

/// API version of the fake kinds.
pub const API_VERSION_TEST_UNITS: &str = "units.test.nyl/v1";

/// A scripted result for a fake's next call of one operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scripted {
    RetryRequired {
        context: Value,
    },
    Failed {
        category: FailureCategory,
        retryable: bool,
    },
    Uncertain,
    /// Succeed with these outputs instead of the derived ones.
    Succeeded {
        outputs: Option<Value>,
    },
    /// `detect-drift` only: the resources no longer match the receipt.
    Drifted,
    /// `observe` only: no declared observation has a value yet.
    Pending,
}

/// Queued results by unit and operation name.
type ScriptEntries = BTreeMap<(UnitName, &'static str), VecDeque<Scripted>>;

/// Scripted results shared by every fake of one test world.
#[derive(Debug, Clone, Default)]
pub struct Script {
    entries: Arc<Mutex<ScriptEntries>>,
    /// `FakeInfra`'s execution recovery policy, when a test sets one.
    execution_policy: Arc<Mutex<Option<RecoveryPolicy>>>,
    /// What `FakePublication`'s `observe` teardown wait sees of Argo CD.
    observer: FakeObserver,
}

impl Script {
    pub fn new() -> Self {
        Self::default()
    }

    /// The scripted Argo CD of `FakePublication`'s `observe` teardown wait,
    /// shared by every clone of the script.
    pub fn observer(&self) -> FakeObserver {
        self.observer.clone()
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

    /// The scripted drift result of the fake's next `detect_drift` of
    /// `unit`, or `None` when none is queued and the fake decides itself. A
    /// scripted failure is a driver error, which records no drift.
    fn drift(&self, unit: &UnitName) -> DriverResult<Option<Drift>> {
        Ok(match self.take(unit, DriverOperation::DetectDrift) {
            None => None,
            Some(Scripted::Drifted) => Some(Drift::Drifted {
                summary: "scripted drift".to_owned(),
            }),
            Some(Scripted::Succeeded { .. }) => Some(Drift::Clean),
            Some(Scripted::Uncertain) => Some(Drift::Unknown {
                reason: "scripted unknown drift".to_owned(),
            }),
            Some(Scripted::Failed { .. } | Scripted::RetryRequired { .. } | Scripted::Pending) => {
                return Err(DriverError::before_effects("scripted drift detection failure"));
            }
        })
    }

    /// What the fake's next `observe` of `unit` finds for the declared
    /// observations `names`: a scripted success holds each value under its
    /// name (a declared name it leaves out is pending), a scripted failure
    /// fails every one, and anything else, or nothing scripted, leaves every
    /// one pending. A scripted driver error is a driver error.
    fn observed(
        &self,
        unit: &UnitName,
        names: &[String],
    ) -> DriverResult<nyl_core::orchestration::driver::ObservedValues> {
        use nyl_core::orchestration::driver::{ObservedValue, ObservedValues};
        let each = |value: &dyn Fn(&str) -> ObservedValue| ObservedValues {
            values: names.iter().map(|name| (name.clone(), value(name))).collect(),
        };
        let pending = |reason: &'static str| {
            each(&|_| ObservedValue::Pending {
                reason: reason.to_owned(),
            })
        };
        Ok(match self.take(unit, DriverOperation::Observe) {
            None => pending("nothing is scripted"),
            Some(Scripted::Pending | Scripted::Drifted | Scripted::Uncertain) => pending("scripted pending"),
            Some(Scripted::Succeeded { outputs }) => {
                each(&|name| match outputs.as_ref().and_then(|outputs| outputs.get(name)) {
                    Some(value) => ObservedValue::Value { value: value.clone() },
                    None => ObservedValue::Pending {
                        reason: format!("scripted: no value for {name}"),
                    },
                })
            }
            Some(Scripted::Failed { .. }) => each(&|_| ObservedValue::Failed {
                reason: "scripted failure".to_owned(),
            }),
            Some(Scripted::RetryRequired { .. }) => {
                return Err(DriverError::before_effects("scripted observation failure"));
            }
        })
    }

    /// The scripted outcome of a reconcile or teardown, when one is queued.
    /// A scripted success falls through to the fake's own behavior with
    /// replaced outputs, reported through `outputs`; a `drifted` script
    /// only applies to `detect-drift`, so it falls through unchanged.
    fn outcome(&self, unit: &UnitName, operation: DriverOperation) -> ScriptedOutcome {
        match self.take(unit, operation) {
            None | Some(Scripted::Drifted | Scripted::Pending) => ScriptedOutcome::Run { outputs: None },
            Some(Scripted::Succeeded { outputs }) => ScriptedOutcome::Run { outputs },
            Some(Scripted::Uncertain) => ScriptedOutcome::Return(Outcome::Uncertain {
                reason: format!("scripted uncertain {}", operation.as_str()),
            }),
            Some(Scripted::RetryRequired { context }) => ScriptedOutcome::Return(Outcome::RetryRequired {
                category: FailureCategory::Ownership,
                message: "scripted retry acknowledgement".to_owned(),
                context,
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

/// Every resource nyl-core, nyl-render, and this crate own, for tests of the
/// fake kinds; applications assemble their own catalogue.
pub fn test_catalogue() -> nyl_render::config::ResourceCatalogue {
    let mut descriptors = nyl_render::resources::catalog::resource_catalogue();
    descriptors.extend_from_slice(crate::catalog::RESOURCES);
    nyl_render::config::ResourceCatalogue(descriptors)
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
            instances: &nyl_core::orchestration::instance::NO_INSTANCES,
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
            incarnations: &nyl_core::orchestration::driver::UnknownIncarnations,
            dirty_source: false,
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

    fn drift_of(detection: DriverResult<Supported<nyl_core::orchestration::driver::DriftDetection>>) -> Drift {
        match detection.unwrap() {
            Supported::Supported(detection) => detection.drift,
            Supported::Unsupported => panic!("the fakes detect drift"),
        }
    }

    /// A scenario's `driver: {operation: detect-drift, outcome: drifted}` step is
    /// how tier 1 makes a fake unit drift; if the verdict never reached the
    /// fakes' `detect_drift`, drift scenarios would only ever see clean units.
    #[test]
    fn test_scripted_drift_reaches_the_fakes_drift_detection_once() {
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
                DriverOperation::DetectDrift,
                &Scripted::Drifted,
                1,
            );
        }
        let image_driver = FakeImage::new(script.clone());
        let detect_image = || image_driver.detect_drift(&context(&directory, &NoTools, None), &image, &receipt(&image));
        let detect_network = || infra.detect_drift(&context(&directory, &NoTools, None), &network, &receipt(&network));
        assert!(matches!(drift_of(detect_image()), Drift::Drifted { .. }));
        assert!(matches!(drift_of(detect_network()), Drift::Drifted { .. }));
        // Only the next call is scripted: afterwards each fake decides itself,
        // and the applied network still matches its state file.
        assert_eq!(drift_of(detect_image()), Drift::Clean);
        assert_eq!(drift_of(detect_network()), Drift::Clean);
        // A drift script applies to drift detection only; a reconcile runs as usual.
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
