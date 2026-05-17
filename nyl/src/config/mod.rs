/// Configuration module for project settings
///
/// This module handles:
/// - `nyl.toml` loading
/// - Path resolution
/// - JSON schema generation for `nyl.toml`
pub mod schema;

use crate::util::fs::{find_config_file, resolve_paths};
use crate::{NylError, Result};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn default_components_search_paths() -> Vec<PathBuf> {
    vec![PathBuf::from("components")]
}

fn default_helm_chart_search_paths() -> Vec<PathBuf> {
    vec![PathBuf::from(".")]
}

fn default_aliases() -> BTreeMap<String, String> {
    BTreeMap::new()
}

fn default_profile_values() -> BTreeMap<String, serde_json::Value> {
    BTreeMap::new()
}

/// Controls when empty `metadata.labels` maps are stripped from emitted manifests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StripEmptyMetadataLabelsMode {
    /// Always strip empty `metadata.labels` maps from emitted manifests.
    #[default]
    Always,

    /// Never strip empty `metadata.labels` maps from emitted manifests.
    Never,

    /// Strip empty `metadata.labels` maps only when running in an ArgoCD environment.
    Argocd,
}

impl StripEmptyMetadataLabelsMode {
    /// Return whether empty metadata labels should be stripped for the current environment.
    pub fn should_strip(self, is_argocd: bool) -> bool {
        match self {
            Self::Always => true,
            Self::Never => false,
            Self::Argocd => is_argocd,
        }
    }
}

/// Kubernetes target metadata used for offline rendering.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct KubernetesTarget {
    /// Kubernetes server version to pass to Helm during offline rendering.
    pub kube_version: Option<String>,

    /// Kubernetes API versions to pass to Helm during offline rendering.
    pub api_versions: Vec<String>,
}

/// Project settings in `[project]` section of `nyl.toml`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ProjectSettings {
    /// Search paths for local component charts.
    pub components_search_paths: Vec<PathBuf>,

    /// Search paths for Helm chart names.
    pub helm_chart_search_paths: Vec<PathBuf>,

    /// Aliases for component-like resources keyed as `<apiVersion>/<kind>`.
    /// Values are component kind targets (local component path or remote shortcut URL).
    pub aliases: BTreeMap<String, String>,

    /// Control when empty `metadata.labels` maps are stripped from emitted manifests.
    pub strip_empty_metadata_labels: StripEmptyMetadataLabelsMode,

    /// Default Kubernetes target metadata for offline rendering.
    pub kubernetes: KubernetesTarget,
}

impl Default for ProjectSettings {
    fn default() -> Self {
        Self {
            components_search_paths: default_components_search_paths(),
            helm_chart_search_paths: default_helm_chart_search_paths(),
            aliases: default_aliases(),
            strip_empty_metadata_labels: StripEmptyMetadataLabelsMode::default(),
            kubernetes: KubernetesTarget::default(),
        }
    }
}

/// Profile configuration in `[profile.<name>]` section of `nyl.toml`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ProfileSettings {
    /// Template values exposed as `values.*` for this profile.
    ///
    /// Example:
    /// `[profile.dev.values]`
    /// `replicas = 2`
    pub values: BTreeMap<String, serde_json::Value>,

    /// Kubernetes target metadata for offline rendering with this profile.
    pub kubernetes: KubernetesTarget,
}

impl Default for ProfileSettings {
    fn default() -> Self {
        Self {
            values: default_profile_values(),
            kubernetes: KubernetesTarget::default(),
        }
    }
}

/// Root structure of `nyl.toml`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ProjectFile {
    pub project: ProjectSettings,
    pub profile: BTreeMap<String, ProfileSettings>,
}

/// Wrapper for project configuration file.
#[derive(Debug, Clone)]
pub struct ProjectConfig {
    /// Path to the configuration file (None if using defaults).
    pub file: Option<PathBuf>,

    /// The loaded project configuration.
    pub config: ProjectFile,
}

impl ProjectConfig {
    /// Config file names searched in priority order.
    pub const FILENAMES: &'static [&'static str] = &["nyl.toml"];

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

    /// Load project configuration from file.
    ///
    /// # Arguments
    /// * `file` - Optional path to config file
    ///
    /// # Returns
    /// * ProjectConfig with loaded or default configuration
    pub fn load(file: Option<PathBuf>) -> Result<Self> {
        Self::load_from_dir(file, None)
    }

    /// Load project configuration from file in a specific directory context.
    ///
    /// # Arguments
    /// * `file` - Optional path to config file
    /// * `dir` - Optional directory to search from (defaults to current directory)
    ///
    /// # Returns
    /// * ProjectConfig with loaded or default configuration
    pub fn load_from_dir(file: Option<PathBuf>, dir: Option<&Path>) -> Result<Self> {
        let file = match file {
            Some(f) => Some(f),
            None => Self::find(dir)?,
        };

        if let Some(ref path) = file {
            Self::load_from_file(path)
        } else {
            Ok(Self {
                file: None,
                config: ProjectFile::default(),
            })
        }
    }

    /// Load project configuration with warning if no config file found.
    ///
    /// # Arguments
    /// * `file` - Optional path to config file
    ///
    /// # Returns
    /// * ProjectConfig with loaded or default configuration
    pub fn load_with_warning(file: Option<PathBuf>) -> Result<Self> {
        Self::load_with_warning_from_dir(file, None)
    }

    /// Load project configuration with warning from a specific directory context
    ///
    /// # Arguments
    /// * `file` - Optional path to config file
    /// * `dir` - Optional directory to search from
    ///
    /// # Returns
    /// * ProjectConfig with loaded or default configuration
    pub fn load_with_warning_from_dir(file: Option<PathBuf>, dir: Option<&Path>) -> Result<Self> {
        let file = match file {
            Some(f) => Some(f),
            None => Self::find(dir)?,
        };

        if let Some(ref path) = file {
            Self::load_from_file(path)
        } else {
            tracing::warn!("No project configuration file found");
            tracing::info!("Using default settings. Initialize with 'nyl new project' to create one.");
            Ok(Self {
                file: None,
                config: ProjectFile::default(),
            })
        }
    }

    /// Load configuration from a specific file.
    fn load_from_file(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Err(NylError::Config(format!(
                "Configuration file does not exist: {}",
                path.display()
            )));
        }

        if path.file_name().and_then(|s| s.to_str()) != Some("nyl.toml") {
            return Err(NylError::Config(format!(
                "Unsupported project configuration file '{}'. Use 'nyl.toml'.",
                path.display()
            )));
        }

        if path.extension().and_then(|s| s.to_str()) != Some("toml") {
            return Err(NylError::Config(format!(
                "Unsupported project configuration format for '{}'. Only TOML is supported.",
                path.display()
            )));
        }

        tracing::debug!("Reading configuration file: {}", path.display());

        let contents = std::fs::read_to_string(path)?;
        let mut project: ProjectFile =
            toml::from_str(&contents).map_err(|e| NylError::Config(format!("Failed to parse TOML config: {}", e)))?;

        // Resolve paths relative to config file parent directory.
        let base_dir = path.parent().unwrap_or_else(|| Path::new("."));
        project.project.components_search_paths = resolve_paths(&project.project.components_search_paths, base_dir);
        project.project.helm_chart_search_paths = resolve_paths(&project.project.helm_chart_search_paths, base_dir);

        Ok(Self {
            file: Some(path.to_path_buf()),
            config: project,
        })
    }

    /// Component roots from configuration.
    pub fn get_components_search_paths(&self) -> &[PathBuf] {
        &self.config.project.components_search_paths
    }

    /// Helm chart search paths from configuration.
    pub fn get_helm_chart_search_paths(&self) -> &[PathBuf] {
        &self.config.project.helm_chart_search_paths
    }

    /// Return alias target for a fully qualified key (`<apiVersion>/<kind>`).
    pub fn get_alias_target(&self, key: &str) -> Option<&str> {
        self.config.project.aliases.get(key).map(String::as_str)
    }

    /// Return alias target for an apiVersion/kind pair.
    pub fn get_alias_target_for_kind(&self, api_version: &str, kind: &str) -> Option<&str> {
        let key = format!("{}/{}", api_version, kind);
        self.get_alias_target(&key)
    }

    /// Return the configured empty-label stripping mode for emitted manifests.
    pub fn get_strip_empty_metadata_labels_mode(&self) -> StripEmptyMetadataLabelsMode {
        self.config.project.strip_empty_metadata_labels
    }

    /// Return whether any profile values are configured.
    pub fn has_profiles(&self) -> bool {
        !self.config.profile.is_empty()
    }

    /// Return configured profile names.
    pub fn profile_names(&self) -> Vec<&str> {
        self.config.profile.keys().map(String::as_str).collect()
    }

    /// Return profile settings for a profile name.
    pub fn get_profile(&self, name: &str) -> Option<&ProfileSettings> {
        self.config.profile.get(name)
    }

    /// Return template values for a profile name.
    pub fn get_profile_values(&self, name: &str) -> Option<&BTreeMap<String, serde_json::Value>> {
        self.get_profile(name).map(|profile| &profile.values)
    }

    /// Return project-level Kubernetes target metadata for offline rendering.
    pub fn get_project_kubernetes_target(&self) -> &KubernetesTarget {
        &self.config.project.kubernetes
    }

    /// Return profile-level Kubernetes target metadata for offline rendering.
    pub fn get_profile_kubernetes_target(&self, name: &str) -> Option<&KubernetesTarget> {
        self.get_profile(name).map(|profile| &profile.kubernetes)
    }

    /// Resolve a local component kind (`<apiVersion>/<kind>`) to a chart directory.
    pub fn resolve_component_chart_dir(&self, kind: &str) -> Result<PathBuf> {
        for root in self.get_components_search_paths() {
            let chart_dir = root.join(kind);
            if chart_dir.join("Chart.yaml").exists() {
                return Ok(chart_dir);
            }
        }

        Err(NylError::Config(format!(
            "Component '{}' was not found in configured components_search_paths",
            kind
        )))
    }

    /// Validate the configuration
    ///
    /// Returns a list of validation warnings
    pub fn validate(&self) -> Vec<String> {
        let mut warnings = Vec::new();

        for path in self.get_components_search_paths() {
            if !path.exists() {
                warnings.push(format!("Components search path does not exist: {}", path.display()));
            }
        }

        for path in self.get_helm_chart_search_paths() {
            if !path.exists() {
                warnings.push(format!("Helm chart search path does not exist: {}", path.display()));
            }
        }

        for (key, target) in &self.config.project.aliases {
            if !key.contains('/') {
                warnings.push(format!(
                    "Alias key '{}' is invalid; expected '<apiVersion>/<kind>'",
                    key
                ));
            }
            if target.trim().is_empty() {
                warnings.push(format!("Alias '{}' has an empty target", key));
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
        assert_eq!(settings.components_search_paths, vec![PathBuf::from("components")]);
        assert_eq!(settings.helm_chart_search_paths, vec![PathBuf::from(".")]);
        assert!(settings.aliases.is_empty());
        assert_eq!(
            settings.strip_empty_metadata_labels,
            StripEmptyMetadataLabelsMode::Always
        );
        assert!(settings.kubernetes.kube_version.is_none());
        assert!(settings.kubernetes.api_versions.is_empty());
    }

    #[test]
    fn test_default_profile_settings() {
        let settings = ProfileSettings::default();
        assert!(settings.values.is_empty());
        assert!(settings.kubernetes.kube_version.is_none());
        assert!(settings.kubernetes.api_versions.is_empty());
    }

    #[test]
    fn test_load_toml_config() {
        let temp = TempDir::new().unwrap();
        let config_path = temp.path().join("nyl.toml");

        let toml_content = r#"
[project]
components_search_paths = ["my-components"]
helm_chart_search_paths = ["lib", "vendor"]
strip_empty_metadata_labels = "argocd"
[project.kubernetes]
kube_version = "1.30.0"
api_versions = ["v1", "apps/v1"]
[project.aliases]
"myapi.io/v1/MyKind" = "oci://registry-1.docker.io/bitnamicharts/nginx@18.2.4"
"#;
        fs::write(&config_path, toml_content).unwrap();

        let config = ProjectConfig::load(Some(config_path.clone())).unwrap();
        assert_eq!(config.file, Some(config_path.clone()));

        assert_eq!(config.get_components_search_paths().len(), 1);
        assert_eq!(config.get_helm_chart_search_paths().len(), 2);
        assert_eq!(
            config.get_alias_target_for_kind("myapi.io/v1", "MyKind"),
            Some("oci://registry-1.docker.io/bitnamicharts/nginx@18.2.4")
        );
        assert_eq!(
            config.get_strip_empty_metadata_labels_mode(),
            StripEmptyMetadataLabelsMode::Argocd
        );
        assert_eq!(
            config.get_project_kubernetes_target().kube_version.as_deref(),
            Some("1.30.0")
        );
        assert_eq!(
            config.get_project_kubernetes_target().api_versions,
            vec!["v1".to_string(), "apps/v1".to_string()]
        );
        assert!(config.get_components_search_paths()[0].is_absolute());
        assert!(config.get_helm_chart_search_paths()[0].is_absolute());
        assert!(config.get_helm_chart_search_paths()[1].is_absolute());
    }

    #[test]
    fn test_load_no_config_returns_defaults() {
        let temp = TempDir::new().unwrap();

        let config = ProjectConfig::load_from_dir(None, Some(temp.path())).unwrap();
        assert!(config.file.is_none());
        assert_eq!(config.get_components_search_paths(), &[PathBuf::from("components")]);
        assert_eq!(config.get_helm_chart_search_paths(), &[PathBuf::from(".")]);
        assert!(config.config.project.aliases.is_empty());
        assert_eq!(
            config.get_strip_empty_metadata_labels_mode(),
            StripEmptyMetadataLabelsMode::Always
        );
        assert!(!config.has_profiles());
    }

    #[test]
    fn test_validate_aliases() {
        let config = ProjectConfig {
            file: None,
            config: ProjectFile {
                project: ProjectSettings {
                    aliases: BTreeMap::from([
                        ("bad-key".to_string(), "oci://example.com/app@1.0.0".to_string()),
                        ("good.io/v1/Empty".to_string(), "   ".to_string()),
                    ]),
                    ..ProjectSettings::default()
                },
                profile: BTreeMap::new(),
            },
        };

        let warnings = config.validate();
        assert!(warnings.iter().any(|w| w.contains("Alias key 'bad-key' is invalid")));
        assert!(warnings.iter().any(|w| w.contains("has an empty target")));
    }

    #[test]
    fn test_resolve_component_chart_dir() {
        let temp = TempDir::new().unwrap();
        let root1 = temp.path().join("comps1");
        let root2 = temp.path().join("comps2");
        fs::create_dir_all(root1.join("v1.example.io/WebApp")).unwrap();
        fs::create_dir_all(root2.join("v1.example.io/WebApp")).unwrap();
        fs::write(root2.join("v1.example.io/WebApp/Chart.yaml"), "apiVersion = \"v2\"").unwrap();

        let config_path = temp.path().join("nyl.toml");
        fs::write(
            &config_path,
            r#"[project]
components_search_paths = ["comps1", "comps2"]
"#,
        )
        .unwrap();

        let config = ProjectConfig::load(Some(config_path)).unwrap();
        let resolved = config.resolve_component_chart_dir("v1.example.io/WebApp").unwrap();
        assert_eq!(resolved, root2.join("v1.example.io/WebApp"));
    }

    #[test]
    fn test_find_uses_nyl_toml_only() {
        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join("nyl-project.yaml"), "settings: {}").unwrap();
        fs::write(temp.path().join("nyl.toml"), "[project]").unwrap();

        let found = ProjectConfig::find(Some(temp.path())).unwrap();
        assert_eq!(found, Some(temp.path().join("nyl.toml")));
    }

    #[test]
    fn test_validate_missing_paths() {
        let config = ProjectConfig {
            file: None,
            config: ProjectFile::default(),
        };

        let warnings = config.validate();
        assert!(warnings
            .iter()
            .any(|w| w.contains("Components search path does not exist")));
    }

    #[test]
    fn test_malformed_toml() {
        let temp = TempDir::new().unwrap();
        let config_path = temp.path().join("nyl.toml");

        fs::write(&config_path, "[project\ninvalid toml").unwrap();

        let result = ProjectConfig::load(Some(config_path));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Failed to parse TOML"));
    }

    #[test]
    fn test_reject_legacy_filename() {
        let temp = TempDir::new().unwrap();
        let config_path = temp.path().join("nyl-project.toml");
        fs::write(&config_path, "[project]").unwrap();

        let result = ProjectConfig::load(Some(config_path));
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Unsupported project configuration file"));
    }

    #[test]
    fn test_load_profile_values_from_toml() {
        let temp = TempDir::new().unwrap();
        let config_path = temp.path().join("nyl.toml");

        let toml_content = r#"
[project]

[profile.dev.values]
replicas = 1
image_tag = "dev-latest"

[profile.prod.values]
replicas = 3
image_tag = "v1.0.0"

[profile.prod.kubernetes]
kube_version = "1.30.0"
api_versions = ["v1", "apps/v1"]
"#;
        fs::write(&config_path, toml_content).unwrap();

        let config = ProjectConfig::load(Some(config_path)).unwrap();
        assert!(config.has_profiles());
        assert_eq!(config.profile_names().len(), 2);
        assert_eq!(
            config.get_profile_values("dev").and_then(|v| v.get("replicas")),
            Some(&serde_json::json!(1))
        );
        assert_eq!(
            config
                .get_profile_kubernetes_target("prod")
                .and_then(|target| target.kube_version.as_deref()),
            Some("1.30.0")
        );
        assert_eq!(
            config
                .get_profile_kubernetes_target("prod")
                .map(|target| target.api_versions.as_slice()),
            Some(["v1".to_string(), "apps/v1".to_string()].as_slice())
        );
    }

    #[test]
    fn test_profile_invalid_shape_rejected() {
        let temp = TempDir::new().unwrap();
        let config_path = temp.path().join("nyl.toml");

        let toml_content = r#"
[project]

[profile.dev]
replicas = 1
"#;
        fs::write(&config_path, toml_content).unwrap();

        let err = ProjectConfig::load(Some(config_path)).unwrap_err().to_string();
        assert!(err.contains("Failed to parse TOML config"));
    }

    #[test]
    fn test_invalid_strip_empty_metadata_labels_mode_rejected() {
        let temp = TempDir::new().unwrap();
        let config_path = temp.path().join("nyl.toml");

        let toml_content = r#"
[project]
strip_empty_metadata_labels = "sometimes"
"#;
        fs::write(&config_path, toml_content).unwrap();

        let err = ProjectConfig::load(Some(config_path)).unwrap_err().to_string();
        assert!(err.contains("Failed to parse TOML config"));
        assert!(err.contains("strip_empty_metadata_labels"));
    }
}
