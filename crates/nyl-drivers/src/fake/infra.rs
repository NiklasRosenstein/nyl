//! `FakeInfra`, standing in for `OpenTofu` and `Terraform`: keeps its
//! resources and outputs in a JSON file per backend path, reports plan and
//! destroy digests for `bind: plan`, and tears down.
//!
//! Outputs derive from the unit name and its `variables` only, never from
//! source bytes, so a comment-only module change re-executes the unit and
//! leaves its outputs, and therefore its consumers, unchanged, as with a real
//! module that derives outputs from its inputs.

use std::ffi::OsString;
use std::path::PathBuf;

use nyl_core::bindings::InputType;
use nyl_core::orchestration::driver::{
    Capabilities, Change, Driver, DriverError, DriverOperation, DriverResult, ExecStatus, ExecutionContext, Inspection,
    KeyInputs, Outcome, OwnershipScope, PlanReport, RecoveryPolicies, RecoveryPolicy, ResolutionContext, Supported,
    Verification,
};
use nyl_core::orchestration::ids::{ChangeDigest, EnvironmentName};
use nyl_core::orchestration::state::{ArtifactDocument, DesiredUnit, Drift, Receipt};
use nyl_core::resources::units::OutputDeclaration;
use serde_json::{json, Map, Value};

use super::{required_str, resolved_spec, source_files, Script, Scripted, ScriptedOutcome};

pub(super) const KIND: &str = "FakeInfra";

/// The fake infrastructure kind.
#[derive(Debug, Clone, Default)]
pub struct FakeInfra {
    script: Script,
}

/// What a planned execution or teardown would change.
struct Planned {
    changes: Change,
    digest: ChangeDigest,
    after: Option<Value>,
}

impl FakeInfra {
    pub fn new(script: Script) -> Self {
        Self { script }
    }

    fn backend(spec: &Value) -> DriverResult<PathBuf> {
        Ok(PathBuf::from(required_str(spec, "/backend/path")?))
    }

    fn read_state(spec: &Value) -> DriverResult<Option<Value>> {
        let path = Self::backend(spec)?;
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|error| {
                DriverError::before_effects(format!("backend {} is not valid JSON: {error}", path.display()))
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(DriverError::before_effects(format!(
                "cannot read backend {}: {error}",
                path.display()
            ))),
        }
    }

    fn variables(spec: &Value) -> Value {
        spec.get("variables").cloned().unwrap_or_else(|| json!({}))
    }

    fn plan_change(unit: &DesiredUnit, spec: &Value) -> DriverResult<Planned> {
        let state = Self::read_state(spec)?;
        let before = state.as_ref().and_then(|state| state.get("inputs").cloned());
        let teardown = crate::command::is_deleting(unit);
        let (changes, after) = if teardown {
            (
                if before.is_some() {
                    Change::Destroy
                } else {
                    Change::None
                },
                None,
            )
        } else {
            let after = Self::variables(spec);
            let changes = match &before {
                None => Change::Create,
                Some(before) if *before == after => Change::None,
                Some(_) => Change::Update,
            };
            (changes, Some(after))
        };
        let document = json!({"unit": unit.unit.name, "before": before, "after": after, "destroy": teardown});
        let digest = ChangeDigest::from_digest_of(
            &nyl_core::digest::canonical_json_bytes(&document)
                .map_err(|error| DriverError::before_effects(error.to_string()))?,
        );
        Ok(Planned { changes, digest, after })
    }

    /// Deterministic values for the declared, non-sensitive outputs.
    pub(super) fn outputs(unit: &DesiredUnit, spec: &Value) -> DriverResult<Value> {
        let declared: std::collections::BTreeMap<String, OutputDeclaration> =
            serde_json::from_value(spec.get("outputs").cloned().unwrap_or_else(|| json!({})))
                .map_err(|error| DriverError::before_effects(format!("spec.outputs: {error}")))?;
        let inputs = nyl_core::digest::canonical_json_bytes(&Self::variables(spec))
            .map_err(|error| DriverError::before_effects(error.to_string()))?;
        let mut outputs = Map::new();
        for (name, declaration) in declared {
            if declaration.sensitive {
                continue;
            }
            let seed = nyl_core::digest::sha256_hex(
                &[unit.unit.name.as_str().as_bytes(), b"/", name.as_bytes(), b"/", &inputs].concat(),
            );
            let short = &seed[..12];
            let value = match declaration.output_type {
                InputType::String => json!(format!("{}-{name}-{short}", unit.unit.name)),
                InputType::Integer => json!(u64::from_str_radix(&seed[..4], 16).unwrap_or_default()),
                InputType::Number => json!(f64::from(u16::from_str_radix(&seed[..4], 16).unwrap_or_default()) / 4.0),
                InputType::Boolean => json!(seed.as_bytes()[0].is_multiple_of(2)),
                InputType::Object => json!({"host": format!("{}.{short}.internal", unit.unit.name), "port": 5432}),
                InputType::Array => json!([short]),
            };
            outputs.insert(name, value);
        }
        Ok(Value::Object(outputs))
    }

    fn awaiting_approval(ctx: &ExecutionContext<'_>, planned: &Planned) -> Option<Outcome> {
        ctx.approved_digest
            .as_ref()
            .filter(|approved| **approved != planned.digest)
            .map(|_| Outcome::AwaitingApproval {
                digest: planned.digest.clone(),
            })
    }
}

impl Driver for FakeInfra {
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
            teardown: true,
            change_digest: true,
            ..Capabilities::default()
        }
    }

    fn recovery(&self, _spec: &Value) -> RecoveryPolicies {
        RecoveryPolicies {
            execution: self.script.execution_policy(RecoveryPolicy::Converge),
            teardown: RecoveryPolicy::Converge,
        }
    }

    fn spec_schema(&self) -> schemars::Schema {
        schemars::json_schema!({
            "type": "object",
            "required": ["source", "backend"],
            "properties": {
                "source": {"type": "object", "required": ["path"]},
                "backend": {"type": "object", "required": ["path"]},
                "variables": {"type": "object"}
            }
        })
    }

    fn validate_spec(&self, spec: &Value) -> Result<(), String> {
        if spec.pointer("/source/path").and_then(Value::as_str).is_none() {
            return Err("spec.source.path is required".to_owned());
        }
        if spec.pointer("/backend/path").and_then(Value::as_str).is_none() {
            return Err("spec.backend.path is required".to_owned());
        }
        if spec.get("variables").is_some_and(|variables| !variables.is_object()) {
            return Err("spec.variables must be an object".to_owned());
        }
        Ok(())
    }

    fn attestations(&self, _spec: &Value) -> Vec<String> {
        Vec::new()
    }

    fn source_fields(&self, _spec: &Value) -> Vec<String> {
        vec!["/source".to_owned()]
    }

    fn ownership_scopes(&self, _environment: &EnvironmentName, spec: &Value) -> Vec<OwnershipScope> {
        spec.pointer("/backend/path")
            .and_then(Value::as_str)
            .map(|path| OwnershipScope::TerraformBackend {
                backend: json!({"local": {"path": path}}).to_string(),
                workspace: "default".to_owned(),
            })
            .into_iter()
            .collect()
    }

    fn execution_key_inputs(&self, ctx: &ResolutionContext<'_>, spec: &Value) -> DriverResult<KeyInputs> {
        Ok(KeyInputs {
            files: source_files(ctx, spec, "/source/path", "spec.source.path")?,
            excluded_fields: Vec::new(),
            fingerprint: None,
        })
    }

    fn plan(&self, _ctx: &ExecutionContext<'_>, unit: &DesiredUnit) -> DriverResult<Supported<PlanReport>> {
        let planned = Self::plan_change(unit, resolved_spec(unit)?)?;
        Ok(Supported::Supported(PlanReport {
            changes: planned.changes,
            summary: format!("{:?}", planned.changes).to_lowercase(),
            change_digest: Some(planned.digest),
            blocked_by: Vec::new(),
        }))
    }

    fn reconcile(&self, ctx: &ExecutionContext<'_>, unit: &DesiredUnit) -> DriverResult<Supported<Outcome>> {
        let scripted_outputs = match self.script.outcome(&unit.unit.name, DriverOperation::Reconcile) {
            ScriptedOutcome::Return(outcome) => return Ok(Supported::Supported(outcome)),
            ScriptedOutcome::Run { outputs } => outputs,
        };
        let spec = resolved_spec(unit)?;
        let planned = Self::plan_change(unit, spec)?;
        if let Some(waiting) = Self::awaiting_approval(ctx, &planned) {
            return Ok(Supported::Supported(waiting));
        }
        let outputs = match scripted_outputs {
            Some(outputs) => outputs,
            None => Self::outputs(unit, spec)?,
        };
        if planned.changes != Change::None {
            let path = Self::backend(spec)?;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|error| DriverError::before_effects(error.to_string()))?;
            }
            let state = json!({"inputs": planned.after, "outputs": outputs});
            std::fs::write(&path, serde_json::to_vec_pretty(&state).expect("JSON value serializes"))
                .map_err(|error| DriverError::after_effects(format!("cannot write {}: {error}", path.display())))?;
        }
        Ok(Supported::Supported(Outcome::Succeeded {
            outputs,
            artifacts: Vec::new(),
            attestations: Vec::new(),
        }))
    }

    fn verify(
        &self,
        _ctx: &ExecutionContext<'_>,
        unit: &DesiredUnit,
        _receipt: &Receipt,
    ) -> DriverResult<Supported<Verification>> {
        let drift = match self.script.verification(&unit.unit.name)? {
            Some(drift) => drift,
            None => match Self::plan_change(unit, resolved_spec(unit)?)?.changes {
                Change::None => Drift::Clean,
                changes => Drift::Drifted {
                    summary: format!("applying would {changes:?}").to_lowercase(),
                },
            },
        };
        Ok(Supported::Supported(Verification {
            drift,
            observations: Vec::new(),
            attestations: Vec::new(),
        }))
    }

    /// Only scripted: a scripted success is applied (with its outputs, or
    /// the derived ones), a failure not applied, and uncertain unknown.
    fn inspect(&self, _ctx: &ExecutionContext<'_>, unit: &DesiredUnit) -> DriverResult<Supported<Inspection>> {
        let inspection = match self.script.take(&unit.unit.name, DriverOperation::Inspect) {
            None => return Ok(Supported::Unsupported),
            Some(Scripted::Succeeded { outputs }) => Inspection::Applied {
                outputs: match outputs {
                    Some(outputs) => outputs,
                    None => Self::outputs(unit, resolved_spec(unit)?)?,
                },
                artifacts: Vec::new(),
            },
            // A drift verdict scripts `verify`, never `inspect`.
            Some(Scripted::Failed { .. } | Scripted::Drifted) => Inspection::NotApplied,
            Some(Scripted::Uncertain) => Inspection::Unknown {
                reason: "scripted unknown inspect".to_owned(),
            },
        };
        Ok(Supported::Supported(inspection))
    }

    fn teardown(&self, ctx: &ExecutionContext<'_>, unit: &DesiredUnit) -> DriverResult<Supported<Outcome>> {
        if let ScriptedOutcome::Return(outcome) = self.script.outcome(&unit.unit.name, DriverOperation::Teardown) {
            return Ok(Supported::Supported(outcome));
        }
        let spec = resolved_spec(unit)?;
        let planned = Self::plan_change(unit, spec)?;
        if let Some(waiting) = Self::awaiting_approval(ctx, &planned) {
            return Ok(Supported::Supported(waiting));
        }
        let path = Self::backend(spec)?;
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(DriverError::after_effects(format!(
                    "cannot remove {}: {error}",
                    path.display()
                )))
            }
        }
        Ok(Supported::Supported(Outcome::Succeeded {
            outputs: json!({}),
            artifacts: Vec::new(),
            attestations: Vec::new(),
        }))
    }

    fn build(
        &self,
        _ctx: &ExecutionContext<'_>,
        _unit: &DesiredUnit,
        _push: bool,
    ) -> DriverResult<Supported<Vec<ArtifactDocument>>> {
        Ok(Supported::Unsupported)
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
