use thiserror::Error;

pub(crate) const API_RESOURCE_NOT_FOUND_PREFIX: &str = "API resource not found for ";

/// Main error type for nyl
#[derive(Error, Debug)]
pub enum NylError {
    #[error("Template rendering error: {0}\nHint: Check template syntax and variable names. Ensure all referenced variables are defined in your profile.")]
    Template(#[from] minijinja::Error),

    #[error("Helm chart error: {0}\nHint: Verify the chart path exists and Helm is installed. Run 'helm version' to check Helm availability.")]
    HelmChart(String),

    #[error("Configuration error: {0}\nHint: Check your nyl.toml syntax and structure. Run 'nyl validate --strict' for detailed validation.")]
    Config(String),

    #[error("Configuration file not found: {0}\nHint: Create a new project with 'nyl new project <name>' or ensure you're in a directory with a valid nyl.toml file.")]
    ConfigNotFound(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("YAML parsing error: {0}\nHint: Check YAML syntax, indentation, and special characters. Use a YAML linter to verify correctness.")]
    Yaml(#[from] serde_norway::Error),

    #[error("YAML parsing error: {0}\nHint: Check YAML syntax, indentation, and special characters. Use a YAML linter to verify correctness.")]
    YamlCompat(#[from] serde_yaml::Error),

    #[error("YAML serialization error: {0}\nHint: Check for unsupported values in rendered manifests.")]
    YamlEmit(#[from] serde_yml::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Kubernetes error: {0}")]
    Kubernetes(String),

    #[error("Kubeconfig error: {0}\nHint: Check your kubeconfig file (~/.kube/config) and ensure the current context is valid.")]
    Kubeconfig(#[from] kube::config::KubeconfigError),

    #[error("Kubeconfig inference error: {0}\nHint: Ensure KUBECONFIG environment variable is set or ~/.kube/config exists with valid configuration.")]
    InferConfig(#[from] kube::config::InferConfigError),

    #[error("Process execution error: {0}\nHint: Ensure the required tool is installed and available in PATH. Check tool-specific documentation for installation.")]
    Process(String),

    #[error("Validation error: {0}\nHint: Fix the validation issues listed above. Use 'nyl validate' to see detailed validation results.")]
    Validation(String),

    #[error("Resource validation error in {file}: {message}\nHint: {hint}")]
    ResourceValidation {
        file: String,
        message: String,
        hint: String,
    },

    #[error("Git error: {0}")]
    Git(#[from] crate::git::GitError),

    #[error("{0}")]
    Other(String),
}

/// Result type alias for nyl operations
pub type Result<T> = std::result::Result<T, NylError>;

impl NylError {
    /// Create a configuration error with context
    pub fn config(msg: impl Into<String>) -> Self {
        NylError::Config(msg.into())
    }

    /// Create a Helm chart error with context
    pub fn helm_chart(msg: impl Into<String>) -> Self {
        NylError::HelmChart(msg.into())
    }

    /// Create a process execution error with context
    pub fn process(msg: impl Into<String>) -> Self {
        NylError::Process(msg.into())
    }

    /// Create a validation error with context
    pub fn validation(msg: impl Into<String>) -> Self {
        NylError::Validation(msg.into())
    }

    /// Create a Kubernetes error with context
    pub fn kubernetes(msg: impl Into<String>) -> Self {
        NylError::Kubernetes(msg.into())
    }

    /// Create a resource validation error with file context
    pub fn resource_validation(file: impl Into<String>, message: impl Into<String>, hint: impl Into<String>) -> Self {
        NylError::ResourceValidation {
            file: file.into(),
            message: message.into(),
            hint: hint.into(),
        }
    }

    /// Returns true if this error is related to configuration
    pub fn is_config_error(&self) -> bool {
        matches!(self, NylError::Config(_) | NylError::ConfigNotFound(_))
    }

    /// Returns true if this error is related to Kubernetes operations
    pub fn is_kubernetes_error(&self) -> bool {
        matches!(
            self,
            NylError::Kubernetes(_) | NylError::Kubeconfig(_) | NylError::InferConfig(_)
        )
    }

    /// Returns true if this error is related to Git operations
    pub fn is_git_error(&self) -> bool {
        matches!(self, NylError::Git(_))
    }

    /// Returns true if this is an API discovery miss for a specific GVK.
    pub fn is_api_resource_not_found_error(&self) -> bool {
        matches!(self, NylError::Config(message) if message.starts_with(API_RESOURCE_NOT_FOUND_PREFIX))
    }
}

impl From<kube::Error> for NylError {
    fn from(err: kube::Error) -> Self {
        match &err {
            kube::Error::Api(api_err) => match api_err.code {
                403 => {
                    // RBAC permission error
                    NylError::Kubernetes(format!(
                        "Permission denied: {}\nHint: Check RBAC permissions for your service account or user. \
                         Ensure appropriate roles/bindings are in place.",
                        api_err.message
                    ))
                }
                404 => {
                    // Resource not found (CRD might not exist)
                    NylError::Kubernetes(format!(
                        "Resource not found: {}\nHint: If this is a custom resource, ensure the CRD is installed. \
                         For standard resources, check the resource name and namespace.",
                        api_err.message
                    ))
                }
                422 => {
                    // Validation error (webhook or schema)
                    NylError::Kubernetes(format!(
                        "Validation failed: {}\nHint: Check webhook logs or resource schema. \
                         The resource may violate admission controller policies or schema constraints.",
                        api_err.message
                    ))
                }
                _ => NylError::Kubernetes(format!("Kubernetes API error ({}): {}", api_err.code, api_err.message)),
            },
            kube::Error::HyperError(_) | kube::Error::HttpError(_) => NylError::Kubernetes(format!(
                "Connection error: {}\nHint: Check if the cluster is reachable and your kubeconfig is correct. \
                     Verify network connectivity and cluster availability.",
                err
            )),
            _ => NylError::Kubernetes(format!("Kubernetes error: {}", err)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_constructor_methods() {
        let config_err = NylError::config("Invalid setting");
        assert!(config_err.is_config_error());
        assert!(!config_err.is_kubernetes_error());
        assert!(!config_err.is_git_error());

        let helm_err = NylError::helm_chart("Chart not found");
        assert!(matches!(helm_err, NylError::HelmChart(_)));

        let process_err = NylError::process("Command failed");
        assert!(matches!(process_err, NylError::Process(_)));

        let validation_err = NylError::validation("Invalid value");
        assert!(matches!(validation_err, NylError::Validation(_)));

        let k8s_err = NylError::kubernetes("API error");
        assert!(k8s_err.is_kubernetes_error());
    }

    #[test]
    fn test_is_config_error() {
        assert!(NylError::Config("test".to_string()).is_config_error());
        assert!(NylError::ConfigNotFound("test".to_string()).is_config_error());
        assert!(!NylError::Process("test".to_string()).is_config_error());
    }

    #[test]
    fn test_is_kubernetes_error() {
        assert!(NylError::Kubernetes("test".to_string()).is_kubernetes_error());
        assert!(!NylError::Config("test".to_string()).is_kubernetes_error());
    }

    #[test]
    fn test_error_display_includes_hints() {
        let config_err = NylError::Config("missing field".to_string());
        let display = format!("{}", config_err);
        assert!(display.contains("Hint:"));
        assert!(display.contains("nyl validate"));

        let helm_err = NylError::HelmChart("chart error".to_string());
        let display = format!("{}", helm_err);
        assert!(display.contains("Hint:"));
        assert!(display.contains("helm version"));
    }

    #[test]
    fn test_is_api_resource_not_found_error() {
        let err = NylError::Config("API resource not found for kyverno.io/v1/ClusterPolicy".to_string());
        assert!(err.is_api_resource_not_found_error());

        let other = NylError::Config("some other config error".to_string());
        assert!(!other.is_api_resource_not_found_error());
    }

    #[test]
    fn test_resource_validation_error() {
        let err = NylError::resource_validation(
            "manifests/app.yaml",
            "Unknown field 'spec.unknownField'",
            "Check the HelmChart API reference",
        );
        let display = format!("{}", err);
        assert!(display.contains("manifests/app.yaml"));
        assert!(display.contains("Unknown field"));
        assert!(display.contains("Hint:"));
        assert!(display.contains("HelmChart API reference"));
    }
}
