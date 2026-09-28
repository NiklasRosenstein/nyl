/// Null secret provider implementation
///
/// This provider stores secrets in memory only and provides no encryption.
/// Useful for development and testing.
use super::{SecretProvider, SecretValue};
use crate::Result;
use std::collections::HashMap;
use std::path::Path;

/// Null provider - stores secrets in memory with no encryption
///
/// This provider is primarily for testing and development. Secrets are
/// stored in a HashMap and are not persisted to disk.
#[derive(Debug, Default)]
pub struct NullProvider {
    secrets: HashMap<String, SecretValue>,
}

impl NullProvider {
    /// Create a new null provider
    pub fn new() -> Self {
        Self::default()
    }
}

impl SecretProvider for NullProvider {
    fn init(&mut self, _config_file: &Path) -> Result<()> {
        // Null provider doesn't need initialization
        Ok(())
    }

    fn keys(&self) -> Result<Vec<String>> {
        Ok(self.secrets.keys().cloned().collect())
    }

    fn get(&self, key: &str) -> Result<SecretValue> {
        self.secrets
            .get(key)
            .cloned()
            .ok_or_else(|| crate::NylError::Config(format!("Secret not found: {}", key)))
    }

    fn set(&mut self, key: &str, value: SecretValue) -> Result<()> {
        self.secrets.insert(key.to_string(), value);
        Ok(())
    }

    fn unset(&mut self, key: &str) -> Result<()> {
        self.secrets.remove(key);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_null_provider_new() {
        let provider = NullProvider::new();
        assert!(provider.secrets.is_empty());
    }

    #[test]
    fn test_null_provider_init() {
        let mut provider = NullProvider::new();
        let temp = TempDir::new().unwrap();
        let result = provider.init(&temp.path().join("test.yaml"));
        assert!(result.is_ok());
    }

    #[test]
    fn test_null_provider_set_get() {
        let mut provider = NullProvider::new();

        provider.set("key1", SecretValue::from("value1")).unwrap();
        provider.set("key2", SecretValue::from("value2")).unwrap();

        let value1 = provider.get("key1").unwrap();
        assert_eq!(value1, SecretValue::String("value1".to_string()));

        let value2 = provider.get("key2").unwrap();
        assert_eq!(value2, SecretValue::String("value2".to_string()));
    }

    #[test]
    fn test_null_provider_get_missing() {
        let provider = NullProvider::new();
        let result = provider.get("missing");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Secret not found"));
    }

    #[test]
    fn test_null_provider_keys() {
        let mut provider = NullProvider::new();
        assert!(provider.keys().unwrap().is_empty());

        provider.set("key1", SecretValue::from("value1")).unwrap();
        provider.set("key2", SecretValue::from("value2")).unwrap();

        let mut keys = provider.keys().unwrap();
        keys.sort();
        assert_eq!(keys, vec!["key1", "key2"]);
    }

    #[test]
    fn test_null_provider_unset() {
        let mut provider = NullProvider::new();

        provider.set("key1", SecretValue::from("value1")).unwrap();
        assert_eq!(provider.keys().unwrap().len(), 1);

        provider.unset("key1").unwrap();
        assert!(provider.keys().unwrap().is_empty());

        // Unsetting non-existent key should not error
        provider.unset("missing").unwrap();
    }

    #[test]
    fn test_null_provider_overwrite() {
        let mut provider = NullProvider::new();

        provider.set("key", SecretValue::from("value1")).unwrap();
        assert_eq!(provider.get("key").unwrap(), SecretValue::String("value1".to_string()));

        provider.set("key", SecretValue::from("value2")).unwrap();
        assert_eq!(provider.get("key").unwrap(), SecretValue::String("value2".to_string()));
    }
}
