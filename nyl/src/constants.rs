//! Constants for Nyl API versions

/// API version for Nyl resources (NylRelease, HelmChart, inline resources)
pub const API_VERSION: &str = "nyl.niklasrosenstein.github.com/v1";

/// API version for ArgoCD-related Nyl resources (ApplicationGenerator)
pub const API_VERSION_ARGOCD: &str = "argocd.nyl.niklasrosenstein.github.com/v1";

/// API version for Component resources
pub const API_VERSION_COMPONENTS: &str = "components.nyl.niklasrosenstein.github.com/v1";

/// API version for Post-processing resources (Kyverno)
pub const API_VERSION_POSTPROCESSING: &str = "post-processing.nyl.niklasrosenstein.github.com/v1";

/// Label name for release identification in Kubernetes secrets
pub const LABEL_RELEASE: &str = "nyl.niklasrosenstein.github.com/release";

/// Label name for revision tracking in Kubernetes secrets
pub const LABEL_REVISION: &str = "nyl.niklasrosenstein.github.com/revision";

/// Secret type for Nyl release state storage
pub const SECRET_TYPE_RELEASE: &str = "nyl.niklasrosenstein.github.com/release.v1";
