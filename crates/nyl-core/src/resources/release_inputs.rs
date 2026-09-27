//! Release input declarations, DeploymentTarget bindings, and their pure rules.
//!
//! Contract: [Release inputs and bindings](../../../../design/release-inputs.md).
//!
//! - [Declarations](../../../../design/release-inputs.md#declarations): a closed
//!   type set, optional `default` and scalar `enum`, and input names usable as
//!   `inputs.<name>`.
//! - [Bindings](../../../../design/release-inputs.md#bindings): keyed by
//!   `<applicationGroup>/<release>`, each setting exactly one source. A binding
//!   replaces the Release default whole.
//!
//! Resolution of the sources, which reads files and Git, lives in the rendering
//! crate. Everything here is a pure function of the declared values.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::{CoreError, Result};

/// Declared input type. Every type is a subset of JSON Schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum InputType {
    /// A JSON string.
    String,
    /// A JSON integer; numbers with a fractional part are rejected.
    Integer,
    /// Any JSON number.
    Number,
    /// `true` or `false`.
    Boolean,
    /// A JSON object. Only the top-level type is checked.
    Object,
    /// A JSON array. Only the top-level type is checked.
    Array,
}

impl InputType {
    /// The type name as authored.
    pub fn name(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Integer => "integer",
            Self::Number => "number",
            Self::Boolean => "boolean",
            Self::Object => "object",
            Self::Array => "array",
        }
    }

    /// Whether `value` has this type. `null` satisfies no type.
    pub fn accepts(self, value: &Value) -> bool {
        match self {
            Self::String => value.is_string(),
            Self::Integer => value.is_i64() || value.is_u64(),
            Self::Number => value.is_number(),
            Self::Boolean => value.is_boolean(),
            Self::Object => value.is_object(),
            Self::Array => value.is_array(),
        }
    }

    fn is_scalar(self) -> bool {
        !matches!(self, Self::Object | Self::Array)
    }
}

/// One typed input a Release declares. Templates read its effective value as `inputs.<name>`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InputDeclaration {
    /// Value type: `string`, `integer`, `number`, `boolean`, `object`, or `array`.
    #[serde(rename = "type")]
    pub input_type: InputType,
    /// Documentation, surfaced in errors and inspection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Value used when the target binds none. Makes the input optional; must satisfy `type` and `enum`.
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "present")]
    pub default: Option<Value>,
    /// Allowed values, for scalar types only. A non-empty list of values of the declared type.
    #[serde(default, rename = "enum", skip_serializing_if = "Option::is_none")]
    pub allowed: Option<Vec<Value>>,
}

impl InputDeclaration {
    /// Check `value` against the declared type and allowed values.
    pub fn check(&self, value: &Value) -> std::result::Result<(), String> {
        if !self.input_type.accepts(value) {
            return Err(format!(
                "expected {}, got {}",
                self.input_type.name(),
                json_type_name(value)
            ));
        }
        if let Some(allowed) = &self.allowed {
            if !allowed.iter().any(|candidate| same_value(candidate, value)) {
                return Err(format!(
                    "{} is not one of the allowed values {}",
                    value,
                    Value::Array(allowed.clone())
                ));
            }
        }
        Ok(())
    }
}

/// Equality for `enum` membership: numbers compare by numeric value, so `1`
/// and `1.0` are the same allowed `number`.
fn same_value(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Number(left), Value::Number(right)) => {
            match (left.as_i64(), right.as_i64(), left.as_u64(), right.as_u64()) {
                (Some(left), Some(right), _, _) => left == right,
                (_, _, Some(left), Some(right)) => left == right,
                _ => left.as_f64() == right.as_f64(),
            }
        }
        _ => left == right,
    }
}

/// Validate the static form of `spec.inputs`.
pub fn validate_declarations(declarations: &BTreeMap<String, InputDeclaration>) -> Result<()> {
    for (name, declaration) in declarations {
        let field = format!("spec.inputs.{name}");
        validate_input_name(&field, name)?;
        if let Some(allowed) = &declaration.allowed {
            if !declaration.input_type.is_scalar() {
                return Err(CoreError::config(format!(
                    "{field}.enum is only allowed for scalar types, not {}",
                    declaration.input_type.name()
                )));
            }
            if allowed.is_empty() {
                return Err(CoreError::config(format!("{field}.enum must not be empty")));
            }
            for value in allowed {
                if !declaration.input_type.accepts(value) {
                    return Err(CoreError::config(format!(
                        "{field}.enum value {value} is not of type {}",
                        declaration.input_type.name()
                    )));
                }
            }
        }
        if let Some(default) = &declaration.default {
            declaration
                .check(default)
                .map_err(|reason| CoreError::config(format!("{field}.default is invalid: {reason}")))?;
        }
    }
    Ok(())
}

/// Input names match `^[a-z][a-zA-Z0-9]*$`, so templates can use `inputs.<name>`.
pub fn validate_input_name(field: &str, name: &str) -> Result<()> {
    let mut characters = name.chars();
    let valid = characters.next().is_some_and(|first| first.is_ascii_lowercase())
        && characters.all(|character| character.is_ascii_alphanumeric());
    if valid {
        Ok(())
    } else {
        Err(CoreError::config(format!(
            "{field}: input name {name:?} must match ^[a-z][a-zA-Z0-9]*$"
        )))
    }
}

/// A Release's identity on a target: `<applicationGroup>/<release>`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ReleaseKey {
    /// ApplicationGroup name.
    pub group: String,
    /// Release `metadata.name`.
    pub release: String,
}

impl ReleaseKey {
    /// Parse a `spec.releaseInputs` key.
    pub fn parse(key: &str) -> Result<Self> {
        match key.split_once('/') {
            Some((group, release)) if !group.is_empty() && !release.is_empty() && !release.contains('/') => Ok(Self {
                group: group.to_owned(),
                release: release.to_owned(),
            }),
            _ => Err(CoreError::config(format!(
                "spec.releaseInputs key {key:?} must have the form <applicationGroup>/<release>"
            ))),
        }
    }
}

impl std::fmt::Display for ReleaseKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}/{}", self.group, self.release)
    }
}

/// Bindings of one target, by Release key and input name.
pub type ReleaseInputBindings = BTreeMap<String, BTreeMap<String, InputBinding>>;

/// Where one Release input's value comes from. Set exactly one field.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(transform = input_binding_constraints)]
pub struct InputBinding {
    /// Inline literal value. DeploymentTargets are static, so it is never templated.
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "present")]
    pub value: Option<Value>,
    /// A YAML or JSON file of this repository.
    #[serde(default, rename = "fromFile", skip_serializing_if = "Option::is_none")]
    pub from_file: Option<FileInputSource>,
    /// A YAML or JSON file of a Git repository at a locked commit.
    #[serde(default, rename = "fromGit", skip_serializing_if = "Option::is_none")]
    pub from_git: Option<GitInputSource>,
    /// Reserved for orchestration: a recorded unit output or artifact field. Rejected outside orchestrated execution.
    #[serde(default, rename = "fromUnit", skip_serializing_if = "Option::is_none")]
    pub from_unit: Option<UnitInputSource>,
    /// Reserved for orchestration: a value recorded through a PromotionPath. Rejected outside orchestrated execution.
    #[serde(default, rename = "fromPromotion", skip_serializing_if = "Option::is_none")]
    pub from_promotion: Option<PromotionInputSource>,
}

/// The source a binding selects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindingKind {
    /// `value`
    Value,
    /// `fromFile`
    FromFile,
    /// `fromGit`
    FromGit,
    /// `fromUnit`
    FromUnit,
    /// `fromPromotion`
    FromPromotion,
}

impl BindingKind {
    /// The field name as authored.
    pub fn field(self) -> &'static str {
        match self {
            Self::Value => "value",
            Self::FromFile => "fromFile",
            Self::FromGit => "fromGit",
            Self::FromUnit => "fromUnit",
            Self::FromPromotion => "fromPromotion",
        }
    }

    /// Whether only orchestrated execution may resolve this kind.
    pub fn requires_orchestration(self) -> bool {
        matches!(self, Self::FromUnit | Self::FromPromotion)
    }
}

impl InputBinding {
    /// The single source this binding sets.
    pub fn kind(&self, field: &str) -> Result<BindingKind> {
        let set = [
            (self.value.is_some(), BindingKind::Value),
            (self.from_file.is_some(), BindingKind::FromFile),
            (self.from_git.is_some(), BindingKind::FromGit),
            (self.from_unit.is_some(), BindingKind::FromUnit),
            (self.from_promotion.is_some(), BindingKind::FromPromotion),
        ]
        .into_iter()
        .filter_map(|(present, kind)| present.then_some(kind))
        .collect::<Vec<_>>();
        match set.as_slice() {
            [kind] => Ok(*kind),
            [] => Err(CoreError::config(format!(
                "{field} must set exactly one of value, fromFile, fromGit, fromUnit, or fromPromotion"
            ))),
            _ => Err(CoreError::config(format!(
                "{field} sets {}; set exactly one source",
                set.iter().map(|kind| kind.field()).collect::<Vec<_>>().join(" and ")
            ))),
        }
    }

    /// Validate the static form of this binding.
    pub fn validate(&self, field: &str) -> Result<()> {
        match self.kind(field)? {
            BindingKind::Value | BindingKind::FromPromotion => Ok(()),
            BindingKind::FromUnit => {
                let source = self.from_unit.as_ref().expect("kind agrees with the set field");
                match (&source.output, &source.artifact) {
                    (Some(_), None) | (None, Some(_)) => Ok(()),
                    _ => Err(CoreError::config(format!(
                        "{field}.fromUnit must set exactly one of output and artifact"
                    ))),
                }
            }
            BindingKind::FromFile => {
                let source = self.from_file.as_ref().expect("kind agrees with the set field");
                crate::local_path::validate_local_path(&format!("{field}.fromFile.path"), &source.path)?;
                crate::json_pointer::validate(&format!("{field}.fromFile.pointer"), &source.pointer)
            }
            BindingKind::FromGit => self
                .from_git
                .as_ref()
                .expect("kind agrees with the set field")
                .validate(&format!("{field}.fromGit")),
        }
    }
}

/// Validate the static form of `spec.releaseInputs`.
pub fn validate_bindings(bindings: &ReleaseInputBindings) -> Result<()> {
    for (key, inputs) in bindings {
        ReleaseKey::parse(key)?;
        for (name, binding) in inputs {
            let field = format!("spec.releaseInputs.{key:?}.{name}");
            validate_input_name(&field, name)?;
            binding.validate(&field)?;
        }
    }
    Ok(())
}

/// A value read from a YAML or JSON file of this repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FileInputSource {
    /// File path relative to the directory containing `nyl.toml`, or from the Git worktree root with a leading `/`. The file must contain a single YAML or JSON document.
    pub path: String,
    /// JSON Pointer selecting the value inside the document; the whole document when empty.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub pointer: String,
}

/// A value read from a file of a Git repository at a locked commit.
///
/// Rendering reads only `commit` and never resolves `revision`; `nyl update
/// source-locks` moves the lock.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(transform = git_input_source_constraints)]
pub struct GitInputSource {
    /// Project-local `gitops.nyl/v1` GitRepository to read from.
    #[serde(rename = "repositoryRef", default, skip_serializing_if = "Option::is_none")]
    pub repository_ref: Option<super::LocalReference>,
    /// Inline repository coordinates to read from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<super::InlineGitRepository>,
    /// Human-readable branch or tag that `nyl update source-locks` resolves.
    pub revision: String,
    /// Full commit ID that rendering reads, refreshed by `nyl update source-locks`.
    pub commit: String,
    /// Repository-relative path of a YAML or JSON file holding one document.
    pub path: String,
    /// JSON Pointer selecting the value inside the document; the whole document when empty.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub pointer: String,
}

impl GitInputSource {
    /// Validate the static form of this source.
    pub fn validate(&self, field: &str) -> Result<()> {
        super::validate_repository_choice(self.repository_ref.as_ref(), self.repository.as_ref(), field)?;
        super::validate_static_required(&format!("{field}.revision"), &self.revision)?;
        super::validate_immutable_git_commit(&format!("{field}.commit"), &self.commit)?;
        super::validate_relative_path(&format!("{field}.path"), &self.path, false, false)?;
        crate::json_pointer::validate(&format!("{field}.pointer"), &self.pointer)
    }
}

fn git_input_source_constraints(schema: &mut schemars::Schema) {
    super::schema::exclusive_fields(
        schema,
        &[
            ("repositoryRef", "Reads from the named GitRepository."),
            ("repository", "Reads from the inline coordinates declared here."),
        ],
        None,
    );
}

/// Reserved orchestration reference to a unit output or artifact field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UnitInputSource {
    /// Environment of the unit, for cross-environment references.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
    /// Producing unit name.
    pub unit: String,
    /// Declared output name. Set exactly one of `output` and `artifact`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// Published artifact name. Set exactly one of `output` and `artifact`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<String>,
    /// Expected artifact kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// JSON Pointer inside the output or artifact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pointer: Option<String>,
    /// Required evidence level: `published` or `attested`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
    /// Required attestations.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attestations: Vec<String>,
}

/// Reserved orchestration reference to a promoted value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PromotionInputSource {
    /// Promoted value name, as declared by a PromotionPath.
    pub value: String,
    /// PromotionPath the value must come from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

fn input_binding_constraints(schema: &mut schemars::Schema) {
    super::schema::exclusive_fields(
        schema,
        &[
            ("value", "Binds an inline literal value."),
            (
                "fromFile",
                "Reads the value from a YAML or JSON file of this repository.",
            ),
            (
                "fromGit",
                "Reads the value from a file of a Git repository at a locked commit.",
            ),
            (
                "fromUnit",
                "Reserved for orchestration: reads a recorded unit output or artifact field.",
            ),
            (
                "fromPromotion",
                "Reserved for orchestration: reads a value recorded through a PromotionPath.",
            ),
        ],
        None,
    );
}

/// Deserialize a present field as `Some`, even when it is `null`, so `null`
/// reaches type checking instead of looking like an absent field.
fn present<'de, D>(deserializer: D) -> std::result::Result<Option<Value>, D::Error>
where
    D: Deserializer<'de>,
{
    Value::deserialize(deserializer).map(Some)
}

/// JSON type name for messages.
pub fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(number) if number.is_i64() || number.is_u64() => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn declaration(value: Value) -> InputDeclaration {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn test_input_types_follow_the_contract() {
        assert!(InputType::Integer.accepts(&json!(2)));
        assert!(!InputType::Integer.accepts(&json!(2.5)));
        assert!(InputType::Number.accepts(&json!(2.5)));
        assert!(InputType::Number.accepts(&json!(2)));
        for input_type in [
            InputType::String,
            InputType::Integer,
            InputType::Number,
            InputType::Boolean,
            InputType::Object,
            InputType::Array,
        ] {
            assert!(!input_type.accepts(&Value::Null), "{input_type:?}");
        }
    }

    #[test]
    fn test_declaration_check_applies_type_and_enum() {
        let tier = declaration(json!({"type": "string", "enum": ["small", "large"]}));
        assert!(tier.check(&json!("small")).is_ok());
        assert!(tier.check(&json!("medium")).unwrap_err().contains("allowed values"));
        assert!(tier
            .check(&json!(1))
            .unwrap_err()
            .contains("expected string, got integer"));
    }

    #[test]
    fn test_declaration_check_compares_enum_numbers_by_value() {
        let ratio = declaration(json!({"type": "number", "enum": [1, 2.5]}));
        assert!(ratio.check(&json!(1.0)).is_ok());
        assert!(ratio.check(&json!(2.5)).is_ok());
        assert!(ratio.check(&json!(1.5)).unwrap_err().contains("allowed values"));
        let count = declaration(json!({"type": "integer", "enum": [u64::MAX]}));
        assert!(count.check(&json!(u64::MAX)).is_ok());
        assert!(count.check(&json!(u64::MAX - 1)).is_err());
    }

    #[test]
    fn test_validate_declarations_rejects_invalid_defaults_enums_and_names() {
        let cases = [
            (json!({"type": "integer", "default": "2"}), "default is invalid"),
            (json!({"type": "string", "default": null}), "default is invalid"),
            (json!({"type": "object", "enum": [{}]}), "only allowed for scalar types"),
            (json!({"type": "string", "enum": []}), "must not be empty"),
            (json!({"type": "string", "enum": [1]}), "is not of type string"),
            (
                json!({"type": "string", "enum": ["a"], "default": "b"}),
                "default is invalid",
            ),
        ];
        for (value, expected) in cases {
            let declarations = BTreeMap::from([("tier".to_owned(), declaration(value.clone()))]);
            let error = validate_declarations(&declarations).unwrap_err().to_string();
            assert!(error.contains(expected), "{value}: {error}");
        }
        let declarations = BTreeMap::from([("Tier".to_owned(), declaration(json!({"type": "string"})))]);
        assert!(validate_declarations(&declarations)
            .unwrap_err()
            .to_string()
            .contains("^[a-z][a-zA-Z0-9]*$"));
    }

    #[test]
    fn test_declarations_reserve_properties_and_items() {
        assert!(serde_json::from_value::<InputDeclaration>(json!({"type": "object", "properties": {}})).is_err());
        assert!(serde_json::from_value::<InputDeclaration>(json!({"type": "array", "items": {}})).is_err());
    }

    #[test]
    fn test_binding_sets_exactly_one_source() {
        let binding: InputBinding = serde_json::from_value(json!({"value": null})).unwrap();
        assert_eq!(binding.kind("b").unwrap(), BindingKind::Value);
        let binding: InputBinding = serde_json::from_value(json!({})).unwrap();
        assert!(binding.kind("b").unwrap_err().to_string().contains("exactly one"));
        let binding: InputBinding =
            serde_json::from_value(json!({"value": 1, "fromFile": {"path": "a.yaml"}})).unwrap();
        assert!(binding
            .kind("b")
            .unwrap_err()
            .to_string()
            .contains("value and fromFile"));
        let binding: InputBinding = serde_json::from_value(json!({"fromPromotion": {"value": "image"}})).unwrap();
        assert!(binding.kind("b").unwrap().requires_orchestration());
    }

    #[test]
    fn test_validate_bindings_checks_keys_names_paths_and_pointers() {
        let cases = [
            (json!({"web": {"image": {"value": 1}}}), "<applicationGroup>/<release>"),
            (json!({"g/r": {"Image": {"value": 1}}}), "^[a-z][a-zA-Z0-9]*$"),
            (
                json!({"g/r": {"image": {"fromFile": {"path": "../x/./y.yaml"}}}}),
                "normalized path",
            ),
            (
                json!({"g/r": {"image": {"fromFile": {"path": "x.yaml", "pointer": "image"}}}}),
                "JSON Pointer",
            ),
            (
                json!({"g/r": {"image": {"fromUnit": {"unit": "db"}}}}),
                "exactly one of output and artifact",
            ),
            (
                json!({"g/r": {"image": {"fromUnit": {"unit": "db", "output": "a", "artifact": "b"}}}}),
                "exactly one of output and artifact",
            ),
        ];
        for (value, expected) in cases {
            let bindings: ReleaseInputBindings = serde_json::from_value(value.clone()).unwrap();
            let error = validate_bindings(&bindings).unwrap_err().to_string();
            assert!(error.contains(expected), "{value}: {error}");
        }
    }

    #[test]
    fn test_from_git_requires_a_repository_revision_and_full_commit() {
        let commit = "3f1c9a0000000000000000000000000000000000";
        let cases = [
            (
                json!({"revision": "main", "commit": commit, "path": "a.json"}),
                "Exactly one of",
            ),
            (
                json!({"repository": {"repoURL": "https://example.invalid/x.git"}, "revision": "main", "commit": "3f1c", "path": "a.json"}),
                "hexadecimal Git object ID",
            ),
            (
                json!({"repositoryRef": {"name": "state"}, "revision": "main", "commit": commit, "path": "../a.json"}),
                "fromGit.path",
            ),
        ];
        for (source, expected) in cases {
            let bindings: ReleaseInputBindings =
                serde_json::from_value(json!({"g/r": {"image": {"fromGit": source.clone()}}})).unwrap();
            let error = validate_bindings(&bindings).unwrap_err().to_string();
            assert!(error.contains(expected), "{source}: {error}");
        }
    }

    #[test]
    fn test_release_key_round_trips() {
        let key = ReleaseKey::parse("platform/web").unwrap();
        assert_eq!(key.group, "platform");
        assert_eq!(key.to_string(), "platform/web");
        assert!(ReleaseKey::parse("platform/web/x").is_err());
    }
}
