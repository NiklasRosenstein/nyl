//! Typed declarations: the closed type set, defaults, and allowed values.
//!
//! Contract: [Declarations](../../../../design/release-inputs.md#declarations).

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
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

    pub(crate) fn is_scalar(self) -> bool {
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
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "super::binding::present"
    )]
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

/// Validate the static form of the declarations at `field_prefix`, such as `spec.inputs`.
pub fn validate_declarations_at(field_prefix: &str, declarations: &BTreeMap<String, InputDeclaration>) -> Result<()> {
    for (name, declaration) in declarations {
        let field = format!("{field_prefix}.{name}");
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
    use serde_json::json;

    use super::*;

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
            let error = validate_declarations_at("spec.inputs", &declarations)
                .unwrap_err()
                .to_string();
            assert!(error.contains(expected), "{value}: {error}");
        }
        let declarations = BTreeMap::from([("Tier".to_owned(), declaration(json!({"type": "string"})))]);
        assert!(validate_declarations_at("spec.inputs", &declarations)
            .unwrap_err()
            .to_string()
            .contains("^[a-z][a-zA-Z0-9]*$"));
    }

    #[test]
    fn test_declarations_reserve_properties_and_items() {
        assert!(serde_json::from_value::<InputDeclaration>(json!({"type": "object", "properties": {}})).is_err());
        assert!(serde_json::from_value::<InputDeclaration>(json!({"type": "array", "items": {}})).is_err());
    }
}
