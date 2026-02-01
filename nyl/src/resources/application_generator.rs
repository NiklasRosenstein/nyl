/// ApplicationGenerator resource for ArgoCD Application generation
///
/// This resource enables ArgoCD to automatically generate Application manifests
/// from Nyl YAML files by scanning directories and creating Applications for
/// each discovered NylRelease.
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::constants::API_VERSION_ARGOCD;
use crate::{NylError, Result};

/// ApplicationGenerator resource for ArgoCD bootstrapping
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ApplicationGenerator {
    #[serde(rename = "apiVersion")]
    pub api_version: String,
    pub kind: String,
    pub metadata: ApplicationGeneratorMetadata,
    pub spec: ApplicationGeneratorSpec,
}

/// Metadata for ApplicationGenerator
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ApplicationGeneratorMetadata {
    /// Generator name
    pub name: String,
    /// Target namespace (typically argocd)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
}

/// Specification for ApplicationGenerator
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ApplicationGeneratorSpec {
    /// Destination for generated Applications
    pub destination: ApplicationDestination,
    /// Source configuration for scanning
    pub source: ApplicationSource,
    /// ArgoCD project name
    #[serde(default = "default_project")]
    pub project: String,
    /// Default sync policy for generated Applications
    #[serde(skip_serializing_if = "Option::is_none", rename = "syncPolicy")]
    pub sync_policy: Option<SyncPolicy>,
    /// Template for Application names
    #[serde(default = "default_application_name_template", rename = "applicationNameTemplate")]
    pub application_name_template: String,
    /// Labels to add to generated Applications
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub labels: HashMap<String, String>,
    /// Annotations to add to generated Applications
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub annotations: HashMap<String, String>,
}

/// Destination configuration for ArgoCD Applications
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ApplicationDestination {
    /// Kubernetes server URL
    pub server: String,
    /// Target namespace for Applications (typically argocd)
    pub namespace: String,
}

/// Source configuration for scanning
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ApplicationSource {
    /// Git repository URL
    #[serde(rename = "repoURL")]
    pub repo_url: String,
    /// Target revision (branch, tag, commit)
    #[serde(default = "default_target_revision", rename = "targetRevision")]
    pub target_revision: String,
    /// Directory path to scan
    pub path: String,
    /// Include patterns for file filtering
    #[serde(default = "default_include")]
    pub include: Vec<String>,
    /// Exclude patterns for file filtering
    #[serde(default = "default_exclude")]
    pub exclude: Vec<String>,
}

/// ArgoCD sync policy
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SyncPolicy {
    /// Automated sync configuration
    #[serde(skip_serializing_if = "Option::is_none")]
    pub automated: Option<AutomatedSyncPolicy>,
    /// Sync options
    #[serde(default, skip_serializing_if = "Vec::is_empty", rename = "syncOptions")]
    pub sync_options: Vec<String>,
}

/// Automated sync policy configuration
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AutomatedSyncPolicy {
    /// Enable automatic pruning
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub prune: bool,
    /// Enable self-healing
    #[serde(default, skip_serializing_if = "std::ops::Not::not", rename = "selfHeal")]
    pub self_heal: bool,
}

// Default value functions
fn default_project() -> String {
    "default".to_string()
}

fn default_target_revision() -> String {
    "HEAD".to_string()
}

fn default_include() -> Vec<String> {
    vec!["*.yaml".to_string(), "*.yml".to_string()]
}

fn default_exclude() -> Vec<String> {
    vec![".*".to_string(), "_*".to_string()]
}

fn default_application_name_template() -> String {
    "{{ .release.name }}".to_string()
}

impl ApplicationGenerator {
    /// Check if a manifest is an ApplicationGenerator resource
    pub fn is_application_generator(manifest: &serde_json::Value) -> bool {
        manifest.get("apiVersion").and_then(|v| v.as_str()) == Some(API_VERSION_ARGOCD)
            && manifest.get("kind").and_then(|v| v.as_str()) == Some("ApplicationGenerator")
    }

    /// Parse ApplicationGenerator from JSON value
    pub fn from_value(value: &serde_json::Value) -> Result<Self> {
        serde_json::from_value(value.clone())
            .map_err(|e| NylError::Config(format!("Invalid ApplicationGenerator resource: {}", e)))
    }

    /// Validate the ApplicationGenerator configuration
    pub fn validate(&self) -> Result<()> {
        // Validate required fields
        if self.spec.source.repo_url.is_empty() {
            return Err(NylError::Config("spec.source.repoURL is required".to_string()));
        }
        if self.spec.source.path.is_empty() {
            return Err(NylError::Config("spec.source.path is required".to_string()));
        }
        if self.spec.destination.server.is_empty() {
            return Err(NylError::Config("spec.destination.server is required".to_string()));
        }
        if self.spec.destination.namespace.is_empty() {
            return Err(NylError::Config("spec.destination.namespace is required".to_string()));
        }
        Ok(())
    }
}

/// Extract all ApplicationGenerator resources from manifests
///
/// Returns a tuple of (extracted generators, filtered manifests without generators)
pub fn extract_application_generators(
    manifests: &[serde_json::Value],
) -> Result<(Vec<ApplicationGenerator>, Vec<serde_json::Value>)> {
    let mut generators = Vec::new();
    let mut filtered = Vec::new();

    for manifest in manifests {
        if ApplicationGenerator::is_application_generator(manifest) {
            let gen = ApplicationGenerator::from_value(manifest)?;
            gen.validate()?;
            generators.push(gen);
        } else {
            filtered.push(manifest.clone());
        }
    }

    Ok((generators, filtered))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_is_application_generator_true() {
        let manifest = json!({
            "apiVersion": "argocd.nyl.niklasrosenstein.github.com/v1",
            "kind": "ApplicationGenerator",
            "metadata": {
                "name": "cluster-apps",
                "namespace": "argocd"
            },
            "spec": {
                "destination": {
                    "server": "https://kubernetes.default.svc",
                    "namespace": "argocd"
                },
                "source": {
                    "repoURL": "https://github.com/example/repo.git",
                    "path": "clusters/default"
                }
            }
        });

        assert!(ApplicationGenerator::is_application_generator(&manifest));
    }

    #[test]
    fn test_is_application_generator_false_wrong_kind() {
        let manifest = json!({
            "apiVersion": "argocd.nyl.niklasrosenstein.github.com/v1",
            "kind": "Application",
            "metadata": {"name": "test"}
        });

        assert!(!ApplicationGenerator::is_application_generator(&manifest));
    }

    #[test]
    fn test_is_application_generator_false_wrong_api_version() {
        let manifest = json!({
            "apiVersion": "nyl.io/v1",
            "kind": "ApplicationGenerator",
            "metadata": {"name": "test"}
        });

        assert!(!ApplicationGenerator::is_application_generator(&manifest));
    }

    #[test]
    fn test_from_value_valid_minimal() {
        let value = json!({
            "apiVersion": "argocd.nyl.niklasrosenstein.github.com/v1",
            "kind": "ApplicationGenerator",
            "metadata": {
                "name": "cluster-apps",
                "namespace": "argocd"
            },
            "spec": {
                "destination": {
                    "server": "https://kubernetes.default.svc",
                    "namespace": "argocd"
                },
                "source": {
                    "repoURL": "https://github.com/example/repo.git",
                    "path": "clusters/default"
                }
            }
        });

        let gen = ApplicationGenerator::from_value(&value).unwrap();
        assert_eq!(gen.metadata.name, "cluster-apps");
        assert_eq!(gen.metadata.namespace, Some("argocd".to_string()));
        assert_eq!(gen.spec.destination.server, "https://kubernetes.default.svc");
        assert_eq!(gen.spec.destination.namespace, "argocd");
        assert_eq!(gen.spec.source.repo_url, "https://github.com/example/repo.git");
        assert_eq!(gen.spec.source.path, "clusters/default");

        // Check defaults
        assert_eq!(gen.spec.project, "default");
        assert_eq!(gen.spec.source.target_revision, "HEAD");
        assert_eq!(gen.spec.source.include, vec!["*.yaml", "*.yml"]);
        assert_eq!(gen.spec.source.exclude, vec![".*", "_*"]);
    }

    #[test]
    fn test_from_value_valid_full() {
        let value = json!({
            "apiVersion": "argocd.nyl.niklasrosenstein.github.com/v1",
            "kind": "ApplicationGenerator",
            "metadata": {
                "name": "cluster-apps",
                "namespace": "argocd"
            },
            "spec": {
                "destination": {
                    "server": "https://kubernetes.default.svc",
                    "namespace": "argocd"
                },
                "source": {
                    "repoURL": "https://github.com/example/repo.git",
                    "targetRevision": "main",
                    "path": "clusters/default",
                    "include": ["*.yaml"],
                    "exclude": ["test_*"]
                },
                "project": "production",
                "syncPolicy": {
                    "automated": {
                        "prune": true,
                        "selfHeal": true
                    },
                    "syncOptions": ["CreateNamespace=true"]
                },
                "applicationNameTemplate": "{{ .release.namespace }}-{{ .release.name }}",
                "labels": {"managed-by": "nyl"},
                "annotations": {"example.com/key": "value"}
            }
        });

        let gen = ApplicationGenerator::from_value(&value).unwrap();
        assert_eq!(gen.spec.project, "production");
        assert_eq!(gen.spec.source.target_revision, "main");
        assert_eq!(gen.spec.source.include, vec!["*.yaml"]);
        assert_eq!(gen.spec.source.exclude, vec!["test_*"]);
        assert_eq!(
            gen.spec.application_name_template,
            "{{ .release.namespace }}-{{ .release.name }}"
        );
        assert_eq!(gen.spec.labels.get("managed-by").unwrap(), "nyl");
        assert_eq!(gen.spec.annotations.get("example.com/key").unwrap(), "value");

        let sync_policy = gen.spec.sync_policy.unwrap();
        let automated = sync_policy.automated.unwrap();
        assert!(automated.prune);
        assert!(automated.self_heal);
        assert_eq!(sync_policy.sync_options, vec!["CreateNamespace=true"]);
    }

    #[test]
    fn test_from_value_invalid_missing_destination() {
        let value = json!({
            "apiVersion": "argocd.nyl.niklasrosenstein.github.com/v1",
            "kind": "ApplicationGenerator",
            "metadata": {"name": "test"},
            "spec": {
                "source": {
                    "repoURL": "https://github.com/example/repo.git",
                    "path": "clusters/default"
                }
            }
        });

        assert!(ApplicationGenerator::from_value(&value).is_err());
    }

    #[test]
    fn test_validate_success() {
        let gen = ApplicationGenerator {
            api_version: "argocd.nyl.niklasrosenstein.github.com/v1".to_string(),
            kind: "ApplicationGenerator".to_string(),
            metadata: ApplicationGeneratorMetadata {
                name: "test".to_string(),
                namespace: Some("argocd".to_string()),
            },
            spec: ApplicationGeneratorSpec {
                destination: ApplicationDestination {
                    server: "https://kubernetes.default.svc".to_string(),
                    namespace: "argocd".to_string(),
                },
                source: ApplicationSource {
                    repo_url: "https://github.com/example/repo.git".to_string(),
                    target_revision: "HEAD".to_string(),
                    path: "clusters/default".to_string(),
                    include: vec!["*.yaml".to_string()],
                    exclude: vec![".*".to_string()],
                },
                project: "default".to_string(),
                sync_policy: None,
                application_name_template: "{{ .release.name }}".to_string(),
                labels: HashMap::new(),
                annotations: HashMap::new(),
            },
        };

        assert!(gen.validate().is_ok());
    }

    #[test]
    fn test_validate_empty_repo_url() {
        let mut gen = create_test_generator();
        gen.spec.source.repo_url = String::new();

        let result = gen.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("repoURL"));
    }

    #[test]
    fn test_validate_empty_path() {
        let mut gen = create_test_generator();
        gen.spec.source.path = String::new();

        let result = gen.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("path"));
    }

    #[test]
    fn test_validate_empty_server() {
        let mut gen = create_test_generator();
        gen.spec.destination.server = String::new();

        let result = gen.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("server"));
    }

    #[test]
    fn test_validate_empty_namespace() {
        let mut gen = create_test_generator();
        gen.spec.destination.namespace = String::new();

        let result = gen.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("namespace"));
    }

    #[test]
    fn test_extract_application_generators_empty() {
        let manifests = vec![];
        let (generators, filtered) = extract_application_generators(&manifests).unwrap();
        assert_eq!(generators.len(), 0);
        assert_eq!(filtered.len(), 0);
    }

    #[test]
    fn test_extract_application_generators_with_generator() {
        let manifests = vec![
            json!({
                "apiVersion": "argocd.nyl.niklasrosenstein.github.com/v1",
                "kind": "ApplicationGenerator",
                "metadata": {
                    "name": "cluster-apps",
                    "namespace": "argocd"
                },
                "spec": {
                    "destination": {
                        "server": "https://kubernetes.default.svc",
                        "namespace": "argocd"
                    },
                    "source": {
                        "repoURL": "https://github.com/example/repo.git",
                        "path": "clusters/default"
                    }
                }
            }),
            json!({
                "apiVersion": "v1",
                "kind": "ConfigMap",
                "metadata": {"name": "test"}
            }),
        ];

        let (generators, filtered) = extract_application_generators(&manifests).unwrap();
        assert_eq!(generators.len(), 1);
        assert_eq!(generators[0].metadata.name, "cluster-apps");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0]["kind"], "ConfigMap");
    }

    #[test]
    fn test_extract_application_generators_multiple() {
        let manifests = vec![
            json!({
                "apiVersion": "argocd.nyl.niklasrosenstein.github.com/v1",
                "kind": "ApplicationGenerator",
                "metadata": {
                    "name": "cluster1",
                    "namespace": "argocd"
                },
                "spec": {
                    "destination": {
                        "server": "https://cluster1.example.com",
                        "namespace": "argocd"
                    },
                    "source": {
                        "repoURL": "https://github.com/example/repo.git",
                        "path": "clusters/cluster1"
                    }
                }
            }),
            json!({
                "apiVersion": "argocd.nyl.niklasrosenstein.github.com/v1",
                "kind": "ApplicationGenerator",
                "metadata": {
                    "name": "cluster2",
                    "namespace": "argocd"
                },
                "spec": {
                    "destination": {
                        "server": "https://cluster2.example.com",
                        "namespace": "argocd"
                    },
                    "source": {
                        "repoURL": "https://github.com/example/repo.git",
                        "path": "clusters/cluster2"
                    }
                }
            }),
        ];

        let (generators, filtered) = extract_application_generators(&manifests).unwrap();
        assert_eq!(generators.len(), 2);
        assert_eq!(generators[0].metadata.name, "cluster1");
        assert_eq!(generators[1].metadata.name, "cluster2");
        assert_eq!(filtered.len(), 0);
    }

    #[test]
    fn test_extract_application_generators_only_regular_resources() {
        let manifests = vec![
            json!({
                "apiVersion": "v1",
                "kind": "ConfigMap",
                "metadata": {"name": "test1"}
            }),
            json!({
                "apiVersion": "v1",
                "kind": "Service",
                "metadata": {"name": "test2"}
            }),
        ];

        let (generators, filtered) = extract_application_generators(&manifests).unwrap();
        assert_eq!(generators.len(), 0);
        assert_eq!(filtered.len(), 2);
    }

    #[test]
    fn test_extract_application_generators_validation_error() {
        let manifests = vec![json!({
            "apiVersion": "argocd.nyl.niklasrosenstein.github.com/v1",
            "kind": "ApplicationGenerator",
            "metadata": {
                "name": "invalid",
                "namespace": "argocd"
            },
            "spec": {
                "destination": {
                    "server": "",  // Invalid: empty server
                    "namespace": "argocd"
                },
                "source": {
                    "repoURL": "https://github.com/example/repo.git",
                    "path": "clusters/default"
                }
            }
        })];

        let result = extract_application_generators(&manifests);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("server"));
    }

    // Helper function for tests
    fn create_test_generator() -> ApplicationGenerator {
        ApplicationGenerator {
            api_version: API_VERSION_ARGOCD.to_string(),
            kind: "ApplicationGenerator".to_string(),
            metadata: ApplicationGeneratorMetadata {
                name: "test".to_string(),
                namespace: Some("argocd".to_string()),
            },
            spec: ApplicationGeneratorSpec {
                destination: ApplicationDestination {
                    server: "https://kubernetes.default.svc".to_string(),
                    namespace: "argocd".to_string(),
                },
                source: ApplicationSource {
                    repo_url: "https://github.com/example/repo.git".to_string(),
                    target_revision: "HEAD".to_string(),
                    path: "clusters/default".to_string(),
                    include: vec!["*.yaml".to_string()],
                    exclude: vec![".*".to_string()],
                },
                project: "default".to_string(),
                sync_policy: None,
                application_name_template: "{{ .release.name }}".to_string(),
                labels: HashMap::new(),
                annotations: HashMap::new(),
            },
        }
    }
}
