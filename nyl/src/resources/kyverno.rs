/// Kyverno policy resource handling for annotation-based policy application
///
/// This module provides support for applying Kyverno policies at different stages
/// of the rendering pipeline using scope annotations on standard Kyverno CRDs.
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::constants::ANNOTATION_KYVERNO_SCOPE;
use crate::{NylError, Result};

/// Scope for Kyverno policy application
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum KyvernoScope {
    /// Applies to sibling resources in the same batch only
    Immediate,
    /// Applies to siblings and all descendant resources
    Subtree,
    /// Applies to all resources (equivalent to file scope since Nyl processes single files)
    Global,
}

impl KyvernoScope {
    /// Parse scope from annotation value
    pub fn from_annotation(value: &str) -> Result<Self> {
        match value {
            "Immediate" => Ok(KyvernoScope::Immediate),
            "Subtree" => Ok(KyvernoScope::Subtree),
            "Global" => Ok(KyvernoScope::Global),
            _ => Err(NylError::Config(format!(
                "Invalid Kyverno scope annotation value: '{}'. \
                 Valid values are: Immediate, Subtree, Global",
                value
            ))),
        }
    }
}

/// Annotated Kyverno policy resource with scope information
#[derive(Debug, Clone)]
pub struct AnnotatedKyvernoPolicy {
    /// The scope for policy application
    pub scope: KyvernoScope,
    /// The actual Kyverno policy resource (as JSON)
    pub policy: serde_json::Value,
    /// Policy name (from metadata.name)
    pub name: String,
    /// Policy kind (ClusterPolicy, MutatingPolicy, etc.)
    pub kind: String,
}

impl AnnotatedKyvernoPolicy {
    /// Extract annotated policy from manifest
    pub fn from_manifest(manifest: &serde_json::Value) -> Result<Self> {
        let scope_str = manifest
            .get("metadata")
            .and_then(|m| m.get("annotations"))
            .and_then(|a| a.get(ANNOTATION_KYVERNO_SCOPE))
            .and_then(|s| s.as_str())
            .ok_or_else(|| NylError::Config("Kyverno policy missing scope annotation".to_string()))?;

        let scope = KyvernoScope::from_annotation(scope_str)?;

        let name = manifest
            .get("metadata")
            .and_then(|m| m.get("name"))
            .and_then(|n| n.as_str())
            .ok_or_else(|| NylError::Config("Kyverno policy missing metadata.name".to_string()))?
            .to_string();

        let kind = manifest
            .get("kind")
            .and_then(|k| k.as_str())
            .ok_or_else(|| NylError::Config("Kyverno policy missing kind".to_string()))?
            .to_string();

        Ok(Self {
            scope,
            policy: manifest.clone(),
            name,
            kind,
        })
    }
}

/// Check if a manifest is a Kyverno policy resource
pub fn is_kyverno_policy(manifest: &serde_json::Value) -> bool {
    let api_version = manifest.get("apiVersion").and_then(|v| v.as_str());
    let kind = manifest.get("kind").and_then(|k| k.as_str());

    match (api_version, kind) {
        (Some(api), Some(k)) => {
            // kyverno.io/v1 or v2 with ClusterPolicy
            (api.starts_with("kyverno.io/") && k == "ClusterPolicy")
                ||
                // policies.kyverno.io/v1 with policy kinds
                (api.starts_with("policies.kyverno.io/")
                    && matches!(
                        k,
                        "MutatingPolicy"
                            | "ValidatingPolicy"
                            | "GeneratingPolicy"
                            | "DeletingPolicy"
                            | "ImageValidatingPolicy"
                    ))
        }
        _ => false,
    }
}

/// Check if manifest has Kyverno scope annotation
pub fn has_kyverno_scope_annotation(manifest: &serde_json::Value) -> bool {
    manifest
        .get("metadata")
        .and_then(|m| m.get("annotations"))
        .and_then(|a| a.get(ANNOTATION_KYVERNO_SCOPE))
        .is_some()
}

/// Extract Kyverno policies by scope from manifests
///
/// Returns (policies with matching scope, remaining manifests)
pub fn extract_policies_by_scope(
    manifests: &[serde_json::Value],
    scope: KyvernoScope,
) -> Result<(Vec<AnnotatedKyvernoPolicy>, Vec<serde_json::Value>)> {
    let mut policies = Vec::new();
    let mut filtered = Vec::new();

    for manifest in manifests {
        if is_kyverno_policy(manifest) && has_kyverno_scope_annotation(manifest) {
            let policy = AnnotatedKyvernoPolicy::from_manifest(manifest)?;
            if policy.scope == scope {
                policies.push(policy);
            } else {
                // Wrong scope, keep in manifests for later extraction
                filtered.push(manifest.clone());
            }
        } else {
            // Not a policy or no annotation → regular resource
            filtered.push(manifest.clone());
        }
    }

    Ok((policies, filtered))
}

/// Extract ALL annotated Kyverno policies from manifests (any scope)
///
/// Returns (policies grouped by scope, manifests without policies)
#[allow(clippy::type_complexity)]
pub fn extract_all_kyverno_policies(
    manifests: &[serde_json::Value],
) -> Result<(
    HashMap<KyvernoScope, Vec<AnnotatedKyvernoPolicy>>,
    Vec<serde_json::Value>,
)> {
    let mut policies_by_scope: HashMap<KyvernoScope, Vec<AnnotatedKyvernoPolicy>> = HashMap::new();
    let mut filtered = Vec::new();

    for manifest in manifests {
        if is_kyverno_policy(manifest) && has_kyverno_scope_annotation(manifest) {
            let policy = AnnotatedKyvernoPolicy::from_manifest(manifest)?;
            policies_by_scope.entry(policy.scope.clone()).or_default().push(policy);
        } else {
            filtered.push(manifest.clone());
        }
    }

    Ok((policies_by_scope, filtered))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_is_kyverno_policy_cluster_policy() {
        let manifest = json!({
            "apiVersion": "kyverno.io/v1",
            "kind": "ClusterPolicy",
            "metadata": {"name": "test"}
        });
        assert!(is_kyverno_policy(&manifest));
    }

    #[test]
    fn test_is_kyverno_policy_cluster_policy_v2() {
        let manifest = json!({
            "apiVersion": "kyverno.io/v2",
            "kind": "ClusterPolicy",
            "metadata": {"name": "test"}
        });
        assert!(is_kyverno_policy(&manifest));
    }

    #[test]
    fn test_is_kyverno_policy_mutating_policy() {
        let manifest = json!({
            "apiVersion": "policies.kyverno.io/v1",
            "kind": "MutatingPolicy",
            "metadata": {"name": "test"}
        });
        assert!(is_kyverno_policy(&manifest));
    }

    #[test]
    fn test_is_kyverno_policy_validating_policy() {
        let manifest = json!({
            "apiVersion": "policies.kyverno.io/v1",
            "kind": "ValidatingPolicy",
            "metadata": {"name": "test"}
        });
        assert!(is_kyverno_policy(&manifest));
    }

    #[test]
    fn test_is_kyverno_policy_false() {
        let manifest = json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {"name": "test"}
        });
        assert!(!is_kyverno_policy(&manifest));
    }

    #[test]
    fn test_has_kyverno_scope_annotation_true() {
        let manifest = json!({
            "apiVersion": "policies.kyverno.io/v1",
            "kind": "MutatingPolicy",
            "metadata": {
                "name": "test",
                "annotations": {
                    "nyl.niklasrosenstein.github.com/apply-policy-scope": "Subtree"
                }
            }
        });
        assert!(has_kyverno_scope_annotation(&manifest));
    }

    #[test]
    fn test_has_kyverno_scope_annotation_false() {
        let manifest = json!({
            "apiVersion": "policies.kyverno.io/v1",
            "kind": "MutatingPolicy",
            "metadata": {"name": "test"}
        });
        assert!(!has_kyverno_scope_annotation(&manifest));
    }

    #[test]
    fn test_annotated_kyverno_policy_from_manifest() {
        let manifest = json!({
            "apiVersion": "policies.kyverno.io/v1",
            "kind": "MutatingPolicy",
            "metadata": {
                "name": "test-policy",
                "annotations": {
                    "nyl.niklasrosenstein.github.com/apply-policy-scope": "Subtree"
                }
            },
            "spec": {}
        });

        let policy = AnnotatedKyvernoPolicy::from_manifest(&manifest).unwrap();
        assert_eq!(policy.scope, KyvernoScope::Subtree);
        assert_eq!(policy.name, "test-policy");
        assert_eq!(policy.kind, "MutatingPolicy");
    }

    #[test]
    fn test_scope_from_annotation_all_values() {
        assert_eq!(
            KyvernoScope::from_annotation("Immediate").unwrap(),
            KyvernoScope::Immediate
        );
        assert_eq!(KyvernoScope::from_annotation("Subtree").unwrap(), KyvernoScope::Subtree);
        assert_eq!(KyvernoScope::from_annotation("Global").unwrap(), KyvernoScope::Global);
    }

    #[test]
    fn test_scope_from_annotation_invalid() {
        assert!(KyvernoScope::from_annotation("Invalid").is_err());
        let err = KyvernoScope::from_annotation("Local").unwrap_err();
        assert!(matches!(err, NylError::Config(_)));
    }

    #[test]
    fn test_extract_policies_by_scope() {
        let manifests = vec![
            json!({
                "apiVersion": "policies.kyverno.io/v1",
                "kind": "MutatingPolicy",
                "metadata": {
                    "name": "policy1",
                    "annotations": {
                        "nyl.niklasrosenstein.github.com/apply-policy-scope": "Immediate"
                    }
                }
            }),
            json!({
                "apiVersion": "v1",
                "kind": "ConfigMap",
                "metadata": {"name": "config"}
            }),
            json!({
                "apiVersion": "policies.kyverno.io/v1",
                "kind": "ValidatingPolicy",
                "metadata": {
                    "name": "policy2",
                    "annotations": {
                        "nyl.niklasrosenstein.github.com/apply-policy-scope": "Global"
                    }
                }
            }),
        ];

        let (immediate_policies, filtered) = extract_policies_by_scope(&manifests, KyvernoScope::Immediate).unwrap();
        assert_eq!(immediate_policies.len(), 1);
        assert_eq!(immediate_policies[0].name, "policy1");
        // filtered should have ConfigMap + Global policy (not extracted)
        assert_eq!(filtered.len(), 2);
    }

    #[test]
    fn test_extract_all_kyverno_policies() {
        let manifests = vec![
            json!({
                "apiVersion": "policies.kyverno.io/v1",
                "kind": "MutatingPolicy",
                "metadata": {
                    "name": "policy1",
                    "annotations": {
                        "nyl.niklasrosenstein.github.com/apply-policy-scope": "Immediate"
                    }
                }
            }),
            json!({
                "apiVersion": "v1",
                "kind": "ConfigMap",
                "metadata": {"name": "config"}
            }),
            json!({
                "apiVersion": "policies.kyverno.io/v1",
                "kind": "ValidatingPolicy",
                "metadata": {
                    "name": "policy2",
                    "annotations": {
                        "nyl.niklasrosenstein.github.com/apply-policy-scope": "Global"
                    }
                }
            }),
            json!({
                "apiVersion": "kyverno.io/v1",
                "kind": "ClusterPolicy",
                "metadata": {
                    "name": "policy3",
                    "annotations": {
                        "nyl.niklasrosenstein.github.com/apply-policy-scope": "Subtree"
                    }
                }
            }),
        ];

        let (policies_by_scope, filtered) = extract_all_kyverno_policies(&manifests).unwrap();
        assert_eq!(policies_by_scope.len(), 3); // Immediate, Global, Subtree
        assert_eq!(filtered.len(), 1); // Just ConfigMap

        assert_eq!(policies_by_scope[&KyvernoScope::Immediate].len(), 1);
        assert_eq!(policies_by_scope[&KyvernoScope::Global].len(), 1);
        assert_eq!(policies_by_scope[&KyvernoScope::Subtree].len(), 1);
    }

    #[test]
    fn test_unannotated_policy_not_extracted() {
        let manifests = vec![json!({
            "apiVersion": "policies.kyverno.io/v1",
            "kind": "MutatingPolicy",
            "metadata": {
                "name": "policy-no-annotation"
            }
        })];

        let (policies_by_scope, filtered) = extract_all_kyverno_policies(&manifests).unwrap();
        assert_eq!(policies_by_scope.len(), 0);
        assert_eq!(filtered.len(), 1); // Policy treated as normal resource
    }
}
