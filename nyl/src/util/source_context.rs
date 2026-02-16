//! Source context tracking for better error messages
//!
//! This module provides utilities to track the source file context during
//! resource parsing, enabling better error messages that include file paths
//! and field locations.

use crate::error::{NylError, Result};
use std::path::PathBuf;

/// Tracks the source file context for parsing operations
#[derive(Debug, Clone)]
pub struct SourceContext {
    /// The file path being parsed
    file_path: PathBuf,
}

impl SourceContext {
    /// Create a new source context for the given file path
    pub fn new(file_path: PathBuf) -> Self {
        Self { file_path }
    }

    /// Get the file path
    pub fn file_path(&self) -> &PathBuf {
        &self.file_path
    }

    /// Parse YAML documents with source context
    ///
    /// This wraps serde_norway parsing to provide better error messages
    /// with file context and field path information.
    pub fn parse_yaml_documents(&self, yaml: &str) -> Result<Vec<serde_json::Value>> {
        let mut documents = Vec::new();

        for doc in yaml.split("\n---\n") {
            let trimmed = doc.trim();

            // Skip empty or comment-only documents
            if trimmed.is_empty()
                || trimmed
                    .lines()
                    .all(|line| line.trim().starts_with('#') || line.trim().is_empty())
            {
                continue;
            }

            let value: serde_json::Value =
                serde_norway::from_str(trimmed).map_err(|e| self.enhance_serde_error(e, "YAML parsing"))?;

            if !value.is_null() {
                documents.push(value);
            }
        }

        Ok(documents)
    }

    /// Parse a single YAML/JSON value with source context
    pub fn parse_yaml<T>(&self, yaml: &str) -> Result<T>
    where
        T: serde::de::DeserializeOwned,
    {
        serde_norway::from_str(yaml).map_err(|e| self.enhance_serde_error(e, "resource parsing"))
    }

    /// Enhance a serde error with file context and helpful hints
    fn enhance_serde_error(&self, error: serde_norway::Error, context: &str) -> NylError {
        let error_msg = error.to_string();
        let classification = Self::classify_error(&error_msg);
        let location_suffix = if let Some(location) = error.location() {
            format!(" (line {}, column {})", location.line(), location.column())
        } else {
            String::new()
        };
        let message = format!("{} in {}{}: {}", classification, context, location_suffix, error_msg);

        // Generate helpful hints based on error type
        let hint = Self::generate_hint(&error_msg);

        NylError::resource_validation(self.file_path.display().to_string(), message, hint)
    }

    /// Classify parser errors into stable buckets for readable diagnostics.
    fn classify_error(error_msg: &str) -> &'static str {
        if Self::extract_unknown_field(error_msg).is_some() {
            return "Unknown field";
        }

        if error_msg.contains("invalid type:") {
            return "Type mismatch";
        }

        if error_msg.contains("missing field") {
            return "Missing required field";
        }

        "YAML parse error"
    }

    /// Extract field name from "unknown field `name`" error messages
    fn extract_unknown_field(error_msg: &str) -> Option<String> {
        // Pattern: "unknown field `fieldname`"
        if let Some(start) = error_msg.find("unknown field `") {
            let after_prefix = &error_msg[start + 15..];
            if let Some(end) = after_prefix.find('`') {
                return Some(after_prefix[..end].to_string());
            }
        }
        None
    }

    /// Generate helpful hints based on error message content
    fn generate_hint(error_msg: &str) -> String {
        if error_msg.contains("unknown field") {
            return "Check for typos in field names. Refer to the resource API documentation for valid fields. \
                    Common mistakes: 'char' instead of 'chart', 'vale' instead of 'value'."
                .to_string();
        }

        if error_msg.contains("invalid type") {
            return "Check that the field value matches the expected type. \
                    For example, numbers should not be quoted, booleans should be true/false."
                .to_string();
        }

        if error_msg.contains("missing field") {
            return "Ensure all required fields are present in the resource definition. \
                    Check the resource documentation for required vs optional fields."
                .to_string();
        }

        "Check the resource definition against the API reference documentation.".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_unknown_field() {
        let msg = "unknown field `unknownField`, expected one of `chart`, `release`";
        assert_eq!(
            SourceContext::extract_unknown_field(msg),
            Some("unknownField".to_string())
        );
    }

    #[test]
    fn test_classify_error_unknown_field() {
        let msg = "unknown field `xyz`";
        let result = SourceContext::classify_error(msg);
        assert_eq!(result, "Unknown field");
    }

    #[test]
    fn test_classify_error_type_mismatch() {
        let msg = "invalid type: string \"abc\", expected u32";
        let result = SourceContext::classify_error(msg);
        assert_eq!(result, "Type mismatch");
    }

    #[test]
    fn test_classify_error_missing_field() {
        let msg = "missing field `chart`";
        let result = SourceContext::classify_error(msg);
        assert_eq!(result, "Missing required field");
    }

    #[test]
    fn test_generate_hint_unknown_field() {
        let hint = SourceContext::generate_hint("unknown field `xyz`");
        assert!(hint.contains("typos"));
        assert!(hint.contains("API documentation"));
    }

    #[test]
    fn test_generate_hint_type_mismatch() {
        let hint = SourceContext::generate_hint("invalid type: string, expected u32");
        assert!(hint.contains("type"));
        assert!(hint.contains("quoted"));
    }

    #[test]
    fn test_generate_hint_missing_field() {
        let hint = SourceContext::generate_hint("missing field `chart`");
        assert!(hint.contains("required"));
        assert!(hint.contains("documentation"));
    }

    #[test]
    fn test_source_context_new() {
        let ctx = SourceContext::new(PathBuf::from("/path/to/file.yaml"));
        assert_eq!(ctx.file_path(), &PathBuf::from("/path/to/file.yaml"));
    }

    #[test]
    fn test_parse_yaml_documents_valid() {
        let ctx = SourceContext::new(PathBuf::from("test.yaml"));
        let yaml = r"
---
key: value
---
another: doc
";
        let result = ctx.parse_yaml_documents(yaml);
        if let Err(e) = &result {
            eprintln!("Parse error: {}", e);
        }
        assert!(result.is_ok());
        let docs = result.unwrap();
        assert_eq!(docs.len(), 2);
    }

    #[test]
    fn test_parse_yaml_documents_invalid() {
        let ctx = SourceContext::new(PathBuf::from("test.yaml"));
        let yaml = "invalid: yaml: content:";
        let result = ctx.parse_yaml_documents(yaml);
        assert!(result.is_err());

        let err = result.unwrap_err();
        let err_msg = format!("{}", err);
        assert!(err_msg.contains("test.yaml"));
        assert!(err_msg.contains("line"));
        assert!(err_msg.contains("Hint:"));
    }

    #[test]
    fn test_parse_yaml_preserves_raw_invalid_type_detail() {
        #[allow(dead_code)]
        #[derive(Debug, serde::Deserialize)]
        struct Typed {
            replicas: u32,
        }

        let ctx = SourceContext::new(PathBuf::from("typed.yaml"));
        let result: Result<Typed> = ctx.parse_yaml("replicas: \"abc\"");
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("Type mismatch"));
        assert!(err_msg.contains("invalid type"));
        assert!(err_msg.contains("expected u32"));
    }
}
