/// Kubernetes resource types and YAML handling
///
/// This module will handle:
/// - Kubernetes resource type definitions
/// - YAML serialization/deserialization with serde-norway
/// - Resource validation
mod client;
mod cluster_server;
mod diff;
pub mod fence;
pub mod field_manager;
pub mod lifecycle;
pub mod memory;
mod namespace_resolution;
mod ordered_delete;
mod ordering;
pub mod prune;
mod resource;
mod state;

pub use client::{builtin_scope, is_known_cluster_scoped_gvk};
pub use client::{DeleteOptions, DeletePropagation, DiscoveryMode, KubeClient, KubeRsClient, MockKubeClient};
pub use cluster_server::{normalize_cluster_url, verifiable_cluster_server, verify_cluster_server, verify_server};
pub use diff::{DiffEngine, ServerComparison};
pub use memory::InMemoryReleaseStorage;
pub use namespace_resolution::{adjust_duplicate_keys_for_namespace_resolution, resolve_manifest_namespaces};
pub use ordered_delete::{delete_in_prune_order, run_concurrently, PruneEvent, PRUNE_TERMINATION_TIMEOUT};
pub use ordering::ResourceOrdering;
pub use resource::{extract_api_version, extract_gvk, extract_kind, extract_name, extract_namespace};
pub use resource::{ApplyOutcome, GroupVersionKind, ObjectIdentity, ResourceKey};
pub use state::{
    enforce_release_history_limit, revisions_beyond_history_limit, KubernetesReleaseStorage, ReleaseInfo, ReleaseMeta,
    ReleasePublication, ReleaseState, ReleaseStatus, ReleaseStorage,
};

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
