use thiserror::Error;

/// Main error type for nyl
#[derive(Error, Debug)]
pub enum NylError {
    #[error("Template error: {0}")]
    Template(#[from] minijinja::Error),

    #[error("Helm chart error: {0}")]
    HelmChart(String),

    #[error("Configuration error: {0}")]
    Config(String),

    #[error("Configuration file not found: {0}")]
    ConfigNotFound(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("YAML error: {0}")]
    Yaml(#[from] serde_norway::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Kubernetes error: {0}")]
    Kubernetes(String),

    #[error("Kubeconfig error: {0}")]
    Kubeconfig(#[from] kube::config::KubeconfigError),

    #[error("Kubeconfig inference error: {0}")]
    InferConfig(#[from] kube::config::InferConfigError),

    #[error("Process execution error: {0}")]
    Process(String),

    #[error("Validation error: {0}")]
    Validation(String),

    #[error("Git error: {0}")]
    Git(#[from] crate::git::GitError),

    #[error("{0}")]
    Other(String),
}

/// Result type alias for nyl operations
pub type Result<T> = std::result::Result<T, NylError>;

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
            kube::Error::HyperError(_) | kube::Error::HttpError(_) => {
                NylError::Kubernetes(format!(
                    "Connection error: {}\nHint: Check if the cluster is reachable and your kubeconfig is correct. \
                     Verify network connectivity and cluster availability.",
                    err
                ))
            }
            _ => NylError::Kubernetes(format!("Kubernetes error: {}", err)),
        }
    }
}
