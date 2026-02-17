use crate::resources::path_glob::path_matches_glob;

/// Known ArgoCD Application field-path patterns.
///
/// This inventory is periodically updated from the ArgoCD Application CRD.
/// Paths use dotted notation with quoted segments where needed.
const VALID_APPLICATION_PATH_PATTERNS: &[&str] = &[
    "apiVersion",
    "kind",
    "metadata.name",
    "metadata.namespace",
    "metadata.labels.**",
    "metadata.annotations.**",
    "metadata.finalizers.**",
    "spec.project",
    "spec.source.repoURL",
    "spec.source.path",
    "spec.source.targetRevision",
    "spec.source.chart",
    "spec.source.helm.**",
    "spec.source.kustomize.**",
    "spec.source.directory.**",
    "spec.source.jsonnet.**",
    "spec.source.plugin.name",
    "spec.source.plugin.env.**",
    "spec.sources.**",
    "spec.destination.server",
    "spec.destination.name",
    "spec.destination.namespace",
    "spec.syncPolicy.**",
    "spec.ignoreDifferences.**",
    "spec.revisionHistoryLimit",
    "spec.info.**",
];

pub fn is_supported_application_field_path(path: &str) -> bool {
    VALID_APPLICATION_PATH_PATTERNS.iter().any(|pattern| {
        path_matches_glob(path, pattern).unwrap_or_else(|_| panic!("invalid field catalog pattern: {}", pattern))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_supported_field_path() {
        assert!(is_supported_application_field_path("spec.syncPolicy.automated.prune"));
        assert!(is_supported_application_field_path(
            "metadata.annotations.\"foo.bar/baz\""
        ));
    }

    #[test]
    fn test_unsupported_field_path() {
        assert!(!is_supported_application_field_path("spec.notRealField.foo"));
    }
}
