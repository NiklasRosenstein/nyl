/// Post-render manifest filtering by kind
///
/// This module provides functionality to filter manifests after rendering,
/// complementing the pre-render source kind filtering.
use crate::{NylError, Result};

/// Filter manifests by kind after rendering.
///
/// This function filters manifests based on their kind and optionally apiVersion.
/// It operates on fully expanded manifests (after HelmChart/Component expansion).
///
/// # Arguments
///
/// * `manifests` - The manifests to filter
/// * `only_kind` - If non-empty, only include manifests matching these kinds
/// * `exclude_kind` - If non-empty, exclude manifests matching these kinds
///
/// # Filter Format
///
/// - If filter contains `/`: match against `apiVersion/kind` (e.g., "apps/v1/Deployment")
/// - If filter has no `/`: match against `kind` only (e.g., "Deployment")
///
/// # Returns
///
/// Filtered list of manifests. Warns if no resources match (doesn't error).
pub fn filter_manifests_by_kind(
    manifests: Vec<serde_json::Value>,
    only_kind: &[String],
    exclude_kind: &[String],
) -> Result<Vec<serde_json::Value>> {
    // If both filters are empty, return all manifests
    if only_kind.is_empty() && exclude_kind.is_empty() {
        return Ok(manifests);
    }

    // Mutual exclusivity is enforced by clap conflicts_with, but double-check
    if !only_kind.is_empty() && !exclude_kind.is_empty() {
        return Err(NylError::Config(
            "Cannot specify both --only-kind and --exclude-kind".to_string(),
        ));
    }

    // Capture original count before moving manifests
    let original_count = manifests.len();

    let filtered: Vec<serde_json::Value> = manifests
        .into_iter()
        .filter(|manifest| {
            let kind = manifest.get("kind").and_then(|k| k.as_str()).unwrap_or("");
            let api_version = manifest.get("apiVersion").and_then(|a| a.as_str()).unwrap_or("");

            // Build full kind string (apiVersion/kind)
            let full_kind = format!("{}/{}", api_version, kind);

            // Apply only_kind filter
            if !only_kind.is_empty() {
                return only_kind.iter().any(|filter| match_kind(filter, kind, &full_kind));
            }

            // Apply exclude_kind filter
            if !exclude_kind.is_empty() {
                return !exclude_kind.iter().any(|filter| match_kind(filter, kind, &full_kind));
            }

            true
        })
        .collect();

    // Warn if no resources match (don't error)
    if filtered.is_empty() && original_count > 0 {
        if !only_kind.is_empty() {
            tracing::warn!("No resources matched --only-kind filter: {}", only_kind.join(", "));
        } else if !exclude_kind.is_empty() {
            tracing::warn!(
                "All resources excluded by --exclude-kind filter: {}",
                exclude_kind.join(", ")
            );
        }
    }

    Ok(filtered)
}

/// Match a filter pattern against kind and full kind string.
///
/// If filter contains `/`, match against full kind (apiVersion/kind).
/// Otherwise, match against kind only.
fn match_kind(filter: &str, kind: &str, full_kind: &str) -> bool {
    if filter.contains('/') {
        // Match full apiVersion/kind
        filter == full_kind
    } else {
        // Match kind only
        filter == kind
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_filter_empty_filters() {
        let manifests = vec![
            json!({"apiVersion": "v1", "kind": "ConfigMap"}),
            json!({"apiVersion": "v1", "kind": "Secret"}),
        ];

        let result = filter_manifests_by_kind(manifests.clone(), &[], &[]).unwrap();
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_filter_only_kind_single() {
        let manifests = vec![
            json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "test1"}}),
            json!({"apiVersion": "v1", "kind": "Secret", "metadata": {"name": "test2"}}),
            json!({"apiVersion": "apps/v1", "kind": "Deployment", "metadata": {"name": "test3"}}),
        ];

        let result = filter_manifests_by_kind(manifests, &["ConfigMap".to_string()], &[]).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["kind"], "ConfigMap");
    }

    #[test]
    fn test_filter_only_kind_multiple() {
        let manifests = vec![
            json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "test1"}}),
            json!({"apiVersion": "v1", "kind": "Secret", "metadata": {"name": "test2"}}),
            json!({"apiVersion": "apps/v1", "kind": "Deployment", "metadata": {"name": "test3"}}),
        ];

        let result =
            filter_manifests_by_kind(manifests, &["ConfigMap".to_string(), "Secret".to_string()], &[]).unwrap();
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_filter_exclude_kind() {
        let manifests = vec![
            json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "test1"}}),
            json!({"apiVersion": "v1", "kind": "Secret", "metadata": {"name": "test2"}}),
            json!({"apiVersion": "apps/v1", "kind": "Deployment", "metadata": {"name": "test3"}}),
        ];

        let result = filter_manifests_by_kind(manifests, &[], &["Secret".to_string()]).unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0]["kind"], "ConfigMap");
        assert_eq!(result[1]["kind"], "Deployment");
    }

    #[test]
    fn test_filter_with_api_version() {
        let manifests = vec![
            json!({"apiVersion": "v1", "kind": "Secret", "metadata": {"name": "test1"}}),
            json!({"apiVersion": "custom.io/v1", "kind": "Secret", "metadata": {"name": "test2"}}),
        ];

        // Filter by full apiVersion/kind should match only v1 Secret
        let result = filter_manifests_by_kind(manifests, &["v1/Secret".to_string()], &[]).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["metadata"]["name"], "test1");
    }

    #[test]
    fn test_filter_case_sensitive() {
        let manifests = vec![json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "test1"}})];

        // Should not match due to case difference
        let result = filter_manifests_by_kind(manifests, &["configmap".to_string()], &[]).unwrap();
        assert_eq!(result.len(), 0);
    }

    #[test]
    fn test_filter_crd_short_form() {
        let manifests = vec![
            json!({"apiVersion": "apiextensions.k8s.io/v1", "kind": "CustomResourceDefinition", "metadata": {"name": "test1"}}),
            json!({"apiVersion": "apps/v1", "kind": "Deployment", "metadata": {"name": "test2"}}),
        ];

        // Common shorthand: CRD
        let result = filter_manifests_by_kind(manifests, &["CustomResourceDefinition".to_string()], &[]).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["kind"], "CustomResourceDefinition");
    }

    #[test]
    fn test_filter_mutual_exclusivity() {
        let manifests = vec![json!({"apiVersion": "v1", "kind": "ConfigMap"})];

        // Both filters should error
        let result = filter_manifests_by_kind(manifests, &["ConfigMap".to_string()], &["Secret".to_string()]);
        assert!(result.is_err());
    }

    #[test]
    fn test_match_kind_with_slash() {
        assert!(match_kind("v1/Secret", "Secret", "v1/Secret"));
        assert!(!match_kind("v1/Secret", "Secret", "custom.io/v1/Secret"));
    }

    #[test]
    fn test_match_kind_without_slash() {
        assert!(match_kind("Secret", "Secret", "v1/Secret"));
        assert!(match_kind("Secret", "Secret", "custom.io/v1/Secret"));
        assert!(!match_kind("ConfigMap", "Secret", "v1/Secret"));
    }
}
