//! Project secret provider adapter for the shared orchestration shell.

use std::path::Path;
use std::sync::OnceLock;

use nyl_core::orchestration::ports::SecretProvider;
use serde_json::Value;

use crate::secrets::SecretsConfig;

/// Lazily opens the project's secrets provider, only when a unit needs it.
/// Read-only status and fingerprinting never open a secrets file.
pub struct ProjectSecrets<'a> {
    root: &'a Path,
    config: OnceLock<Result<SecretsConfig, String>>,
}

impl<'a> ProjectSecrets<'a> {
    /// Use the secrets configuration located from this project directory.
    pub fn new(root: &'a Path) -> Self {
        Self {
            root,
            config: OnceLock::new(),
        }
    }
}

impl SecretProvider for ProjectSecrets<'_> {
    fn get(&self, key: &str) -> Result<Value, String> {
        let config = self.config.get_or_init(|| {
            SecretsConfig::load_from_dir(None, Some(self.root))
                .map_err(|_| "cannot open the project's secrets provider".to_owned())
        });
        let config = config.as_ref().map_err(Clone::clone)?;
        let value = config.get(key).map_err(|_| format!("secret {key:?} is unavailable"))?;
        serde_json::to_value(value).map_err(|_| format!("secret {key:?} cannot be encoded"))
    }
}
