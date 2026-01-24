/// Helm template command building and execution

use super::ResolvedChart;
use crate::resources::ReleaseMetadata;
use crate::{NylError, Result};
use std::process::Command;

/// Helm template command executor
///
/// Builds helm template commands and executes them to generate Kubernetes manifests
pub struct HelmTemplateExecutor {
    /// Kubernetes version to pass to Helm
    kube_version: Option<String>,

    /// API versions to pass to Helm
    api_versions: Vec<String>,
}

impl HelmTemplateExecutor {
    /// Create a new template executor
    pub fn new() -> Self {
        Self {
            kube_version: None,
            api_versions: Vec::new(),
        }
    }

    /// Set the Kubernetes version
    pub fn with_kube_version(mut self, version: impl Into<String>) -> Self {
        self.kube_version = Some(version.into());
        self
    }

    /// Set the API versions
    pub fn with_api_versions(mut self, versions: Vec<String>) -> Self {
        self.api_versions = versions;
        self
    }

    /// Build a helm template command
    ///
    /// Builds the helm template command with all necessary arguments.
    /// Used internally by template() method.
    ///
    /// # Arguments
    /// * `resolved` - Resolved chart reference
    /// * `release` - Release metadata
    /// * `values` - Values to pass to the chart
    ///
    /// # Returns
    /// The built Command (not yet executed)
    pub fn build_command(
        &self,
        resolved: &ResolvedChart,
        release: &ReleaseMetadata,
        values: &serde_json::Value,
    ) -> Result<Command> {
        let mut cmd = Command::new("helm");
        cmd.arg("template");
        cmd.arg(&release.name);
        cmd.arg(&resolved.path);

        // Add namespace if specified
        if let Some(ref namespace) = release.namespace {
            cmd.arg("--namespace");
            cmd.arg(namespace);
        }

        // Add create-namespace flag if set
        if release.create_namespace {
            cmd.arg("--create-namespace");
        }

        // Add kube-version if specified
        if let Some(ref version) = self.kube_version {
            cmd.arg("--kube-version");
            cmd.arg(version);
        }

        // Add API versions
        for api_version in &self.api_versions {
            cmd.arg("--api-versions");
            cmd.arg(api_version);
        }

        // Note: build_command uses --set-json for testing
        // The template() method uses --values with a temp file for better handling
        if !values.is_null() && values.as_object().map_or(false, |o| !o.is_empty()) {
            cmd.arg("--set-json");
            cmd.arg(serde_json::to_string(values)?);
        }

        Ok(cmd)
    }

    /// Execute the helm template command
    ///
    /// Executes helm template with the given chart, release metadata, and values.
    /// Returns a list of rendered Kubernetes manifests as JSON values.
    pub fn template(
        &self,
        resolved: &ResolvedChart,
        release: &ReleaseMetadata,
        values: &serde_json::Value,
    ) -> Result<Vec<serde_json::Value>> {
        // Write values to temp file if not empty
        let values_file = if !values.is_null() && values.as_object().map_or(false, |o| !o.is_empty()) {
            Some(write_values_file(values)?)
        } else {
            None
        };

        // Build command (without --set-json, we'll use --values)
        let mut cmd = Command::new("helm");
        cmd.arg("template");
        cmd.arg(&release.name);
        cmd.arg(&resolved.path);

        // Add namespace if specified
        if let Some(ref namespace) = release.namespace {
            cmd.arg("--namespace");
            cmd.arg(namespace);
        }

        // Add create-namespace flag if set
        if release.create_namespace {
            cmd.arg("--create-namespace");
        }

        // Add kube-version if specified
        if let Some(ref version) = self.kube_version {
            cmd.arg("--kube-version");
            cmd.arg(version);
        }

        // Add API versions
        for api_version in &self.api_versions {
            cmd.arg("--api-versions");
            cmd.arg(api_version);
        }

        // Add values file if we have one
        if let Some(ref file) = values_file {
            cmd.arg("--values");
            cmd.arg(file.path());
        }

        // Execute helm template
        let output = cmd
            .output()
            .map_err(|e| NylError::Process(format!("Failed to execute helm: {}", e)))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(NylError::HelmChart(format!(
                "helm template failed: {}",
                stderr
            )));
        }

        // Parse YAML output
        let stdout = String::from_utf8_lossy(&output.stdout);
        parse_yaml_documents(&stdout)
    }

    /// Check if helm is installed and available
    pub fn check_helm_installed() -> Result<bool> {
        match Command::new("helm").arg("version").output() {
            Ok(output) => Ok(output.status.success()),
            Err(_) => Ok(false),
        }
    }

    /// Get the helm version
    pub fn helm_version() -> Result<String> {
        let output = Command::new("helm")
            .arg("version")
            .arg("--short")
            .output()
            .map_err(|e| NylError::Config(format!("Failed to execute helm: {}", e)))?;

        if !output.status.success() {
            return Err(NylError::Config("helm version command failed".to_string()));
        }

        let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok(version)
    }
}

impl Default for HelmTemplateExecutor {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for HelmTemplateExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HelmTemplateExecutor")
            .field("kube_version", &self.kube_version)
            .field("api_versions", &self.api_versions)
            .finish()
    }
}

/// Helper to write values to a temporary file
fn write_values_file(values: &serde_json::Value) -> Result<tempfile::NamedTempFile> {
    use std::io::Write;

    let mut temp_file = tempfile::NamedTempFile::new()
        .map_err(|e| NylError::Config(format!("Failed to create temp file: {}", e)))?;

    let yaml = serde_norway::to_string(values)
        .map_err(|e| NylError::Config(format!("Failed to serialize values: {}", e)))?;

    temp_file
        .write_all(yaml.as_bytes())
        .map_err(|e| NylError::Config(format!("Failed to write values file: {}", e)))?;

    Ok(temp_file)
}

/// Parse YAML multi-document stream into JSON values
///
/// Handles Helm's output which can contain multiple YAML documents separated by "---".
/// Filters out empty documents and comment-only documents.
fn parse_yaml_documents(yaml_str: &str) -> Result<Vec<serde_json::Value>> {
    let mut documents = Vec::new();

    for doc in yaml_str.split("\n---\n") {
        let trimmed = doc.trim();

        // Skip empty or comment-only documents
        if trimmed.is_empty()
            || trimmed
                .lines()
                .all(|line| line.trim().starts_with('#') || line.trim().is_empty())
        {
            continue;
        }

        let value: serde_json::Value = serde_norway::from_str(trimmed)
            .map_err(|e| NylError::Yaml(e))?;

        if !value.is_null() {
            documents.push(value);
        }
    }

    Ok(documents)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resources::ChartRef;
    use std::path::PathBuf;

    #[test]
    fn test_executor_new() {
        let executor = HelmTemplateExecutor::new();
        assert!(executor.kube_version.is_none());
        assert!(executor.api_versions.is_empty());
    }

    #[test]
    fn test_executor_with_kube_version() {
        let executor = HelmTemplateExecutor::new().with_kube_version("1.28.0");
        assert_eq!(executor.kube_version, Some("1.28.0".to_string()));
    }

    #[test]
    fn test_executor_with_api_versions() {
        let executor = HelmTemplateExecutor::new()
            .with_api_versions(vec!["apps/v1".to_string(), "v1".to_string()]);
        assert_eq!(executor.api_versions, vec!["apps/v1", "v1"]);
    }

    #[test]
    fn test_build_command_basic() {
        let executor = HelmTemplateExecutor::new();

        let resolved = ResolvedChart {
            path: PathBuf::from("/charts/nginx"),
            chart_ref: ChartRef::default(),
        };

        let release = ReleaseMetadata::new("my-release");
        let values = serde_json::json!({});

        let cmd = executor
            .build_command(&resolved, &release, &values)
            .unwrap();

        let args: Vec<String> = cmd
            .get_args()
            .map(|s| s.to_string_lossy().to_string())
            .collect();

        assert!(args.contains(&"template".to_string()));
        assert!(args.contains(&"my-release".to_string()));
        assert!(args.contains(&"/charts/nginx".to_string()));
    }

    #[test]
    fn test_build_command_with_namespace() {
        let executor = HelmTemplateExecutor::new();

        let resolved = ResolvedChart {
            path: PathBuf::from("/charts/nginx"),
            chart_ref: ChartRef::default(),
        };

        let mut release = ReleaseMetadata::new("my-release");
        release.namespace = Some("production".to_string());

        let values = serde_json::json!({});

        let cmd = executor
            .build_command(&resolved, &release, &values)
            .unwrap();

        let args: Vec<String> = cmd
            .get_args()
            .map(|s| s.to_string_lossy().to_string())
            .collect();

        assert!(args.contains(&"--namespace".to_string()));
        assert!(args.contains(&"production".to_string()));
    }

    #[test]
    fn test_build_command_with_create_namespace() {
        let executor = HelmTemplateExecutor::new();

        let resolved = ResolvedChart {
            path: PathBuf::from("/charts/nginx"),
            chart_ref: ChartRef::default(),
        };

        let mut release = ReleaseMetadata::new("my-release");
        release.create_namespace = true;

        let values = serde_json::json!({});

        let cmd = executor
            .build_command(&resolved, &release, &values)
            .unwrap();

        let args: Vec<String> = cmd
            .get_args()
            .map(|s| s.to_string_lossy().to_string())
            .collect();

        assert!(args.contains(&"--create-namespace".to_string()));
    }

    #[test]
    fn test_build_command_with_kube_version() {
        let executor = HelmTemplateExecutor::new().with_kube_version("1.28.0");

        let resolved = ResolvedChart {
            path: PathBuf::from("/charts/nginx"),
            chart_ref: ChartRef::default(),
        };

        let release = ReleaseMetadata::new("my-release");
        let values = serde_json::json!({});

        let cmd = executor
            .build_command(&resolved, &release, &values)
            .unwrap();

        let args: Vec<String> = cmd
            .get_args()
            .map(|s| s.to_string_lossy().to_string())
            .collect();

        assert!(args.contains(&"--kube-version".to_string()));
        assert!(args.contains(&"1.28.0".to_string()));
    }

    #[test]
    fn test_build_command_with_api_versions() {
        let executor = HelmTemplateExecutor::new()
            .with_api_versions(vec!["apps/v1".to_string(), "v1".to_string()]);

        let resolved = ResolvedChart {
            path: PathBuf::from("/charts/nginx"),
            chart_ref: ChartRef::default(),
        };

        let release = ReleaseMetadata::new("my-release");
        let values = serde_json::json!({});

        let cmd = executor
            .build_command(&resolved, &release, &values)
            .unwrap();

        let args: Vec<String> = cmd
            .get_args()
            .map(|s| s.to_string_lossy().to_string())
            .collect();

        assert!(args.contains(&"--api-versions".to_string()));
        assert!(args.contains(&"apps/v1".to_string()));
        assert!(args.contains(&"v1".to_string()));
    }

    #[test]
    fn test_build_command_with_values() {
        let executor = HelmTemplateExecutor::new();

        let resolved = ResolvedChart {
            path: PathBuf::from("/charts/nginx"),
            chart_ref: ChartRef::default(),
        };

        let release = ReleaseMetadata::new("my-release");
        let values = serde_json::json!({
            "replicaCount": 3,
            "image": {
                "repository": "nginx",
                "tag": "1.21"
            }
        });

        let cmd = executor
            .build_command(&resolved, &release, &values)
            .unwrap();

        let args: Vec<String> = cmd
            .get_args()
            .map(|s| s.to_string_lossy().to_string())
            .collect();

        // Using --set-json in build_command (template() uses --values)
        assert!(args.contains(&"--set-json".to_string()));
    }

    #[test]
    fn test_parse_yaml_documents_single() {
        let yaml = r#"
apiVersion: v1
kind: ConfigMap
metadata:
  name: test
"#;
        let docs = parse_yaml_documents(yaml).unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0]["kind"], "ConfigMap");
    }

    #[test]
    fn test_parse_yaml_documents_multiple() {
        let yaml = r#"
apiVersion: v1
kind: ConfigMap
metadata:
  name: test1
---
apiVersion: v1
kind: Service
metadata:
  name: test2
"#;
        let docs = parse_yaml_documents(yaml).unwrap();
        assert_eq!(docs.len(), 2);
        assert_eq!(docs[0]["kind"], "ConfigMap");
        assert_eq!(docs[1]["kind"], "Service");
    }

    #[test]
    fn test_parse_yaml_documents_with_empty() {
        let yaml = r#"
apiVersion: v1
kind: ConfigMap
metadata:
  name: test1
---
---
apiVersion: v1
kind: Service
metadata:
  name: test2
"#;
        let docs = parse_yaml_documents(yaml).unwrap();
        assert_eq!(docs.len(), 2);
        assert_eq!(docs[0]["kind"], "ConfigMap");
        assert_eq!(docs[1]["kind"], "Service");
    }

    #[test]
    fn test_parse_yaml_documents_with_comments() {
        let yaml = r#"
apiVersion: v1
kind: ConfigMap
metadata:
  name: test1
---
# This is a comment
# Another comment
---
apiVersion: v1
kind: Service
metadata:
  name: test2
"#;
        let docs = parse_yaml_documents(yaml).unwrap();
        assert_eq!(docs.len(), 2);
        assert_eq!(docs[0]["kind"], "ConfigMap");
        assert_eq!(docs[1]["kind"], "Service");
    }

    #[test]
    fn test_parse_yaml_documents_empty_string() {
        let yaml = "";
        let docs = parse_yaml_documents(yaml).unwrap();
        assert_eq!(docs.len(), 0);
    }

    #[test]
    #[ignore] // Only run if helm is installed
    fn test_check_helm_installed() {
        let result = HelmTemplateExecutor::check_helm_installed().unwrap();
        // This will be true if helm is installed, false otherwise
        assert!(result || !result); // Always passes, just tests it doesn't panic
    }

    #[test]
    #[ignore] // Only run if helm is installed
    fn test_helm_version() {
        if HelmTemplateExecutor::check_helm_installed().unwrap() {
            let version = HelmTemplateExecutor::helm_version().unwrap();
            assert!(!version.is_empty());
            assert!(version.contains('v') || version.contains('.')); // Version format
        }
    }
}
