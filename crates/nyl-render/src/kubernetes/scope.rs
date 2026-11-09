//! The one place that decides whether a Kubernetes kind is namespaced.
//!
//! Every consumer asks [`ScopeResolver`]: the Kubernetes GitOps tree and its
//! AppProject derivation, and `nyl apply`, `diff`, `release rollback`, and the direct
//! reconciler, so a declaration yields one answer whichever command reads it. The
//! lookup order is fixed:
//!
//! 1. The CustomResourceDefinitions among the manifests (or the rendered tree). They
//!    are what will be applied, and a dry run never creates them.
//! 2. The Cluster's recorded Kubernetes capabilities, which include the scopes and
//!    API versions of CRDs vendored by `nyl capture cluster --crds`. Only a declared
//!    Cluster supplies them.
//! 3. The live cluster, as the last resort ([`ScopeResolver::resolve`]), and only for a
//!    command that connects to one. Nothing else in the declaration decides a scope
//!    through it.
//!
//! A CRD is read the same way everywhere: it names a group, a kind, and a scope, and,
//! when it lists versions, serves at least one of them; a kind with no served version
//! cannot be requested, so such a CRD declares nothing. Scope belongs to the kind, not
//! to a version. When several CRD documents declare one kind, the later document in
//! manifest order wins, as the later of two documents for one object does. A kind
//! none of the sources knows has an unknown scope, never a guess.

use std::collections::BTreeMap;

use serde_json::Value;

use super::{GroupVersionKind, KubeClient};
use crate::resources::{ClusterKubernetesCapabilities, ResourceScope};
use crate::Result;

/// The kind a CustomResourceDefinition defines, as the CRD declares it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrdDeclaration {
    pub group: String,
    pub kind: String,
    pub scope: ResourceScope,
    /// The versions the CRD serves, which are requestable.
    pub served_versions: Vec<String>,
}

impl CrdDeclaration {
    /// The declaration in a CustomResourceDefinition's `spec`, or `None` when it
    /// names no group, kind, or valid scope, or lists versions and serves none.
    pub fn from_spec(spec: &Value) -> Option<Self> {
        let group = spec
            .get("group")
            .and_then(Value::as_str)
            .filter(|group| !group.is_empty())?;
        let kind = spec
            .get("names")
            .and_then(|names| names.get("kind"))
            .and_then(Value::as_str)
            .filter(|kind| !kind.is_empty())?;
        let scope = match spec.get("scope").and_then(Value::as_str)? {
            "Namespaced" => ResourceScope::Namespaced,
            "Cluster" => ResourceScope::Cluster,
            _ => return None,
        };
        let served_versions: Vec<String> = spec
            .get("versions")
            .and_then(Value::as_array)
            .map(|versions| {
                versions
                    .iter()
                    .filter(|version| version.get("served").and_then(Value::as_bool) == Some(true))
                    .filter_map(|version| version.get("name").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        if spec.get("versions").and_then(Value::as_array).is_some() && served_versions.is_empty() {
            return None;
        }
        Some(Self {
            group: group.to_string(),
            kind: kind.to_string(),
            scope,
            served_versions,
        })
    }

    /// The declaration of `manifest`, when it is a CustomResourceDefinition.
    pub fn from_manifest(manifest: &Value) -> Option<Self> {
        let in_api_extensions = manifest
            .get("apiVersion")
            .and_then(Value::as_str)
            .is_some_and(|api_version| api_version.split('/').next() == Some("apiextensions.k8s.io"));
        if !in_api_extensions || manifest.get("kind").and_then(Value::as_str) != Some("CustomResourceDefinition") {
            return None;
        }
        Self::from_spec(manifest.get("spec")?)
    }

    /// The group, version, and kind of every served version.
    pub fn gvks(&self) -> impl Iterator<Item = GroupVersionKind> + '_ {
        self.served_versions.iter().map(|version| GroupVersionKind {
            group: self.group.clone(),
            version: version.clone(),
            kind: self.kind.clone(),
        })
    }
}

/// Resolves a kind's scope: manifest CRDs, then the Cluster's capabilities, then,
/// through [`Self::resolve`] only, the live cluster. See the module documentation.
#[derive(Debug, Default, Clone)]
pub struct ScopeResolver {
    declared: BTreeMap<(String, String), ResourceScope>,
    capabilities: Option<ClusterKubernetesCapabilities>,
}

impl ScopeResolver {
    /// A resolver over the CRDs among `manifests` and the capabilities of a declared
    /// Cluster, when one is in play.
    pub fn new<'m>(
        capabilities: Option<&ClusterKubernetesCapabilities>,
        manifests: impl IntoIterator<Item = &'m Value>,
    ) -> Self {
        let mut declared = BTreeMap::new();
        for declaration in manifests.into_iter().filter_map(CrdDeclaration::from_manifest) {
            declared.insert((declaration.group, declaration.kind), declaration.scope);
        }
        Self {
            declared,
            capabilities: capabilities.cloned(),
        }
    }

    /// The scope the declaration gives a kind (`""` names the core group): a manifest
    /// CRD, then the Cluster's capabilities. `None` when neither knows it.
    pub fn scope(&self, group: &str, kind: &str) -> Option<ResourceScope> {
        self.declared
            .get(&(group.to_string(), kind.to_string()))
            .copied()
            .or_else(|| {
                self.capabilities
                    .as_ref()
                    .and_then(|capabilities| capabilities.scope_of(group, kind))
            })
    }

    /// Whether the declaration says the kind is namespaced.
    pub fn is_namespaced(&self, gvk: &GroupVersionKind) -> Option<bool> {
        self.scope(&gvk.group, &gvk.kind)
            .map(|scope| scope == ResourceScope::Namespaced)
    }

    /// Whether the kind is namespaced: as the declaration says, otherwise as the live
    /// cluster reports it, which is only asked when nothing declared answers.
    pub async fn resolve(&self, client: &dyn KubeClient, gvk: &GroupVersionKind) -> Result<bool> {
        match self.is_namespaced(gvk) {
            Some(namespaced) => Ok(namespaced),
            None => client.is_namespaced(gvk).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kubernetes::MockKubeClient;
    use serde_json::json;

    fn crd(group: &str, kind: &str, scope: &str, versions: Value) -> Value {
        json!({
            "apiVersion": "apiextensions.k8s.io/v1",
            "kind": "CustomResourceDefinition",
            "metadata": {"name": format!("{}s.{group}", kind.to_lowercase())},
            "spec": {"group": group, "scope": scope, "names": {"kind": kind}, "versions": versions}
        })
    }

    fn served() -> Value {
        json!([{"name": "v1", "served": true, "storage": true}])
    }

    fn gvk(version_kind: &str, kind: &str) -> GroupVersionKind {
        GroupVersionKind::from_api_version_kind(version_kind, kind).unwrap()
    }

    /// Cluster capabilities that record `Widget` as namespaced and `Gadget` as cluster-scoped.
    fn capabilities() -> ClusterKubernetesCapabilities {
        ClusterKubernetesCapabilities {
            kube_version: None,
            api_versions: vec!["example.com/v1/Widget".to_string(), "example.com/v1/Gadget".to_string()],
            cluster_scoped_kinds: vec!["example.com/Gadget".to_string()],
            vendored_crds: false,
        }
    }

    #[test]
    fn test_scope_resolver_prefers_manifest_crds_over_the_cluster_capabilities() {
        let cluster_wide = crd("example.com", "Widget", "Cluster", served());
        let resolver = ScopeResolver::new(Some(&capabilities()), [&cluster_wide]);

        // The manifest CRD overrides the recorded scope; the other kind keeps the recorded one.
        assert_eq!(resolver.scope("example.com", "Widget"), Some(ResourceScope::Cluster));
        assert_eq!(resolver.scope("example.com", "Gadget"), Some(ResourceScope::Cluster));
        let without = ScopeResolver::new(Some(&capabilities()), []);
        assert_eq!(without.scope("example.com", "Widget"), Some(ResourceScope::Namespaced));
    }

    #[test]
    fn test_scope_resolver_scope_of_a_kind_nothing_declares_is_unknown() {
        let resolver = ScopeResolver::new(Some(&capabilities()), []);
        assert_eq!(resolver.scope("example.com", "Unlisted"), None);
        assert_eq!(ScopeResolver::default().scope("example.com", "Widget"), None);
    }

    #[tokio::test]
    async fn test_scope_resolver_resolve_asks_the_live_cluster_only_when_nothing_declared_answers() {
        // The mock cluster reports every non-built-in kind as namespaced.
        let client = MockKubeClient::new();
        let cluster_wide = crd("example.com", "Widget", "Cluster", served());
        let resolver = ScopeResolver::new(Some(&capabilities()), [&cluster_wide]);

        assert!(!resolver
            .resolve(&client, &gvk("example.com/v1", "Widget"))
            .await
            .unwrap());
        assert!(!resolver
            .resolve(&client, &gvk("example.com/v1", "Gadget"))
            .await
            .unwrap());
        assert!(resolver
            .resolve(&client, &gvk("example.com/v1", "Unlisted"))
            .await
            .unwrap());
    }

    /// A CRD that serves no version declares no kind; one that does declares it whatever
    /// the version of the object, and one that lists no versions still counts.
    #[test]
    fn test_scope_resolver_reads_only_crds_that_serve_a_version() {
        let unserved = crd(
            "example.com",
            "Widget",
            "Cluster",
            json!([{"name": "v1", "served": false}]),
        );
        let served_beta = crd(
            "example.com",
            "Gadget",
            "Namespaced",
            json!([{"name": "v1beta1", "served": true}, {"name": "v1", "served": false}]),
        );
        let no_versions = json!({"apiVersion": "apiextensions.k8s.io/v1", "kind": "CustomResourceDefinition",
            "spec": {"group": "example.com", "scope": "Cluster", "names": {"kind": "Plain"}}});

        let resolver = ScopeResolver::new(None, [&unserved, &served_beta, &no_versions]);

        assert_eq!(resolver.scope("example.com", "Widget"), None);
        assert_eq!(resolver.scope("example.com", "Gadget"), Some(ResourceScope::Namespaced));
        assert_eq!(resolver.scope("example.com", "Plain"), Some(ResourceScope::Cluster));
        let declaration = CrdDeclaration::from_manifest(&served_beta).unwrap();
        assert_eq!(
            declaration.gvks().collect::<Vec<_>>(),
            [gvk("example.com/v1beta1", "Gadget")]
        );
    }

    /// Only a CustomResourceDefinition of apiextensions.k8s.io declares a kind.
    #[test]
    fn test_scope_resolver_ignores_documents_that_are_not_crds() {
        let lookalike = json!({"apiVersion": "other.io/v1", "kind": "CustomResourceDefinition",
            "spec": {"group": "example.com", "scope": "Cluster", "names": {"kind": "Widget"}}});
        let invalid_scope = crd("example.com", "Gadget", "Everywhere", served());

        let resolver = ScopeResolver::new(None, [&lookalike, &invalid_scope]);

        assert_eq!(resolver.scope("example.com", "Widget"), None);
        assert_eq!(resolver.scope("example.com", "Gadget"), None);
    }

    /// Documents that disagree on a kind's scope resolve the same way every time: the
    /// later one in manifest order wins.
    #[test]
    fn test_scope_resolver_conflicting_crds_resolve_to_the_later_document() {
        let namespaced = crd("example.com", "Widget", "Namespaced", served());
        let cluster = crd("example.com", "Widget", "Cluster", served());

        let later_cluster = ScopeResolver::new(None, [&namespaced, &cluster]);
        let later_namespaced = ScopeResolver::new(None, [&cluster, &namespaced]);

        assert_eq!(
            later_cluster.scope("example.com", "Widget"),
            Some(ResourceScope::Cluster)
        );
        assert_eq!(
            later_namespaced.scope("example.com", "Widget"),
            Some(ResourceScope::Namespaced)
        );
    }
}
