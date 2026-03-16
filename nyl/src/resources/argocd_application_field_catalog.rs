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

/// Known ArgoCD Application field-path patterns whose values are arrays and can
/// therefore support `+field` append semantics in NylRelease overrides.
const VALID_APPLICATION_ARRAY_FIELD_PATTERNS: &[&str] = &[
    "metadata.finalizers",
    "spec.source.helm.valueFiles",
    "spec.source.helm.parameters",
    "spec.source.helm.fileParameters",
    "spec.source.kustomize.images",
    "spec.source.kustomize.replicas",
    "spec.source.kustomize.components",
    "spec.source.kustomize.patches",
    "spec.source.jsonnet.extVars",
    "spec.source.jsonnet.tlas",
    "spec.source.plugin.env",
    "spec.sources",
    "spec.syncPolicy.syncOptions",
    "spec.ignoreDifferences",
    "spec.info",
];

pub fn is_supported_application_field_path(path: &str) -> bool {
    VALID_APPLICATION_PATH_PATTERNS
        .iter()
        .any(|pattern| path_matches_glob(path, pattern).unwrap_or(false))
}

pub fn is_supported_application_array_field_path(path: &str) -> bool {
    VALID_APPLICATION_ARRAY_FIELD_PATTERNS
        .iter()
        .any(|pattern| path_matches_glob(path, pattern).unwrap_or(false))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resources::path_glob::validate_path_glob_pattern;

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

    #[test]
    fn test_supported_array_field_path() {
        assert!(is_supported_application_array_field_path("spec.syncPolicy.syncOptions"));
        assert!(is_supported_application_array_field_path("spec.info"));
    }

    #[test]
    fn test_unsupported_array_field_path() {
        assert!(!is_supported_application_array_field_path("spec.syncPolicy.automated"));
    }

    #[test]
    fn test_field_catalog_patterns_are_valid() {
        for pattern in VALID_APPLICATION_PATH_PATTERNS {
            validate_path_glob_pattern(pattern).unwrap_or_else(|e| {
                panic!("invalid field catalog pattern '{}': {}", pattern, e);
            });
        }
        for pattern in VALID_APPLICATION_ARRAY_FIELD_PATTERNS {
            validate_path_glob_pattern(pattern).unwrap_or_else(|e| {
                panic!("invalid array field catalog pattern '{}': {}", pattern, e);
            });
        }
    }
}
