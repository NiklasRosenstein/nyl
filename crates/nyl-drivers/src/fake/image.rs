//! `FakeImage`, standing in for `OciImage`: returns a digest derived from the
//! execution key and publishes a `ContainerImage` artifact named `image`.

use std::ffi::OsString;

use nyl_core::constants::API_VERSION_ARTIFACTS;
use nyl_core::orchestration::driver::{
    Capabilities, Change, Driver, DriverError, DriverOperation, DriverResult, ExecStatus, ExecutionContext, Inspection,
    KeyInputs, Outcome, OwnershipScope, PlanReport, RecoveryPolicies, RecoveryPolicy, ResolutionContext, Supported,
    Verification,
};
use nyl_core::orchestration::ids::{EnvironmentName, ExecutionKey};
use nyl_core::orchestration::state::{ArtifactDocument, ArtifactMetadata, ArtifactUnit, DesiredUnit, Drift, Receipt};
use serde_json::{json, Value};

use super::{required_str, resolved_spec, source_files, Script, ScriptedOutcome};

pub(super) const KIND: &str = "FakeImage";

/// The fake image kind.
#[derive(Debug, Clone, Default)]
pub struct FakeImage {
    script: Script,
}

impl FakeImage {
    pub fn new(script: Script) -> Self {
        Self { script }
    }

    fn artifact(unit: &DesiredUnit, key: &ExecutionKey, spec: &Value) -> DriverResult<ArtifactDocument> {
        let repository = required_str(spec, "/repository")?;
        let digest = format!(
            "sha256:{}",
            nyl_core::digest::sha256_hex(format!("image:{key}").as_bytes())
        );
        let hex = key.as_str().trim_start_matches("sha256:");
        let mut tags = vec![format!("nyl-{}-{}", unit.unit.environment, &hex[..16])];
        if let Some(extra) = spec.get("tags").and_then(Value::as_array) {
            tags.extend(extra.iter().filter_map(Value::as_str).map(str::to_owned));
        }
        let uid = unit
            .unit
            .uid
            .clone()
            .ok_or_else(|| DriverError::before_effects("an executing unit has a uid"))?;
        Ok(ArtifactDocument {
            api_version: API_VERSION_ARTIFACTS.to_owned(),
            kind: "ContainerImage".to_owned(),
            metadata: ArtifactMetadata {
                name: "image".to_owned(),
                unit: ArtifactUnit {
                    environment: unit.unit.environment.clone(),
                    name: unit.unit.name.clone(),
                    uid,
                },
            },
            execution_key: key.clone(),
            spec: json!({
                "repository": repository,
                "digest": digest,
                "reference": format!("{repository}@{digest}"),
                "platforms": spec.get("platforms").cloned().unwrap_or_else(|| json!(["linux/amd64"])),
                "tags": tags,
            }),
        })
    }
}

impl Driver for FakeImage {
    fn api_version(&self) -> &'static str {
        super::API_VERSION_TEST_UNITS
    }

    fn kind(&self) -> &'static str {
        KIND
    }

    fn behavior_version(&self) -> u32 {
        1
    }

    fn capabilities(&self, _spec: &Value) -> Capabilities {
        Capabilities {
            plan: true,
            reconcile: true,
            verify: true,
            build: true,
            ..Capabilities::default()
        }
    }

    fn recovery(&self, _spec: &Value) -> RecoveryPolicies {
        RecoveryPolicies {
            execution: RecoveryPolicy::Converge,
            teardown: RecoveryPolicy::Converge,
        }
    }

    fn spec_schema(&self) -> schemars::Schema {
        schemars::json_schema!({
            "type": "object",
            "required": ["context", "repository"],
            "properties": {
                "context": {"type": "object", "required": ["path"]},
                "repository": {"type": "string"}
            }
        })
    }

    fn validate_spec(&self, spec: &Value) -> Result<(), String> {
        if spec.pointer("/context/path").and_then(Value::as_str).is_none() {
            return Err("spec.context.path is required".to_owned());
        }
        if spec.get("repository").and_then(Value::as_str).is_none() {
            return Err("spec.repository is required".to_owned());
        }
        if spec.get("outputs").is_some() {
            return Err("spec.outputs is not allowed: this kind publishes the image artifact".to_owned());
        }
        Ok(())
    }

    fn attestations(&self, _spec: &Value) -> Vec<String> {
        Vec::new()
    }

    fn source_fields(&self, _spec: &Value) -> Vec<String> {
        vec!["/context".to_owned()]
    }

    fn ownership_scopes(&self, environment: &EnvironmentName, spec: &Value) -> Vec<OwnershipScope> {
        spec.get("repository")
            .and_then(Value::as_str)
            .map(|repository| OwnershipScope::OciRepository {
                repository: repository.to_owned(),
                tag_prefix: format!("nyl-{environment}-"),
            })
            .into_iter()
            .collect()
    }

    fn execution_key_inputs(&self, ctx: &ResolutionContext<'_>, spec: &Value) -> DriverResult<KeyInputs> {
        Ok(KeyInputs {
            files: source_files(ctx, spec, "/context/path", "spec.context.path")?,
            excluded_fields: vec!["/builder".to_owned(), "/cache".to_owned()],
            fingerprint: None,
        })
    }

    fn plan(&self, ctx: &ExecutionContext<'_>, unit: &DesiredUnit) -> DriverResult<Supported<PlanReport>> {
        let current = ctx
            .observed
            .and_then(|observed| observed.receipt.as_ref())
            .is_some_and(|receipt| Some(&receipt.execution_key) == unit.execution_key.as_ref());
        Ok(Supported::Supported(PlanReport {
            changes: if current { Change::None } else { Change::Create },
            summary: if current {
                "image is current".to_owned()
            } else {
                "build and push a new image".to_owned()
            },
            change_digest: None,
            blocked_by: Vec::new(),
        }))
    }

    fn reconcile(&self, _ctx: &ExecutionContext<'_>, unit: &DesiredUnit) -> DriverResult<Supported<Outcome>> {
        if let ScriptedOutcome::Return(outcome) = self.script.outcome(&unit.unit.name, DriverOperation::Reconcile) {
            return Ok(Supported::Supported(outcome));
        }
        let key = unit
            .execution_key
            .as_ref()
            .ok_or_else(|| DriverError::before_effects("an executing unit has an execution key"))?;
        let artifact = Self::artifact(unit, key, resolved_spec(unit)?)?;
        Ok(Supported::Supported(Outcome::Succeeded {
            versions: std::collections::BTreeMap::new(),
            outputs: json!({}),
            artifacts: vec![artifact],
            attestations: Vec::new(),
        }))
    }

    fn verify(
        &self,
        _ctx: &ExecutionContext<'_>,
        unit: &DesiredUnit,
        _receipt: &Receipt,
    ) -> DriverResult<Supported<Verification>> {
        // The fake registry never changes, so only a script makes it drift.
        Ok(Supported::Supported(Verification {
            drift: self.script.verification(&unit.unit.name)?.unwrap_or(Drift::Clean),
            observations: Vec::new(),
            attestations: Vec::new(),
        }))
    }

    fn inspect(&self, _ctx: &ExecutionContext<'_>, _unit: &DesiredUnit) -> DriverResult<Supported<Inspection>> {
        Ok(Supported::Unsupported)
    }

    fn teardown(&self, _ctx: &ExecutionContext<'_>, _unit: &DesiredUnit) -> DriverResult<Supported<Outcome>> {
        Ok(Supported::Unsupported)
    }

    fn build(
        &self,
        _ctx: &ExecutionContext<'_>,
        unit: &DesiredUnit,
        _args: &[OsString],
    ) -> DriverResult<Supported<Vec<ArtifactDocument>>> {
        let spec = resolved_spec(unit)?;
        let key = ExecutionKey::from_digest_of(
            &nyl_core::digest::canonical_json_bytes(spec)
                .map_err(|error| DriverError::before_effects(error.to_string()))?,
        );
        Ok(Supported::Supported(vec![Self::artifact(unit, &key, spec)?]))
    }

    fn exec(
        &self,
        _ctx: &ExecutionContext<'_>,
        _unit: &DesiredUnit,
        _args: &[OsString],
    ) -> DriverResult<Supported<ExecStatus>> {
        Ok(Supported::Unsupported)
    }
}
