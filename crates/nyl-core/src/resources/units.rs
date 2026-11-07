//! Fields every unit kind shares.
//!
//! Contract: [Units](../../../../design/orchestration-core.md#units).
//!
//! A unit's envelope (`apiVersion`, `kind`, `metadata.name`,
//! `metadata.labels`) is literal, so environments select units before
//! rendering. Its `spec` is a template rendered once per selecting
//! Environment; the types here parse the rendered spec. Every kind flattens
//! [`CommonUnitFields`] into its spec, so their schema and documentation are
//! identical everywhere.

use std::collections::BTreeMap;
use std::fmt;

use schemars::JsonSchema;
use serde::de::{Deserializer, Error as _};
use serde::{Deserialize, Serialize};

use super::gitops::validate_static_required;
use crate::bindings::{validate_input_name, InputType};
use crate::constants::API_VERSION_UNITS;
use crate::orchestration::ids::{EnvironmentName, UnitName};
use crate::orchestration::time::Duration;
use crate::{CoreError, Result};

/// The literal part of a unit document, read before its spec renders.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct UnitEnvelope {
    #[serde(rename = "apiVersion")]
    pub api_version: String,
    pub kind: String,
    pub metadata: UnitMetadata,
}

/// A unit's literal identity and selection labels.
pub use super::gitops::GitOpsResourceMetadata as UnitMetadata;

impl UnitEnvelope {
    /// The validated unit name.
    pub fn name(&self) -> Result<UnitName> {
        UnitName::new(&self.metadata.name)
            .map_err(|error| CoreError::config(format!("{} metadata.name: {error}", self.kind)))
    }

    /// Whether the document is in a unit API group, `units.<domain>/<version>`.
    pub fn is_unit_api_version(api_version: &str) -> bool {
        api_version
            .split_once('/')
            .is_some_and(|(group, version)| group.starts_with("units.") && !version.is_empty())
    }
}

/// The members of a unit's `spec` that [`CommonUnitFields`] defines, which
/// the engine reads and no driver may overwrite.
pub const COMMON_UNIT_FIELDS: [&str; 8] = [
    "enabled",
    "outputs",
    "dependsOn",
    "approval",
    "deletionPolicy",
    "timeout",
    "env",
    "attestations",
];

/// The longest `spec.timeout` a unit may declare.
pub const MAX_UNIT_TIMEOUT: Duration = Duration::from_secs(7 * 86_400);

/// Fields every unit kind shares.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CommonUnitFields {
    /// Evaluated after rendering; `false` leaves the environment's ownership set, which deletes the unit under its `deletionPolicy`.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub enabled: bool,
    /// Declared outputs, for kinds that let units declare them. Only declared outputs are recorded.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub outputs: BTreeMap<String, OutputDeclaration>,
    /// Units that must have a current receipt first, for ordering without a data reference.
    #[serde(rename = "dependsOn", default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<DependsOn>,
    /// Whether executions need an invocation's approval.
    #[serde(default, skip_serializing_if = "ApprovalPolicy::is_auto")]
    pub approval: ApprovalPolicy,
    /// What happens to the unit's resources when it leaves the ownership set. Default `Teardown` where the kind supports teardown, else `Retain`.
    #[serde(rename = "deletionPolicy", default, skip_serializing_if = "Option::is_none")]
    pub deletion_policy: Option<DeletionPolicy>,
    /// Deadline for one execution, teardown, or verification, at most `7d`. Default per kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<Duration>,
    /// Variables and secrets admitted into the unit's otherwise empty tool environment.
    #[serde(default, skip_serializing_if = "EnvAdmission::is_empty")]
    pub env: EnvAdmission,
    /// Attestations of the unit's receipts: names its kind produces itself, or entries with `from: external` that only `nyl attest` supplies.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attestations: Vec<AttestationDeclaration>,
}

fn default_true() -> bool {
    true
}

#[allow(clippy::trivially_copy_pass_by_ref)] // serde's skip_serializing_if passes a reference.
fn is_true(value: &bool) -> bool {
    *value
}

/// What a kind lets its units declare, for [`CommonUnitFields::validate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KindSupport {
    /// Units declare `outputs`; otherwise the kind publishes fixed artifacts.
    pub declared_outputs: bool,
    /// The kind can tear its resources down.
    pub teardown: bool,
}

impl CommonUnitFields {
    pub fn validate(&self, support: KindSupport) -> Result<()> {
        if !support.declared_outputs && !self.outputs.is_empty() {
            return Err(CoreError::config(
                "spec.outputs is not allowed: this kind publishes artifacts instead of declared outputs",
            ));
        }
        for name in self.outputs.keys() {
            validate_input_name("spec.outputs", name)?;
        }
        for (index, entry) in self.depends_on.iter().enumerate() {
            entry.validate(&format!("spec.dependsOn[{index}]"))?;
        }
        if self.deletion_policy == Some(DeletionPolicy::Teardown) && !support.teardown {
            return Err(CoreError::config(
                "spec.deletionPolicy Teardown is not allowed: this kind cannot tear down its resources",
            ));
        }
        if self.timeout.is_some_and(|timeout| timeout > MAX_UNIT_TIMEOUT) {
            return Err(CoreError::config(format!(
                "spec.timeout may be at most {MAX_UNIT_TIMEOUT}: a run holds its lease for the whole timeout"
            )));
        }
        self.env.validate()?;
        for (index, attestation) in self.attestations.iter().enumerate() {
            validate_static_required(&format!("spec.attestations[{index}].name"), &attestation.name)?;
        }
        Ok(())
    }

    /// The effective deletion policy.
    pub fn effective_deletion_policy(&self, support: KindSupport) -> DeletionPolicy {
        self.deletion_policy.unwrap_or(if support.teardown {
            DeletionPolicy::Teardown
        } else {
            DeletionPolicy::Retain
        })
    }
}

/// One declared output: a type from the Release input type set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OutputDeclaration {
    /// Value type: `string`, `integer`, `number`, `boolean`, `object`, or `array`.
    #[serde(rename = "type")]
    pub output_type: InputType,
    /// Documentation, surfaced in inspection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Validated but never persisted, and never referenceable.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub sensitive: bool,
}

/// A unit that must have a current receipt before this one runs: a name, or
/// an entry naming another declared environment or stronger evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum DependsOn {
    /// A unit of the same environment.
    Name(String),
    /// A unit, optionally of another declared environment, with required evidence.
    Entry(DependsOnEntry),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DependsOnEntry {
    /// Another declared environment; default this one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
    /// The producing unit.
    pub unit: String,
    /// Required evidence: `published` (a current receipt, the default) or `attested`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<Evidence>,
    /// The producer's attestations to require, instead of all it declares.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attestations: Vec<String>,
}

/// Evidence a consumer requires from its producer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "lowercase")]
pub enum Evidence {
    /// A current receipt.
    #[default]
    Published,
    /// A current receipt whose required attestations pass.
    Attested,
}

impl DependsOn {
    /// The producing unit's name.
    pub fn unit(&self) -> &str {
        match self {
            Self::Name(name) => name,
            Self::Entry(entry) => &entry.unit,
        }
    }

    /// The producing environment, when it is another one.
    pub fn environment(&self) -> Option<&str> {
        match self {
            Self::Name(_) => None,
            Self::Entry(entry) => entry.environment.as_deref(),
        }
    }

    /// The evidence the entry requires of its producer.
    pub fn evidence(&self) -> Evidence {
        match self {
            Self::Name(_) => Evidence::default(),
            Self::Entry(entry) => entry.evidence.unwrap_or_default(),
        }
    }

    /// The producer's attestations the entry names; empty for a name.
    pub fn attestations(&self) -> &[String] {
        match self {
            Self::Name(_) => &[],
            Self::Entry(entry) => &entry.attestations,
        }
    }

    fn validate(&self, field: &str) -> Result<()> {
        UnitName::new(self.unit()).map_err(|error| CoreError::config(format!("{field}: {error}")))?;
        if let Some(environment) = self.environment() {
            EnvironmentName::new(environment).map_err(|error| CoreError::config(format!("{field}: {error}")))?;
        }
        for (index, name) in self.attestations().iter().enumerate() {
            validate_static_required(&format!("{field}.attestations[{index}]"), name)?;
        }
        Ok(())
    }
}

/// When a unit's executions need an invocation's approval.
///
/// Written as `auto` (the default), `manual` (short for
/// `{mode: manual, bind: desired}`), or `{mode, bind, requireDigest}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema, Default)]
#[schemars(transform = approval_schema)]
pub struct ApprovalPolicy {
    /// `auto` executes without approval; `manual` waits for `--approve`.
    pub mode: ApprovalMode,
    /// What an approval authorizes: one execution of the `desired` document, or the changes a reviewed `plan` showed.
    pub bind: ApprovalBind,
    /// Refuse approvals without a change digest, for `bind: plan`.
    #[serde(rename = "requireDigest", default, skip_serializing_if = "std::ops::Not::not")]
    pub require_digest: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalMode {
    #[default]
    Auto,
    Manual,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalBind {
    #[default]
    Desired,
    Plan,
}

impl ApprovalPolicy {
    pub fn is_auto(&self) -> bool {
        self.mode == ApprovalMode::Auto
    }
}

impl<'de> Deserialize<'de> for ApprovalPolicy {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Long {
            mode: ApprovalMode,
            #[serde(default)]
            bind: ApprovalBind,
            #[serde(rename = "requireDigest", default)]
            require_digest: bool,
        }
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Form {
            Short(ApprovalMode),
            Long(Long),
        }
        let policy = match Form::deserialize(deserializer)
            .map_err(|_| D::Error::custom("approval must be auto, manual, or {mode, bind, requireDigest}"))?
        {
            Form::Short(mode) => Self {
                mode,
                ..Self::default()
            },
            Form::Long(long) => Self {
                mode: long.mode,
                bind: long.bind,
                require_digest: long.require_digest,
            },
        };
        if policy.mode == ApprovalMode::Auto && (policy.bind == ApprovalBind::Plan || policy.require_digest) {
            return Err(D::Error::custom("approval bind and requireDigest require mode manual"));
        }
        if policy.require_digest && policy.bind != ApprovalBind::Plan {
            return Err(D::Error::custom("approval requireDigest requires bind plan"));
        }
        Ok(policy)
    }
}

fn approval_schema(schema: &mut schemars::Schema) {
    let long = schema.clone();
    *schema = schemars::json_schema!({
        "description": "`auto` (default), `manual` (short for {mode: manual, bind: desired}), or {mode, bind, requireDigest}.",
        "anyOf": [{"type": "string", "enum": ["auto", "manual"]}, long]
    });
}

/// What happens to a unit's resources when it leaves the ownership set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum DeletionPolicy {
    /// Tear the resources down, after `--allow-teardown`.
    Teardown,
    /// Leave the resources in place.
    Retain,
}

impl fmt::Display for DeletionPolicy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Teardown => "Teardown",
            Self::Retain => "Retain",
        })
    }
}

/// Variables admitted into a unit's tool environment. Values are never part
/// of the execution key; only names are.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EnvAdmission {
    /// Names of runner environment variables copied as they are.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub passthrough: Vec<String>,
    /// Variables read from the project's secrets provider, by variable name and secret key. An object or array secret is passed as compact JSON.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub secrets: BTreeMap<String, String>,
}

impl EnvAdmission {
    pub fn is_empty(&self) -> bool {
        self.passthrough.is_empty() && self.secrets.is_empty()
    }

    fn validate(&self) -> Result<()> {
        let names = self.passthrough.iter().chain(self.secrets.keys());
        for name in names {
            let valid = !name.is_empty()
                && !name.starts_with(|character: char| character.is_ascii_digit())
                && name
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || character == '_');
            if !valid {
                return Err(CoreError::config(format!(
                    "spec.env variable name {name:?} must consist of letters, digits, and '_' and not start with a digit"
                )));
            }
        }
        if let Some(name) = self.passthrough.iter().find(|name| self.secrets.contains_key(*name)) {
            return Err(CoreError::config(format!(
                "spec.env variable {name} is both passed through and read from a secret"
            )));
        }
        Ok(())
    }
}

/// One attestation a unit's receipts can carry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AttestationDeclaration {
    /// Attestation name, such as `healthy`.
    pub name: String,
    /// `external` for an attestation only `nyl attest` supplies; omitted for one the kind produces itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<AttestationSource>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum AttestationSource {
    External,
}

/// The API version of the built-in unit kinds.
pub fn is_builtin_unit_api_version(api_version: &str) -> bool {
    api_version == API_VERSION_UNITS
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_approval_accepts_short_and_long_forms() {
        let parse = |value| serde_json::from_value::<ApprovalPolicy>(value);
        assert_eq!(parse(json!("auto")).unwrap(), ApprovalPolicy::default());
        assert_eq!(
            parse(json!("manual")).unwrap(),
            ApprovalPolicy {
                mode: ApprovalMode::Manual,
                bind: ApprovalBind::Desired,
                require_digest: false
            }
        );
        assert!(parse(json!({"mode": "manual", "bind": "plan", "requireDigest": true})).is_ok());
        assert!(parse(json!({"mode": "manual", "requireDigest": true})).is_err());
        assert!(parse(json!({"mode": "auto", "bind": "plan"})).is_err());
        assert!(parse(json!("sometimes")).is_err());
    }

    #[test]
    fn test_depends_on_accepts_names_and_entries() {
        let common: CommonUnitFields = serde_json::from_value(json!({
            "dependsOn": ["network", {"environment": "dev", "unit": "kubernetes", "attestations": ["healthy"]}]
        }))
        .unwrap();
        common
            .validate(KindSupport {
                declared_outputs: true,
                teardown: false,
            })
            .unwrap();
        assert_eq!(common.depends_on[0].unit(), "network");
        assert_eq!(common.depends_on[1].environment(), Some("dev"));
    }
}
