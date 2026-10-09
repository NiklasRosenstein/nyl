//! Resource ordering for the Kubernetes apply and prune sequence.
//!
//! Apply order follows Argo CD's sync engine (gitops-engine `pkg/sync/sync_tasks.go`),
//! which adopted Helm's install order: Namespaces first, then cluster policy,
//! ServiceAccounts, Secrets and ConfigMaps, storage, CustomResourceDefinitions, RBAC,
//! Services, workloads, Ingresses, and APIServices. Every other kind, including custom
//! resources and admission webhook configurations, follows the known kinds.
//!
//! Like Argo CD, [`ResourceOrdering::apply_batches`] groups consecutive resources of
//! one kind into a batch. Resources in a batch are applied concurrently and each batch
//! finishes before the next starts, so a kind never races a kind it may depend on.
//!
//! Pruning deletes concurrently, as Argo CD does, except that CustomResourceDefinitions,
//! APIServices, and admission webhook configurations are deleted after everything else
//! (Argo CD's `PruneLast`), so they are never removed before the resources that relate
//! to them.

use crate::kubernetes::{extract_gvk, GroupVersionKind};
use crate::Result;
use serde_json::Value;
use std::ops::Range;

/// Known kinds in apply order, mirroring Argo CD's `kindOrder`.
const KIND_ORDER: &[&str] = &[
    "Namespace",
    "NetworkPolicy",
    "ResourceQuota",
    "LimitRange",
    "PodSecurityPolicy",
    "PodDisruptionBudget",
    "ServiceAccount",
    "Secret",
    "SecretList",
    "ConfigMap",
    "StorageClass",
    "PersistentVolume",
    "PersistentVolumeClaim",
    "CustomResourceDefinition",
    "ClusterRole",
    "ClusterRoleList",
    "ClusterRoleBinding",
    "ClusterRoleBindingList",
    "Role",
    "RoleList",
    "RoleBinding",
    "RoleBindingList",
    "Service",
    "DaemonSet",
    "Pod",
    "ReplicationController",
    "ReplicaSet",
    "Deployment",
    "HorizontalPodAutoscaler",
    "StatefulSet",
    "Job",
    "CronJob",
    "IngressClass",
    "Ingress",
    "APIService",
];

/// Sort key: known kinds by their position, then unknown kinds grouped by group and
/// kind so each forms one batch. Unparseable manifests sort last.
type OrderKey = (usize, String, String);

/// Resource ordering utility
pub struct ResourceOrdering;

impl ResourceOrdering {
    fn order_key(resource: &Value) -> OrderKey {
        match extract_gvk(resource) {
            Ok(gvk) => Self::gvk_order_key(&gvk),
            Err(_) => (KIND_ORDER.len() + 1, String::new(), String::new()),
        }
    }

    fn gvk_order_key(gvk: &GroupVersionKind) -> OrderKey {
        match KIND_ORDER.iter().position(|kind| *kind == gvk.kind) {
            Some(position) => (position, String::new(), String::new()),
            None => (KIND_ORDER.len(), gvk.group.clone(), gvk.kind.clone()),
        }
    }

    /// Sort resources into apply order.
    ///
    /// The sort is stable: resources of one kind keep their relative input order.
    pub fn sort_by_priority(resources: &mut [Value]) -> Result<()> {
        resources.sort_by_cached_key(Self::order_key);
        Ok(())
    }

    /// Split manifests into consecutive apply batches of one kind.
    ///
    /// Each returned range is a run of adjacent manifests with the same order key.
    /// Manifests within a batch may be applied concurrently; a batch must finish
    /// before the next begins. Input that is not sorted still yields a barrier at
    /// every kind change, so the given order is never violated across kinds.
    pub fn apply_batches(resources: &[Value]) -> Vec<Range<usize>> {
        let mut batches: Vec<Range<usize>> = Vec::new();
        let mut current = None;
        for (index, resource) in resources.iter().enumerate() {
            let key = Self::order_key(resource);
            match batches.last_mut() {
                Some(batch) if current.as_ref() == Some(&key) => batch.end = index + 1,
                _ => batches.push(index..index + 1),
            }
            current = Some(key);
        }
        batches
    }

    /// Whether pruning defers this kind until every other pruned resource is gone.
    ///
    /// CustomResourceDefinitions define, APIServices serve, and admission webhooks admit
    /// other resources, so they stay in place while those resources are deleted.
    /// Deleting a CRD first would cascade to its custom resources, racing their own
    /// prune and any controller finalizers.
    pub fn is_prune_last(gvk: &GroupVersionKind) -> bool {
        matches!(
            (gvk.group.as_str(), gvk.kind.as_str()),
            ("apiextensions.k8s.io", "CustomResourceDefinition")
                | ("apiregistration.k8s.io", "APIService")
                | (
                    "admissionregistration.k8s.io",
                    "MutatingWebhookConfiguration" | "ValidatingWebhookConfiguration"
                )
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn manifest(api_version: &str, kind: &str, name: &str) -> Value {
        json!({"apiVersion": api_version, "kind": kind, "metadata": {"name": name}})
    }

    fn kinds(resources: &[Value]) -> Vec<&str> {
        resources.iter().map(|r| r["kind"].as_str().unwrap()).collect()
    }

    #[test]
    fn test_sort_follows_argo_cd_kind_order() {
        let mut resources = vec![
            manifest(
                "admissionregistration.k8s.io/v1",
                "ValidatingWebhookConfiguration",
                "hook",
            ),
            manifest("example.com/v1", "Widget", "w"),
            manifest("apiregistration.k8s.io/v1", "APIService", "api"),
            manifest("apps/v1", "Deployment", "d"),
            manifest("v1", "Service", "svc"),
            manifest("rbac.authorization.k8s.io/v1", "RoleBinding", "rb"),
            manifest("rbac.authorization.k8s.io/v1", "Role", "r"),
            manifest("rbac.authorization.k8s.io/v1", "ClusterRole", "cr"),
            manifest("apiextensions.k8s.io/v1", "CustomResourceDefinition", "crd"),
            manifest("v1", "ConfigMap", "cm"),
            manifest("v1", "Secret", "s"),
            manifest("v1", "ServiceAccount", "sa"),
            manifest("v1", "Namespace", "ns"),
        ];

        ResourceOrdering::sort_by_priority(&mut resources).unwrap();

        assert_eq!(
            kinds(&resources),
            vec![
                "Namespace",
                "ServiceAccount",
                "Secret",
                "ConfigMap",
                "CustomResourceDefinition",
                "ClusterRole",
                "Role",
                "RoleBinding",
                "Service",
                "Deployment",
                "APIService",
                "ValidatingWebhookConfiguration",
                "Widget",
            ]
        );
    }

    #[test]
    fn test_sort_is_stable_within_a_kind() {
        let mut resources = vec![
            manifest("v1", "ConfigMap", "cm2"),
            manifest("v1", "Namespace", "ns"),
            manifest("v1", "ConfigMap", "cm1"),
            manifest("v1", "ConfigMap", "cm3"),
        ];

        ResourceOrdering::sort_by_priority(&mut resources).unwrap();

        let names: Vec<&str> = resources
            .iter()
            .map(|r| r["metadata"]["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["ns", "cm2", "cm1", "cm3"]);
    }

    /// Each kind forms one batch, so a kind never shares a concurrent batch with a
    /// kind it may depend on, including custom kinds of different groups.
    #[test]
    fn test_apply_batches_hold_one_kind_each() {
        let mut resources = vec![
            manifest("example.com/v1", "Widget", "w1"),
            manifest("other.io/v1", "Widget", "o1"),
            manifest("example.com/v1", "Gadget", "g1"),
            manifest("v1", "ConfigMap", "cm1"),
            manifest("example.com/v1", "Widget", "w2"),
            manifest("v1", "ConfigMap", "cm2"),
            manifest("v1", "Namespace", "ns"),
        ];
        ResourceOrdering::sort_by_priority(&mut resources).unwrap();

        let batches: Vec<Vec<&str>> = ResourceOrdering::apply_batches(&resources)
            .into_iter()
            .map(|batch| {
                resources[batch]
                    .iter()
                    .map(|r| r["metadata"]["name"].as_str().unwrap())
                    .collect()
            })
            .collect();
        assert_eq!(
            batches,
            vec![vec!["ns"], vec!["cm1", "cm2"], vec!["g1"], vec!["w1", "w2"], vec!["o1"]]
        );
    }

    #[test]
    fn test_apply_batches_keep_barriers_for_unsorted_input() {
        let resources = vec![
            manifest("v1", "ConfigMap", "a"),
            manifest("v1", "Namespace", "b"),
            manifest("v1", "ConfigMap", "c"),
        ];
        assert_eq!(ResourceOrdering::apply_batches(&resources), vec![0..1, 1..2, 2..3]);
        assert!(ResourceOrdering::apply_batches(&[]).is_empty());
    }

    #[test]
    fn test_is_prune_last_requires_the_built_in_group() {
        let gvk = |group: &str, kind: &str| GroupVersionKind {
            group: group.to_string(),
            version: "v1".to_string(),
            kind: kind.to_string(),
        };
        assert!(ResourceOrdering::is_prune_last(&gvk(
            "apiregistration.k8s.io",
            "APIService"
        )));
        assert!(ResourceOrdering::is_prune_last(&gvk(
            "admissionregistration.k8s.io",
            "ValidatingWebhookConfiguration"
        )));
        assert!(ResourceOrdering::is_prune_last(&gvk(
            "apiextensions.k8s.io",
            "CustomResourceDefinition"
        )));
        assert!(!ResourceOrdering::is_prune_last(&gvk("example.com", "APIService")));
        assert!(!ResourceOrdering::is_prune_last(&gvk("apps", "Deployment")));
    }
}
