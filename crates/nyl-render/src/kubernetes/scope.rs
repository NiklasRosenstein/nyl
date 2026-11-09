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
//!    Cluster supplies them. A stale vendored snapshot therefore takes precedence over
//!    the live cluster; `nyl capture cluster --crds` refreshes it.
//! 3. The built-in Kubernetes kinds, whose scope is fixed ([`builtin_scope`]).
//! 4. The live cluster, as the last resort ([`ScopeResolver::resolve`]), and only for a
//!    command that connects to one (`apply`, `diff`, and `release rollback`). Nothing
//!    else in the declaration decides a scope through it.
//!
//! When a kind's scope comes from the Cluster's recorded capabilities, `resolve` also
//! asks the live cluster, which that command has already connected to, and reports a
//! disagreement once per kind as [`ScopeMismatch`], a warning with a stable code. The
//! recorded answer is still the one used, so the result is what render and
//! publication give. Render paths never call `resolve`, so they never touch a cluster.
//!
//! Steps 1 to 3 are the declaration ([`ScopeResolver::scope`]); a caller adds the
//! CRDs it knows of: the tree those of the target, `apply`, `diff`, and rollback those
//! of the manifests they read, and the direct reconciler those of every Release of the
//! target.
//!
//! A CRD is read the same way everywhere: it names a group, a kind, and a scope, and,
//! when it lists versions, serves at least one of them; a kind with no served version
//! cannot be requested, so such a CRD declares nothing. Scope belongs to the kind, not
//! to a version. When several CRD documents declare one kind, the later document in
//! manifest order wins, as the later of two documents for one object does. A kind
//! none of the sources knows has an unknown scope, never a guess.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use nyl_core::orchestration::codes::Code;
use serde_json::Value;

use super::{builtin_scope, GroupVersionKind, KubeClient};
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

/// A kind the Cluster's recorded capabilities and the live cluster disagree on.
/// The recorded scope is the one used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeMismatch {
    /// The API group, `""` for the core group.
    pub group: String,
    pub kind: String,
    /// What the Cluster's capabilities (vendored CRDs included) record.
    pub recorded: ResourceScope,
    /// What the live cluster's discovery reports.
    pub live: ResourceScope,
}

impl ScopeMismatch {
    /// The stable code of the warning.
    pub const CODE: Code = Code::KubernetesScopeMismatch;

    /// What happened.
    pub fn message(&self) -> String {
        let describe = |scope: ResourceScope| match scope {
            ResourceScope::Namespaced => "namespaced",
            ResourceScope::Cluster => "cluster-scoped",
        };
        let group = if self.group.is_empty() { "core" } else { &self.group };
        format!(
            "The Cluster records {group}/{} as {}, but the live cluster reports it as {}; the recorded scope is used",
            self.kind,
            describe(self.recorded),
            describe(self.live)
        )
    }

    /// How to fix it.
    pub const FIX: &'static str =
        "re-run `nyl capture cluster <name> --crds` for the target's Cluster to refresh its recorded capabilities and vendored CRDs, then commit the result";
}

/// Which kinds were compared with the live cluster, shared by every clone of a resolver
/// so a kind is compared, and reported, once per run.
#[derive(Debug, Default)]
struct Comparisons {
    checked: BTreeSet<(String, String)>,
    mismatches: Vec<ScopeMismatch>,
}

/// Resolves a kind's scope: manifest CRDs, then the Cluster's capabilities, then,
/// through [`Self::resolve`] only, the live cluster. See the module documentation.
#[derive(Debug, Default, Clone)]
pub struct ScopeResolver {
    declared: BTreeMap<(String, String), ResourceScope>,
    capabilities: Option<ClusterKubernetesCapabilities>,
    comparisons: Arc<Mutex<Comparisons>>,
}

impl ScopeResolver {
    /// A resolver over the CRDs among `manifests` and the capabilities of a declared
    /// Cluster, when one is in play.
    pub fn new<'m>(
        capabilities: Option<&ClusterKubernetesCapabilities>,
        manifests: impl IntoIterator<Item = &'m Value>,
    ) -> Self {
        Self::from_capabilities(capabilities).with_manifests(manifests)
    }

    /// A resolver over the capabilities of a declared Cluster alone (`None` without
    /// one); add the CRDs of the manifests being read with [`Self::with_manifests`].
    pub fn from_capabilities(capabilities: Option<&ClusterKubernetesCapabilities>) -> Self {
        Self {
            declared: BTreeMap::new(),
            capabilities: capabilities.cloned(),
            comparisons: Arc::default(),
        }
    }

    /// This resolver that also knows the CRDs among `manifests`. They come after the
    /// CRDs it already holds, so on a conflict they win.
    #[must_use]
    pub fn with_manifests<'m>(mut self, manifests: impl IntoIterator<Item = &'m Value>) -> Self {
        for declaration in manifests.into_iter().filter_map(CrdDeclaration::from_manifest) {
            self.declared
                .insert((declaration.group, declaration.kind), declaration.scope);
        }
        self
    }

    /// The scope the declaration gives a kind (`""` names the core group): a manifest
    /// CRD, then the Cluster's capabilities, then the built-in kinds. `None` when none
    /// of them knows it.
    pub fn scope(&self, group: &str, kind: &str) -> Option<ResourceScope> {
        self.declared_scope(group, kind).map(|(scope, _)| scope)
    }

    /// The declared scope of a kind, and whether the Cluster's capabilities gave it.
    fn declared_scope(&self, group: &str, kind: &str) -> Option<(ResourceScope, bool)> {
        if let Some(scope) = self.declared.get(&(group.to_string(), kind.to_string())) {
            return Some((*scope, false));
        }
        let recorded = self
            .capabilities
            .as_ref()
            .and_then(|capabilities| capabilities.scope_of(group, kind));
        match recorded {
            Some(scope) => Some((scope, true)),
            None => builtin_scope(group, kind).map(|scope| (scope, false)),
        }
    }

    /// Whether the declaration says the kind is namespaced.
    pub fn is_namespaced(&self, gvk: &GroupVersionKind) -> Option<bool> {
        self.scope(&gvk.group, &gvk.kind)
            .map(|scope| scope == ResourceScope::Namespaced)
    }

    /// Whether the kind is namespaced: as the declaration says, otherwise as the live
    /// cluster reports it, which decides only when nothing declared answers.
    ///
    /// A scope the Cluster's capabilities gave is also compared with the live
    /// cluster, which is cheap because the command has connected to it already; a
    /// disagreement is recorded once per kind ([`Self::mismatches`]) and warned about,
    /// and the recorded scope is still the answer.
    pub async fn resolve(&self, client: &dyn KubeClient, gvk: &GroupVersionKind) -> Result<bool> {
        match self.declared_scope(&gvk.group, &gvk.kind) {
            Some((scope, from_capabilities)) => {
                if from_capabilities {
                    self.compare_with_live(client, gvk, scope).await;
                }
                Ok(scope == ResourceScope::Namespaced)
            }
            None => client.is_namespaced(gvk).await,
        }
    }

    async fn compare_with_live(&self, client: &dyn KubeClient, gvk: &GroupVersionKind, recorded: ResourceScope) {
        let key = (gvk.group.clone(), gvk.kind.clone());
        if !self.comparisons.lock().unwrap().checked.insert(key) {
            return;
        }
        // A kind the live cluster does not serve has nothing to disagree with.
        let Ok(live_namespaced) = client.is_namespaced(gvk).await else {
            return;
        };
        let live = if live_namespaced {
            ResourceScope::Namespaced
        } else {
            ResourceScope::Cluster
        };
        if live == recorded {
            return;
        }
        let mismatch = ScopeMismatch {
            group: gvk.group.clone(),
            kind: gvk.kind.clone(),
            recorded,
            live,
        };
        tracing::warn!(
            "{}: {}; fix: {}",
            ScopeMismatch::CODE,
            mismatch.message(),
            ScopeMismatch::FIX
        );
        self.comparisons.lock().unwrap().mismatches.push(mismatch);
    }

    /// The kinds found so far whose recorded scope the live cluster contradicts, once
    /// each, shared by every clone of this resolver.
    pub fn mismatches(&self) -> Vec<ScopeMismatch> {
        self.comparisons.lock().unwrap().mismatches.clone()
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
    fn test_scope_resolver_knows_built_in_kinds_after_the_declaration() {
        let resolver = ScopeResolver::default();
        assert_eq!(resolver.scope("", "ConfigMap"), Some(ResourceScope::Namespaced));
        assert_eq!(resolver.scope("", "Namespace"), Some(ResourceScope::Cluster));
        // A CRD in the manifests still decides a kind sharing a built-in name.
        let crd = crd("example.com", "Secret", "Cluster", served());
        assert_eq!(
            ScopeResolver::new(None, [&crd]).scope("example.com", "Secret"),
            Some(ResourceScope::Cluster)
        );
    }

    #[test]
    fn test_scope_resolver_scope_of_a_kind_nothing_declares_is_unknown() {
        let resolver = ScopeResolver::new(Some(&capabilities()), []);
        assert_eq!(resolver.scope("example.com", "Unlisted"), None);
        assert_eq!(ScopeResolver::default().scope("example.com", "Widget"), None);
    }

    /// A recorded scope the live cluster contradicts is reported once per kind, as a
    /// warning with a stable code, and still decides the answer; a manifest CRD or a
    /// recorded scope the live cluster agrees with reports nothing.
    #[tokio::test]
    async fn test_scope_resolver_resolve_reports_a_recorded_scope_the_live_cluster_contradicts_once_per_kind() {
        let live = MockKubeClient::new();
        live.set_kind_scope("Widget", false);
        live.set_kind_scope("Gadget", false);
        live.set_kind_scope("Manifested", false);
        let manifested = crd("example.com", "Manifested", "Namespaced", served());
        let mut capabilities = capabilities();
        capabilities.api_versions.push("example.com/v1/Manifested".to_string());
        let resolver = ScopeResolver::new(Some(&capabilities), [&manifested]);

        for _ in 0..2 {
            // Recorded namespaced, live cluster-scoped: the recorded answer is used.
            assert!(resolver.resolve(&live, &gvk("example.com/v1", "Widget")).await.unwrap());
            assert!(resolver
                .clone()
                .resolve(&live, &gvk("example.com/v2", "Widget"))
                .await
                .unwrap());
            // Recorded and live agree (cluster-scoped), so nothing is reported.
            assert!(!resolver.resolve(&live, &gvk("example.com/v1", "Gadget")).await.unwrap());
            // A manifest CRD decides, and the live cluster is not compared.
            assert!(resolver
                .resolve(&live, &gvk("example.com/v1", "Manifested"))
                .await
                .unwrap());
        }

        assert_eq!(
            resolver.mismatches(),
            [ScopeMismatch {
                group: "example.com".to_string(),
                kind: "Widget".to_string(),
                recorded: ResourceScope::Namespaced,
                live: ResourceScope::Cluster,
            }]
        );
        let mismatch = &resolver.mismatches()[0];
        assert_eq!(ScopeMismatch::CODE.as_str(), "NYL-KUBERNETES-SCOPE-MISMATCH");
        assert!(
            mismatch.message().contains("example.com/Widget"),
            "{}",
            mismatch.message()
        );
        assert!(
            ScopeMismatch::FIX.contains("nyl capture cluster"),
            "{}",
            ScopeMismatch::FIX
        );
        assert!(ScopeMismatch::FIX.contains("--crds"), "{}", ScopeMismatch::FIX);
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
