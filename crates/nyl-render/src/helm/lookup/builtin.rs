//! The built-in kinds a lookup render lists in discovery.
//!
//! Helm maps every object it renders, and every kind a chart looks up, through
//! discovery, and cannot map a kind discovery does not list. Discovery therefore
//! lists the built-in kinds of every API version the render declares, from this
//! static table of the Kubernetes types `k8s-openapi` knows. The table does not
//! depend on the render, the chart, or what the lookup source holds, so
//! `.Capabilities.APIVersions` is the same on the first render of a Release and
//! on every render after it.
//!
//! Beyond Helm's own default API versions, discovery thereby adds the
//! `group/version/Kind` entries of these kinds (an offline render lists API
//! versions only), so `.Capabilities.APIVersions.Has "apps/v1/Deployment"` is
//! true in a lookup render. A kind that is not built in is listed only when it
//! is declared as `group/version/Kind` or shipped in the chart's `crds/`.

use std::collections::BTreeSet;
use std::sync::OnceLock;

use k8s_openapi::{ClusterResourceScope, NamespaceResourceScope, Resource};

use super::model::KindInfo;

/// Whether a resource scope is the namespaced one.
trait Scope {
    const NAMESPACED: bool;
}

impl Scope for NamespaceResourceScope {
    const NAMESPACED: bool = true;
}

impl Scope for ClusterResourceScope {
    const NAMESPACED: bool = false;
}

fn kind_of<T: Resource>() -> KindInfo
where
    T::Scope: Scope,
{
    KindInfo::new(T::API_VERSION, T::KIND, <T::Scope as Scope>::NAMESPACED)
}

macro_rules! builtin_kinds {
    ($($kind:ty,)+) => {
        fn collect() -> Vec<KindInfo> {
            vec![$(kind_of::<$kind>()),+]
        }

        /// The plural each type serves, to check the guessed plural against.
        #[cfg(test)]
        fn served_plurals() -> Vec<(String, &'static str)> {
            vec![$((
                format!("{}/{}", <$kind as Resource>::API_VERSION, <$kind as Resource>::KIND),
                <$kind as Resource>::URL_PATH_SEGMENT,
            )),+]
        }
    };
}

builtin_kinds! {
    k8s_openapi::api::admissionregistration::v1::MutatingAdmissionPolicy,
    k8s_openapi::api::admissionregistration::v1::MutatingAdmissionPolicyBinding,
    k8s_openapi::api::admissionregistration::v1::MutatingWebhookConfiguration,
    k8s_openapi::api::admissionregistration::v1::ValidatingAdmissionPolicy,
    k8s_openapi::api::admissionregistration::v1::ValidatingAdmissionPolicyBinding,
    k8s_openapi::api::admissionregistration::v1::ValidatingWebhookConfiguration,
    k8s_openapi::api::admissionregistration::v1alpha1::MutatingAdmissionPolicy,
    k8s_openapi::api::admissionregistration::v1alpha1::MutatingAdmissionPolicyBinding,
    k8s_openapi::api::admissionregistration::v1beta1::MutatingAdmissionPolicy,
    k8s_openapi::api::admissionregistration::v1beta1::MutatingAdmissionPolicyBinding,
    k8s_openapi::api::apiserverinternal::v1alpha1::StorageVersion,
    k8s_openapi::api::apps::v1::ControllerRevision,
    k8s_openapi::api::apps::v1::DaemonSet,
    k8s_openapi::api::apps::v1::Deployment,
    k8s_openapi::api::apps::v1::ReplicaSet,
    k8s_openapi::api::apps::v1::StatefulSet,
    k8s_openapi::api::authentication::v1::SelfSubjectReview,
    k8s_openapi::api::authentication::v1::TokenReview,
    k8s_openapi::api::authorization::v1::LocalSubjectAccessReview,
    k8s_openapi::api::authorization::v1::SelfSubjectAccessReview,
    k8s_openapi::api::authorization::v1::SelfSubjectRulesReview,
    k8s_openapi::api::authorization::v1::SubjectAccessReview,
    k8s_openapi::api::autoscaling::v1::HorizontalPodAutoscaler,
    k8s_openapi::api::autoscaling::v2::HorizontalPodAutoscaler,
    k8s_openapi::api::batch::v1::CronJob,
    k8s_openapi::api::batch::v1::Job,
    k8s_openapi::api::certificates::v1::CertificateSigningRequest,
    k8s_openapi::api::certificates::v1::ClusterTrustBundle,
    k8s_openapi::api::certificates::v1::PodCertificateRequest,
    k8s_openapi::api::certificates::v1beta1::ClusterTrustBundle,
    k8s_openapi::api::certificates::v1beta1::PodCertificateRequest,
    k8s_openapi::api::coordination::v1::Lease,
    k8s_openapi::api::coordination::v1alpha2::LeaseCandidate,
    k8s_openapi::api::coordination::v1beta1::LeaseCandidate,
    k8s_openapi::api::core::v1::Binding,
    k8s_openapi::api::core::v1::ComponentStatus,
    k8s_openapi::api::core::v1::ConfigMap,
    k8s_openapi::api::core::v1::Endpoints,
    k8s_openapi::api::core::v1::Event,
    k8s_openapi::api::core::v1::LimitRange,
    k8s_openapi::api::core::v1::Namespace,
    k8s_openapi::api::core::v1::Node,
    k8s_openapi::api::core::v1::PersistentVolume,
    k8s_openapi::api::core::v1::PersistentVolumeClaim,
    k8s_openapi::api::core::v1::Pod,
    k8s_openapi::api::core::v1::PodTemplate,
    k8s_openapi::api::core::v1::ReplicationController,
    k8s_openapi::api::core::v1::ResourceQuota,
    k8s_openapi::api::core::v1::Secret,
    k8s_openapi::api::core::v1::Service,
    k8s_openapi::api::core::v1::ServiceAccount,
    k8s_openapi::api::discovery::v1::EndpointSlice,
    k8s_openapi::api::events::v1::Event,
    k8s_openapi::api::flowcontrol::v1::FlowSchema,
    k8s_openapi::api::flowcontrol::v1::PriorityLevelConfiguration,
    k8s_openapi::api::lifecycle::v1alpha1::Eviction,
    k8s_openapi::api::lifecycle::v1alpha1::EvictionRequest,
    k8s_openapi::api::networking::v1::IPAddress,
    k8s_openapi::api::networking::v1::Ingress,
    k8s_openapi::api::networking::v1::IngressClass,
    k8s_openapi::api::networking::v1::NetworkPolicy,
    k8s_openapi::api::networking::v1::ServiceCIDR,
    k8s_openapi::api::node::v1::RuntimeClass,
    k8s_openapi::api::policy::v1::PodDisruptionBudget,
    k8s_openapi::api::rbac::v1::ClusterRole,
    k8s_openapi::api::rbac::v1::ClusterRoleBinding,
    k8s_openapi::api::rbac::v1::Role,
    k8s_openapi::api::rbac::v1::RoleBinding,
    k8s_openapi::api::resource::v1::DeviceClass,
    k8s_openapi::api::resource::v1::DeviceTaintRule,
    k8s_openapi::api::resource::v1::ResourceClaim,
    k8s_openapi::api::resource::v1::ResourceClaimTemplate,
    k8s_openapi::api::resource::v1::ResourceSlice,
    k8s_openapi::api::resource::v1alpha3::DeviceTaintRule,
    k8s_openapi::api::resource::v1alpha3::ResourcePoolStatusRequest,
    k8s_openapi::api::resource::v1beta1::DeviceClass,
    k8s_openapi::api::resource::v1beta1::ResourceClaim,
    k8s_openapi::api::resource::v1beta1::ResourceClaimTemplate,
    k8s_openapi::api::resource::v1beta1::ResourceSlice,
    k8s_openapi::api::resource::v1beta2::DeviceClass,
    k8s_openapi::api::resource::v1beta2::DeviceTaintRule,
    k8s_openapi::api::resource::v1beta2::ResourceClaim,
    k8s_openapi::api::resource::v1beta2::ResourceClaimTemplate,
    k8s_openapi::api::resource::v1beta2::ResourceSlice,
    k8s_openapi::api::scheduling::v1::PriorityClass,
    k8s_openapi::api::scheduling::v1alpha3::CompositePodGroup,
    k8s_openapi::api::scheduling::v1alpha3::PodGroup,
    k8s_openapi::api::scheduling::v1alpha3::Workload,
    k8s_openapi::api::scheduling::v1beta1::PodGroup,
    k8s_openapi::api::scheduling::v1beta1::Workload,
    k8s_openapi::api::storage::v1::CSIDriver,
    k8s_openapi::api::storage::v1::CSINode,
    k8s_openapi::api::storage::v1::CSIStorageCapacity,
    k8s_openapi::api::storage::v1::StorageClass,
    k8s_openapi::api::storage::v1::VolumeAttachment,
    k8s_openapi::api::storage::v1::VolumeAttributesClass,
    k8s_openapi::api::storagemigration::v1::StorageVersionMigration,
    k8s_openapi::api::storagemigration::v1beta1::StorageVersionMigration,
    k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition,
    k8s_openapi::kube_aggregator::pkg::apis::apiregistration::v1::APIService,
}

/// Every built-in kind of every API version `k8s-openapi` knows.
pub fn builtin_kinds() -> &'static [KindInfo] {
    static KINDS: OnceLock<Vec<KindInfo>> = OnceLock::new();
    KINDS.get_or_init(collect)
}

/// The built-in kinds of the API versions in `group_versions`.
pub fn builtin_kinds_of(group_versions: &BTreeSet<String>) -> impl Iterator<Item = KindInfo> + '_ {
    builtin_kinds()
        .iter()
        .filter(|kind| group_versions.contains(&kind.api_version()))
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches the guessed plural drifting from the one the API serves: Helm requests the
    /// plural discovery advertised, and the lookup source indexes objects by the guessed one.
    #[test]
    fn test_every_builtin_kind_has_the_plural_the_api_serves() {
        let served = served_plurals();
        assert!(served.len() > 90, "{}", served.len());
        for (gvk, plural) in served {
            let kind = gvk.rsplit('/').next().unwrap();
            assert_eq!(super::super::model::guess_resource_names(kind).0, plural, "{gvk}");
        }
    }

    #[test]
    fn test_builtin_scopes_and_versions_come_from_the_types() {
        let find = |api_version: &str, kind: &str| {
            builtin_kinds()
                .iter()
                .find(|known| known.api_version() == api_version && known.kind == kind)
                .cloned()
        };
        assert!(find("apps/v1", "Deployment").unwrap().namespaced);
        assert!(!find("rbac.authorization.k8s.io/v1", "ClusterRole").unwrap().namespaced);
        assert!(!find("v1", "Namespace").unwrap().namespaced);
        assert!(find("apps/v1", "Widget").is_none());
        let declared = BTreeSet::from(["apps/v1".to_owned()]);
        let listed = builtin_kinds_of(&declared)
            .map(|kind| kind.kind)
            .collect::<BTreeSet<_>>();
        assert!(
            listed.contains("StatefulSet") && !listed.contains("Secret"),
            "{listed:?}"
        );
    }
}
