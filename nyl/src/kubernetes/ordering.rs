//! Resource ordering for correct Kubernetes apply sequence
//!
//! This module provides functionality to sort Kubernetes resources by priority
//! to ensure they are applied in the correct order. For example:
//! - Namespaces must be created before resources in them
//! - CRDs must be created before custom resources
//! - ConfigMaps/Secrets should be created before Deployments that reference them

use crate::kubernetes::{extract_gvk, GroupVersionKind};
use crate::Result;
use serde_json::Value;

/// Priority levels for resource ordering (lower number = applied first)
const PRIORITY_NAMESPACE: u32 = 0;
const PRIORITY_CRD: u32 = 10;
const PRIORITY_SERVICE_ACCOUNT: u32 = 20;
const PRIORITY_RBAC: u32 = 25; // Role, RoleBinding, ClusterRole, ClusterRoleBinding
const PRIORITY_CONFIG: u32 = 30; // ConfigMap, Secret
const PRIORITY_SERVICE: u32 = 40;
const PRIORITY_WORKLOAD: u32 = 50; // Deployment, StatefulSet, DaemonSet, Job
const PRIORITY_OTHER: u32 = 100;

/// Resource ordering utility
pub struct ResourceOrdering;

impl ResourceOrdering {
    /// Sort resources by priority (Namespace → CRD → RBAC → Config → Workload)
    ///
    /// This performs a stable sort, meaning resources with the same priority
    /// maintain their relative order from the input.
    pub fn sort_by_priority(resources: &mut [Value]) -> Result<()> {
        resources.sort_by_cached_key(|resource| {
            // Extract GVK and get priority
            extract_gvk(resource)
                .ok()
                .map_or(PRIORITY_OTHER, |gvk| Self::get_priority(&gvk))
        });

        Ok(())
    }

    /// Get priority for a resource based on its GVK
    fn get_priority(gvk: &GroupVersionKind) -> u32 {
        match gvk.kind.as_str() {
            // Namespaces first (required for namespaced resources)
            "Namespace" => PRIORITY_NAMESPACE,

            // CRDs second (required for custom resources)
            "CustomResourceDefinition" => PRIORITY_CRD,

            // Service accounts third (required for RBAC)
            "ServiceAccount" => PRIORITY_SERVICE_ACCOUNT,

            // RBAC resources
            "Role" | "RoleBinding" | "ClusterRole" | "ClusterRoleBinding" => PRIORITY_RBAC,

            // Config resources (often referenced by workloads)
            "ConfigMap" | "Secret" => PRIORITY_CONFIG,

            // Services (can be created before or with workloads)
            "Service" | "Endpoints" | "EndpointSlice" => PRIORITY_SERVICE,

            // Workload resources
            "Deployment" | "StatefulSet" | "DaemonSet" | "Job" | "CronJob" | "ReplicaSet" | "Pod" => PRIORITY_WORKLOAD,

            // Everything else
            _ => PRIORITY_OTHER,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_namespace_sorted_first() {
        let mut resources = vec![
            json!({
                "apiVersion": "apps/v1",
                "kind": "Deployment",
                "metadata": {"name": "deploy"}
            }),
            json!({
                "apiVersion": "v1",
                "kind": "Namespace",
                "metadata": {"name": "ns"}
            }),
        ];

        ResourceOrdering::sort_by_priority(&mut resources).unwrap();

        assert_eq!(resources[0]["kind"], "Namespace");
        assert_eq!(resources[1]["kind"], "Deployment");
    }

    #[test]
    fn test_crd_before_custom_resource() {
        let mut resources = vec![
            json!({
                "apiVersion": "custom.io/v1",
                "kind": "CustomThing",
                "metadata": {"name": "custom"}
            }),
            json!({
                "apiVersion": "apiextensions.k8s.io/v1",
                "kind": "CustomResourceDefinition",
                "metadata": {"name": "crd"}
            }),
        ];

        ResourceOrdering::sort_by_priority(&mut resources).unwrap();

        assert_eq!(resources[0]["kind"], "CustomResourceDefinition");
        assert_eq!(resources[1]["kind"], "CustomThing");
    }

    #[test]
    fn test_configmap_before_deployment() {
        let mut resources = vec![
            json!({
                "apiVersion": "apps/v1",
                "kind": "Deployment",
                "metadata": {"name": "deploy"}
            }),
            json!({
                "apiVersion": "v1",
                "kind": "ConfigMap",
                "metadata": {"name": "config"}
            }),
        ];

        ResourceOrdering::sort_by_priority(&mut resources).unwrap();

        assert_eq!(resources[0]["kind"], "ConfigMap");
        assert_eq!(resources[1]["kind"], "Deployment");
    }

    #[test]
    fn test_service_account_before_rbac() {
        let mut resources = vec![
            json!({
                "apiVersion": "rbac.authorization.k8s.io/v1",
                "kind": "RoleBinding",
                "metadata": {"name": "rb"}
            }),
            json!({
                "apiVersion": "v1",
                "kind": "ServiceAccount",
                "metadata": {"name": "sa"}
            }),
        ];

        ResourceOrdering::sort_by_priority(&mut resources).unwrap();

        assert_eq!(resources[0]["kind"], "ServiceAccount");
        assert_eq!(resources[1]["kind"], "RoleBinding");
    }

    #[test]
    fn test_full_ordering_sequence() {
        let mut resources = vec![
            json!({"apiVersion": "apps/v1", "kind": "Deployment", "metadata": {"name": "d"}}),
            json!({"apiVersion": "v1", "kind": "Service", "metadata": {"name": "svc"}}),
            json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "cm"}}),
            json!({"apiVersion": "rbac.authorization.k8s.io/v1", "kind": "Role", "metadata": {"name": "r"}}),
            json!({"apiVersion": "v1", "kind": "ServiceAccount", "metadata": {"name": "sa"}}),
            json!({"apiVersion": "apiextensions.k8s.io/v1", "kind": "CustomResourceDefinition", "metadata": {"name": "crd"}}),
            json!({"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": "ns"}}),
        ];

        ResourceOrdering::sort_by_priority(&mut resources).unwrap();

        let kinds: Vec<&str> = resources.iter().map(|r| r["kind"].as_str().unwrap()).collect();

        assert_eq!(
            kinds,
            vec![
                "Namespace",
                "CustomResourceDefinition",
                "ServiceAccount",
                "Role",
                "ConfigMap",
                "Service",
                "Deployment"
            ]
        );
    }

    #[test]
    fn test_stable_sort_preserves_order() {
        let mut resources = vec![
            json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "cm1"}}),
            json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "cm2"}}),
            json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "cm3"}}),
        ];

        ResourceOrdering::sort_by_priority(&mut resources).unwrap();

        // Order should be preserved for resources with same priority
        assert_eq!(resources[0]["metadata"]["name"], "cm1");
        assert_eq!(resources[1]["metadata"]["name"], "cm2");
        assert_eq!(resources[2]["metadata"]["name"], "cm3");
    }

    #[test]
    fn test_secret_same_priority_as_configmap() {
        let mut resources = vec![
            json!({"apiVersion": "apps/v1", "kind": "Deployment", "metadata": {"name": "d"}}),
            json!({"apiVersion": "v1", "kind": "Secret", "metadata": {"name": "s"}}),
            json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "cm"}}),
        ];

        ResourceOrdering::sort_by_priority(&mut resources).unwrap();

        // Both Secret and ConfigMap should come before Deployment
        assert_eq!(resources[2]["kind"], "Deployment");

        // ConfigMap and Secret order should be stable (original order preserved)
        let first_two_kinds: Vec<&str> = resources[..2].iter().map(|r| r["kind"].as_str().unwrap()).collect();
        assert!(first_two_kinds.contains(&"Secret"));
        assert!(first_two_kinds.contains(&"ConfigMap"));
    }

    #[test]
    fn test_get_priority_namespace() {
        let gvk = GroupVersionKind {
            group: String::new(),
            version: "v1".to_string(),
            kind: "Namespace".to_string(),
        };
        assert_eq!(ResourceOrdering::get_priority(&gvk), PRIORITY_NAMESPACE);
    }

    #[test]
    fn test_get_priority_crd() {
        let gvk = GroupVersionKind {
            group: "apiextensions.k8s.io".to_string(),
            version: "v1".to_string(),
            kind: "CustomResourceDefinition".to_string(),
        };
        assert_eq!(ResourceOrdering::get_priority(&gvk), PRIORITY_CRD);
    }

    #[test]
    fn test_get_priority_unknown() {
        let gvk = GroupVersionKind {
            group: "custom.io".to_string(),
            version: "v1".to_string(),
            kind: "UnknownResource".to_string(),
        };
        assert_eq!(ResourceOrdering::get_priority(&gvk), PRIORITY_OTHER);
    }
}
