/// Template engine module using MiniJinja
///
/// This module will handle:
/// - Template rendering with MiniJinja
/// - Custom filters and functions
/// - Template context management
use minijinja::Environment;

use crate::{profiles::Profile, secrets::SecretsConfig, Result};

pub struct TemplateEngine {
    env: Environment<'static>,
}

impl TemplateEngine {
    /// Create a new template engine with custom filters
    pub fn new() -> Self {
        let mut env = Environment::new();

        // Register custom filters
        env.add_filter("b64encode", filters::b64encode);
        env.add_filter("b64decode", filters::b64decode);

        Self { env }
    }

    /// Render a template with the given context
    pub fn render(&self, template: &str, context: &serde_json::Value) -> Result<String> {
        let tmpl = self.env.template_from_str(template)?;
        let result = tmpl.render(context)?;
        Ok(result)
    }
}

impl Default for TemplateEngine {
    fn default() -> Self {
        Self::new()
    }
}

/// Template context for rendering
pub struct TemplateContext {
    pub values: serde_json::Value,
    pub secrets: serde_json::Value,
    pub environment: String,
}

impl TemplateContext {
    /// Build a template context from profile, secrets, and environment name
    pub fn build(profile: &Profile, secrets: &SecretsConfig, env_name: &str) -> Result<Self> {
        let values = serde_json::to_value(&profile.values)?;

        let secret_keys = secrets.keys()?;
        let mut secrets_map = serde_json::Map::new();
        for key in secret_keys {
            let value = secrets.get(&key)?;
            secrets_map.insert(key.clone(), secret_value_to_json(&value));
        }

        Ok(Self {
            values,
            secrets: serde_json::Value::Object(secrets_map),
            environment: env_name.to_string(),
        })
    }

    /// Convert context to JSON value for template rendering
    pub fn to_json(&self) -> serde_json::Value {
        let env = Self::filter_env_vars(std::env::vars());

        serde_json::json!({
            "values": self.values,
            "secrets": self.secrets,
            "environment": self.environment,
            "env": env,
        })
    }

    /// Filter environment variables to only include NYL_ prefixed ones.
    ///
    /// This prevents accidental leakage of sensitive CI/runtime secrets into manifests.
    fn filter_env_vars<I>(vars: I) -> serde_json::Map<String, serde_json::Value>
    where
        I: Iterator<Item = (String, String)>,
    {
        vars.filter(|(k, _)| k.starts_with("NYL_"))
            .map(|(k, v)| (k, serde_json::Value::String(v)))
            .collect()
    }
}

/// Convert secret value to JSON
fn secret_value_to_json(value: &crate::secrets::SecretValue) -> serde_json::Value {
    match value {
        crate::secrets::SecretValue::String(s) => serde_json::Value::String(s.clone()),
        crate::secrets::SecretValue::Object(obj) => {
            let map: serde_json::Map<String, serde_json::Value> =
                obj.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            serde_json::Value::Object(map)
        }
        crate::secrets::SecretValue::Array(arr) => serde_json::Value::Array(arr.clone()),
        crate::secrets::SecretValue::Value(v) => v.clone(),
    }
}

/// Custom template filters
mod filters {
    use base64::{engine::general_purpose, Engine as _};

    /// Base64 encode a string
    pub fn b64encode(value: String) -> Result<String, minijinja::Error> {
        Ok(general_purpose::STANDARD.encode(value.as_bytes()))
    }

    /// Base64 decode a string
    pub fn b64decode(value: String) -> Result<String, minijinja::Error> {
        let decoded = general_purpose::STANDARD.decode(value.as_bytes()).map_err(|e| {
            minijinja::Error::new(
                minijinja::ErrorKind::InvalidOperation,
                format!("Base64 decode failed: {}", e),
            )
        })?;
        String::from_utf8(decoded).map_err(|e| {
            minijinja::Error::new(
                minijinja::ErrorKind::InvalidOperation,
                format!("UTF-8 decode failed: {}", e),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_b64encode_filter() {
        let engine = TemplateEngine::new();
        let context = serde_json::json!({
            "text": "hello world"
        });
        let result = engine.render("{{ text | b64encode }}", &context).unwrap();
        assert_eq!(result, "aGVsbG8gd29ybGQ=");
    }

    #[test]
    fn test_b64decode_filter() {
        let engine = TemplateEngine::new();
        let context = serde_json::json!({
            "encoded": "aGVsbG8gd29ybGQ="
        });
        let result = engine.render("{{ encoded | b64decode }}", &context).unwrap();
        assert_eq!(result, "hello world");
    }

    #[test]
    fn test_b64_roundtrip() {
        let engine = TemplateEngine::new();
        let context = serde_json::json!({
            "text": "The quick brown fox"
        });
        let result = engine.render("{{ text | b64encode | b64decode }}", &context).unwrap();
        assert_eq!(result, "The quick brown fox");
    }

    #[test]
    fn test_render_with_context() {
        let engine = TemplateEngine::new();
        let context = serde_json::json!({
            "name": "world",
            "count": 42
        });
        let result = engine.render("Hello {{ name }}! Count: {{ count }}", &context).unwrap();
        assert_eq!(result, "Hello world! Count: 42");
    }

    #[test]
    fn test_invalid_base64_decode() {
        let engine = TemplateEngine::new();
        let context = serde_json::json!({
            "bad": "not-valid-base64!!!"
        });
        let result = engine.render("{{ bad | b64decode }}", &context);
        assert!(result.is_err());
    }

    #[test]
    fn test_filter_env_vars() {
        // Create mock environment variables without touching global state
        let mock_vars = vec![
            ("NYL_TEST_VAR".to_string(), "visible".to_string()),
            ("SECRET_KEY".to_string(), "should_not_be_visible".to_string()),
            ("NYL_ANOTHER".to_string(), "also_visible".to_string()),
            ("PATH".to_string(), "/usr/bin".to_string()),
            ("NYL_CONFIG".to_string(), "test_config".to_string()),
        ];

        let filtered = TemplateContext::filter_env_vars(mock_vars.into_iter());

        // Check that only NYL_ prefixed vars are included
        assert_eq!(filtered.len(), 3);
        assert!(filtered.contains_key("NYL_TEST_VAR"));
        assert_eq!(filtered.get("NYL_TEST_VAR").unwrap().as_str().unwrap(), "visible");
        assert!(filtered.contains_key("NYL_ANOTHER"));
        assert_eq!(filtered.get("NYL_ANOTHER").unwrap().as_str().unwrap(), "also_visible");
        assert!(filtered.contains_key("NYL_CONFIG"));
        assert_eq!(filtered.get("NYL_CONFIG").unwrap().as_str().unwrap(), "test_config");

        // Verify non-NYL_ prefixed vars are excluded
        assert!(!filtered.contains_key("SECRET_KEY"));
        assert!(!filtered.contains_key("PATH"));
    }
}
