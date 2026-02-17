/// Helm template command building and execution
use super::ResolvedChart;
use crate::{NylError, Result};
use std::process::Command;

/// Parameters for building a Helm template command
pub struct HelmTemplateParams<'a> {
    /// Resolved chart reference
    pub resolved: &'a ResolvedChart,
    /// Helm release name
    pub release_name: &'a str,
    /// Optional release namespace
    pub release_namespace: Option<&'a str>,
    /// Optional path to values file to pass to Helm via --values
    pub values_file: Option<&'a std::path::Path>,
}

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
    #[must_use]
    pub fn with_kube_version(mut self, version: impl Into<String>) -> Self {
        self.kube_version = Some(version.into());
        self
    }

    /// Set the API versions
    #[must_use]
    pub fn with_api_versions(mut self, versions: Vec<String>) -> Self {
        self.api_versions = versions;
        self
    }

    /// Build a Helm template command
    ///
    /// Builds the Helm template command with all necessary arguments.
    /// Used internally by template() method and for testing.
    ///
    /// # Arguments
    /// * `params` - Parameters for building the command
    ///
    /// # Returns
    /// The built Command (not yet executed)
    pub fn build_command(&self, params: HelmTemplateParams) -> Command {
        let mut cmd = Command::new("helm");
        cmd.arg("template");
        cmd.arg(params.release_name);
        cmd.arg(&params.resolved.path);

        // Add namespace if specified
        if let Some(namespace) = params.release_namespace {
            cmd.arg("--namespace");
            cmd.arg(namespace);
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

        // Add values file if provided
        if let Some(file_path) = params.values_file {
            cmd.arg("--values");
            cmd.arg(file_path);
        }

        cmd
    }

    /// Execute the helm template command
    ///
    /// Executes helm template with the given chart, release name, namespace, and values.
    /// Returns a list of rendered Kubernetes manifests as JSON values.
    pub fn template(
        &self,
        resolved: &ResolvedChart,
        release_name: &str,
        release_namespace: Option<&str>,
        values: &serde_json::Value,
    ) -> Result<Vec<serde_json::Value>> {
        tracing::debug!(
            "Rendering Helm chart: {} (release: {})",
            resolved.path.display(),
            release_name
        );

        // Write values to temp file if not empty
        let values_file = if !values.is_null() && values.as_object().is_some_and(|o| !o.is_empty()) {
            Some(write_values_file(values)?)
        } else {
            None
        };

        // Build command using shared build_command method
        let mut cmd = self.build_command(HelmTemplateParams {
            resolved,
            release_name,
            release_namespace,
            values_file: values_file.as_ref().map(|f| f.path()),
        });

        // Log the command being executed
        tracing::debug!("Executing helm command: {:?}", cmd);

        // Execute helm template
        let output = cmd
            .output()
            .map_err(|e| NylError::Process(format!("Failed to execute helm: {}", e)))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(NylError::HelmChart(format!("helm template failed: {}", stderr)));
        }

        tracing::debug!("Helm chart rendered successfully");

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

    let mut temp_file =
        tempfile::NamedTempFile::new().map_err(|e| NylError::Config(format!("Failed to create temp file: {}", e)))?;

    let yaml =
        serde_norway::to_string(values).map_err(|e| NylError::Config(format!("Failed to serialize values: {}", e)))?;

    temp_file
        .write_all(yaml.as_bytes())
        .map_err(|e| NylError::Config(format!("Failed to write values file: {}", e)))?;

    Ok(temp_file)
}

/// Parse YAML multi-document stream into JSON values
///
/// Handles Helm's output with Kubernetes-compatible scalar semantics.
fn parse_yaml_documents(yaml_str: &str) -> Result<Vec<serde_json::Value>> {
    crate::yaml::parse_yaml_documents_k8s_compatible(yaml_str).map_err(Into::into)
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
        let executor = HelmTemplateExecutor::new().with_api_versions(vec!["apps/v1".to_string(), "v1".to_string()]);
        assert_eq!(executor.api_versions, vec!["apps/v1", "v1"]);
    }

    #[test]
    fn test_build_command_basic() {
        let executor = HelmTemplateExecutor::new();

        let resolved = ResolvedChart {
            path: PathBuf::from("/charts/nginx"),
            chart_ref: ChartRef::default(),
        };

        let cmd = executor.build_command(HelmTemplateParams {
            resolved: &resolved,
            release_name: "my-release",
            release_namespace: None,
            values_file: None,
        });

        let args: Vec<String> = cmd.get_args().map(|s| s.to_string_lossy().to_string()).collect();

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

        let cmd = executor.build_command(HelmTemplateParams {
            resolved: &resolved,
            release_name: "my-release",
            release_namespace: Some("production"),
            values_file: None,
        });

        let args: Vec<String> = cmd.get_args().map(|s| s.to_string_lossy().to_string()).collect();

        assert!(args.contains(&"--namespace".to_string()));
        assert!(args.contains(&"production".to_string()));
    }

    #[test]
    fn test_build_command_with_kube_version() {
        let executor = HelmTemplateExecutor::new().with_kube_version("1.28.0");

        let resolved = ResolvedChart {
            path: PathBuf::from("/charts/nginx"),
            chart_ref: ChartRef::default(),
        };

        let cmd = executor.build_command(HelmTemplateParams {
            resolved: &resolved,
            release_name: "my-release",
            release_namespace: None,
            values_file: None,
        });

        let args: Vec<String> = cmd.get_args().map(|s| s.to_string_lossy().to_string()).collect();

        assert!(args.contains(&"--kube-version".to_string()));
        assert!(args.contains(&"1.28.0".to_string()));
    }

    #[test]
    fn test_build_command_with_api_versions() {
        let executor = HelmTemplateExecutor::new().with_api_versions(vec!["apps/v1".to_string(), "v1".to_string()]);

        let resolved = ResolvedChart {
            path: PathBuf::from("/charts/nginx"),
            chart_ref: ChartRef::default(),
        };

        let cmd = executor.build_command(HelmTemplateParams {
            resolved: &resolved,
            release_name: "my-release",
            release_namespace: None,
            values_file: None,
        });

        let args: Vec<String> = cmd.get_args().map(|s| s.to_string_lossy().to_string()).collect();

        assert!(args.contains(&"--api-versions".to_string()));
        assert!(args.contains(&"apps/v1".to_string()));
        assert!(args.contains(&"v1".to_string()));
    }

    #[test]
    fn test_build_command_with_values() {
        use std::io::Write;
        let executor = HelmTemplateExecutor::new();

        let resolved = ResolvedChart {
            path: PathBuf::from("/charts/nginx"),
            chart_ref: ChartRef::default(),
        };

        // Create a temporary values file
        let mut temp_file = tempfile::NamedTempFile::new().unwrap();
        let values_yaml = "replicaCount: 3\nimage:\n  repository: nginx\n  tag: \"1.21\"\n";
        temp_file.write_all(values_yaml.as_bytes()).unwrap();
        temp_file.flush().unwrap();

        let cmd = executor.build_command(HelmTemplateParams {
            resolved: &resolved,
            release_name: "my-release",
            release_namespace: None,
            values_file: Some(temp_file.path()),
        });

        let args: Vec<String> = cmd.get_args().map(|s| s.to_string_lossy().to_string()).collect();

        // Now uses --values with file path instead of --set-json
        assert!(args.contains(&"--values".to_string()));
    }

    #[test]
    fn test_parse_yaml_documents_single() {
        let yaml = r"
apiVersion: v1
kind: ConfigMap
metadata:
  name: test
";
        let docs = parse_yaml_documents(yaml).unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0]["kind"], "ConfigMap");
    }

    #[test]
    fn test_parse_yaml_documents_multiple() {
        let yaml = r"
apiVersion: v1
kind: ConfigMap
metadata:
  name: test1
---
apiVersion: v1
kind: Service
metadata:
  name: test2
";
        let docs = parse_yaml_documents(yaml).unwrap();
        assert_eq!(docs.len(), 2);
        assert_eq!(docs[0]["kind"], "ConfigMap");
        assert_eq!(docs[1]["kind"], "Service");
    }

    #[test]
    fn test_parse_yaml_documents_with_empty() {
        let yaml = r"
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
";
        let docs = parse_yaml_documents(yaml).unwrap();
        assert_eq!(docs.len(), 2);
        assert_eq!(docs[0]["kind"], "ConfigMap");
        assert_eq!(docs[1]["kind"], "Service");
    }

    #[test]
    fn test_parse_yaml_documents_with_comments() {
        let yaml = r"
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
";
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
    fn test_parse_yaml_documents_k8s_boolean_scalars() {
        let yaml = r"
apiVersion: v1
kind: ConfigMap
data:
  args:
    - --appendonly
    - no
";
        let docs = parse_yaml_documents(yaml).unwrap();
        assert_eq!(docs[0]["data"]["args"][0], "--appendonly");
        assert_eq!(docs[0]["data"]["args"][1], false);
    }

    #[test]
    fn test_parse_yaml_documents_k8s_quoted_boolean_like_strings() {
        let yaml = r#"
apiVersion: v1
kind: ConfigMap
data:
  args:
    - --appendonly
    - "no"
"#;
        let docs = parse_yaml_documents(yaml).unwrap();
        assert_eq!(docs[0]["data"]["args"][1], "no");
    }

    #[test]
    #[ignore = "Only run if helm is installed"]
    fn test_check_helm_installed() {
        // This test just verifies the function doesn't panic
        let _ = HelmTemplateExecutor::check_helm_installed().unwrap();
    }

    #[test]
    #[ignore = "Only run if helm is installed"]
    fn test_helm_version() {
        if HelmTemplateExecutor::check_helm_installed().unwrap() {
            let version = HelmTemplateExecutor::helm_version().unwrap();
            assert!(!version.is_empty());
            assert!(version.contains('v') || version.contains('.')); // Version format
        }
    }
}
