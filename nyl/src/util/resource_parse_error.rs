use crate::NylError;

const MANIFEST_RESOURCE_SOURCE: &str = "<manifest resource>";
const UNKNOWN_FIELD_HINT: &str = "Check for typos and verify field names against the resource reference documentation.";

/// Convert serde_json parse errors for typed resources into consistent diagnostics.
pub fn map_resource_parse_error(resource_kind: &str, error: &serde_json::Error) -> NylError {
    let raw_error = error.to_string();

    if let Some(field_name) = extract_unknown_field_name(&raw_error) {
        let message = format!("Unknown field '{}' in {}.", field_name, resource_kind);
        return NylError::resource_validation(MANIFEST_RESOURCE_SOURCE, message, UNKNOWN_FIELD_HINT);
    }

    let cleaned_error = normalize_raw_error(&raw_error);
    let message = format!("Failed to parse {}: {}", resource_kind, cleaned_error);
    NylError::resource_validation(
        MANIFEST_RESOURCE_SOURCE,
        message,
        "Check the resource structure and value types against the resource reference documentation.",
    )
}

fn extract_unknown_field_name(error_msg: &str) -> Option<String> {
    let marker = "unknown field `";
    let start = error_msg.find(marker)?;
    let after_marker = &error_msg[start + marker.len()..];
    let end = after_marker.find('`')?;
    Some(after_marker[..end].to_string())
}

fn normalize_raw_error(error_msg: &str) -> String {
    error_msg.replace(", there are no fields", "")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use serde_json::json;

    #[test]
    fn test_unknown_field_message_is_normalized() {
        #[allow(dead_code)]
        #[derive(Debug, Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Empty {}

        let serde_err = serde_json::from_value::<Empty>(json!({
            "argocd": {}
        }))
        .unwrap_err();

        let err = map_resource_parse_error("NylRelease", &serde_err);
        let display = err.to_string();

        assert!(display.contains("Unknown field 'argocd' in NylRelease."));
        assert!(!display.contains("there are no fields"));
        assert!(display.contains("Check for typos"));
    }

    #[test]
    fn test_non_unknown_field_message_uses_fallback() {
        #[allow(dead_code)]
        #[derive(Debug, Deserialize)]
        struct Typed {
            replicas: u32,
        }

        let serde_err = serde_json::from_value::<Typed>(json!({
            "replicas": "three"
        }))
        .unwrap_err();

        let err = map_resource_parse_error("HelmChart", &serde_err);
        let display = err.to_string();

        assert!(display.contains("Failed to parse HelmChart:"));
        assert!(display.contains("invalid type"));
    }
}
