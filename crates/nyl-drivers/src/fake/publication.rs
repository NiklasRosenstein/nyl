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
//! After resolution the spec is `{target, mode: publish|observe,
//! teardownWait?, teardownSettings?, releases: {"<group>/<release>":
//! {<input>: <value>}}}`. The execution key's inputs are the render's
//! portable input record, so a change to any file the target reads, or to a
//! resolved Release input, re-executes the unit.
//!
//! Teardown runs the contract's two phases through Git, each a
//! compare-and-swap publication of the target's prefix:
//!
//! 1. Without a wait completion and without a recorded wait, publish the
//!    tree without its workload Applications
//!    ([`without_workload_applications`]) and report `Outcome::Waiting` with
//!    the phase 1 commit; with a recorded wait, report the same wait again
//!    and publish nothing.
//! 2. Once the run passes the wait's completion
//!    (`ExecutionContext::teardown_wait`), remove exactly the files the
//!    ownership index records ([`prepare_target_removal`]).
//!
//! When the wait ends is the orchestration shell's decision
//! (`rules::teardown_wait`), read from `teardownWait`, and the fake Argo CD
//! observer ([`super::FakeObserver`]) answers its `observe` strategy.
//!
//! Teardown readiness comes from the stored desired document alone: the
//! KubernetesPublication driver (M5) records the target's effective
//! settings at resolution, and the fake reads the same shape from
//! `teardownSettings` (`catalogPrunes`, `applicationDeletionPolicy`,
//! `namespaceDeletePolicies`, `catalogOwnedByParent`), which a scenario
//! declares. A document without it records no settings, so the fake
//! reports readiness as unsupported (no requirements).

use std::collections::BTreeMap;
use std::ffi::OsString;

use nyl_core::constants::API_VERSION_ARTIFACTS;
use nyl_core::orchestration::driver::{
    Capabilities, Change, Driver, DriverError, DriverOperation, DriverResult, ExecStatus, ExecutionContext, Inspection,
    KeyInputs, Outcome, OwnershipScope, PlanReport, RecoveryPolicies, RecoveryPolicy, ResolutionContext, Supported,
    TeardownSettings, Verification,
};
use nyl_core::orchestration::ids::{CommitId, EnvironmentName};
use nyl_core::orchestration::ports::CommitIdentity;
use nyl_core::orchestration::state::{ArtifactDocument, ArtifactMetadata, ArtifactUnit, DesiredUnit, Drift, Receipt};
use nyl_core::orchestration::time::Duration;
use nyl_render::gitops::inputs::InputSnapshot;
use nyl_render::gitops::publication::{
    prepare_target_publication, prepare_target_removal, render_target, without_workload_applications,
    PublicationAuthor, RenderedTarget, TargetRender,
};
use serde_json::{json, Value};

use super::{required_str, resolved_spec, Script, ScriptedOutcome};

pub(super) const KIND: &str = "FakePublication";

/// The phase a teardown waits in after publishing phase 1.
const PHASE_APPLICATIONS_REMOVED: &str = "applications-removed";

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

    /// The commit author a driver call that writes needs, or a failure
    /// before effects without one.
    fn author(ctx: &ExecutionContext<'_>, unit: &DesiredUnit) -> DriverResult<PublicationAuthor> {
        let identity: &CommitIdentity = ctx.commit_identity.ok_or_else(|| {
            DriverError::before_effects(format!(
                "unit {} publishes a commit and needs a commit identity",
                unit.unit.name
            ))
        })?;
        Ok(PublicationAuthor {
            identity: identity.clone(),
            time: ctx.now,
        })
    }

    /// `teardownWait`: `{strategy: observe|delay|manual, duration?}`, where
    /// `delay` requires a `duration` like `10m` and no other strategy takes one.
    fn validate_teardown_wait(value: &Value) -> Result<(), String> {
        let wait = value
            .as_object()
            .ok_or_else(|| "spec.teardownWait must be a mapping".to_owned())?;
        if let Some(key) = wait.keys().find(|key| !matches!(key.as_str(), "strategy" | "duration")) {
            return Err(format!(
                "spec.teardownWait has unknown field {key:?}; it has strategy and duration"
            ));
        }
        let strategy = wait.get("strategy").and_then(Value::as_str);
        match (strategy, wait.get("duration")) {
            (Some("delay"), Some(Value::String(duration))) => duration
                .parse::<Duration>()
                .map(drop)
                .map_err(|error| format!("spec.teardownWait.duration: {error}")),
            (Some("delay"), _) => {
                Err("spec.teardownWait.duration is required for strategy delay, such as 10m".to_owned())
            }
            (Some("observe" | "manual"), None) => Ok(()),
            (Some(strategy @ ("observe" | "manual")), Some(_)) => Err(format!(
                "spec.teardownWait.duration applies only to strategy delay, not {strategy}"
            )),
            _ => Err("spec.teardownWait.strategy must be observe, delay, or manual".to_owned()),
        }
    }

    /// The recorded `teardownSettings`, with absent fields at their
    /// defaults: an absent catalog setting is external (`None`).
    fn recorded_settings(value: &Value) -> Result<TeardownSettings, String> {
        let mut settings = serde_json::to_value(TeardownSettings::default()).map_err(|error| error.to_string())?;
        let (Value::Object(defaults), Value::Object(recorded)) = (&mut settings, value) else {
            return Err("spec.teardownSettings must be a mapping".to_owned());
        };
        if let Some(key) = recorded.keys().find(|key| !defaults.contains_key(*key)) {
            return Err(format!("spec.teardownSettings has unknown field {key:?}"));
        }
        defaults.extend(recorded.clone());
        serde_json::from_value(settings).map_err(|error| format!("spec.teardownSettings: {error}"))
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
            teardown: true,
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
                "teardownWait": {
                    "type": "object",
                    "required": ["strategy"],
                    "properties": {
                        "strategy": {"type": "string", "enum": ["observe", "delay", "manual"]},
                        "duration": {"type": "string"}
                    },
                    "additionalProperties": false
                },
                "teardownSettings": {
                    "type": "object",
                    "properties": {
                        "catalogPrunes": {"type": ["boolean", "null"]},
                        "applicationDeletionPolicy": {"type": ["string", "null"]},
                        "namespaceDeletePolicies": {
                            "type": "object",
                            "additionalProperties": {"type": "string", "enum": ["Automatic", "Retain", "Confirm"]}
                        },
                        "catalogOwnedByParent": {"type": "boolean"}
                    },
                    "additionalProperties": false
                },
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
        if let Some(wait) = spec.get("teardownWait") {
            Self::validate_teardown_wait(wait)?;
        }
        if let Some(settings) = spec.get("teardownSettings") {
            Self::recorded_settings(settings)?;
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
        if crate::command::is_deleting(unit) {
            let waiting = ctx.observed.and_then(|observed| observed.waiting.as_ref());
            let summary = match waiting {
                Some(waiting) => format!(
                    "teardown waits after phase 1 ({}); then remove the files the ownership index records",
                    waiting.phase1_commit.as_ref().map_or("unpublished", CommitId::as_str)
                ),
                None => "teardown in two phases: publish the catalog without workload Applications, then, once teardownWait ends, remove the files the ownership index records".to_owned(),
            };
            return Ok(Supported::Supported(PlanReport {
                changes: Change::Destroy,
                summary,
                change_digest: None,
                blocked_by: Vec::new(),
            }));
        }
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
        let author = Self::author(ctx, unit)?;
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
        // Credential variables come from the execution's admitted
        // environment (PATH and the unit's declared passthrough), never from
        // the process environment.
        let variable = |name: &str| ctx.env.get(name).cloned();
        let prepared = prepare_target_publication(&rendered, &ctx.project_dir, &message, &author, &variable)
            .map_err(|error| DriverError::before_effects(format!("cannot prepare the publication: {error}")))?;
        let published = prepared
            .push()
            .map_err(|error| DriverError::after_effects(format!("cannot publish the tree: {error}")))?;
        Ok(Supported::Supported(Outcome::Succeeded {
            versions: std::collections::BTreeMap::new(),
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

    fn teardown(&self, ctx: &ExecutionContext<'_>, unit: &DesiredUnit) -> DriverResult<Supported<Outcome>> {
        if let ScriptedOutcome::Return(outcome) = self.script.outcome(&unit.unit.name, DriverOperation::Teardown) {
            return Ok(Supported::Supported(outcome));
        }
        let author = Self::author(ctx, unit)?;
        let spec = resolved_spec(unit)?;
        let variable = |name: &str| ctx.env.get(name).cloned();
        if ctx.teardown_wait.is_some() {
            // Phase 2: the wait ended; remove exactly what the index owns.
            let rendered = Self::render(&ctx.worktree, &ctx.project_dir, &ctx.cache_dir, spec)?;
            let message = format!(
                "nyl: teardown {}/{}: remove the owned files",
                unit.unit.environment, unit.unit.name
            );
            prepare_target_removal(&rendered, &ctx.project_dir, &message, &author, &variable)
                .map_err(|error| DriverError::before_effects(format!("cannot prepare the removal: {error}")))?
                .push()
                .map_err(|error| DriverError::after_effects(format!("cannot remove the tree: {error}")))?;
            return Ok(Supported::Supported(Outcome::Succeeded {
                versions: std::collections::BTreeMap::new(),
                outputs: json!({}),
                artifacts: Vec::new(),
                attestations: Vec::new(),
            }));
        }
        if let Some(waiting) = ctx.observed.and_then(|observed| observed.waiting.as_ref()) {
            // Phase 1 is published already; the wait continues.
            return Ok(Supported::Supported(Outcome::Waiting {
                phase: waiting.phase.clone(),
                reason: waiting.reason.clone(),
                until: None,
                published: waiting.phase1_commit.clone(),
            }));
        }
        let rendered = Self::render(&ctx.worktree, &ctx.project_dir, &ctx.cache_dir, spec)?;
        let phase1 = without_workload_applications(&rendered)
            .map_err(|error| DriverError::before_effects(format!("cannot prepare phase 1 of the teardown: {error}")))?;
        let message = format!(
            "nyl: teardown {}/{}: remove the workload Applications",
            unit.unit.environment, unit.unit.name
        );
        let published = prepare_target_publication(&phase1, &ctx.project_dir, &message, &author, &variable)
            .map_err(|error| DriverError::before_effects(format!("cannot prepare phase 1 of the teardown: {error}")))?
            .push()
            .map_err(|error| DriverError::after_effects(format!("cannot publish phase 1 of the teardown: {error}")))?;
        let commit = CommitId::new(published.commit)
            .map_err(|error| DriverError::after_effects(format!("phase 1 commit: {error}")))?;
        Ok(Supported::Supported(Outcome::Waiting {
            phase: PHASE_APPLICATIONS_REMOVED.to_owned(),
            reason: format!(
                "phase 1 removed the workload Applications from the catalog of {}",
                published.path_prefix
            ),
            until: None,
            published: Some(commit),
        }))
    }

    /// The `teardownSettings` the stored document records; unsupported
    /// (no requirements) when it records none.
    fn teardown_settings(&self, unit: &DesiredUnit) -> DriverResult<Supported<TeardownSettings>> {
        let spec = unit.resolved_spec.as_ref().unwrap_or(&unit.spec);
        match spec.get("teardownSettings") {
            None => Ok(Supported::Unsupported),
            Some(settings) => Self::recorded_settings(settings)
                .map(Supported::Supported)
                .map_err(DriverError::before_effects),
        }
    }

    fn build(
        &self,
        _ctx: &ExecutionContext<'_>,
        _unit: &DesiredUnit,
        _options: &[OsString],
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
