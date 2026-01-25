/// Configuration module for project settings
///
/// This module handles:
/// - ProjectConfig structure
/// - YAML/JSON loading
/// - Path resolution
use crate::profiles::Profile;
use crate::secrets::SecretProviderConfig;
use crate::util::fs::{find_config_file, resolve_paths};
use crate::{NylError, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Core project settings stored in nyl-project.yaml
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ProjectSettings {
    /// Path to the directory that contains Nyl components
    pub components_path: Option<PathBuf>,

    /// Search path for additional resources used by the project
    #[serde(default = "default_search_path")]
    pub search_path: Vec<PathBuf>,
}

fn default_search_path() -> Vec<PathBuf> {
    vec![PathBuf::from(".")]
}

impl Default for ProjectSettings {
    fn default() -> Self {
        Self {
            components_path: None,
            search_path: default_search_path(),
        }
    }
}

/// Project configuration with settings, profiles, and secrets
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Project {
    /// Project settings
    pub settings: ProjectSettings,

    /// Profiles configuration (typed in Phase 2)
    #[serde(default)]
    pub profiles: HashMap<String, Profile>,

    /// Secrets provider configuration
    #[serde(default)]
    pub secrets: Option<SecretProviderConfig>,
}

impl Default for Project {
    fn default() -> Self {
        Self {
            settings: ProjectSettings::default(),
            profiles: HashMap::new(),
            secrets: None,
        }
    }
}

/// Wrapper for project configuration file
#[derive(Debug, Clone)]
pub struct ProjectConfig {
    /// Path to the configuration file (None if using defaults)
    pub file: Option<PathBuf>,

    /// The loaded project configuration
    pub config: Project,
}

impl ProjectConfig {
    /// Config file names searched in priority order
    /// Hidden files (starting with .) are checked first to avoid conflicts with ArgoCD
    pub const FILENAMES: &'static [&'static str] = &[
        ".nyl-project.toml",
        ".nyl-project.yaml",
        ".nyl-project.json",
        "nyl-project.toml",
        "nyl-project.yaml",
        "nyl-project.json",
    ];

    /// Find the project configuration file
    ///
    /// Searches current directory and parent directories for config files.
    ///
    /// # Arguments
    /// * `cwd` - Starting directory (defaults to current working directory)
    ///
    /// # Returns
    /// * `Some(PathBuf)` - Path to config file if found
    /// * `None` - No config file found
    pub fn find(cwd: Option<&Path>) -> Result<Option<PathBuf>> {
        find_config_file(Self::FILENAMES, cwd, false)
    }

    /// Load project configuration from file
    ///
    /// If no file is specified, attempts to find one. If no file is found,
    /// returns a default configuration.
    ///
    /// # Arguments
    /// * `file` - Optional path to config file
    ///
    /// # Returns
    /// * ProjectConfig with loaded or default configuration
    pub fn load(file: Option<PathBuf>) -> Result<Self> {
        let file = match file {
            Some(f) => Some(f),
            None => Self::find(None)?,
        };

        if let Some(ref path) = file {
            Self::load_from_file(path)
        } else {
            Ok(Self {
                file: None,
                config: Project::default(),
            })
        }
    }

    /// Load project configuration with warning if no config file found
    ///
    /// This is the recommended method for commands that operate on Kubernetes
    /// resources, as it warns users when no project configuration is present.
    ///
    /// # Arguments
    /// * `file` - Optional path to config file
    ///
    /// # Returns
    /// * ProjectConfig with loaded or default configuration
    pub fn load_with_warning(file: Option<PathBuf>) -> Result<Self> {
        let file = match file {
            Some(f) => Some(f),
            None => Self::find(None)?,
        };

        if let Some(ref path) = file {
            Self::load_from_file(path)
        } else {
            eprintln!("⚠️  No project configuration file found.");
            eprintln!("   Using default settings. Initialize with 'nyl new project' to create one.");
            Ok(Self {
                file: None,
                config: Project::default(),
            })
        }
    }

    /// Load configuration from a specific file
    fn load_from_file(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Err(NylError::Config(format!(
                "Configuration file does not exist: {}",
                path.display()
            )));
        }

        let contents = std::fs::read_to_string(path)?;

        let mut project: Project = match path.extension().and_then(|s| s.to_str()) {
            Some("json") => serde_json::from_str(&contents)
                .map_err(|e| NylError::Config(format!("Failed to parse JSON config: {}", e)))?,
            Some("toml") => toml::from_str(&contents)
                .map_err(|e| NylError::Config(format!("Failed to parse TOML config: {}", e)))?,
            _ => serde_norway::from_str(&contents)
                .map_err(|e| NylError::Config(format!("Failed to parse YAML config: {}", e)))?,
        };

        // Resolve search paths relative to config file parent directory
        let base_dir = path.parent().unwrap_or_else(|| Path::new("."));
        project.settings.search_path = resolve_paths(&project.settings.search_path, base_dir);

        // Resolve components_path if it's relative
        if let Some(comp_path) = &project.settings.components_path {
            if !comp_path.is_absolute() {
                project.settings.components_path = Some(base_dir.join(comp_path));
            }
        }

        Ok(Self {
            file: Some(path.to_path_buf()),
            config: project,
        })
    }

    /// Get the components directory path
    ///
    /// Returns the configured components_path, or defaults to "components"
    /// relative to the config file (or current directory if no config file).
    pub fn get_components_path(&self) -> PathBuf {
        let base = self
            .file
            .as_ref()
            .and_then(|f| f.parent())
            .unwrap_or_else(|| Path::new("."));

        match &self.config.settings.components_path {
            Some(path) => path.clone(),
            None => base.join("components"),
        }
    }

    /// Validate the configuration
    ///
    /// Returns a list of validation warnings
    pub fn validate(&self) -> Vec<String> {
        let mut warnings = Vec::new();

        // Check if components directory exists
        let components_path = self.get_components_path();
        if !components_path.exists() {
            warnings.push(format!(
                "Components directory does not exist: {}",
                components_path.display()
            ));
        }

        // Check if search paths exist
        for path in &self.config.settings.search_path {
            if !path.exists() {
                warnings.push(format!("Search path does not exist: {}", path.display()));
            }
        }

        warnings
    }
}

#[cfg(test)]
#[allow(clippy::needless_raw_string_hashes)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_default_project_settings() {
        let settings = ProjectSettings::default();
        assert!(settings.components_path.is_none());
        assert_eq!(settings.search_path, vec![PathBuf::from(".")]);
    }

    #[test]
    fn test_load_yaml_config() {
        let temp = TempDir::new().unwrap();
        let config_path = temp.path().join("nyl-project.yaml");

        let yaml_content = r#"
settings:
  components_path: my-components
  search_path:
    - lib
    - vendor
"#;
        fs::write(&config_path, yaml_content).unwrap();

        let config = ProjectConfig::load(Some(config_path.clone())).unwrap();
        assert_eq!(config.file, Some(config_path.clone()));

        // Check that paths were resolved to absolute
        let expected_components = temp.path().join("my-components");
        assert_eq!(config.config.settings.components_path, Some(expected_components));

        assert_eq!(config.config.settings.search_path.len(), 2);
        assert!(config.config.settings.search_path[0].is_absolute());
        assert!(config.config.settings.search_path[1].is_absolute());
    }

    #[test]
    fn test_load_json_config() {
        let temp = TempDir::new().unwrap();
        let config_path = temp.path().join("nyl-project.json");

        let json_content = r#"{
  "settings": {
    "search_path": ["."]
  }
}"#;
        fs::write(&config_path, json_content).unwrap();

        let config = ProjectConfig::load(Some(config_path.clone())).unwrap();
        assert_eq!(config.file, Some(config_path));
    }

    #[test]
    fn test_load_toml_config() {
        let temp = TempDir::new().unwrap();
        let config_path = temp.path().join(".nyl-project.toml");

        let toml_content = r#"
[settings]
components_path = "my-components"
search_path = ["lib", "vendor"]
"#;
        fs::write(&config_path, toml_content).unwrap();

        let config = ProjectConfig::load(Some(config_path.clone())).unwrap();
        assert_eq!(config.file, Some(config_path.clone()));

        // Check that paths were resolved to absolute
        let expected_components = temp.path().join("my-components");
        assert_eq!(config.config.settings.components_path, Some(expected_components));

        assert_eq!(config.config.settings.search_path.len(), 2);
        assert!(config.config.settings.search_path[0].is_absolute());
        assert!(config.config.settings.search_path[1].is_absolute());
    }

    #[test]
    fn test_load_no_config_returns_defaults() {
        let temp = TempDir::new().unwrap();
        std::env::set_current_dir(temp.path()).unwrap();

        let config = ProjectConfig::load(None).unwrap();
        assert!(config.file.is_none());
        assert!(config.config.settings.components_path.is_none());
    }

    #[test]
    fn test_get_components_path_with_config() {
        let temp = TempDir::new().unwrap();
        let config_path = temp.path().join("nyl-project.yaml");

        fs::write(&config_path, "settings:\n  components_path: my-comps").unwrap();

        let config = ProjectConfig::load(Some(config_path)).unwrap();
        let components = config.get_components_path();

        assert_eq!(components, temp.path().join("my-comps"));
    }

    #[test]
    fn test_get_components_path_default() {
        let temp = TempDir::new().unwrap();
        let config_path = temp.path().join("nyl-project.yaml");

        fs::write(&config_path, "settings: {}").unwrap();

        let config = ProjectConfig::load(Some(config_path)).unwrap();
        let components = config.get_components_path();

        assert_eq!(components, temp.path().join("components"));
    }

    #[test]
    fn test_validate_missing_components_dir() {
        let config = ProjectConfig {
            file: None,
            config: Project::default(),
        };

        let warnings = config.validate();
        assert!(warnings
            .iter()
            .any(|w| w.contains("Components directory does not exist")));
    }

    #[test]
    fn test_path_resolution() {
        let temp = TempDir::new().unwrap();
        let config_path = temp.path().join("nyl-project.yaml");

        let yaml_content = r#"
settings:
  search_path:
    - relative/path
    - /absolute/path
"#;
        fs::write(&config_path, yaml_content).unwrap();

        let config = ProjectConfig::load(Some(config_path)).unwrap();

        // First path should be resolved relative to config file parent
        assert!(config.config.settings.search_path[0].is_absolute());
        assert!(config.config.settings.search_path[0].starts_with(temp.path()));

        // Second path should remain absolute
        assert_eq!(config.config.settings.search_path[1], PathBuf::from("/absolute/path"));
    }

    #[test]
    fn test_malformed_yaml() {
        let temp = TempDir::new().unwrap();
        let config_path = temp.path().join("nyl-project.yaml");

        fs::write(&config_path, "this is: not: valid: yaml:").unwrap();

        let result = ProjectConfig::load(Some(config_path));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Failed to parse YAML"));
    }

    #[test]
    fn test_malformed_json() {
        let temp = TempDir::new().unwrap();
        let config_path = temp.path().join("nyl-project.json");

        fs::write(&config_path, r#"{"invalid": json"#).unwrap();

        let result = ProjectConfig::load(Some(config_path));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Failed to parse JSON"));
    }

    #[test]
    fn test_malformed_toml() {
        let temp = TempDir::new().unwrap();
        let config_path = temp.path().join("nyl-project.toml");

        fs::write(&config_path, "[settings\ninvalid toml").unwrap();

        let result = ProjectConfig::load(Some(config_path));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Failed to parse TOML"));
    }
}
