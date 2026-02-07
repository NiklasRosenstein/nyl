/// Kyverno post-processor resource definition
///
/// This resource instructs Nyl to apply Kyverno policies to resources at render time.
/// The Kyverno resource itself is not emitted in the final output.
use serde::{Deserialize, Serialize};

use crate::constants::API_VERSION_POSTPROCESSING;
use crate::{NylError, Result};

/// Kyverno resource for post-processing manifests with policies
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Kyverno {
    #[serde(rename = "apiVersion")]
    pub api_version: String,
    pub kind: String,
    pub metadata: KyvernoMetadata,
    pub spec: KyvernoSpec,
}

/// Metadata for Kyverno resource
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct KyvernoMetadata {
    /// Resource name
    pub name: String,
    /// Namespace (optional, primarily for organizational purposes)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
}

/// Scope for Kyverno policy application
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "PascalCase")]
pub enum KyvernoScope {
    /// Applies only to resources produced in the same YAML file;
    /// or if defined in a Helm chart, only to resources from that chart
    #[default]
    Local,
    /// Applies only to resources produced in the same YAML file,
    /// even if the Kyverno resource was defined in a Helm chart
    Root,
    /// Applies to all resources produced in `nyl render`
    Global,
}

/// Kyverno policy specification
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct KyvernoSpec {
    /// Scope of policy application (default: Local)
    #[serde(default)]
    pub scope: KyvernoScope,

    /// Paths to Kyverno policy files (relative to the manifest file)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub policies: Vec<String>,

    /// Inline Kyverno policies (full policy resources)
    #[serde(default, skip_serializing_if = "Vec::is_empty", rename = "inlinePolicies")]
    pub inline_policies: Vec<serde_json::Value>,

    /// Shorthand for ClusterPolicy rules
    #[serde(default, skip_serializing_if = "Vec::is_empty", rename = "clusterPolicyRules")]
    pub cluster_policy_rules: Vec<serde_json::Value>,

    /// Shorthand for validating policy rules
    #[serde(default, skip_serializing_if = "Vec::is_empty", rename = "validatingPolicyRules")]
    pub validating_policy_rules: Vec<serde_json::Value>,

    /// Shorthand for mutating policy rules
    #[serde(default, skip_serializing_if = "Vec::is_empty", rename = "mutatingPolicyRules")]
    pub mutating_policy_rules: Vec<serde_json::Value>,

    /// Shorthand for generating policy rules
    #[serde(default, skip_serializing_if = "Vec::is_empty", rename = "generatingPolicyRules")]
    pub generating_policy_rules: Vec<serde_json::Value>,

    /// Shorthand for deleting policy rules (cleanup policies)
    #[serde(default, skip_serializing_if = "Vec::is_empty", rename = "deletingPolicyRules")]
    pub deleting_policy_rules: Vec<serde_json::Value>,

    /// Shorthand for image validation policy rules
    #[serde(default, skip_serializing_if = "Vec::is_empty", rename = "imageValidatingPolicyRules")]
    pub image_validating_policy_rules: Vec<serde_json::Value>,
}

impl Kyverno {
    /// Check if a manifest is a Kyverno resource
    pub fn is_kyverno(manifest: &serde_json::Value) -> bool {
        manifest.get("apiVersion").and_then(|v| v.as_str()) == Some(API_VERSION_POSTPROCESSING)
            && manifest.get("kind").and_then(|v| v.as_str()) == Some("Kyverno")
    }

    /// Parse Kyverno from JSON value
    pub fn from_value(value: &serde_json::Value) -> Result<Self> {
        serde_json::from_value(value.clone())
            .map_err(|e| NylError::Config(format!("Invalid Kyverno resource: {}", e)))
    }

    /// Get all policies as full Kyverno policy resources
    pub fn get_all_policies(&self) -> Vec<serde_json::Value> {
        let mut policies = self.spec.inline_policies.clone();

        // Add shorthand rules as ClusterPolicy resources
        if !self.spec.cluster_policy_rules.is_empty() {
            policies.push(self.create_cluster_policy("cluster-policy", &self.spec.cluster_policy_rules));
        }

        if !self.spec.validating_policy_rules.is_empty() {
            policies.push(self.create_cluster_policy("validating-policy", &self.spec.validating_policy_rules));
        }

        if !self.spec.mutating_policy_rules.is_empty() {
            policies.push(self.create_cluster_policy("mutating-policy", &self.spec.mutating_policy_rules));
        }

        if !self.spec.generating_policy_rules.is_empty() {
            policies.push(self.create_cluster_policy("generating-policy", &self.spec.generating_policy_rules));
        }

        if !self.spec.deleting_policy_rules.is_empty() {
            policies.push(self.create_cluster_policy("deleting-policy", &self.spec.deleting_policy_rules));
        }

        if !self.spec.image_validating_policy_rules.is_empty() {
            policies.push(self.create_cluster_policy(
                "image-validating-policy",
                &self.spec.image_validating_policy_rules,
            ));
        }

        policies
    }

    /// Create a ClusterPolicy resource from rules
    fn create_cluster_policy(&self, name_suffix: &str, rules: &[serde_json::Value]) -> serde_json::Value {
        serde_json::json!({
            "apiVersion": "kyverno.io/v1",
            "kind": "ClusterPolicy",
            "metadata": {
                "name": format!("{}-{}", self.metadata.name, name_suffix),
            },
            "spec": {
                "rules": rules
            }
        })
    }
}

/// Extract Kyverno resources from manifests
///
/// Returns a tuple of (Kyverno resources, filtered manifests without Kyverno)
pub fn extract_kyverno_resources(
    manifests: &[serde_json::Value],
) -> Result<(Vec<Kyverno>, Vec<serde_json::Value>)> {
    let mut kyverno_resources = Vec::new();
    let mut filtered = Vec::new();

    for manifest in manifests {
        if Kyverno::is_kyverno(manifest) {
            kyverno_resources.push(Kyverno::from_value(manifest)?);
        } else {
            filtered.push(manifest.clone());
        }
    }

    Ok((kyverno_resources, filtered))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_is_kyverno_true() {
        let manifest = json!({
            "apiVersion": "post-processing.nyl.niklasrosenstein.github.com/v1",
            "kind": "Kyverno",
            "metadata": {
                "name": "test-policy"
            },
            "spec": {
                "scope": "Local"
            }
        });

        assert!(Kyverno::is_kyverno(&manifest));
    }

    #[test]
    fn test_is_kyverno_false_wrong_kind() {
        let manifest = json!({
            "apiVersion": "post-processing.nyl.niklasrosenstein.github.com/v1",
            "kind": "ConfigMap",
            "metadata": {
                "name": "test"
            }
        });

        assert!(!Kyverno::is_kyverno(&manifest));
    }

    #[test]
    fn test_from_value_with_inline_policies() {
        let value = json!({
            "apiVersion": "post-processing.nyl.niklasrosenstein.github.com/v1",
            "kind": "Kyverno",
            "metadata": {
                "name": "test-policy"
            },
            "spec": {
                "scope": "Local",
                "inlinePolicies": [
                    {
                        "apiVersion": "kyverno.io/v1",
                        "kind": "ClusterPolicy",
                        "metadata": {"name": "test"},
                        "spec": {"rules": []}
                    }
                ]
            }
        });

        let kyverno = Kyverno::from_value(&value).unwrap();
        assert_eq!(kyverno.metadata.name, "test-policy");
        assert_eq!(kyverno.spec.scope, KyvernoScope::Local);
        assert_eq!(kyverno.spec.inline_policies.len(), 1);
    }

    #[test]
    fn test_from_value_with_shorthand_rules() {
        let value = json!({
            "apiVersion": "post-processing.nyl.niklasrosenstein.github.com/v1",
            "kind": "Kyverno",
            "metadata": {
                "name": "mutation-policy"
            },
            "spec": {
                "scope": "Global",
                "mutatingPolicyRules": [
                    {
                        "name": "add-label",
                        "match": {
                            "resources": {
                                "kinds": ["Deployment"]
                            }
                        },
                        "mutate": {
                            "patchStrategicMerge": {
                                "metadata": {
                                    "labels": {
                                        "managed-by": "nyl"
                                    }
                                }
                            }
                        }
                    }
                ]
            }
        });

        let kyverno = Kyverno::from_value(&value).unwrap();
        assert_eq!(kyverno.spec.scope, KyvernoScope::Global);
        assert_eq!(kyverno.spec.mutating_policy_rules.len(), 1);
    }

    #[test]
    fn test_get_all_policies() {
        let kyverno = Kyverno {
            api_version: "post-processing.nyl.niklasrosenstein.github.com/v1".to_string(),
            kind: "Kyverno".to_string(),
            metadata: KyvernoMetadata {
                name: "test".to_string(),
                namespace: None,
            },
            spec: KyvernoSpec {
                scope: KyvernoScope::Local,
                policies: vec![],
                inline_policies: vec![json!({"apiVersion": "kyverno.io/v1", "kind": "ClusterPolicy"})],
                cluster_policy_rules: vec![json!({"name": "rule1"})],
                validating_policy_rules: vec![],
                mutating_policy_rules: vec![json!({"name": "rule2"})],
                generating_policy_rules: vec![],
                deleting_policy_rules: vec![],
                image_validating_policy_rules: vec![],
            },
        };

        let policies = kyverno.get_all_policies();
        // 1 inline + 1 cluster + 1 mutating = 3 policies
        assert_eq!(policies.len(), 3);
    }

    #[test]
    fn test_extract_kyverno_resources() {
        let manifests = vec![
            json!({
                "apiVersion": "post-processing.nyl.niklasrosenstein.github.com/v1",
                "kind": "Kyverno",
                "metadata": {"name": "policy1"},
                "spec": {"scope": "Local"}
            }),
            json!({
                "apiVersion": "v1",
                "kind": "ConfigMap",
                "metadata": {"name": "config"}
            }),
            json!({
                "apiVersion": "post-processing.nyl.niklasrosenstein.github.com/v1",
                "kind": "Kyverno",
                "metadata": {"name": "policy2"},
                "spec": {"scope": "Global"}
            }),
        ];

        let (kyverno_resources, filtered) = extract_kyverno_resources(&manifests).unwrap();
        assert_eq!(kyverno_resources.len(), 2);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0]["kind"], "ConfigMap");
    }

    #[test]
    fn test_scope_serialization() {
        let scope = KyvernoScope::Local;
        let serialized = serde_json::to_string(&scope).unwrap();
        assert_eq!(serialized, "\"Local\"");

        let scope = KyvernoScope::Global;
        let serialized = serde_json::to_string(&scope).unwrap();
        assert_eq!(serialized, "\"Global\"");
    }

    #[test]
    fn test_create_cluster_policy() {
        let kyverno = Kyverno {
            api_version: "post-processing.nyl.niklasrosenstein.github.com/v1".to_string(),
            kind: "Kyverno".to_string(),
            metadata: KyvernoMetadata {
                name: "my-policy".to_string(),
                namespace: None,
            },
            spec: KyvernoSpec {
                scope: KyvernoScope::Local,
                policies: vec![],
                inline_policies: vec![],
                cluster_policy_rules: vec![json!({"name": "test-rule"})],
                validating_policy_rules: vec![],
                mutating_policy_rules: vec![],
                generating_policy_rules: vec![],
                deleting_policy_rules: vec![],
                image_validating_policy_rules: vec![],
            },
        };

        let policy = kyverno.create_cluster_policy("test", &kyverno.spec.cluster_policy_rules);
        assert_eq!(policy["apiVersion"], "kyverno.io/v1");
        assert_eq!(policy["kind"], "ClusterPolicy");
        assert_eq!(policy["metadata"]["name"], "my-policy-test");
        assert_eq!(policy["spec"]["rules"].as_array().unwrap().len(), 1);
    }
}
