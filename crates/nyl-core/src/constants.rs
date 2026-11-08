//! Constants for Nyl API versions

/// API version for HelmChart, RemoteManifest, and inline rendering resources.
pub const API_VERSION: &str = "k8s.nyl/v1";

/// API version for shared Git repository resources.
pub const API_VERSION_GITOPS: &str = "gitops.nyl/v1";

/// The second format version of the state kinds that changed since
/// `gitops.nyl/v1` (`DesiredUnit` and `ObservedUnit`); the other state kinds
/// stay on [`API_VERSION_GITOPS`].
pub const API_VERSION_GITOPS_V2: &str = "gitops.nyl/v2";

/// API version for Kubernetes rendered-GitOps configuration resources.
pub const API_VERSION_K8S_GITOPS: &str = "k8s.gitops.nyl/v1";

/// API version of the built-in unit kinds.
pub const API_VERSION_UNITS: &str = "units.gitops.nyl/v1";

/// API version of the built-in artifact kinds.
pub const API_VERSION_ARTIFACTS: &str = "artifacts.gitops.nyl/v1";

/// API version for Component resources
pub const API_VERSION_COMPONENTS: &str = "components.k8s.nyl/v1";

// Labels, annotations, and Secret types use the `.nyl` API groups of the
// layer that owns them, like the resource API versions above: `k8s.nyl/` for
// Kubernetes rendering and `k8s.gitops.nyl/` for Kubernetes GitOps.

/// Label naming the release a release-state Secret records.
pub const LABEL_RELEASE: &str = "k8s.nyl/release";

/// Label carrying the revision a release-state Secret records.
pub const LABEL_REVISION: &str = "k8s.nyl/revision";

/// Secret type of release-state Secrets.
pub const SECRET_TYPE_RELEASE: &str = "k8s.nyl/release.v1";

/// Annotation recording the API version of the resource a manifest was expanded from.
pub const ANNOTATION_PARENT_API_VERSION: &str = "k8s.nyl/parent-api-version";

/// Annotation recording the kind of the resource a manifest was expanded from.
pub const ANNOTATION_PARENT_KIND: &str = "k8s.nyl/parent-kind";

/// Annotation recording the name of the resource a manifest was expanded from.
pub const ANNOTATION_PARENT_NAME: &str = "k8s.nyl/parent-name";

/// Annotation recording the namespace of the resource a manifest was expanded from.
pub const ANNOTATION_PARENT_NAMESPACE: &str = "k8s.nyl/parent-namespace";

/// Annotation declaring when Nyl applies a Kyverno policy during rendering.
pub const ANNOTATION_KYVERNO_SCOPE: &str = "k8s.nyl/apply-policy-scope";

/// Annotation carrying a Helm manifest's chart-relative source path from
/// rendering to the rendered layout, which removes it before writing.
pub const ANNOTATION_HELM_SOURCE: &str = "k8s.gitops.nyl/helm-source";

/// Annotation recording the owning ApplicationGroup and destination of a
/// published catalog Application or AppProject.
pub const ANNOTATION_PUBLICATION_CONTEXT: &str = "k8s.gitops.nyl/publication-context";

/// Identifiers from before Nyl used `.nyl` API groups.
///
/// Nyl reads them only where existing clusters, projects, or published trees
/// still carry them, and recognizes the old API versions only to name their
/// replacement. Nothing writes them.
pub mod legacy {
    /// API version of HelmChart and RemoteManifest before `k8s.nyl/v1`; rejected with a fix.
    pub const API_VERSION: &str = "nyl.niklasrosenstein.github.com/v1";

    /// API group of Nyl resources before the `.nyl` groups.
    pub const API_GROUP: &str = "nyl.niklasrosenstein.github.com";

    /// API version of component invocations before `components.k8s.nyl/v1`; rejected with a fix.
    pub const API_VERSION_COMPONENTS: &str = "components.nyl.niklasrosenstein.github.com/v1";

    /// Release label of release-state Secrets recorded before [`super::LABEL_RELEASE`]; deprecated.
    pub const LABEL_RELEASE: &str = "nyl.niklasrosenstein.github.com/release";

    /// Revision label of release-state Secrets recorded before [`super::LABEL_REVISION`]; deprecated.
    pub const LABEL_REVISION: &str = "nyl.niklasrosenstein.github.com/revision";

    /// Kyverno scope annotation before [`super::ANNOTATION_KYVERNO_SCOPE`]; deprecated.
    pub const ANNOTATION_KYVERNO_SCOPE: &str = "nyl.niklasrosenstein.github.com/apply-policy-scope";

    /// Publication context annotation in trees published before
    /// [`super::ANNOTATION_PUBLICATION_CONTEXT`]; read from baselines.
    pub const ANNOTATION_PUBLICATION_CONTEXT: &str = "nyl.niklasrosenstein.github.com/publication-context";
}
