/// Kyverno post-processor resource definition
///
/// This resource instructs Nyl to apply Kyverno policies to resources at render time.
/// The Kyverno resource itself is not emitted in the final output.
///
/// # Policy types
///
/// - `clusterPolicyRules`: Generates `ClusterPolicy` (`kyverno.io/v1`) resources.
///   Works with any kyverno CLI version. Items are individual rules wrapped in `spec.rules`.
/// - `mutatingPolicies`, `validatingPolicies`, `generatingPolicies`, `deletingPolicies`,
///   `imageValidatingPolicies`: Generate their respective policy types under
///   `policies.kyverno.io/v1`. Requires kyverno CLI >= 1.17. Each item is the full `spec`
///   of the target CRD (spec passthrough). An optional `name` field in each item is extracted
///   and used as `metadata.name`; otherwise a default name is generated.
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

    /// Shorthand for ClusterPolicy rules (kyverno.io/v1)
    #[serde(default, skip_serializing_if = "Vec::is_empty", rename = "clusterPolicyRules")]
    pub cluster_policy_rules: Vec<serde_json::Value>,

    /// ValidatingPolicy specs (policies.kyverno.io/v1, requires kyverno >= 1.17)
    #[serde(default, skip_serializing_if = "Vec::is_empty", rename = "validatingPolicies")]
    pub validating_policies: Vec<serde_json::Value>,

    /// MutatingPolicy specs (policies.kyverno.io/v1, requires kyverno >= 1.17)
    #[serde(default, skip_serializing_if = "Vec::is_empty", rename = "mutatingPolicies")]
    pub mutating_policies: Vec<serde_json::Value>,

    /// GeneratingPolicy specs (policies.kyverno.io/v1, requires kyverno >= 1.17)
    #[serde(default, skip_serializing_if = "Vec::is_empty", rename = "generatingPolicies")]
    pub generating_policies: Vec<serde_json::Value>,

    /// DeletingPolicy specs (policies.kyverno.io/v1, requires kyverno >= 1.17)
    #[serde(default, skip_serializing_if = "Vec::is_empty", rename = "deletingPolicies")]
    pub deleting_policies: Vec<serde_json::Value>,

    /// ImageValidatingPolicy specs (policies.kyverno.io/v1, requires kyverno >= 1.17)
    #[serde(default, skip_serializing_if = "Vec::is_empty", rename = "imageValidatingPolicies")]
    pub image_validating_policies: Vec<serde_json::Value>,
}

impl Kyverno {
    /// Check if a manifest is a Kyverno resource
    pub fn is_kyverno(manifest: &serde_json::Value) -> bool {
        manifest.get("apiVersion").and_then(|v| v.as_str()) == Some(API_VERSION_POSTPROCESSING)
            && manifest.get("kind").and_then(|v| v.as_str()) == Some("Kyverno")
    }

    /// Parse Kyverno from JSON value
    pub fn from_value(value: &serde_json::Value) -> Result<Self> {
        serde_json::from_value(value.clone()).map_err(|e| NylError::Config(format!("Invalid Kyverno resource: {}", e)))
    }

    /// Get all policies as full Kyverno policy resources
    pub fn get_all_policies(&self) -> Vec<serde_json::Value> {
        let mut policies = self.spec.inline_policies.clone();

        // Add ClusterPolicy from shorthand rules (wraps rules in spec.rules)
        if !self.spec.cluster_policy_rules.is_empty() {
            policies.push(self.create_cluster_policy(&self.spec.cluster_policy_rules));
        }

        // Add new policy types (spec passthrough, one policy per spec item)
        if !self.spec.validating_policies.is_empty() {
            policies.extend(self.create_policies(
                "ValidatingPolicy",
                "validating-policy",
                &self.spec.validating_policies,
            ));
        }

        if !self.spec.mutating_policies.is_empty() {
            policies.extend(self.create_policies("MutatingPolicy", "mutating-policy", &self.spec.mutating_policies));
        }

        if !self.spec.generating_policies.is_empty() {
            policies.extend(self.create_policies(
                "GeneratingPolicy",
                "generating-policy",
                &self.spec.generating_policies,
            ));
        }

        if !self.spec.deleting_policies.is_empty() {
            policies.extend(self.create_policies("DeletingPolicy", "deleting-policy", &self.spec.deleting_policies));
        }

        if !self.spec.image_validating_policies.is_empty() {
            policies.extend(self.create_policies(
                "ImageValidatingPolicy",
                "image-validating-policy",
                &self.spec.image_validating_policies,
            ));
        }

        policies
    }

    /// Create a ClusterPolicy resource from rules (kyverno.io/v1)
    ///
    /// Each item in `rules` is a single rule; they are all wrapped into one ClusterPolicy.
    fn create_cluster_policy(&self, rules: &[serde_json::Value]) -> serde_json::Value {
        serde_json::json!({
            "apiVersion": "kyverno.io/v1",
            "kind": "ClusterPolicy",
            "metadata": {
                "name": format!("{}-cluster-policy", self.metadata.name),
            },
            "spec": {
                "rules": rules
            }
        })
    }

    /// Create policy resources from spec passthroughs (policies.kyverno.io/v1)
    ///
    /// Each item in `specs` is the full `spec` of the target CRD. An optional `name` field
    /// in each item is extracted and used as `metadata.name`; otherwise a default name is
    /// generated from the Kyverno resource name and the index.
    fn create_policies(&self, kind: &str, suffix: &str, specs: &[serde_json::Value]) -> Vec<serde_json::Value> {
        specs
            .iter()
            .enumerate()
            .map(|(idx, spec)| {
                let mut spec = spec.clone();
                let name = spec
                    .as_object_mut()
                    .and_then(|o| o.remove("name"))
                    .and_then(|v| v.as_str().map(String::from))
                    .unwrap_or_else(|| format!("{}-{}-{}", self.metadata.name, suffix, idx));
                serde_json::json!({
                    "apiVersion": "policies.kyverno.io/v1",
                    "kind": kind,
                    "metadata": { "name": name },
                    "spec": spec
                })
            })
            .collect()
    }
}

/// Extract Kyverno resources from manifests
///
/// Returns a tuple of (Kyverno resources, filtered manifests without Kyverno)
pub fn extract_kyverno_resources(manifests: &[serde_json::Value]) -> Result<(Vec<Kyverno>, Vec<serde_json::Value>)> {
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
    fn test_from_value_with_mutating_policies() {
        let value = json!({
            "apiVersion": "post-processing.nyl.niklasrosenstein.github.com/v1",
            "kind": "Kyverno",
            "metadata": {
                "name": "mutation-policy"
            },
            "spec": {
                "scope": "Global",
                "mutatingPolicies": [
                    {
                        "name": "add-managed-by-label",
                        "matchConstraints": {
                            "resourceRules": [{
                                "apiGroups": [""],
                                "apiVersions": ["v1"],
                                "operations": ["CREATE"],
                                "resources": ["configmaps"]
                            }]
                        },
                        "mutations": [{
                            "patchType": "ApplyConfiguration",
                            "applyConfiguration": {
                                "expression": "Object{metadata: Object.metadata{labels: Object.metadata.labels{managedBy: \"nyl\"}}}"
                            }
                        }]
                    }
                ]
            }
        });

        let kyverno = Kyverno::from_value(&value).unwrap();
        assert_eq!(kyverno.spec.scope, KyvernoScope::Global);
        assert_eq!(kyverno.spec.mutating_policies.len(), 1);
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
                validating_policies: vec![],
                mutating_policies: vec![json!({"matchConstraints": {}, "mutations": []})],
                generating_policies: vec![],
                deleting_policies: vec![],
                image_validating_policies: vec![],
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
    fn test_create_policies() {
        let validating_spec = json!({"name": "my-validate", "validations": [{"expression": "true"}]});
        let mutating_spec = json!({"matchConstraints": {}, "mutations": []});

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
                validating_policies: vec![validating_spec],
                mutating_policies: vec![mutating_spec],
                generating_policies: vec![],
                deleting_policies: vec![],
                image_validating_policies: vec![],
            },
        };

        // Test ClusterPolicy (wraps rules in spec.rules)
        let policy = kyverno.create_cluster_policy(&kyverno.spec.cluster_policy_rules);
        assert_eq!(policy["apiVersion"], "kyverno.io/v1");
        assert_eq!(policy["kind"], "ClusterPolicy");
        assert_eq!(policy["metadata"]["name"], "my-policy-cluster-policy");
        assert_eq!(policy["spec"]["rules"].as_array().unwrap().len(), 1);

        // Test ValidatingPolicy (spec passthrough with name extraction)
        let policies = kyverno.create_policies(
            "ValidatingPolicy",
            "validating-policy",
            &kyverno.spec.validating_policies,
        );
        assert_eq!(policies.len(), 1);
        assert_eq!(policies[0]["apiVersion"], "policies.kyverno.io/v1");
        assert_eq!(policies[0]["kind"], "ValidatingPolicy");
        assert_eq!(policies[0]["metadata"]["name"], "my-validate");
        // name should be removed from spec
        assert!(policies[0]["spec"]["name"].is_null());
        assert_eq!(policies[0]["spec"]["validations"][0]["expression"], "true");

        // Test MutatingPolicy (spec passthrough with default name)
        let policies = kyverno.create_policies("MutatingPolicy", "mutating-policy", &kyverno.spec.mutating_policies);
        assert_eq!(policies.len(), 1);
        assert_eq!(policies[0]["apiVersion"], "policies.kyverno.io/v1");
        assert_eq!(policies[0]["kind"], "MutatingPolicy");
        assert_eq!(policies[0]["metadata"]["name"], "my-policy-mutating-policy-0");
    }
}
