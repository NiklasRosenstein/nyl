//! `FakePublication`, standing in for `KubernetesPublication` in M3: renders
//! the unit's DeploymentTarget with the resolved Release inputs as a pinned
//! input snapshot, publishes the tree with its ownership index to the
//! target's publication branch, and records a `PublishedTree` artifact named
//! `tree`.
//!
//! Contract: [Kubernetes publication unit](../../../../design/orchestration-core.md#kubernetes-publication-unit)
//! and [Artifacts](../../../../design/orchestration-core.md#artifacts);
//! [Fake kinds](../../../../design/reference-scenarios.md#fake-kinds).
//! Rendering and publishing go through `nyl_render::gitops::publication`, the
//! narrow API the real driver will use in M5.
//!
//! After resolution the spec is `{target, mode: publish|observe, releases:
//! {"<group>/<release>": {<input>: <value>}}}`. The execution key's inputs
//! are the render's portable input record, so a change to any file the
//! target reads, or to a resolved Release input, re-executes the unit.

use std::collections::BTreeMap;
use std::ffi::OsString;

use nyl_core::constants::API_VERSION_ARTIFACTS;
use nyl_core::orchestration::driver::{
    Capabilities, Change, Driver, DriverError, DriverOperation, DriverResult, ExecStatus, ExecutionContext, Inspection,
    KeyInputs, Outcome, OwnershipScope, PlanReport, RecoveryPolicies, RecoveryPolicy, ResolutionContext, Supported,
    Verification,
};
use nyl_core::orchestration::ids::EnvironmentName;
use nyl_core::orchestration::state::{ArtifactDocument, ArtifactMetadata, ArtifactUnit, DesiredUnit, Drift, Receipt};
use nyl_render::gitops::inputs::InputSnapshot;
use nyl_render::gitops::publication::{
    prepare_target_publication, render_target, PublicationSignature, RenderedTarget, TargetRender,
};
use serde_json::{json, Value};

use super::{required_str, resolved_spec, Script, ScriptedOutcome};

pub(super) const KIND: &str = "FakePublication";

/// The fake publication kind.
#[derive(Debug, Clone, Default)]
pub struct FakePublication {
    script: Script,
}

impl FakePublication {
    pub fn new(script: Script) -> Self {
        Self { script }
    }

    /// The resolved `spec.releases` as a pinned input snapshot.
    fn snapshot(spec: &Value) -> DriverResult<InputSnapshot> {
        let Some(releases) = spec.get("releases") else {
            return Ok(InputSnapshot::new());
        };
        serde_json::from_value::<BTreeMap<String, BTreeMap<String, Value>>>(releases.clone())
            .map_err(|error| DriverError::before_effects(format!("spec.releases: {error}")))
    }

    /// A render whose input record does not cover it executes every time,
    /// as the contract requires of an uncacheable render: its key carries
    /// the resolution time, which no later resolution repeats.
    fn uncacheable_fingerprint(cacheable: bool, now: nyl_core::orchestration::time::Timestamp) -> Option<String> {
        (!cacheable).then(|| format!("uncacheable render resolved at {}", now.to_rfc3339()))
    }

    fn render(
        worktree: &std::path::Path,
        project_dir: &std::path::Path,
        cache_dir: &std::path::Path,
        spec: &Value,
    ) -> DriverResult<RenderedTarget> {
        let target = required_str(spec, "/target")?;
        let inputs = Self::snapshot(spec)?;
        render_target(&TargetRender {
            worktree,
            project_dir,
            target,
            inputs: &inputs,
            cache_dir: Some(cache_dir),
        })
        .map_err(|error| DriverError::before_effects(format!("cannot render DeploymentTarget {target:?}: {error}")))
    }
}

impl Driver for FakePublication {
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
            ..Capabilities::default()
        }
    }

    fn recovery(&self, _spec: &Value) -> RecoveryPolicies {
        // Publishing the same tree again is a no-op, and a stale branch is
        // refused by compare-and-swap, so re-executing always converges.
        RecoveryPolicies {
            execution: RecoveryPolicy::Converge,
            teardown: RecoveryPolicy::Converge,
        }
    }

    fn spec_schema(&self) -> schemars::Schema {
        schemars::json_schema!({
            "type": "object",
            "required": ["target"],
            "properties": {
                "target": {"type": "string"},
                "mode": {"type": "string", "enum": ["publish", "observe"]},
                "releases": {
                    "type": "object",
                    "additionalProperties": {"type": "object"}
                }
            }
        })
    }

    fn validate_spec(&self, spec: &Value) -> Result<(), String> {
        if !spec.is_object() {
            return Err("spec must be a mapping".to_owned());
        }
        if spec.get("target").and_then(Value::as_str).is_none() {
            return Err("spec.target is required and must be a string".to_owned());
        }
        match spec.get("mode") {
            None => {}
            Some(Value::String(mode)) if mode == "publish" || mode == "observe" => {}
            Some(other) => return Err(format!("spec.mode must be publish or observe, got {other}")),
        }
        if let Some(releases) = spec.get("releases") {
            let releases = releases
                .as_object()
                .ok_or_else(|| "spec.releases must be a mapping of <group>/<release> keys".to_owned())?;
            for (key, inputs) in releases {
                if !inputs.is_object() {
                    return Err(format!("spec.releases.{key} must be a mapping of input names"));
                }
            }
        }
        Ok(())
    }

    fn attestations(&self, _spec: &Value) -> Vec<String> {
        Vec::new()
    }

    /// None: the publication destination (repository, revision, and path
    /// prefix) belongs to the DeploymentTarget, which the spec names but
    /// does not contain, and claims are computed from the spec alone. Two
    /// targets sharing one destination are already refused by the rendered
    /// GitOps rules, and the ownership index refuses a tree owned by another
    /// target at publication.
    fn ownership_scopes(&self, _environment: &EnvironmentName, _spec: &Value) -> Vec<OwnershipScope> {
        Vec::new()
    }

    fn execution_key_inputs(&self, ctx: &ResolutionContext<'_>, spec: &Value) -> DriverResult<KeyInputs> {
        let rendered = Self::render(&ctx.worktree, &ctx.project_dir, &ctx.cache_dir, spec)?;
        Ok(KeyInputs {
            files: rendered.input_record,
            excluded_fields: Vec::new(),
            fingerprint: Self::uncacheable_fingerprint(rendered.cacheable, ctx.now),
        })
    }

    fn plan(&self, ctx: &ExecutionContext<'_>, unit: &DesiredUnit) -> DriverResult<Supported<PlanReport>> {
        let receipt = ctx.observed.and_then(|observed| observed.receipt.as_ref());
        let current = receipt.is_some_and(|receipt| Some(&receipt.execution_key) == unit.execution_key.as_ref());
        let (changes, summary) = match receipt {
            _ if current => (Change::None, "published tree is current"),
            Some(_) => (Change::Update, "publish a changed tree"),
            None => (Change::Create, "publish the tree"),
        };
        Ok(Supported::Supported(PlanReport {
            changes,
            summary: summary.to_owned(),
            change_digest: None,
            blocked_by: Vec::new(),
        }))
    }

    fn reconcile(&self, ctx: &ExecutionContext<'_>, unit: &DesiredUnit) -> DriverResult<Supported<Outcome>> {
        let outputs = match self.script.outcome(&unit.unit.name, DriverOperation::Reconcile) {
            ScriptedOutcome::Return(outcome) => return Ok(Supported::Supported(outcome)),
            ScriptedOutcome::Run { outputs } => outputs,
        };
        let identity = ctx.commit_identity.ok_or_else(|| {
            DriverError::before_effects(format!(
                "unit {} publishes a commit and needs a commit identity",
                unit.unit.name
            ))
        })?;
        let key = unit
            .execution_key
            .as_ref()
            .ok_or_else(|| DriverError::before_effects("an executing unit has an execution key"))?;
        let uid = unit
            .unit
            .uid
            .clone()
            .ok_or_else(|| DriverError::before_effects("an executing unit has a uid"))?;
        let spec = resolved_spec(unit)?;
        let rendered = Self::render(&ctx.worktree, &ctx.project_dir, &ctx.cache_dir, spec)?;
        let message = format!("nyl: publish {}/{}", unit.unit.environment, unit.unit.name);
        let signature = PublicationSignature {
            identity: identity.clone(),
            time: ctx.now,
        };
        // Credential variables come from the execution's admitted
        // environment (PATH and the unit's declared passthrough), never from
        // the process environment.
        let variable = |name: &str| ctx.env.get(name).cloned();
        let prepared = prepare_target_publication(&rendered, &ctx.project_dir, &message, &signature, &variable)
            .map_err(|error| DriverError::before_effects(format!("cannot prepare the publication: {error}")))?;
        let published = prepared
            .push()
            .map_err(|error| DriverError::after_effects(format!("cannot publish the tree: {error}")))?;
        Ok(Supported::Supported(Outcome::Succeeded {
            outputs: outputs.unwrap_or_else(|| json!({})),
            artifacts: vec![ArtifactDocument {
                api_version: API_VERSION_ARTIFACTS.to_owned(),
                kind: "PublishedTree".to_owned(),
                metadata: ArtifactMetadata {
                    name: "tree".to_owned(),
                    unit: ArtifactUnit {
                        environment: unit.unit.environment.clone(),
                        name: unit.unit.name.clone(),
                        uid,
                    },
                },
                execution_key: key.clone(),
                spec: json!({
                    "repository": published.repository,
                    "branch": published.branch,
                    "pathPrefix": published.path_prefix,
                    "commit": published.commit,
                    "indexDigest": published.index_digest,
                }),
            }],
            attestations: Vec::new(),
        }))
    }

    fn verify(
        &self,
        _ctx: &ExecutionContext<'_>,
        _unit: &DesiredUnit,
        _receipt: &Receipt,
    ) -> DriverResult<Supported<Verification>> {
        Ok(Supported::Supported(Verification {
            drift: Drift::Clean,
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_validate_spec_checks_field_types() {
        let driver = FakePublication::default();
        assert!(driver
            .validate_spec(&json!({"target": "dev", "mode": "publish", "releases": {"web/web": {"image": "x"}}}))
            .is_ok());
        assert!(driver.validate_spec(&json!({"mode": "publish"})).is_err());
        assert!(driver.validate_spec(&json!({"target": 1})).is_err());
        assert!(driver
            .validate_spec(&json!({"target": "dev", "mode": "apply"}))
            .is_err());
        assert!(driver.validate_spec(&json!({"target": "dev", "releases": []})).is_err());
        assert!(driver
            .validate_spec(&json!({"target": "dev", "releases": {"web/web": "x"}}))
            .is_err());
    }

    #[test]
    fn test_uncacheable_render_never_repeats_its_key() {
        use chrono::TimeZone;
        let (first, second) = (
            chrono::Utc.timestamp_opt(1, 0).unwrap(),
            chrono::Utc.timestamp_opt(2, 0).unwrap(),
        );
        assert_eq!(FakePublication::uncacheable_fingerprint(true, first), None);
        let one = FakePublication::uncacheable_fingerprint(false, first).unwrap();
        let two = FakePublication::uncacheable_fingerprint(false, second).unwrap();
        assert_ne!(one, two);
    }

    #[test]
    fn test_snapshot_reads_resolved_releases() {
        let snapshot =
            FakePublication::snapshot(&json!({"target": "dev", "releases": {"web/web": {"image": "x"}}})).unwrap();
        assert_eq!(snapshot["web/web"]["image"], json!("x"));
        assert!(FakePublication::snapshot(&json!({"target": "dev"})).unwrap().is_empty());
    }
}
