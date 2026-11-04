//! Source context tracking for better error messages
//!
//! This module provides utilities to track the source file context during
//! resource parsing, enabling better error messages that include file paths
//! and field locations.

use crate::error::{NylError, Result};
use nyl_core::resources::parse::parse_error_hint;
use std::path::PathBuf;

/// Tracks the source file context for parsing operations
#[derive(Debug, Clone)]
pub struct SourceContext {
    /// The file path being parsed
    file_path: PathBuf,
}

/// The resource parse classification, labelling other parser failures as YAML errors.
fn classify_error(error_msg: &str) -> &'static str {
    nyl_core::resources::parse::classify_parse_error(error_msg).unwrap_or("YAML parse error")
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
    /// This wraps YAML parsing to provide better error messages
    /// with file context and field path information.
    pub fn parse_yaml_documents(&self, yaml: &str) -> Result<Vec<serde_json::Value>> {
        let mut documents = crate::yaml::parse_yaml_documents_k8s_compatible(yaml)
            .map_err(|e| self.enhance_yaml_error(e, "YAML parsing"))?;
        // A deprecated API version reads as the current one everywhere after
        // loading, so it never changes rendered output.
        for document in &mut documents {
            super::deprecation::warn_deprecated_api_version(&self.file_path, document);
            nyl_core::resources::schema::canonicalize_api_version(document);
        }
        Ok(documents)
    }

    /// Parse a single YAML/JSON value with source context
    pub fn parse_yaml<T>(&self, yaml: &str) -> Result<T>
    where
        T: serde::de::DeserializeOwned,
    {
        let parsed = crate::yaml::parse_yaml_value_k8s_compatible(yaml)
            .map_err(|e| self.enhance_yaml_error(e, "resource parsing"))?;

        serde_json::from_value(parsed).map_err(|e| self.enhance_serde_json_error(e, "resource parsing"))
    }

    /// Enhance a serde error with file context and helpful hints
    fn enhance_yaml_error(&self, error: serde_saphyr::DeserializeError, context: &str) -> NylError {
        let error_msg = error.to_string();
        let classification = classify_error(&error_msg);
        let location_suffix = if let Some(location) = error.location() {
            format!(" (line {}, column {})", location.line(), location.column())
        } else {
            String::new()
        };
        let message = format!("{} in {}{}: {}", classification, context, location_suffix, error_msg);

        // Generate helpful hints based on error type
        let hint = parse_error_hint(&error_msg);

        NylError::resource_validation(self.file_path.display().to_string(), message, hint)
    }

    /// Enhance a serde_json error with file context and helpful hints.
    fn enhance_serde_json_error(&self, error: serde_json::Error, context: &str) -> NylError {
        let error_msg = error.to_string();
        let classification = classify_error(&error_msg);
        let message = format!("{} in {}: {}", classification, context, error_msg);
        let hint = parse_error_hint(&error_msg);
        NylError::resource_validation(self.file_path.display().to_string(), message, hint)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn test_parse_yaml_documents_k8s_boolean_scalar() {
        let ctx = SourceContext::new(PathBuf::from("test.yaml"));
        let yaml = r"
---
args:
  - no
";
        let docs = ctx.parse_yaml_documents(yaml).unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0]["args"][0], false);
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
