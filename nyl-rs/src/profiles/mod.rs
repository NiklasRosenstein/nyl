/// Profile management module
///
/// This module handles:
/// - Profile configuration with typed fields
/// - Precedence-based loading from multiple sources
/// - Kubeconfig sources (local files, SSH remote)
/// - Profile value merging

use crate::config::ProjectConfig;
use crate::{NylError, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// A profile defines environment-specific configuration
///
/// Profiles include:
/// - Values for template rendering
/// - Kubeconfig source for cluster access
/// - SSH tunnel configuration (Phase 3)
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Profile {
    /// Template values for this profile
    #[serde(default)]
    pub values: HashMap<String, serde_json::Value>,

    /// Kubeconfig source configuration
    #[serde(default)]
    pub kubeconfig: KubeconfigSource,

    /// SSH tunnel configuration (Phase 3)
    pub tunnel: Option<SshTunnel>,
}

/// Source for Kubernetes configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum KubeconfigSource {
    /// Use local kubeconfig file
    Local {
        /// Path to kubeconfig file (defaults to ~/.kube/config)
        #[serde(skip_serializing_if = "Option::is_none")]
        path: Option<PathBuf>,

        /// Kubernetes context to use
        #[serde(skip_serializing_if = "Option::is_none")]
        context: Option<String>,
    },

    /// Fetch kubeconfig from remote host via SSH
    Ssh {
        /// SSH username
        user: String,

        /// SSH hostname
        host: String,

        /// SSH port
        #[serde(default = "default_ssh_port")]
        port: u16,

        /// Path to kubeconfig on remote host
        path: String,

        /// SSH identity file (defaults to ~/.ssh/id_rsa)
        #[serde(skip_serializing_if = "Option::is_none")]
        identity_file: Option<PathBuf>,

        /// Kubernetes context to use
        #[serde(skip_serializing_if = "Option::is_none")]
        context: Option<String>,
    },
}

fn default_ssh_port() -> u16 {
    22
}

impl Default for KubeconfigSource {
    fn default() -> Self {
        Self::Local {
            path: None,
            context: None,
        }
    }
}

/// SSH tunnel configuration (Phase 3)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SshTunnel {
    /// SSH username
    pub user: String,

    /// SSH hostname
    pub host: String,

    /// SSH port
    #[serde(default = "default_ssh_port")]
    pub port: u16,

    /// Local port to bind tunnel
    pub local_port: u16,

    /// Remote host to forward to
    pub remote_host: String,

    /// Remote port to forward to
    pub remote_port: u16,

    /// SSH identity file
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identity_file: Option<PathBuf>,
}

/// Profile configuration with precedence-based loading
///
/// Profiles are loaded from multiple sources with this precedence:
/// 1. Explicit file path (highest priority)
/// 2. nyl-profiles.yaml in current/parent directories
/// 3. Profiles from nyl-project.yaml
/// 4. ~/.config/nyl/profiles.yaml (global)
#[derive(Debug)]
pub struct ProfileConfig {
    /// Path to the profile configuration file (if any)
    pub file: Option<PathBuf>,

    /// Loaded profiles
    pub profiles: HashMap<String, Profile>,
}

impl ProfileConfig {
    /// Profile configuration filenames searched in priority order
    pub const FILENAMES: &'static [&'static str] = &["nyl-profiles.yaml", "nyl-profiles.json"];

    /// Load profile configuration with precedence
    ///
    /// # Arguments
    /// * `file` - Optional explicit file path (highest priority)
    ///
    /// # Returns
    /// ProfileConfig loaded from the highest-priority source
    pub fn load(file: Option<PathBuf>) -> Result<Self> {
        // 1. If explicit file provided, use it
        if let Some(path) = file {
            return Self::load_from_file(&path);
        }

        // 2. Search for nyl-profiles.yaml in current/parent directories
        if let Some(path) = Self::find_profiles_file()? {
            return Self::load_from_file(&path);
        }

        // 3. Try to load from project config
        if let Some(config) = Self::load_from_project_config()? {
            return Ok(config);
        }

        // 4. Try global config directory
        if let Some(config) = Self::load_from_global()? {
            return Ok(config);
        }

        // No profiles found, return empty config
        Ok(Self {
            file: None,
            profiles: HashMap::new(),
        })
    }

    /// Find nyl-profiles.yaml file in current or parent directories
    fn find_profiles_file() -> Result<Option<PathBuf>> {
        crate::util::fs::find_config_file(Self::FILENAMES, None, false)
    }

    /// Load profiles from a specific file
    fn load_from_file(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Err(NylError::Config(format!(
                "Profile file does not exist: {}",
                path.display()
            )));
        }

        let contents = std::fs::read_to_string(path)?;

        let profiles: HashMap<String, Profile> =
            if path.extension().and_then(|s| s.to_str()) == Some("json") {
                serde_json::from_str(&contents).map_err(|e| {
                    NylError::Config(format!("Failed to parse profile JSON: {}", e))
                })?
            } else {
                serde_norway::from_str(&contents).map_err(|e| {
                    NylError::Config(format!("Failed to parse profile YAML: {}", e))
                })?
            };

        Ok(Self {
            file: Some(path.to_path_buf()),
            profiles,
        })
    }

    /// Load profiles from project config (nyl-project.yaml)
    fn load_from_project_config() -> Result<Option<Self>> {
        let project_config = ProjectConfig::load(None)?;

        if project_config.config.profiles.is_empty() {
            return Ok(None);
        }

        Ok(Some(Self {
            file: project_config.file,
            profiles: project_config.config.profiles,
        }))
    }

    /// Load profiles from global config directory (~/.config/nyl/profiles.yaml)
    fn load_from_global() -> Result<Option<Self>> {
        let config_dir = dirs::config_dir()
            .ok_or_else(|| NylError::Config("Could not determine config directory".to_string()))?;

        let nyl_dir = config_dir.join("nyl");
        if !nyl_dir.exists() {
            return Ok(None);
        }

        for filename in Self::FILENAMES {
            let path = nyl_dir.join(filename);
            if path.exists() {
                return Ok(Some(Self::load_from_file(&path)?));
            }
        }

        Ok(None)
    }

    /// Get a specific profile by name
    pub fn get(&self, name: &str) -> Option<&Profile> {
        self.profiles.get(name)
    }

    /// Get a mutable reference to a specific profile
    pub fn get_mut(&mut self, name: &str) -> Option<&mut Profile> {
        self.profiles.get_mut(name)
    }

    /// Check if a profile exists
    pub fn contains(&self, name: &str) -> bool {
        self.profiles.contains_key(name)
    }

    /// List all profile names
    pub fn names(&self) -> Vec<&str> {
        self.profiles.keys().map(|s| s.as_str()).collect()
    }
}

impl Profile {
    /// Create a new empty profile
    pub fn new() -> Self {
        Self::default()
    }

    /// Merge values from another profile
    ///
    /// Phase 2: Simple HashMap merge (overwrite)
    /// Phase 3: Deep merge for nested structures
    pub fn merge(&mut self, other: &Profile) {
        for (key, value) in &other.values {
            self.values.insert(key.clone(), value.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_profile_default() {
        let profile = Profile::default();
        assert!(profile.values.is_empty());
        assert!(matches!(profile.kubeconfig, KubeconfigSource::Local { .. }));
        assert!(profile.tunnel.is_none());
    }

    #[test]
    fn test_profile_deserialization_local_kubeconfig() {
        let yaml = r#"
values:
  environment: development
  replicas: 3
kubeconfig:
  type: local
  context: minikube
"#;
        let profile: Profile = serde_norway::from_str(yaml).unwrap();
        assert_eq!(profile.values.len(), 2);
        assert_eq!(profile.values["environment"], "development");
        assert_eq!(profile.values["replicas"], 3);

        match profile.kubeconfig {
            KubeconfigSource::Local { context, .. } => {
                assert_eq!(context.unwrap(), "minikube");
            }
            _ => panic!("Expected Local kubeconfig"),
        }
    }

    #[test]
    fn test_profile_deserialization_ssh_kubeconfig() {
        let yaml = r#"
values:
  environment: production
kubeconfig:
  type: ssh
  user: admin
  host: k8s-master.example.com
  port: 2222
  path: /etc/kubernetes/admin.conf
  context: prod-cluster
"#;
        let profile: Profile = serde_norway::from_str(yaml).unwrap();

        match profile.kubeconfig {
            KubeconfigSource::Ssh {
                user,
                host,
                port,
                path,
                context,
                ..
            } => {
                assert_eq!(user, "admin");
                assert_eq!(host, "k8s-master.example.com");
                assert_eq!(port, 2222);
                assert_eq!(path, "/etc/kubernetes/admin.conf");
                assert_eq!(context.unwrap(), "prod-cluster");
            }
            _ => panic!("Expected SSH kubeconfig"),
        }
    }

    #[test]
    fn test_profile_config_load_from_file() {
        let temp = TempDir::new().unwrap();
        let profiles_path = temp.path().join("nyl-profiles.yaml");

        let yaml = r#"
dev:
  values:
    environment: development
    debug: true
prod:
  values:
    environment: production
    debug: false
"#;
        fs::write(&profiles_path, yaml).unwrap();

        let config = ProfileConfig::load_from_file(&profiles_path).unwrap();
        assert_eq!(config.file, Some(profiles_path));
        assert_eq!(config.profiles.len(), 2);

        let dev = config.get("dev").unwrap();
        assert_eq!(dev.values["environment"], "development");
        assert_eq!(dev.values["debug"], true);

        let prod = config.get("prod").unwrap();
        assert_eq!(prod.values["environment"], "production");
        assert_eq!(prod.values["debug"], false);
    }

    #[test]
    fn test_profile_config_load_json() {
        let temp = TempDir::new().unwrap();
        let profiles_path = temp.path().join("nyl-profiles.json");

        let json = r#"{
  "dev": {
    "values": {
      "environment": "development"
    }
  }
}"#;
        fs::write(&profiles_path, json).unwrap();

        let config = ProfileConfig::load_from_file(&profiles_path).unwrap();
        assert_eq!(config.profiles.len(), 1);
        assert!(config.contains("dev"));
    }

    #[test]
    fn test_profile_config_not_found() {
        let temp = TempDir::new().unwrap();
        let missing = temp.path().join("missing.yaml");

        let result = ProfileConfig::load_from_file(&missing);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("does not exist"));
    }

    #[test]
    fn test_profile_config_empty_load() {
        let temp = TempDir::new().unwrap();
        std::env::set_current_dir(temp.path()).unwrap();

        let config = ProfileConfig::load(None).unwrap();
        assert!(config.file.is_none());
        assert!(config.profiles.is_empty());
    }

    #[test]
    fn test_profile_merge() {
        let mut profile1 = Profile::new();
        profile1.values.insert("key1".to_string(), serde_json::json!("value1"));
        profile1.values.insert("key2".to_string(), serde_json::json!(42));

        let mut profile2 = Profile::new();
        profile2.values.insert("key2".to_string(), serde_json::json!(99));
        profile2.values.insert("key3".to_string(), serde_json::json!(true));

        profile1.merge(&profile2);

        assert_eq!(profile1.values.len(), 3);
        assert_eq!(profile1.values["key1"], "value1");
        assert_eq!(profile1.values["key2"], 99); // Overwritten
        assert_eq!(profile1.values["key3"], true);
    }

    #[test]
    fn test_profile_config_methods() {
        let mut config = ProfileConfig {
            file: None,
            profiles: HashMap::new(),
        };

        let mut dev = Profile::new();
        dev.values.insert("env".to_string(), serde_json::json!("dev"));
        config.profiles.insert("dev".to_string(), dev);

        assert!(config.contains("dev"));
        assert!(!config.contains("prod"));

        assert_eq!(config.names(), vec!["dev"]);

        let profile = config.get("dev").unwrap();
        assert_eq!(profile.values["env"], "dev");

        config.get_mut("dev").unwrap().values.insert(
            "modified".to_string(),
            serde_json::json!(true),
        );

        assert_eq!(config.get("dev").unwrap().values["modified"], true);
    }

    #[test]
    fn test_ssh_tunnel_deserialization() {
        let yaml = r#"
user: tunneler
host: bastion.example.com
port: 22
local_port: 6443
remote_host: k8s-api.internal
remote_port: 6443
"#;
        let tunnel: SshTunnel = serde_norway::from_str(yaml).unwrap();
        assert_eq!(tunnel.user, "tunneler");
        assert_eq!(tunnel.host, "bastion.example.com");
        assert_eq!(tunnel.port, 22);
        assert_eq!(tunnel.local_port, 6443);
        assert_eq!(tunnel.remote_host, "k8s-api.internal");
        assert_eq!(tunnel.remote_port, 6443);
    }
}
