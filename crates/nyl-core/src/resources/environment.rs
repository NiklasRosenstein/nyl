//! Environments and environment groups.
//!
//! Contract: [Environment](../../../../design/orchestration-core.md#environment)
//! and [Environment groups](../../../../design/orchestration-core.md#environment-groups).
//! Both are static at discovery, like a DeploymentTarget: their envelope and
//! spec are literal.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::gitops::{validate_repository_choice, validate_static_required, GitOpsResourceMetadata};
use super::strict::DenyUnknownFields;
use super::{GitSource, InlineGitRepository, LabelSelector, LocalReference};
use crate::constants::API_VERSION_GITOPS;
use crate::orchestration::ids::EnvironmentName;
use crate::{CoreError, Result};

/// Kind name of [`Environment`].
pub const KIND_ENVIRONMENT: &str = "Environment";
/// Kind name of [`EnvironmentGroup`].
pub const KIND_ENVIRONMENT_GROUP: &str = "EnvironmentGroup";

/// A named place that units are reconciled into, with its own state.
///
/// An Environment selects units by their literal labels and gives their
/// templates its `values`. The selected, enabled units are its ownership set:
/// a unit that leaves the set is deleted under its deletion policy.
///
/// ## When needed
///
/// To reconcile units with `nyl reconcile -e <environment>`.
///
/// ## If omitted
///
/// No orchestration happens; rendering and publication work as before.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
#[schemars(example = super::schema::resource_example(super::schema::ResourceKind::Environment))]
pub struct Environment {
    /// API group and version defining this resource.
    #[serde(rename = "apiVersion")]
    pub api_version: String,
    /// Resource kind within the API group.
    pub kind: String,
    /// Resource identity and metadata.
    pub metadata: GitOpsResourceMetadata,
    /// Configuration for this resource.
    #[serde(default)]
    pub spec: EnvironmentSpec,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentSpec {
    /// Matches literal unit `metadata.labels` across all unit kinds. An empty selector selects every unit.
    #[serde(rename = "unitSelector", default)]
    pub unit_selector: LabelSelector,
    /// Values unit templates see as `values`, merged recursively over the values of the EnvironmentGroup selecting this environment.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub values: BTreeMap<String, serde_json::Value>,
    /// Where the environment's state lives.
    #[serde(default)]
    pub state: EnvironmentState,
    /// The source commit runs use. Omitted, the commit of the worktree a run starts from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<EnvironmentSource>,
    /// Refs that pinned commits and run source commits must be reachable from. Omitted, the group's, else each repository's default branch.
    #[serde(rename = "protectedRefs", default, skip_serializing_if = "Option::is_none")]
    pub protected_refs: Option<Vec<String>>,
    /// Whether runs may use source commits outside `protectedRefs`. Omitted, the group's, else `false`.
    #[serde(rename = "allowUnprotectedSource", default, skip_serializing_if = "Option::is_none")]
    pub allow_unprotected_source: Option<bool>,
    /// Environment-wide attestations, recorded with `nyl attest` for the environment's current state.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attestations: Vec<String>,
    /// CI deployment admission: `required` makes state-writing runs use the source of the current admission. Omitted, the group's, else `off`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admission: Option<Admission>,
}

/// Whether state-writing runs require a CI deployment admission.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Admission {
    Off,
    Required,
}

/// The location of an environment's state: a repository and the refs within it.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentState {
    /// GitRepository holding the state, read through its `repoURL` and written through its `publishURL`. Mutually exclusive with `repository`. Omitted with `repository`: the group's, else the checkout's `origin`, else the local repository.
    #[serde(rename = "repositoryRef", default, skip_serializing_if = "Option::is_none")]
    pub repository_ref: Option<LocalReference>,
    /// Inline repository coordinates holding the state. Mutually exclusive with `repositoryRef`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<InlineGitRepository>,
    /// Ref of the desired state. A short name is a branch. Default `nyl/<environment>/desired`.
    #[serde(rename = "desiredRef", default, skip_serializing_if = "Option::is_none")]
    pub desired_ref: Option<String>,
    /// Ref of the observed state; may equal `desiredRef`. Default `nyl/<environment>/observed`.
    #[serde(rename = "observedRef", default, skip_serializing_if = "Option::is_none")]
    pub observed_ref: Option<String>,
    /// Directory of the environment's state within its refs, so several environments can share one ref. Default the root.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub path: String,
    /// Prefix, ending in `/`, of the leases, runs, and signals refs. Default `refs/nyl/`.
    #[serde(rename = "coordinationRefPrefix", default, skip_serializing_if = "Option::is_none")]
    pub coordination_ref_prefix: Option<String>,
    /// Ref of the deployment admission record. Default the desired ref.
    #[serde(rename = "admissionRef", default, skip_serializing_if = "Option::is_none")]
    pub admission_ref: Option<String>,
}

/// The source commit of an environment's runs: a ref whose tip each run uses,
/// optionally pinned to a commit that `nyl update source-locks` moves, or the
/// commit the newest promotion recorded.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[schemars(transform = environment_source_schema)]
pub struct EnvironmentSource {
    #[serde(flatten)]
    pub git: GitSource,
    /// The source commit recorded by the newest promotion on a path; excludes the other fields.
    #[serde(rename = "fromPromotion", default, skip_serializing_if = "Option::is_none")]
    pub from_promotion: Option<PromotedSource>,
    #[serde(flatten)]
    #[schemars(skip)]
    pub unknown_fields: DenyUnknownFields,
}

/// A source that promotion paths supply.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PromotedSource {
    /// The PromotionPath that supplies the source. Mutually exclusive with `paths`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Several PromotionPaths; the newest record on any of them wins. Mutually exclusive with `path`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    /// Where the PromotionRecord lives: `state` (default) or `source`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record: Option<String>,
    /// The record `nyl promote` writes here with `record: source`, never by hand.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub promoted: Option<serde_json::Value>,
}

fn environment_source_schema(schema: &mut schemars::Schema) {
    super::strict::deny_additional_properties(schema);
    if let Some(serde_json::Value::Object(properties)) = schema.get_mut("properties") {
        properties.remove("path");
    }
}

/// Environments that belong together, with configuration they share.
///
/// A group selects declared Environments by their literal labels, the labels
/// `-l` matches, and names them for `-g`. Its fields are defaults beneath each
/// member's own.
///
/// ## When needed
///
/// To reconcile several environments with `-g <group>`, or to share state,
/// protected refs, or values among them.
///
/// ## If omitted
///
/// Each Environment carries all of its own configuration.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
#[schemars(example = super::schema::resource_example(super::schema::ResourceKind::EnvironmentGroup))]
pub struct EnvironmentGroup {
    /// API group and version defining this resource.
    #[serde(rename = "apiVersion")]
    pub api_version: String,
    /// Resource kind within the API group.
    pub kind: String,
    /// Resource identity and metadata.
    pub metadata: GitOpsResourceMetadata,
    /// Configuration for this resource.
    pub spec: EnvironmentGroupSpec,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentGroupSpec {
    /// Matches literal Environment `metadata.labels`. Must not be empty.
    #[serde(rename = "environmentSelector")]
    pub environment_selector: LabelSelector,
    /// Defaults beneath each member's `values`; the member wins.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub values: BTreeMap<String, serde_json::Value>,
    /// Default state repository of the members.
    #[serde(default)]
    pub state: EnvironmentGroupState,
    /// Default `protectedRefs` of the members.
    #[serde(rename = "protectedRefs", default, skip_serializing_if = "Option::is_none")]
    pub protected_refs: Option<Vec<String>>,
    /// Default `allowUnprotectedSource` of the members.
    #[serde(rename = "allowUnprotectedSource", default, skip_serializing_if = "Option::is_none")]
    pub allow_unprotected_source: Option<bool>,
    /// Default `admission` of the members.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admission: Option<Admission>,
}

/// The state repository a group's members default to. Refs and paths stay
/// each environment's own.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentGroupState {
    /// GitRepository holding the members' state. Mutually exclusive with `repository`.
    #[serde(rename = "repositoryRef", default, skip_serializing_if = "Option::is_none")]
    pub repository_ref: Option<LocalReference>,
    /// Inline repository coordinates holding the members' state. Mutually exclusive with `repositoryRef`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<InlineGitRepository>,
}

impl Environment {
    /// The validated name.
    pub fn name(&self) -> Result<EnvironmentName> {
        EnvironmentName::new(&self.metadata.name)
            .map_err(|error| CoreError::config(format!("Environment metadata.name: {error}")))
    }

    pub fn validate(&self) -> Result<()> {
        validate_envelope(&self.api_version, &self.kind, KIND_ENVIRONMENT)?;
        self.name()?;
        let state = &self.spec.state;
        if state.repository_ref.is_some() || state.repository.is_some() {
            validate_repository_choice(state.repository_ref.as_ref(), state.repository.as_ref(), "spec.state")?;
        }
        for (field, value) in [("desiredRef", &state.desired_ref), ("observedRef", &state.observed_ref)] {
            if let Some(value) = value {
                validate_static_required(&format!("spec.state.{field}"), value)?;
            }
        }
        if !state.path.is_empty() {
            super::gitops::validate_relative_path("spec.state.path", &state.path, false, false)?;
        }
        if let Some(prefix) = &state.coordination_ref_prefix {
            if !prefix.starts_with("refs/") || !prefix.ends_with('/') {
                return Err(CoreError::config(
                    "spec.state.coordinationRefPrefix must start with refs/ and end with /",
                ));
            }
        }
        if let Some(source) = &self.spec.source {
            source.validate("spec.source")?;
        }
        if let Some(refs) = &self.spec.protected_refs {
            for (index, value) in refs.iter().enumerate() {
                validate_static_required(&format!("spec.protectedRefs[{index}]"), value)?;
            }
        }
        Ok(())
    }
}

impl EnvironmentSource {
    /// A source follows a `revision`, optionally pinned by `commit`, in this
    /// repository or a named one, and never names a `path`.
    pub fn validate(&self, field: &str) -> Result<()> {
        let git = &self.git;
        if let Some(promoted) = &self.from_promotion {
            if git != &GitSource::default() {
                return Err(CoreError::config(format!(
                    "{field}.fromPromotion excludes repositoryRef, repository, revision, and commit"
                )));
            }
            if promoted.path.is_some() != promoted.paths.is_empty() {
                return Err(CoreError::config(format!(
                    "{field}.fromPromotion needs exactly one of path and paths"
                )));
            }
            return match promoted.record.as_deref() {
                None | Some("state" | "source") => Ok(()),
                Some(other) => Err(CoreError::config(format!(
                    "{field}.fromPromotion.record {other:?} must be state or source"
                ))),
            };
        }
        if git.path.is_some() {
            return Err(CoreError::config(format!(
                "{field}.path is not allowed: an environment's source names a commit, not a directory"
            )));
        }
        if git.is_remote() {
            validate_repository_choice(git.repository_ref.as_ref(), git.repository.as_ref(), field)?;
        }
        let revision = git
            .revision
            .as_deref()
            .ok_or_else(|| CoreError::config(format!("{field}.revision is required")))?;
        validate_static_required(&format!("{field}.revision"), revision)?;
        if let Some(commit) = &git.commit {
            super::gitops::validate_immutable_git_commit(&format!("{field}.commit"), commit)?;
        }
        Ok(())
    }
}

impl EnvironmentGroup {
    pub fn validate(&self) -> Result<()> {
        validate_envelope(&self.api_version, &self.kind, KIND_ENVIRONMENT_GROUP)?;
        validate_static_required("metadata.name", &self.metadata.name)?;
        if self.spec.environment_selector.match_labels.is_empty() {
            return Err(CoreError::config(format!(
                "EnvironmentGroup {}: spec.environmentSelector must not be empty; a group lists its members by labels",
                self.metadata.name
            )));
        }
        let state = &self.spec.state;
        if state.repository_ref.is_some() || state.repository.is_some() {
            validate_repository_choice(state.repository_ref.as_ref(), state.repository.as_ref(), "spec.state")?;
        }
        Ok(())
    }

    /// Whether the group selects an Environment with these labels.
    pub fn selects(&self, labels: &BTreeMap<String, String>) -> bool {
        self.spec
            .environment_selector
            .match_labels
            .iter()
            .all(|(key, value)| labels.get(key) == Some(value))
    }

    /// Whether the group carries shared fields, as opposed to only selecting.
    pub fn has_shared_fields(&self) -> bool {
        let spec = &self.spec;
        !spec.values.is_empty()
            || spec.state.repository_ref.is_some()
            || spec.state.repository.is_some()
            || spec.protected_refs.is_some()
            || spec.allow_unprotected_source.is_some()
            || spec.admission.is_some()
    }
}

fn validate_envelope(api_version: &str, kind: &str, expected: &str) -> Result<()> {
    if api_version != API_VERSION_GITOPS {
        return Err(CoreError::config(format!(
            "{expected} apiVersion must be {API_VERSION_GITOPS:?}"
        )));
    }
    if kind != expected {
        return Err(CoreError::config(format!("Expected kind {expected:?}, got {kind:?}")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn environment(spec: serde_json::Value) -> Result<Environment> {
        let environment: Environment = serde_json::from_value(json!({
            "apiVersion": "gitops.nyl/v1",
            "kind": "Environment",
            "metadata": {"name": "production", "labels": {"stage": "production"}},
            "spec": spec,
        }))?;
        environment.validate()?;
        Ok(environment)
    }

    #[test]
    fn test_environment_accepts_the_contract_example() {
        let environment = environment(json!({
            "unitSelector": {"matchLabels": {"tier": "platform"}},
            "values": {"stateKey": "production/database"},
            "state": {
                "repositoryRef": {"name": "platform-state"},
                "desiredRef": "nyl/production/desired",
                "observedRef": "nyl/production/observed",
                "coordinationRefPrefix": "refs/nyl/"
            },
            "source": {"revision": "main", "commit": "3e7b9c0a1d2e3f4a5b6c7d8e9f0a1b2c3d4e5f6a"},
            "protectedRefs": ["main", "release/*"],
            "allowUnprotectedSource": false,
            "attestations": ["qa"],
            "admission": "required"
        }))
        .unwrap();
        assert_eq!(environment.name().unwrap().as_str(), "production");
    }

    #[test]
    fn test_environment_source_names_a_revision_and_never_a_path() {
        assert!(environment(json!({"source": {"path": "infra", "revision": "main"}}))
            .unwrap_err()
            .to_string()
            .contains("spec.source.path is not allowed"));
        assert!(
            environment(json!({"source": {"commit": "3e7b9c0a1d2e3f4a5b6c7d8e9f0a1b2c3d4e5f6a"}}))
                .unwrap_err()
                .to_string()
                .contains("spec.source.revision is required")
        );
        assert!(environment(json!({"source": {"fromPromotion": {"path": "dev-to-prod"}}})).is_ok());
        assert!(
            environment(json!({"source": {"fromPromotion": {"path": "dev-to-prod"}, "revision": "main"}})).is_err()
        );
        // Typos in a flattened source fail like everywhere else.
        assert!(environment(json!({"source": {"revison": "main"}}))
            .unwrap_err()
            .to_string()
            .contains("unknown field"));
    }

    #[test]
    fn test_environment_group_requires_a_selector_and_reports_shared_fields() {
        let group = |spec: serde_json::Value| -> Result<EnvironmentGroup> {
            let group: EnvironmentGroup = serde_json::from_value(json!({
                "apiVersion": "gitops.nyl/v1",
                "kind": "EnvironmentGroup",
                "metadata": {"name": "production"},
                "spec": spec,
            }))?;
            group.validate()?;
            Ok(group)
        };
        assert!(group(json!({"environmentSelector": {}})).is_err());
        let selecting = group(json!({"environmentSelector": {"matchLabels": {"stage": "production"}}})).unwrap();
        assert!(!selecting.has_shared_fields());
        assert!(selecting.selects(&BTreeMap::from([("stage".into(), "production".into())])));
        let sharing = group(json!({
            "environmentSelector": {"matchLabels": {"stage": "production"}},
            "values": {"cluster": "prod-1"}
        }))
        .unwrap();
        assert!(sharing.has_shared_fields());
    }
}
