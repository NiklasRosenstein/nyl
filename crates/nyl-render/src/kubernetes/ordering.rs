//! Resource ordering for the Kubernetes apply and prune sequence.
//!
//! Apply order follows Argo CD's sync engine (gitops-engine `pkg/sync/sync_tasks.go`),
//! which adopted Helm's install order: Namespaces first, then cluster policy,
//! ServiceAccounts, Secrets and ConfigMaps, storage, CustomResourceDefinitions, RBAC,
//! Services, workloads, Ingresses, and APIServices. Every other kind, including custom
//! resources, follows these built-in kinds. A built-in kind is matched by API group and
//! kind, so a custom kind that shares a built-in name (such as a CNI's
//! `NetworkPolicy`) still follows the CustomResourceDefinition or APIService that
//! registers it.
//!
//! Admission webhook configurations are applied after everything else: registering a
//! webhook before the resources it admits would send their requests to a backend
//! that was created moments earlier and may not be ready.
//!
//! Like Argo CD, [`ResourceOrdering::apply_batches`] groups consecutive resources of
//! one kind into a batch. Resources in a batch are applied concurrently and each batch
//! finishes before the next starts, so a kind never races a kind it may depend on.

use crate::kubernetes::{extract_gvk, GroupVersionKind};
use crate::Result;
use serde_json::Value;
use std::ops::Range;

/// Built-in kinds in apply order, mirroring Argo CD's `kindOrder`, each with the API
/// groups that serve it.
const KIND_ORDER: &[(&[&str], &str)] = &[
    (&[""], "Namespace"),
    (&["networking.k8s.io", "extensions"], "NetworkPolicy"),
    (&[""], "ResourceQuota"),
    (&[""], "LimitRange"),
    (&["policy", "extensions"], "PodSecurityPolicy"),
    (&["policy"], "PodDisruptionBudget"),
    (&[""], "ServiceAccount"),
    (&[""], "Secret"),
    (&[""], "SecretList"),
    (&[""], "ConfigMap"),
    (&["storage.k8s.io"], "StorageClass"),
    (&[""], "PersistentVolume"),
    (&[""], "PersistentVolumeClaim"),
    (&[API_EXTENSIONS_GROUP], "CustomResourceDefinition"),
    (&[RBAC_GROUP], "ClusterRole"),
    (&[RBAC_GROUP], "ClusterRoleList"),
    (&[RBAC_GROUP], "ClusterRoleBinding"),
    (&[RBAC_GROUP], "ClusterRoleBindingList"),
    (&[RBAC_GROUP], "Role"),
    (&[RBAC_GROUP], "RoleList"),
    (&[RBAC_GROUP], "RoleBinding"),
    (&[RBAC_GROUP], "RoleBindingList"),
    (&[""], "Service"),
    (&["apps", "extensions"], "DaemonSet"),
    (&[""], "Pod"),
    (&[""], "ReplicationController"),
    (&["apps", "extensions"], "ReplicaSet"),
    (&["apps", "extensions"], "Deployment"),
    (&["autoscaling"], "HorizontalPodAutoscaler"),
    (&["apps"], "StatefulSet"),
    (&["batch"], "Job"),
    (&["batch"], "CronJob"),
    (&["networking.k8s.io"], "IngressClass"),
    (&["networking.k8s.io", "extensions"], "Ingress"),
    (&[API_REGISTRATION_GROUP], "APIService"),
];

const API_EXTENSIONS_GROUP: &str = "apiextensions.k8s.io";
const API_REGISTRATION_GROUP: &str = "apiregistration.k8s.io";
const ADMISSION_REGISTRATION_GROUP: &str = "admissionregistration.k8s.io";
const RBAC_GROUP: &str = "rbac.authorization.k8s.io";

/// Position in apply order. Variants sort in declaration order.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum OrderKey {
    /// A built-in kind, by its position in [`KIND_ORDER`].
    BuiltIn(usize),
    /// Any other kind, grouped by API group and kind so each forms one batch.
    Other { group: String, kind: String },
    /// Admission webhook configurations, applied after the resources they admit.
    AdmissionWebhook,
    /// Manifests without a parseable apiVersion and kind.
    Unparseable,
}

/// Resource ordering utility
pub struct ResourceOrdering;

impl ResourceOrdering {
    fn order_key(resource: &Value) -> OrderKey {
        extract_gvk(resource).map_or(OrderKey::Unparseable, |gvk| Self::gvk_order_key(&gvk))
    }

    fn gvk_order_key(gvk: &GroupVersionKind) -> OrderKey {
        if Self::is_admission_webhook(gvk) {
            return OrderKey::AdmissionWebhook;
        }
        let built_in = KIND_ORDER
            .iter()
            .position(|(groups, kind)| *kind == gvk.kind && groups.contains(&gvk.group.as_str()));
        match built_in {
            Some(position) => OrderKey::BuiltIn(position),
            None => OrderKey::Other {
                group: gvk.group.clone(),
                kind: gvk.kind.clone(),
            },
        }
    }

    /// Sort resources into apply order.
    ///
    /// The sort is stable: resources of one kind keep their relative input order.
    pub fn sort_by_priority(resources: &mut [Value]) -> Result<()> {
        resources.sort_by_cached_key(Self::order_key);
        Ok(())
    }

    /// Split resources, given by their group/version/kind in apply order, into
    /// consecutive apply batches of one kind.
    ///
    /// Each returned range is a run of adjacent resources with the same order key.
    /// Resources within a batch may be applied concurrently; a batch must finish
    /// before the next begins. Input that is not sorted still yields a barrier at
    /// every kind change, so the given order is never violated across kinds.
    pub fn apply_batches<'a>(gvks: impl IntoIterator<Item = &'a GroupVersionKind>) -> Vec<Range<usize>> {
        let mut batches: Vec<Range<usize>> = Vec::new();
        let mut current = None;
        for (index, gvk) in gvks.into_iter().enumerate() {
            let key = Self::gvk_order_key(gvk);
            match batches.last_mut() {
                Some(batch) if current.as_ref() == Some(&key) => batch.end = index + 1,
                _ => batches.push(index..index + 1),
            }
            current = Some(key);
        }
        batches
    }

    /// Whether this kind registers the kinds of another API group: a
    /// CustomResourceDefinition or an APIService, whose `spec.group` names that group.
    pub fn registers_api_group(gvk: &GroupVersionKind) -> bool {
        matches!(
            (gvk.group.as_str(), gvk.kind.as_str()),
            (API_EXTENSIONS_GROUP, "CustomResourceDefinition") | (API_REGISTRATION_GROUP, "APIService")
        )
    }

    /// Whether this kind is an admission webhook configuration.
    pub fn is_admission_webhook(gvk: &GroupVersionKind) -> bool {
        gvk.group == ADMISSION_REGISTRATION_GROUP
            && matches!(
                gvk.kind.as_str(),
                "MutatingWebhookConfiguration" | "ValidatingWebhookConfiguration"
            )
    }

    /// Whether pruning defers this kind until every other pruned resource is gone.
    ///
    /// CustomResourceDefinitions define, APIServices serve, and admission webhooks admit
    /// other resources, so they stay in place while those resources are deleted.
    /// Deleting a CRD first would cascade to its custom resources, racing their own
    /// prune and any controller finalizers.
    pub fn is_prune_last(gvk: &GroupVersionKind) -> bool {
        Self::registers_api_group(gvk) || Self::is_admission_webhook(gvk)
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

    fn batches_of(resources: &[Value]) -> Vec<Range<usize>> {
        let gvks: Vec<GroupVersionKind> = resources.iter().map(|r| extract_gvk(r).unwrap()).collect();
        ResourceOrdering::apply_batches(&gvks)
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
                "Widget",
                "ValidatingWebhookConfiguration",
            ]
        );
    }

    /// A custom kind named like a built-in kind is ordered as a custom resource, after
    /// the CustomResourceDefinition or APIService that registers its group.
    #[test]
    fn test_sort_orders_custom_kinds_sharing_built_in_names_after_their_registration() {
        let mut resources = vec![
            manifest("projectcalico.org/v3", "NetworkPolicy", "served"),
            manifest("crd.antrea.io/v1beta1", "NetworkPolicy", "custom"),
            manifest("networking.k8s.io/v1", "NetworkPolicy", "built-in"),
            manifest("apiregistration.k8s.io/v1", "APIService", "v3.projectcalico.org"),
            manifest(
                "apiextensions.k8s.io/v1",
                "CustomResourceDefinition",
                "networkpolicies.crd.antrea.io",
            ),
        ];

        ResourceOrdering::sort_by_priority(&mut resources).unwrap();

        let names: Vec<&str> = resources
            .iter()
            .map(|r| r["metadata"]["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            vec![
                "built-in",
                "networkpolicies.crd.antrea.io",
                "v3.projectcalico.org",
                "custom",
                "served"
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

        let batches: Vec<Vec<&str>> = batches_of(&resources)
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
        assert_eq!(batches_of(&resources), vec![0..1, 1..2, 2..3]);
        assert!(batches_of(&[]).is_empty());
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
