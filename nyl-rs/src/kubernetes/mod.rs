/// Kubernetes resource types and YAML handling
///
/// This module will handle:
/// - Kubernetes resource type definitions
/// - YAML serialization/deserialization with serde-norway
/// - Resource validation
mod client;
mod resource;
mod state;

pub use client::{KubeClient, KubeRsClient, MockKubeClient};
pub use resource::{ApplyOutcome, GroupVersionKind, ResourceKey};
pub use resource::{extract_api_version, extract_gvk, extract_kind, extract_name, extract_namespace};
pub use state::{KubernetesReleaseStorage, ReleaseState, ReleaseStatus, ReleaseStorage};

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
