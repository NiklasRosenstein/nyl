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
use super::units::DeletionPolicy;
use super::{GitSource, InlineGitRepository, LabelSelector, LocalReference};
use crate::bindings::{validate_declarations_at, InputDeclaration};
use crate::constants::API_VERSION_GITOPS;
use crate::orchestration::codes::{Code, Finding};
use crate::orchestration::ids::EnvironmentName;
use crate::orchestration::time::Duration;
use crate::{CoreError, Result};

/// Kind name of [`Environment`].
pub const KIND_ENVIRONMENT: &str = "Environment";
/// Kind name of [`EnvironmentGroup`].
pub const KIND_ENVIRONMENT_GROUP: &str = "EnvironmentGroup";
/// Kind name of [`EnvironmentTemplate`].
pub const KIND_ENVIRONMENT_TEMPLATE: &str = "EnvironmentTemplate";

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
#[schemars(example = super::schema::environment_example())]
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
    /// Whether `reconcile` repairs drift by itself: `converge` executes units whose latest drift observation is `drifted` again (units not safe to re-run await approval); `manual` leaves them `drifted` until `--reapply`. Omitted, the group's, else `converge`.
    #[serde(rename = "driftRepair", default, skip_serializing_if = "Option::is_none")]
    pub drift_repair: Option<DriftRepair>,
}

/// What a preview environment looks like, instantiated with parameters.
///
/// Instances are created with `nyl state init --template` (or
/// `nyl reconcile --template`), exist only in state, and behave like declared
/// environments for every command. The template has the Environment's fields
/// plus `parameters`, `deletionPolicy`, `allowTeardown`, `ttl`,
/// `maxInstances`, and `keepSource`. Strings in `values`, `state`, and `source`
/// are templates: they read `params` and `environment.name` when an instance
/// is created from the template, and `state` reads only `environment.name`,
/// because it locates the state that holds the parameters.
///
/// ## When needed
///
/// To give every pull request its own environment, created and removed by CI.
///
/// ## If omitted
///
/// Every environment is declared with an `Environment` resource in source.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
#[schemars(example = super::schema::environment_template_example())]
pub struct EnvironmentTemplate {
    /// API group and version defining this resource.
    #[serde(rename = "apiVersion")]
    pub api_version: String,
    /// Resource kind within the API group.
    pub kind: String,
    /// Resource identity and metadata. Instances take its labels.
    pub metadata: GitOpsResourceMetadata,
    /// Configuration for this resource.
    #[serde(default)]
    pub spec: EnvironmentTemplateSpec,
}

/// The Environment's fields, templated, plus what only instances have.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentTemplateSpec {
    /// Typed parameters of an instance, exposed to templates as `params`; `--param <name>=<value>` parses its value as YAML and checks it against the declaration. A parameter without a `default` is required.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub parameters: BTreeMap<String, InputDeclaration>,
    /// Matches literal unit `metadata.labels` across all unit kinds. An empty selector selects every unit.
    #[serde(rename = "unitSelector", default)]
    pub unit_selector: LabelSelector,
    /// Values unit templates see as `values`, with every string rendered as a template over `params` and `environment.name` when an instance is created.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub values: BTreeMap<String, serde_json::Value>,
    /// Where an instance's state lives. Strings may read `environment.name` only. With `path` such as `{{ environment.name }}`, every instance shares the refs, one directory each.
    #[serde(default)]
    pub state: EnvironmentState,
    /// The source commit runs use, which may template `revision` from `params`. Omitted, an instance runs from the commit recorded when it was created or last reconciled from a checkout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<EnvironmentSource>,
    /// Refs that pinned commits and run source commits must be reachable from. Omitted, the group's, else each repository's default branch.
    #[serde(rename = "protectedRefs", default, skip_serializing_if = "Option::is_none")]
    pub protected_refs: Option<Vec<String>>,
    /// Whether runs may use source commits outside `protectedRefs`; previews run from pull request branches and set `true`. Omitted, `false`.
    #[serde(rename = "allowUnprotectedSource", default, skip_serializing_if = "Option::is_none")]
    pub allow_unprotected_source: Option<bool>,
    /// Environment-wide attestations, recorded with `nyl attest` for an instance's current state.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attestations: Vec<String>,
    /// CI deployment admission: `required` makes state-writing runs use the source of the current admission. Omitted, `off`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admission: Option<Admission>,
    /// Whether `reconcile` repairs drift by itself. Omitted, `converge`.
    #[serde(rename = "driftRepair", default, skip_serializing_if = "Option::is_none")]
    pub drift_repair: Option<DriftRepair>,
    /// `Teardown` overrides every unit's own policy, including an explicit `Retain`, for the kinds that support teardown; units of other kinds are left in place and dropped from state when the instance is removed. `Retain` or omitted leaves each unit's own policy.
    #[serde(rename = "deletionPolicy", default, skip_serializing_if = "Option::is_none")]
    pub deletion_policy: Option<DeletionPolicy>,
    /// Whether omission teardown needs no `--allow-teardown`: a unit removed from the template or its selection is torn down by the next reconcile. Default `false`.
    #[serde(rename = "allowTeardown", default, skip_serializing_if = "std::ops::Not::not")]
    pub allow_teardown: bool,
    /// How long an instance lives without activity, such as `7d`: `state init --template`, `reconcile --template`, and every other `reconcile` of the instance (unless `--no-extend`) move its `expiresAt` to now plus `ttl`. A `reconcile` that finds the instance expired removes it, the way `nyl state delete --teardown` does, unless it is `--renew` or `reconcile --template`; `reconcile --template <name>` without an instance sweeps every instance of the template and never extends. Omitted, instances never expire.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl: Option<Duration>,
    /// The most instances of this template that may exist at once, expired ones included until they are removed. At the limit, `state init --template` and `reconcile --template` refuse to create another instance (exit 2) and name the expired instances that could be removed; `--make-room` removes them first, oldest expiry first and only as many as needed. Needs `state.path` ending in `{{ environment.name }}` so Nyl can list the instances. Omitted, unlimited.
    #[serde(rename = "maxInstances", default, skip_serializing_if = "Option::is_none")]
    pub max_instances: Option<u32>,
    /// Keep each instance's recorded source commit fetchable with a keep ref of its own, `refs/nyl-keep/<keepId>/<commit>`, in the source repository, so teardown still finds its source after the pull request branch is gone. `nyl state delete` removes the refs. Needs push access to `refs/nyl-keep/*` in the source repository, which a read-only token (GitHub's default `contents: read`, a fork pull request) lacks: without it `state init --template` and `reconcile --template` refuse (`NYL-INSTANCE-KEEP-FAILED`), so grant the access or set `false`, and then remove instances before their branches disappear. Default `true`.
    #[serde(rename = "keepSource", default = "default_true", skip_serializing_if = "is_true")]
    pub keep_source: bool,
}

fn default_true() -> bool {
    true
}

impl Default for EnvironmentTemplateSpec {
    /// A template that declares nothing: `keepSource` is on, like an omitted field.
    fn default() -> Self {
        serde_json::from_value(serde_json::json!({})).expect("an empty spec deserializes")
    }
}

#[allow(clippy::trivially_copy_pass_by_ref)] // serde's skip_serializing_if passes a reference.
fn is_true(value: &bool) -> bool {
    *value
}

/// Whether reconcile repairs drift on its own.
///
/// Contract: [Drift](../../../../design/orchestration-core.md#drift).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DriftRepair {
    /// The next reconcile executes drifted units whose recovery policy is
    /// `converge` again; a drifted unit that is not safe to re-run awaits
    /// approval.
    #[default]
    Converge,
    /// Reconcile repairs no drift by itself: drifted units stay `drifted`
    /// until `nyl reconcile --unit <unit> --reapply --reason <text>`.
    Manual,
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
#[schemars(example = super::schema::environment_group_example())]
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
    /// Default `driftRepair` of the members.
    #[serde(rename = "driftRepair", default, skip_serializing_if = "Option::is_none")]
    pub drift_repair: Option<DriftRepair>,
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

impl EnvironmentTemplate {
    /// The validated name.
    pub fn name(&self) -> Result<EnvironmentName> {
        EnvironmentName::new(&self.metadata.name)
            .map_err(|error| CoreError::config(format!("EnvironmentTemplate metadata.name: {error}")))
    }

    /// What is checked without parameters: the envelope, the parameter
    /// declarations, and the limits. The templated fields are validated as
    /// the Environment each instance renders to ([`Environment::validate`]).
    pub fn validate(&self) -> Result<()> {
        validate_envelope(&self.api_version, &self.kind, KIND_ENVIRONMENT_TEMPLATE)?;
        validate_static_required("metadata.name", &self.metadata.name)?;
        self.name()?;
        validate_declarations_at("spec.parameters", &self.spec.parameters)?;
        for (name, declaration) in &self.spec.parameters {
            if declaration.default.is_none() && declaration.allowed.as_ref().is_some_and(Vec::is_empty) {
                return Err(CoreError::config(format!(
                    "spec.parameters.{name}.enum must not be empty"
                )));
            }
        }
        if self.spec.max_instances == Some(0) {
            return Err(CoreError::config(
                "spec.maxInstances must be at least 1; omit it for no limit",
            ));
        }
        if self.spec.ttl.is_some_and(|ttl| ttl.as_secs() == 0) {
            return Err(CoreError::config(
                "spec.ttl must be longer than zero; omit it for no expiry",
            ));
        }
        let state = &self.spec.state;
        if state.repository_ref.is_some() || state.repository.is_some() {
            validate_repository_choice(state.repository_ref.as_ref(), state.repository.as_ref(), "spec.state")?;
        }
        if let Some(prefix) = &state.coordination_ref_prefix {
            if !prefix.starts_with("refs/") || !prefix.ends_with('/') {
                return Err(CoreError::config(
                    "spec.state.coordinationRefPrefix must start with refs/ and end with /",
                ));
            }
        }
        Ok(())
    }

    /// The Environment an instance named `name` renders to, before its
    /// strings are rendered: the fields the template shares with an
    /// Environment, with the template's labels. Templated strings are still
    /// templates.
    pub fn environment_skeleton(&self, name: &str) -> Environment {
        let spec = &self.spec;
        Environment {
            api_version: API_VERSION_GITOPS.to_owned(),
            kind: KIND_ENVIRONMENT.to_owned(),
            metadata: GitOpsResourceMetadata {
                name: name.to_owned(),
                labels: self.metadata.labels.clone(),
            },
            spec: EnvironmentSpec {
                unit_selector: spec.unit_selector.clone(),
                values: spec.values.clone(),
                state: spec.state.clone(),
                source: spec.source.clone(),
                protected_refs: spec.protected_refs.clone(),
                allow_unprotected_source: spec.allow_unprotected_source,
                attestations: spec.attestations.clone(),
                admission: spec.admission,
                drift_repair: spec.drift_repair,
            },
        }
    }

    /// The parameters of an instance: `given` over each declared default,
    /// every one checked against its declaration. A parameter that is
    /// neither given nor defaulted, one that is not declared, and a value of
    /// the wrong type or outside `enum` are errors naming the parameter.
    pub fn bind_parameters(
        &self,
        given: &BTreeMap<String, serde_json::Value>,
    ) -> std::result::Result<BTreeMap<String, serde_json::Value>, String> {
        let declared = &self.spec.parameters;
        if let Some(unknown) = given.keys().find(|name| !declared.contains_key(*name)) {
            let known = declared.keys().cloned().collect::<Vec<_>>().join(", ");
            return Err(format!(
                "parameter {unknown:?} is not declared by EnvironmentTemplate {} (declared: {})",
                self.metadata.name,
                if known.is_empty() { "none".to_owned() } else { known }
            ));
        }
        let mut bound = BTreeMap::new();
        for (name, declaration) in declared {
            let Some(value) = given.get(name).or(declaration.default.as_ref()) else {
                return Err(format!(
                    "parameter {name:?} of EnvironmentTemplate {} has no default; pass --param {name}=<value>",
                    self.metadata.name
                ));
            };
            declaration
                .check(value)
                .map_err(|reason| format!("parameter {name:?}: {reason}"))?;
            bound.insert(name.clone(), value.clone());
        }
        Ok(bound)
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
            git.validate_repository(field)?;
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
            || spec.drift_repair.is_some()
    }
}

/// The group whose shared fields apply to `environment`, if any.
///
/// Contract: [Environment groups](../../../../design/orchestration-core.md#environment-groups).
/// Groups that only select may overlap. An environment selected by more
/// than one group that carries shared fields is an error naming the groups
/// ([`Code::EnvironmentGroupOverlap`]), because the order of their defaults
/// would be arbitrary.
pub fn sharing_group<'a>(
    environment: &Environment,
    groups: &'a [EnvironmentGroup],
) -> std::result::Result<Option<&'a EnvironmentGroup>, Finding> {
    let sharing = groups
        .iter()
        .filter(|group| group.has_shared_fields() && group.selects(&environment.metadata.labels))
        .collect::<Vec<_>>();
    match sharing.as_slice() {
        [] => Ok(None),
        [group] => Ok(Some(group)),
        several => Err(Finding::new(
            Code::EnvironmentGroupOverlap,
            format!(
                "Environment {} is selected by EnvironmentGroups {} that all carry shared fields",
                environment.metadata.name,
                several
                    .iter()
                    .map(|group| group.metadata.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            "narrow the groups' environmentSelector, or keep shared fields in one of them",
        )),
    }
}

/// `environment` with the defaults of the group sharing it applied: the one
/// owner of the group-defaults rule, called by every entry point (unit
/// rendering at S, and the shell's entry fields).
///
/// Contract: [Environment](../../../../design/orchestration-core.md#environment)
/// and [Environment groups](../../../../design/orchestration-core.md#environment-groups).
/// - `values` merge recursively: the group's beneath the Environment's, and
///   the Environment wins; arrays and scalars are replaced, objects merge.
/// - `protectedRefs`, `allowUnprotectedSource`, `admission`, and
///   `driftRepair` take the group's value when the Environment omits them.
/// - `state` is left as written: the state location rule
///   (`nyl_orchestration::rules::state_location`) applies the group's
///   repository itself.
pub fn effective_environment(
    environment: &Environment,
    groups: &[EnvironmentGroup],
) -> std::result::Result<Environment, Finding> {
    let mut effective = environment.clone();
    let Some(group) = sharing_group(environment, groups)? else {
        return Ok(effective);
    };
    let mut values = serde_json::Value::Object(group.spec.values.clone().into_iter().collect());
    merge_values(
        &mut values,
        serde_json::Value::Object(environment.spec.values.clone().into_iter().collect()),
    );
    if let serde_json::Value::Object(map) = values {
        effective.spec.values = map.into_iter().collect();
    }
    let spec = &mut effective.spec;
    if spec.protected_refs.is_none() {
        spec.protected_refs.clone_from(&group.spec.protected_refs);
    }
    if spec.allow_unprotected_source.is_none() {
        spec.allow_unprotected_source = group.spec.allow_unprotected_source;
    }
    if spec.admission.is_none() {
        spec.admission = group.spec.admission;
    }
    if spec.drift_repair.is_none() {
        spec.drift_repair = group.spec.drift_repair;
    }
    Ok(effective)
}

/// Merge `overlay` into `base`: objects merge key by key, anything else is replaced.
fn merge_values(base: &mut serde_json::Value, overlay: serde_json::Value) {
    match (base, overlay) {
        (serde_json::Value::Object(base), serde_json::Value::Object(overlay)) => {
            for (key, value) in overlay {
                match base.get_mut(&key) {
                    Some(existing) => merge_values(existing, value),
                    None => {
                        base.insert(key, value);
                    }
                }
            }
        }
        (base, overlay) => *base = overlay,
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

    fn group(name: &str, spec: &serde_json::Value) -> EnvironmentGroup {
        serde_json::from_value(json!({
            "apiVersion": "gitops.nyl/v1", "kind": "EnvironmentGroup", "metadata": {"name": name}, "spec": spec
        }))
        .unwrap()
    }

    #[test]
    fn test_only_one_sharing_group_may_select_an_environment() {
        let environment = environment(json!({})).unwrap();
        let selecting = group(
            "all-production",
            &json!({"environmentSelector": {"matchLabels": {"stage": "production"}}}),
        );
        let sharing = group(
            "production",
            &json!({"environmentSelector": {"matchLabels": {"stage": "production"}}, "values": {"tier": "prod"}}),
        );
        let groups = [selecting.clone(), sharing.clone()];
        assert_eq!(
            sharing_group(&environment, &groups).unwrap().unwrap().metadata.name,
            "production"
        );
        let other = group(
            "prod-1",
            &json!({"environmentSelector": {"matchLabels": {"stage": "production"}}, "protectedRefs": ["main"]}),
        );
        let finding = sharing_group(&environment, &[selecting, sharing, other]).unwrap_err();
        assert_eq!(finding.code, Code::EnvironmentGroupOverlap);
        assert!(finding.message.contains("production, prod-1"), "{finding}");
    }

    #[test]
    fn test_group_defaults_sit_beneath_the_environment() {
        let environment = environment(json!({
            "values": {"db": {"size": "large"}, "replicas": 3},
            "protectedRefs": ["release/*"]
        }))
        .unwrap();
        let shared = group(
            "production",
            &json!({
                "environmentSelector": {"matchLabels": {"stage": "production"}},
                "values": {"db": {"size": "small", "engine": "postgres"}, "region": "eu"},
                "protectedRefs": ["main"],
                "allowUnprotectedSource": true
            }),
        );
        let effective = effective_environment(&environment, &[shared]).unwrap();
        assert_eq!(
            serde_json::to_value(&effective.spec.values).unwrap(),
            json!({"db": {"size": "large", "engine": "postgres"}, "replicas": 3, "region": "eu"})
        );
        assert_eq!(effective.spec.protected_refs, Some(vec!["release/*".to_owned()]));
        assert_eq!(effective.spec.allow_unprotected_source, Some(true));
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

    /// Catches a typo such as `driftRepair: off` being accepted (and read
    /// as the default) instead of failing at discovery.
    #[test]
    fn test_drift_repair_parses_converge_and_manual_only() {
        assert_eq!(environment(json!({})).unwrap().spec.drift_repair, None);
        assert_eq!(
            environment(json!({"driftRepair": "converge"}))
                .unwrap()
                .spec
                .drift_repair,
            Some(DriftRepair::Converge)
        );
        assert_eq!(
            environment(json!({"driftRepair": "manual"})).unwrap().spec.drift_repair,
            Some(DriftRepair::Manual)
        );
        assert!(environment(json!({"driftRepair": "off"})).is_err());
    }

    /// Catches a group's `driftRepair` overriding a member that declares
    /// its own, or not applying to a member that omits it, and a group that
    /// carries only `driftRepair` not counting as sharing (so two such
    /// groups would silently compete).
    #[test]
    fn test_group_drift_repair_applies_only_when_the_environment_omits_it() {
        let shared = group(
            "production",
            &json!({
                "environmentSelector": {"matchLabels": {"stage": "production"}},
                "driftRepair": "manual"
            }),
        );
        assert!(shared.has_shared_fields());
        let omitted = environment(json!({})).unwrap();
        assert_eq!(
            effective_environment(&omitted, std::slice::from_ref(&shared))
                .unwrap()
                .spec
                .drift_repair,
            Some(DriftRepair::Manual)
        );
        let declared = environment(json!({"driftRepair": "converge"})).unwrap();
        assert_eq!(
            effective_environment(&declared, &[shared]).unwrap().spec.drift_repair,
            Some(DriftRepair::Converge)
        );
    }
}
