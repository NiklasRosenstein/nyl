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

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("YAML error: {0}")]
    Yaml(#[from] serde_norway::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Kubernetes error: {0}")]
    Kubernetes(#[from] kube::Error),

    #[error("Process execution error: {0}")]
    Process(String),

    #[error("Validation error: {0}")]
    Validation(String),

    #[error("{0}")]
    Other(String),
}

/// Result type alias for nyl operations
pub type Result<T> = std::result::Result<T, NylError>;
