/// Kubernetes resource types and YAML handling
///
/// This module will handle:
/// - Kubernetes resource type definitions
/// - YAML serialization/deserialization with serde-norway
/// - Resource validation
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KubernetesResource {
    #[serde(rename = "apiVersion")]
    pub api_version: String,
    pub kind: String,
    pub metadata: ResourceMetadata,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceMetadata {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
}
