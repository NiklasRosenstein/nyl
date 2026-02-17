/// Profile management module
///
/// This module handles:
/// - Profile configuration with typed fields
/// - Precedence-based loading from multiple sources
/// - Kubeconfig sources (local files, SSH remote)
/// - Profile value merging
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
/// 3. ~/.config/nyl/profiles.yaml (global)
#[derive(Debug)]
pub struct ProfileConfig {
    /// Path to the profile configuration file (if any)
    pub file: Option<PathBuf>,

    /// Loaded profiles
    pub profiles: HashMap<String, Profile>,
}

impl ProfileConfig {
    /// Profile configuration filenames searched in priority order
    /// Hidden files (starting with .) are checked first to avoid conflicts with ArgoCD
    pub const FILENAMES: &'static [&'static str] = &[
        ".nyl-profiles.yaml",
        ".nyl-profiles.json",
        "nyl-profiles.yaml",
        "nyl-profiles.json",
    ];

    /// Load profile configuration with precedence
    ///
    /// # Arguments
    /// * `file` - Optional explicit file path (highest priority)
    ///
    /// # Returns
    /// ProfileConfig loaded from the highest-priority source
    pub fn load(file: Option<PathBuf>) -> Result<Self> {
        Self::load_from_dir(file, None)
    }

    /// Load profile configuration from a specific directory context
    ///
    /// # Arguments
    /// * `file` - Optional explicit file path (highest priority)
    /// * `dir` - Optional directory to search from (defaults to current directory)
    ///
    /// # Returns
    /// ProfileConfig loaded from the highest-priority source
    pub fn load_from_dir(file: Option<PathBuf>, dir: Option<&Path>) -> Result<Self> {
        // 1. If explicit file provided, use it
        if let Some(path) = file {
            return Self::load_from_file(&path);
        }

        // 2. Search for nyl-profiles.yaml in specified/current directory or parent directories
        if let Some(path) = Self::find_profiles_file_in_dir(dir)? {
            return Self::load_from_file(&path);
        }

        // 3. Try global config directory
        if let Some(config) = Self::load_from_global()? {
            return Ok(config);
        }

        // No profiles found, return empty config
        Ok(Self {
            file: None,
            profiles: HashMap::new(),
        })
    }

    /// Find nyl-profiles.yaml file starting from a specific directory
    fn find_profiles_file_in_dir(dir: Option<&Path>) -> Result<Option<PathBuf>> {
        crate::util::fs::find_config_file(Self::FILENAMES, dir, false)
    }

    /// Load profiles from a specific file
    fn load_from_file(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Err(NylError::Config(format!(
                "Profile file does not exist: {}",
                path.display()
            )));
        }

        tracing::debug!("Reading profiles file: {}", path.display());

        let contents = std::fs::read_to_string(path)?;

        let profiles: HashMap<String, Profile> = if path.extension().and_then(|s| s.to_str()) == Some("json") {
            serde_json::from_str(&contents)
                .map_err(|e| NylError::Config(format!("Failed to parse profile JSON: {}", e)))?
        } else {
            // Use SourceContext for better YAML error messages
            let source_ctx = crate::util::SourceContext::new(path.to_path_buf());
            source_ctx.parse_yaml(&contents)?
        };

        Ok(Self {
            file: Some(path.to_path_buf()),
            profiles,
        })
    }

    /// Load profiles from global config directory (~/.config/nyl/profiles.yaml)
    fn load_from_global() -> Result<Option<Self>> {
        let config_dir =
            dirs::config_dir().ok_or_else(|| NylError::Config("Could not determine config directory".to_string()))?;

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

    /// Merge values from another profile using deep merge
    ///
    /// Rules:
    /// - Scalars: other overwrites self
    /// - Arrays: other replaces self (no append)
    /// - Objects: recursive merge
    pub fn merge(&mut self, other: &Profile) {
        for (key, value) in &other.values {
            self.values.insert(
                key.clone(),
                deep_merge_value(self.values.get(key).cloned(), value.clone()),
            );
        }
    }
}

/// Deep merge two JSON values (overlay wins over base)
///
/// Merge behavior:
/// - If base doesn't exist, use overlay
/// - Both objects: recursively merge keys
/// - Arrays or scalars: overlay replaces base
pub fn deep_merge_value(base: Option<serde_json::Value>, overlay: serde_json::Value) -> serde_json::Value {
    match (base, overlay) {
        (None, overlay) => overlay,

        // Both objects - recursive merge
        (Some(serde_json::Value::Object(mut base_map)), serde_json::Value::Object(overlay_map)) => {
            for (key, overlay_value) in overlay_map {
                let base_value = base_map.remove(&key);
                base_map.insert(key, deep_merge_value(base_value, overlay_value));
            }
            serde_json::Value::Object(base_map)
        }

        // Arrays or scalars - overlay replaces
        (Some(_base), overlay) => overlay,
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
        let yaml = r"
values:
  environment: development
  replicas: 3
kubeconfig:
  type: local
  context: minikube
";
        let profile: Profile = serde_norway::from_str(yaml).unwrap();
        assert_eq!(profile.values.len(), 2);
        assert_eq!(profile.values["environment"], "development");
        assert_eq!(profile.values["replicas"], 3);

        match profile.kubeconfig {
            KubeconfigSource::Local { context, .. } => {
                assert_eq!(context.unwrap(), "minikube");
            }
            KubeconfigSource::Ssh { .. } => panic!("Expected Local kubeconfig"),
        }
    }

    #[test]
    fn test_profile_deserialization_ssh_kubeconfig() {
        let yaml = r"
values:
  environment: production
kubeconfig:
  type: ssh
  user: admin
  host: k8s-master.example.com
  port: 2222
  path: /etc/kubernetes/admin.conf
  context: prod-cluster
";
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
            KubeconfigSource::Local { .. } => panic!("Expected SSH kubeconfig"),
        }
    }

    #[test]
    fn test_profile_config_load_from_file() {
        let temp = TempDir::new().unwrap();
        let profiles_path = temp.path().join("nyl-profiles.yaml");

        let yaml = r"
dev:
  values:
    environment: development
    debug: true
prod:
  values:
    environment: production
    debug: false
";
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
        assert!(result.unwrap_err().to_string().contains("does not exist"));
    }

    #[test]
    fn test_profile_config_empty_load() {
        let temp = TempDir::new().unwrap();

        let config = ProfileConfig::load_from_dir(None, Some(temp.path())).unwrap();
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

        config
            .get_mut("dev")
            .unwrap()
            .values
            .insert("modified".to_string(), serde_json::json!(true));

        assert_eq!(config.get("dev").unwrap().values["modified"], true);
    }

    #[test]
    fn test_ssh_tunnel_deserialization() {
        let yaml = r"
user: tunneler
host: bastion.example.com
port: 22
local_port: 6443
remote_host: k8s-api.internal
remote_port: 6443
";
        let tunnel: SshTunnel = serde_norway::from_str(yaml).unwrap();
        assert_eq!(tunnel.user, "tunneler");
        assert_eq!(tunnel.host, "bastion.example.com");
        assert_eq!(tunnel.port, 22);
        assert_eq!(tunnel.local_port, 6443);
        assert_eq!(tunnel.remote_host, "k8s-api.internal");
        assert_eq!(tunnel.remote_port, 6443);
    }

    #[test]
    fn test_deep_merge_value_none_base() {
        let overlay = serde_json::json!({"key": "value"});
        let result = deep_merge_value(None, overlay);
        assert_eq!(result, serde_json::json!({"key": "value"}));
    }

    #[test]
    fn test_deep_merge_value_scalar_replace() {
        let base = Some(serde_json::json!("old"));
        let overlay = serde_json::json!("new");
        let result = deep_merge_value(base, overlay);
        assert_eq!(result, serde_json::json!("new"));
    }

    #[test]
    fn test_deep_merge_value_array_replace() {
        let base = Some(serde_json::json!([1, 2, 3]));
        let overlay = serde_json::json!([4, 5]);
        let result = deep_merge_value(base, overlay);
        assert_eq!(result, serde_json::json!([4, 5]));
    }

    #[test]
    fn test_deep_merge_value_object_merge() {
        let base = Some(serde_json::json!({
            "a": 1,
            "b": 2,
            "nested": {
                "x": 10,
                "y": 20
            }
        }));
        let overlay = serde_json::json!({
            "b": 99,
            "c": 3,
            "nested": {
                "y": 99,
                "z": 30
            }
        });
        let result = deep_merge_value(base, overlay);
        assert_eq!(
            result,
            serde_json::json!({
                "a": 1,
                "b": 99,
                "c": 3,
                "nested": {
                    "x": 10,
                    "y": 99,
                    "z": 30
                }
            })
        );
    }

    #[test]
    fn test_deep_merge_value_deeply_nested() {
        let base = Some(serde_json::json!({
            "level1": {
                "level2": {
                    "level3": {
                        "a": 1,
                        "b": 2
                    }
                }
            }
        }));
        let overlay = serde_json::json!({
            "level1": {
                "level2": {
                    "level3": {
                        "b": 99,
                        "c": 3
                    }
                }
            }
        });
        let result = deep_merge_value(base, overlay);
        assert_eq!(
            result,
            serde_json::json!({
                "level1": {
                    "level2": {
                        "level3": {
                            "a": 1,
                            "b": 99,
                            "c": 3
                        }
                    }
                }
            })
        );
    }

    #[test]
    fn test_deep_merge_value_type_conflict() {
        // When types differ, overlay wins
        let base = Some(serde_json::json!({"key": "string"}));
        let overlay = serde_json::json!({"key": 123});
        let result = deep_merge_value(base, overlay);
        assert_eq!(result, serde_json::json!({"key": 123}));
    }

    #[test]
    fn test_profile_merge_shallow() {
        let mut base = Profile::default();
        base.values.insert("a".to_string(), serde_json::json!(1));
        base.values.insert("b".to_string(), serde_json::json!(2));

        let mut overlay = Profile::default();
        overlay.values.insert("b".to_string(), serde_json::json!(99));
        overlay.values.insert("c".to_string(), serde_json::json!(3));

        base.merge(&overlay);

        assert_eq!(base.values.get("a").unwrap(), &serde_json::json!(1));
        assert_eq!(base.values.get("b").unwrap(), &serde_json::json!(99));
        assert_eq!(base.values.get("c").unwrap(), &serde_json::json!(3));
    }

    #[test]
    fn test_profile_merge_deep() {
        let mut base = Profile::default();
        base.values.insert(
            "config".to_string(),
            serde_json::json!({
                "replicas": 1,
                "image": {
                    "repository": "nginx",
                    "tag": "1.20"
                }
            }),
        );

        let mut overlay = Profile::default();
        overlay.values.insert(
            "config".to_string(),
            serde_json::json!({
                "replicas": 3,
                "image": {
                    "tag": "1.21",
                    "pullPolicy": "Always"
                }
            }),
        );

        base.merge(&overlay);

        let expected = serde_json::json!({
            "replicas": 3,
            "image": {
                "repository": "nginx",
                "tag": "1.21",
                "pullPolicy": "Always"
            }
        });

        assert_eq!(base.values.get("config").unwrap(), &expected);
    }

    #[test]
    fn test_profile_merge_multiple() {
        let mut base = Profile::default();
        base.values.insert("a".to_string(), serde_json::json!(1));

        let mut overlay1 = Profile::default();
        overlay1.values.insert("b".to_string(), serde_json::json!(2));

        let mut overlay2 = Profile::default();
        overlay2.values.insert("c".to_string(), serde_json::json!(3));

        base.merge(&overlay1);
        base.merge(&overlay2);

        assert_eq!(base.values.get("a").unwrap(), &serde_json::json!(1));
        assert_eq!(base.values.get("b").unwrap(), &serde_json::json!(2));
        assert_eq!(base.values.get("c").unwrap(), &serde_json::json!(3));
    }
}
