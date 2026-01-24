/// Secrets management module
///
/// This module provides a pluggable secret provider system for managing
/// sensitive configuration values.
///
/// Phase 2: NullProvider only
/// Phase 3: SOPS and Kubernetes providers

use crate::{NylError, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

mod null;
pub use null::NullProvider;

/// A secret value that can be a string or structured data
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SecretValue {
    /// Simple string secret
    String(String),

    /// Structured secret (JSON object)
    Object(HashMap<String, serde_json::Value>),

    /// Array secret
    Array(Vec<serde_json::Value>),

    /// Other JSON value
    Value(serde_json::Value),
}

impl From<String> for SecretValue {
    fn from(s: String) -> Self {
        SecretValue::String(s)
    }
}

impl From<&str> for SecretValue {
    fn from(s: &str) -> Self {
        SecretValue::String(s.to_string())
    }
}

/// Trait for secret providers
///
/// Secret providers manage encrypted or external secret storage
pub trait SecretProvider: Send + Sync {
    /// Initialize the provider with a configuration file
    ///
    /// # Arguments
    /// * `config_file` - Path to the secrets configuration file
    fn init(&mut self, config_file: &Path) -> Result<()>;

    /// List all available secret keys
    fn keys(&self) -> Result<Vec<String>>;

    /// Get a secret value by key
    ///
    /// # Arguments
    /// * `key` - Secret key to retrieve
    fn get(&self, key: &str) -> Result<SecretValue>;

    /// Set a secret value
    ///
    /// # Arguments
    /// * `key` - Secret key to set
    /// * `value` - Secret value to store
    fn set(&mut self, key: &str, value: SecretValue) -> Result<()>;

    /// Remove a secret
    ///
    /// # Arguments
    /// * `key` - Secret key to remove
    fn unset(&mut self, key: &str) -> Result<()>;
}

/// Configuration for secret providers
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum SecretProviderConfig {
    /// Null provider (no encryption, in-memory only)
    Null(NullProviderConfig),

    // Phase 3: Additional providers
    // Sops(SopsProviderConfig),
    // Kubernetes(KubernetesProviderConfig),
}

/// Configuration for the null provider
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NullProviderConfig {
    // No configuration needed for null provider
}

impl Default for SecretProviderConfig {
    fn default() -> Self {
        SecretProviderConfig::Null(NullProviderConfig::default())
    }
}

/// Secrets configuration manager
///
/// Manages loading and accessing secret providers
pub struct SecretsConfig {
    /// Path to the secrets configuration file
    pub file: Option<PathBuf>,

    /// The configured secret provider
    provider: Box<dyn SecretProvider>,
}

impl std::fmt::Debug for SecretsConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretsConfig")
            .field("file", &self.file)
            .field("provider", &"<SecretProvider>")
            .finish()
    }
}

impl SecretsConfig {
    /// Secrets configuration filenames in priority order
    pub const FILENAMES: &'static [&'static str] = &["nyl-secrets.yaml", "nyl-secrets.json"];

    /// Load secrets configuration
    ///
    /// # Arguments
    /// * `file` - Optional explicit file path
    ///
    /// # Returns
    /// SecretsConfig with initialized provider
    pub fn load(file: Option<PathBuf>) -> Result<Self> {
        let file = match file {
            Some(f) => Some(f),
            None => Self::find_secrets_file()?,
        };

        if let Some(ref path) = file {
            Self::load_from_file(path)
        } else {
            // No secrets file found, use default (Null provider)
            Ok(Self {
                file: None,
                provider: Box::new(NullProvider::new()),
            })
        }
    }

    /// Find secrets configuration file
    fn find_secrets_file() -> Result<Option<PathBuf>> {
        crate::util::fs::find_config_file(Self::FILENAMES, None, false)
    }

    /// Load secrets configuration from file
    fn load_from_file(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Err(NylError::Config(format!(
                "Secrets file does not exist: {}",
                path.display()
            )));
        }

        let contents = std::fs::read_to_string(path)?;

        let config: SecretProviderConfig =
            if path.extension().and_then(|s| s.to_str()) == Some("json") {
                serde_json::from_str(&contents).map_err(|e| {
                    NylError::Config(format!("Failed to parse secrets JSON: {}", e))
                })?
            } else {
                serde_norway::from_str(&contents).map_err(|e| {
                    NylError::Config(format!("Failed to parse secrets YAML: {}", e))
                })?
            };

        let mut provider = Self::create_provider(&config)?;
        provider.init(path)?;

        Ok(Self {
            file: Some(path.to_path_buf()),
            provider,
        })
    }

    /// Create a provider instance from configuration
    fn create_provider(config: &SecretProviderConfig) -> Result<Box<dyn SecretProvider>> {
        match config {
            SecretProviderConfig::Null(_) => Ok(Box::new(NullProvider::new())),
            // Phase 3: Add other providers
        }
    }

    /// Get a secret value
    pub fn get(&self, key: &str) -> Result<SecretValue> {
        self.provider.get(key)
    }

    /// Set a secret value
    pub fn set(&mut self, key: &str, value: SecretValue) -> Result<()> {
        self.provider.set(key, value)
    }

    /// Remove a secret
    pub fn unset(&mut self, key: &str) -> Result<()> {
        self.provider.unset(key)
    }

    /// List all secret keys
    pub fn keys(&self) -> Result<Vec<String>> {
        self.provider.keys()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_secret_value_string() {
        let value = SecretValue::from("test-secret");
        match value {
            SecretValue::String(s) => assert_eq!(s, "test-secret"),
            _ => panic!("Expected String variant"),
        }
    }

    #[test]
    fn test_secret_value_deserialization() {
        let json = r#""simple-string""#;
        let value: SecretValue = serde_json::from_str(json).unwrap();
        assert_eq!(value, SecretValue::String("simple-string".to_string()));

        let json = r#"{"key": "value"}"#;
        let value: SecretValue = serde_json::from_str(json).unwrap();
        match value {
            SecretValue::Object(map) => {
                assert_eq!(map.get("key").unwrap(), "value");
            }
            _ => panic!("Expected Object variant"),
        }
    }

    #[test]
    fn test_secrets_config_default() {
        let temp = TempDir::new().unwrap();
        std::env::set_current_dir(temp.path()).unwrap();

        let config = SecretsConfig::load(None).unwrap();
        assert!(config.file.is_none());

        // Default provider should be NullProvider - it returns empty keys
        assert!(config.keys().unwrap().is_empty());
    }

    #[test]
    fn test_secrets_config_load_null_provider() {
        let temp = TempDir::new().unwrap();
        let secrets_path = temp.path().join("nyl-secrets.yaml");

        fs::write(&secrets_path, "type: null\n").unwrap();

        let config = SecretsConfig::load_from_file(&secrets_path).unwrap();
        assert_eq!(config.file, Some(secrets_path));
        assert!(config.keys().unwrap().is_empty());
    }

    #[test]
    fn test_secrets_config_operations() {
        let mut config = SecretsConfig {
            file: None,
            provider: Box::new(NullProvider::new()),
        };

        // Set a secret
        config
            .set("api_key", SecretValue::from("secret-123"))
            .unwrap();

        // Get the secret
        let value = config.get("api_key").unwrap();
        assert_eq!(value, SecretValue::String("secret-123".to_string()));

        // List keys
        let keys = config.keys().unwrap();
        assert_eq!(keys, vec!["api_key"]);

        // Unset the secret
        config.unset("api_key").unwrap();
        assert!(config.keys().unwrap().is_empty());
    }

    #[test]
    fn test_secrets_config_missing_file() {
        let temp = TempDir::new().unwrap();
        let missing = temp.path().join("missing.yaml");

        let result = SecretsConfig::load_from_file(&missing);
        assert!(result.is_err());
    }

    #[test]
    fn test_secret_provider_config_deserialization() {
        let yaml = "type: null\n";
        let config: SecretProviderConfig = serde_norway::from_str(yaml).unwrap();
        assert!(matches!(config, SecretProviderConfig::Null(_)));
    }
}
