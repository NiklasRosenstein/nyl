//! Constants for Nyl API versions

/// API version for HelmChart, RemoteManifest, and inline rendering resources.
pub const API_VERSION: &str = "k8s.nyl/v1";

/// API version for shared Git repository resources.
pub const API_VERSION_GITOPS: &str = "gitops.nyl/v1";

/// API version for Kubernetes rendered-GitOps configuration resources.
pub const API_VERSION_K8S_GITOPS: &str = "k8s.gitops.nyl/v1";

/// API version for Component resources
pub const API_VERSION_COMPONENTS: &str = "components.k8s.nyl/v1";

/// Label name for release identification in Kubernetes secrets
pub const LABEL_RELEASE: &str = "nyl.niklasrosenstein.github.com/release";

/// Label name for revision tracking in Kubernetes secrets
pub const LABEL_REVISION: &str = "nyl.niklasrosenstein.github.com/revision";

/// Secret type for Nyl release state storage
pub const SECRET_TYPE_RELEASE: &str = "nyl.niklasrosenstein.github.com/release.v1";

/// Annotation for tracking parent resource API version
pub const ANNOTATION_PARENT_API_VERSION: &str = "nyl.niklasrosenstein.github.com/parent-api-version";

/// Annotation for tracking parent resource kind
pub const ANNOTATION_PARENT_KIND: &str = "nyl.niklasrosenstein.github.com/parent-kind";

/// Annotation for tracking parent resource name
pub const ANNOTATION_PARENT_NAME: &str = "nyl.niklasrosenstein.github.com/parent-name";

/// Annotation for tracking parent resource namespace
pub const ANNOTATION_PARENT_NAMESPACE: &str = "nyl.niklasrosenstein.github.com/parent-namespace";

/// Annotation for declaring Kyverno policy scope
pub const ANNOTATION_KYVERNO_SCOPE: &str = "nyl.niklasrosenstein.github.com/apply-policy-scope";
