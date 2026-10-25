//! Units: the fields every unit kind shares, and the built-in `Command` kind.
//!
//! Contract: [Units](../../../../design/orchestration-core.md#units) and
//! [Command unit](../../../../design/orchestration-core.md#command-unit).
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
use super::strict::DenyUnknownFields;
use crate::bindings::{validate_input_name, InputType};
use crate::constants::API_VERSION_UNITS;
use crate::orchestration::ids::{EnvironmentName, UnitName};
use crate::orchestration::time::Duration;
use crate::{CoreError, Result};

/// Kind name of the built-in [`CommandSpec`] unit.
pub const KIND_COMMAND: &str = "Command";

/// The literal part of a unit document, read before its spec renders.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct UnitEnvelope {
    #[serde(rename = "apiVersion")]
    pub api_version: String,
    pub kind: String,
    pub metadata: UnitMetadata,
}

/// A unit's identity and selection labels. Both are literal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UnitMetadata {
    /// The unit's identity for references, receipts, leases, and deletion; unique per environment across all unit kinds.
    pub name: String,
    /// Literal labels that Environment `unitSelector`s match.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
}

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

    fn validate(&self, field: &str) -> Result<()> {
        UnitName::new(self.unit()).map_err(|error| CoreError::config(format!("{field}: {error}")))?;
        if let Some(environment) = self.environment() {
            EnvironmentName::new(environment).map_err(|error| CoreError::config(format!("{field}: {error}")))?;
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

/// Runs commands from the environment's source commit.
///
/// The command starts from an empty environment plus `PATH`, a fresh `HOME`,
/// `NYL_INPUTS` with the resolved `values`, `NYL_OUTPUTS` for its outputs,
/// `NYL_UNIT`, `NYL_ENVIRONMENT`, `NYL_OPERATION`, and what `env` admits.
/// There is no sandbox beyond the working directory and that environment.
///
/// ## When needed
///
/// For a step no built-in kind covers, such as seeding a database.
///
/// ## If omitted
///
/// Nothing runs outside the built-in kinds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[schemars(transform = super::strict::deny_additional_properties)]
pub struct CommandSpec {
    /// Files, as local-path globs, whose content enters the execution key. Other files may be read but never trigger re-execution.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    /// Values the command reads from `NYL_INPUTS`; references such as `fromUnit` may appear at any depth.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub values: serde_json::Value,
    /// Argument vector of the reconcile step, executed without a shell. A relative first element is resolved against `workingDir`.
    pub command: Vec<String>,
    /// Argument vector reporting drift: exit 0 for clean, 2 for drift, anything else for an error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify: Option<Vec<String>>,
    /// Argument vector removing the command's effects. Without it the kind cannot tear down and defaults to `deletionPolicy: Retain`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub teardown: Option<Vec<String>>,
    /// Argument vector whose stdout enters the execution key, such as tool versions. Runs during resolution without secrets; must be fast and deterministic.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<Vec<String>>,
    /// Working directory under the local path rule, where `/` alone names the worktree root. Default the directory containing `nyl.toml`.
    #[serde(rename = "workingDir", default, skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
    /// The author's claim that re-running with identical inputs is safe: failures become retryable and uncertain executions re-run.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub idempotent: bool,
    #[serde(flatten)]
    pub common: CommonUnitFields,
    #[serde(flatten)]
    #[schemars(skip)]
    pub unknown_fields: DenyUnknownFields,
}

impl CommandSpec {
    /// What the kind supports for this spec: declared outputs always,
    /// teardown only with a `teardown` step.
    pub fn support(&self) -> KindSupport {
        KindSupport {
            declared_outputs: true,
            teardown: self.teardown.is_some(),
        }
    }

    pub fn validate(&self) -> Result<()> {
        validate_argv("spec.command", &self.command)?;
        for (field, argv) in [
            ("spec.verify", &self.verify),
            ("spec.teardown", &self.teardown),
            ("spec.fingerprint", &self.fingerprint),
        ] {
            if let Some(argv) = argv {
                validate_argv(field, argv)?;
            }
        }
        for (index, pattern) in self.files.iter().enumerate() {
            let field = format!("spec.files[{index}]");
            crate::local_path::validate_local_path(&field, pattern)?;
            glob::Pattern::new(pattern)
                .map_err(|error| CoreError::config(format!("Invalid {field} glob {pattern:?}: {error}")))?;
        }
        if let Some(directory) = &self.working_dir {
            if directory != "/" {
                crate::local_path::validate_local_path("spec.workingDir", directory)?;
            }
        }
        if !(self.values.is_null() || self.values.is_object()) {
            return Err(CoreError::config("spec.values must be an object"));
        }
        self.common.validate(self.support())
    }
}

fn validate_argv(field: &str, argv: &[String]) -> Result<()> {
    match argv.first() {
        Some(program) if !program.is_empty() => Ok(()),
        _ => Err(CoreError::config(format!(
            "{field} must be a non-empty argument vector whose first element names the program"
        ))),
    }
}

/// The API version of the built-in unit kinds.
pub fn is_builtin_unit_api_version(api_version: &str) -> bool {
    api_version == API_VERSION_UNITS
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn command(spec: serde_json::Value) -> Result<CommandSpec> {
        let spec: CommandSpec = serde_json::from_value(spec)?;
        spec.validate()?;
        Ok(spec)
    }

    #[test]
    fn test_command_accepts_the_contract_example() {
        let spec = command(json!({
            "files": ["../scripts/seed/**"],
            "values": {"bucket": {"fromUnit": {"unit": "storage", "output": "bucket"}}},
            "command": ["./run.sh"],
            "verify": ["./verify.sh"],
            "teardown": ["./remove.sh"],
            "fingerprint": ["./tool-versions.sh"],
            "workingDir": "../scripts/seed",
            "env": {"passthrough": ["AWS_REGION", "AWS_PROFILE"], "secrets": {"DB_PASSWORD": "database-password"}},
            "idempotent": true,
            "outputs": {"seedVersion": {"type": "string"}},
            "timeout": "10m"
        }))
        .unwrap();
        assert_eq!(spec.common.timeout, Some(Duration::from_mins(10)));
        assert_eq!(
            spec.common.effective_deletion_policy(spec.support()),
            DeletionPolicy::Teardown
        );
    }

    #[test]
    fn test_command_without_teardown_retains_and_rejects_teardown_policy() {
        let spec = command(json!({"command": ["./run.sh"]})).unwrap();
        assert_eq!(
            spec.common.effective_deletion_policy(spec.support()),
            DeletionPolicy::Retain
        );
        let error = command(json!({"command": ["./run.sh"], "deletionPolicy": "Teardown"}))
            .unwrap_err()
            .to_string();
        assert!(error.contains("cannot tear down"), "{error}");
    }

    /// Catches a timeout so large that deadline arithmetic overflows.
    #[test]
    fn test_command_rejects_a_timeout_above_seven_days() {
        assert!(command(json!({"command": ["./run.sh"], "timeout": "7d"})).is_ok());
        let error = command(json!({"command": ["./run.sh"], "timeout": "100000000d"}))
            .unwrap_err()
            .to_string();
        assert!(error.contains("spec.timeout may be at most 7d"), "{error}");
    }

    #[test]
    fn test_command_rejects_unknown_fields_next_to_flattened_common_fields() {
        let error = command(json!({"command": ["./run.sh"], "dependOn": ["network"]}))
            .unwrap_err()
            .to_string();
        assert!(error.contains("unknown field `dependOn`"), "{error}");
    }

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
