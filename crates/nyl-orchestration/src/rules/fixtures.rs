//! Builders of state documents, coordination refs, and a driver for the
//! rules' tests.

use std::collections::BTreeMap;

use nyl_core::constants::API_VERSION_UNITS;
use nyl_core::orchestration::driver::{
    Capabilities, Driver, DriverResult, ExecutionContext, KeyInputs, Outcome, OwnershipScope, PlanReport,
    RecoveryPolicies, RecoveryPolicy, ResolutionContext, Supported, Verification,
};
use nyl_core::orchestration::ids::{CommitId, EnvironmentName, ExecutionKey, RunId, Uid, UnitName};
use nyl_core::orchestration::ports::RunState;
use nyl_core::orchestration::state::{
    ArtifactDigest, ArtifactDocument, ArtifactMetadata, ArtifactUnit, DesiredUnit, DesiredUnitKind, Lease, LeaseKind,
    Lifecycle, ObservedUnit, ObservedUnitKind, Operation, Readiness, Receipt, Run, RunKind, StateV1, UnitIdentity,
};
use nyl_core::orchestration::time::Timestamp;
use serde_json::{json, Value};

pub const COMMIT: &str = "3e7b9c0a1d2e3f4a5b6c7d8e9f0a1b2c3d4e5f6a";
pub const OTHER_COMMIT: &str = "41f0000000000000000000000000000000000000";
pub const RUN_1: &str = "0192f0c4-8d0a-7c3e-9a51-3c1f0e7d2b90";
pub const RUN_2: &str = "0192f0c4-8d0a-7c3e-9a51-3c1f0e7d2b91";
/// The run of [`observed_numbered`]'s receipts.
pub const RUN: &str = "00000000-0000-7000-8000-00000000000a";

pub fn env() -> EnvironmentName {
    EnvironmentName::new("dev").unwrap()
}

pub fn name(unit: &str) -> UnitName {
    UnitName::new(unit).unwrap()
}

pub fn commit(value: &str) -> CommitId {
    CommitId::new(value).unwrap()
}

pub fn run(value: &str) -> RunId {
    RunId::new(value).unwrap()
}

/// A distinct uid per unit name.
pub fn uid(unit: &str) -> Uid {
    let suffix = unit
        .bytes()
        .fold(0u64, |hash, byte| hash.wrapping_mul(31).wrapping_add(u64::from(byte)));
    Uid::new(format!("0192f0c4-8d0a-7c3e-9a51-{:012x}", suffix % 0x1_0000_0000_0000)).unwrap()
}

/// Incarnation `number`'s uid.
pub fn uid_numbered(number: u64) -> Uid {
    Uid::new(format!("00000000-0000-7000-8000-{number:012x}")).unwrap()
}

pub fn key(seed: &str) -> ExecutionKey {
    ExecutionKey::from_digest_of(seed.as_bytes())
}

/// An execution key of 64 `digit`s.
pub fn hex_key(digit: char) -> ExecutionKey {
    ExecutionKey::new(format!("sha256:{}", digit.to_string().repeat(64))).unwrap()
}

fn identity(unit: &str, kind: &str) -> UnitIdentity {
    UnitIdentity {
        environment: env(),
        name: name(unit),
        uid: Some(uid(unit)),
        api_version: API_VERSION_UNITS.to_owned(),
        kind: kind.to_owned(),
    }
}

/// A ready, active desired unit whose execution key derives from `seed`.
pub fn desired(unit: &str, kind: &str, spec: Value, seed: &str) -> DesiredUnit {
    DesiredUnit {
        api_version: StateV1,
        kind: DesiredUnitKind,
        unit: identity(unit, kind),
        lifecycle: Lifecycle::Active,
        source_commit: Some(commit(COMMIT)),
        source_revision: Some(commit(COMMIT)),
        spec,
        resolved_spec: None,
        provenance: BTreeMap::new(),
        execution_key: Some(key(seed)),
        fingerprint: None,
        attestations: Vec::new(),
        readiness: Readiness::Ready,
    }
}

pub fn receipt(seed: &str, outputs: Value, artifacts: Vec<ArtifactDigest>) -> Receipt {
    let at = "2026-09-24T10:00:00Z".parse().unwrap();
    Receipt {
        execution_key: key(seed),
        behavior_version: 1,
        run: run(RUN_1),
        read_desired: None,
        started_at: at,
        finished_at: at,
        versions: BTreeMap::new(),
        approval: None,
        provenance: BTreeMap::new(),
        outputs,
        artifacts,
    }
}

pub fn observed(unit: &str, kind: &str, receipt: Option<Receipt>) -> ObservedUnit {
    ObservedUnit {
        api_version: nyl_core::orchestration::state::StateV1,
        operator_modified: None,
        retry: None,
        kind: ObservedUnitKind,
        unit: identity(unit, kind),
        receipt,
        condition: None,
        desired: None,

        waiting: None,
    }
}

/// A Command unit of incarnation `uid_number`, with `key` (blocked without).
pub fn desired_numbered(unit: &str, uid_number: u64, key: Option<ExecutionKey>) -> DesiredUnit {
    let mut document = desired(unit, "Command", json!({"command": ["./run.sh"]}), unit);
    document.unit.uid = Some(uid_numbered(uid_number));
    document.execution_key = key;
    document
}

/// An observed Command unit of incarnation `uid_number`, with a receipt of
/// `key` by [`RUN`] when given.
pub fn observed_numbered(unit: &str, uid_number: u64, key: Option<ExecutionKey>) -> ObservedUnit {
    let receipt = key.map(|key| Receipt {
        execution_key: key,
        run: run(RUN),
        ..receipt(unit, json!({}), Vec::new())
    });
    let mut document = observed(unit, "Command", receipt);
    document.unit.uid = Some(uid_numbered(uid_number));
    document
}

pub fn artifact(unit: &str, artifact: &str, kind: &str, seed: &str, spec: Value) -> ArtifactDocument {
    ArtifactDocument {
        api_version: "artifacts.gitops.nyl/v1".to_owned(),
        kind: kind.to_owned(),
        metadata: ArtifactMetadata {
            name: artifact.to_owned(),
            unit: ArtifactUnit {
                environment: env(),
                name: name(unit),
                uid: uid(unit),
            },
        },
        execution_key: key(seed),
        spec,
    }
}

pub fn listed(document: &ArtifactDocument) -> ArtifactDigest {
    ArtifactDigest {
        name: document.metadata.name.clone(),
        kind: document.kind.clone(),
        digest: document.digest(),
    }
}

/// The network producer with a declared `vpcId` and a sensitive `token`.
pub fn network_spec() -> Value {
    json!({
        "outputs": {
            "vpcId": {"type": "string"},
            "subnets": {"type": "object"},
            "token": {"type": "string", "sensitive": true},
        },
    })
}

/// An unfinished reconcile run of `id` without units, by runner `ci`.
pub fn run_record(id: &str, deadline: Timestamp) -> Run {
    Run {
        api_version: StateV1,
        kind: RunKind,
        run: run(id),
        runner: "ci".to_owned(),
        operation: Operation::Reconcile,
        source_commit: None,
        read_desired: None,
        read_observed: None,
        deadline,
        units: Vec::new(),
        finished: false,
    }
}

/// The run ref of `run`, without checkpoints, read at version `v1`.
pub fn run_state(run: Run) -> RunState {
    RunState {
        run,
        checkpoints: Vec::new(),
        created: Vec::new(),
        foreign: false,
        version: "v1".to_owned(),
    }
}

/// An unbroken reconcile lease of run `id` without units, taken an hour
/// before `deadline` by runner `ci`.
pub fn lease_record(id: &str, deadline: Timestamp) -> Lease {
    Lease {
        api_version: StateV1,
        kind: LeaseKind,
        run: run(id),
        runner: "ci".to_owned(),
        operation: Operation::Reconcile,
        units: Vec::new(),
        deadline,
        taken_at: deadline - chrono::Duration::hours(1),
        broken: None,
    }
}

/// A driver of `units.gitops.nyl/v1` kind `kind`. Its execution recovery
/// policy is `converge` when the spec sets `idempotent: true`, `inspect`
/// when it sets `inspect: true`, else `manual`. The `Publication` kind tears
/// down and claims `spec.prefix` in one repository; `spec.broken` fails
/// validation.
pub struct TestDriver {
    pub kind: &'static str,
}

impl Driver for TestDriver {
    fn api_version(&self) -> &'static str {
        API_VERSION_UNITS
    }
    fn kind(&self) -> &'static str {
        self.kind
    }
    fn behavior_version(&self) -> u32 {
        1
    }
    fn default_timeout(&self) -> nyl_core::orchestration::time::Duration {
        nyl_core::orchestration::time::Duration::from_mins(17)
    }
    fn capabilities(&self, _: &Value) -> Capabilities {
        Capabilities {
            teardown: self.kind == "Publication",
            ..Capabilities::default()
        }
    }
    fn recovery(&self, spec: &Value) -> RecoveryPolicies {
        let execution = if spec["idempotent"] == json!(true) {
            RecoveryPolicy::Converge
        } else if spec["inspect"] == json!(true) {
            RecoveryPolicy::Inspect
        } else {
            RecoveryPolicy::Manual
        };
        RecoveryPolicies {
            execution,
            teardown: RecoveryPolicy::Manual,
        }
    }
    fn spec_schema(&self) -> schemars::Schema {
        schemars::json_schema!({})
    }
    fn validate_spec(&self, spec: &Value) -> Result<(), String> {
        if spec.get("broken").is_some() {
            return Err("spec.broken is not a field".to_owned());
        }
        Ok(())
    }
    fn secret_refs(&self, spec: &Value) -> Result<Vec<String>, String> {
        Ok(spec
            .get("secretRefs")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect())
    }
    fn credential_environment(&self, spec: &Value, process_env: &BTreeMap<String, String>) -> BTreeMap<String, String> {
        spec.get("credentialVariable")
            .and_then(Value::as_str)
            .and_then(|name| process_env.get(name).map(|value| (name.to_owned(), value.clone())))
            .into_iter()
            .collect()
    }
    fn ownership_scopes(&self, _: &EnvironmentName, spec: &Value) -> Vec<OwnershipScope> {
        spec.get("prefix")
            .and_then(Value::as_str)
            .map(|prefix| OwnershipScope {
                namespace: "test-tree".to_owned(),
                key: vec!["https://example.com/deploy.git".to_owned(), "main".to_owned()],
                path: prefix
                    .split('/')
                    .filter(|part| !part.is_empty() && *part != ".")
                    .map(str::to_owned)
                    .collect(),
                description: format!("tree {prefix}"),
            })
            .into_iter()
            .collect()
    }
    fn execution_key_inputs(&self, _: &ResolutionContext<'_>, _: &Value) -> DriverResult<KeyInputs> {
        Ok(KeyInputs::default())
    }
    fn plan(&self, _: &ExecutionContext<'_>, _: &DesiredUnit) -> DriverResult<Supported<PlanReport>> {
        Ok(Supported::Unsupported)
    }
    fn reconcile(&self, _: &ExecutionContext<'_>, _: &DesiredUnit) -> DriverResult<Supported<Outcome>> {
        Ok(Supported::Unsupported)
    }
    fn verify(&self, _: &ExecutionContext<'_>, _: &DesiredUnit, _: &Receipt) -> DriverResult<Supported<Verification>> {
        Ok(Supported::Unsupported)
    }
}
