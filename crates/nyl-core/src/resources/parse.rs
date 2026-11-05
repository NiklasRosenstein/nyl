//! Typed resource parsing and its diagnostics.
//!
//! Every place that deserializes a resource document reports a mismatch the
//! same way: the resource kind, the path of the offending field, a stable
//! classification, and a hint on how to fix it. File parsing in nyl-render
//! uses the same classification and hints.

use crate::error::{CoreError, Result};

/// Deserialize a typed resource from an already loaded document.
///
/// A mismatch fails with [`CoreError::ResourceParse`], which names the field
/// path (for example `spec.chart.version`) when the failure is below the
/// document root.
pub fn parse_resource<T>(value: &serde_json::Value, kind: &str) -> Result<T>
where
    T: serde::de::DeserializeOwned,
{
    serde_path_to_error::deserialize(value).map_err(|error| {
        let path = error.path().to_string();
        let detail = error.into_inner().to_string();
        let location = if path == "." {
            String::new()
        } else {
            format!(" at `{path}`")
        };
        CoreError::ResourceParse {
            kind: kind.to_string(),
            message: format!(
                "{}{location}: {detail}",
                classify_parse_error(&detail).unwrap_or("Parse error")
            ),
            hint: parse_error_hint(&detail).to_string(),
        }
    })
}

/// Classify a deserializer message into a stable, readable bucket, or `None`
/// when it is none of the recognized resource mismatches.
pub fn classify_parse_error(message: &str) -> Option<&'static str> {
    if message.contains("unknown field `") {
        Some("Unknown field")
    } else if message.contains("invalid type:") {
        Some("Type mismatch")
    } else if message.contains("missing field") {
        Some("Missing required field")
    } else {
        None
    }
}

/// The fix hint for a deserializer message.
pub fn parse_error_hint(message: &str) -> &'static str {
    if message.contains("unknown field") {
        "Check for typos in field names. Refer to the resource API documentation for valid fields. \
         Common mistakes: 'char' instead of 'chart', 'vale' instead of 'value'."
    } else if message.contains("invalid type") {
        "Check that the field value matches the expected type. \
         For example, numbers should not be quoted, booleans should be true/false."
    } else if message.contains("missing field") {
        "Ensure all required fields are present in the resource definition. \
         Check the resource documentation for required vs optional fields."
    } else {
        "Check the resource definition against the API reference documentation."
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use serde_json::json;

    #[allow(dead_code)]
    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Chart {
        name: String,
        version: Option<String>,
    }

    #[allow(dead_code)]
    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Spec {
        chart: Chart,
        values: Option<serde_json::Value>,
    }

    #[allow(dead_code)]
    #[derive(Debug, Deserialize)]
    struct Resource {
        spec: Spec,
    }

    fn parse_error(value: serde_json::Value) -> (String, String) {
        match parse_resource::<Resource>(&value, "HelmChart").unwrap_err() {
            CoreError::ResourceParse { kind, message, hint } => {
                assert_eq!(kind, "HelmChart");
                (message, hint)
            }
            other => panic!("expected a resource parse error, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_resource_unknown_field_names_its_path() {
        let (message, hint) = parse_error(json!({"spec": {"chart": {"name": "web"}, "vaules": {}}}));
        assert!(
            message.starts_with("Unknown field at `spec.vaules`: unknown field `vaules`"),
            "{message}"
        );
        assert!(hint.contains("typos"));
    }

    #[test]
    fn test_parse_resource_type_mismatch_names_its_path() {
        let (message, hint) = parse_error(json!({"spec": {"chart": {"name": "web", "version": [1, 2]}}}));
        assert!(
            message.starts_with("Type mismatch at `spec.chart.version`:"),
            "{message}"
        );
        assert!(hint.contains("expected type"));
    }

    #[test]
    fn test_parse_resource_missing_field_names_its_parent() {
        let (message, hint) = parse_error(json!({"spec": {"chart": {}}}));
        assert!(
            message.starts_with("Missing required field at `spec.chart`: missing field `name`"),
            "{message}"
        );
        assert!(hint.contains("required"));
    }

    #[test]
    fn test_parse_resource_root_failure_has_no_path() {
        let (message, _) = parse_error(json!("not an object"));
        assert!(message.starts_with("Type mismatch: invalid type"), "{message}");
    }
}
