//! Helm `lookup` from the live cluster.
//!
//! `nyl render`, `diff`, and `apply` can answer a chart's `lookup` from the
//! cluster instead of the target's last publication (`--lookup=live`), and do
//! so by default for `--context` without GitOps configuration, where the user
//! has already chosen the cluster. Helm still talks only to the local API Nyl
//! serves ([`crate::helm::lookup`]); this module is what that API reads from.
//!
//! - Only GET and LIST reach the cluster, and only for objects that belong to
//!   the Release being rendered: the resources its latest recorded revision
//!   applied (`nyl apply` records one), and, with a target, the objects its last
//!   publication holds. Anything else is absent, as in a snapshot.
//! - A cluster that cannot be read is an error, never an absent object.
//! - What the cluster holds can change under a cache without Nyl observing it,
//!   so a render that used live lookups is never cached.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::sync::{Arc, Mutex, OnceLock};

use kube::api::{Api, DynamicObject, ListParams};
use serde_json::Value;

use super::state::{KubernetesReleaseStorage, ReleaseStorage};
use crate::helm::lookup::{KindInfo, LookupBackend, ObjectStore, ReleaseScope, StoreError, LIVE_UNAVAILABLE_CODE};

/// The live Helm `lookup` of a command that selects a cluster with `--context`
/// and has no GitOps configuration: the objects of the Release's latest
/// recorded revision, and the scope that names the Release.
pub fn context_lookup(
    config: kube::Config,
    refresh: BTreeSet<crate::helm::lookup::ObjectRef>,
) -> crate::Result<(crate::helm::lookup::LookupConfig, crate::helm::lookup::ScopeSource)> {
    let live = LiveLookup::new(config, None)?;
    let scope = crate::helm::lookup::ScopeSource::new(|release| {
        Ok(Some(ReleaseScope {
            directory: release.name.clone(),
            namespace: release.namespace.clone(),
            release: Some(release.clone()),
        }))
    });
    Ok((crate::helm::lookup::LookupConfig::new(Arc::new(live), refresh), scope))
}

/// An object that belongs to a Release.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ObjectIdentity {
    pub api_version: String,
    pub kind: String,
    pub namespace: Option<String>,
    pub name: String,
}

/// The objects a Release's last publication holds, which belong to the Release
/// too. Without a target there is no publication.
pub type PublishedMembers = Arc<dyn Fn(&ReleaseScope) -> Result<Vec<ObjectIdentity>, StoreError> + Send + Sync>;

/// The live cluster, as the source of a Release's lookups.
#[derive(Clone)]
pub struct LiveLookup {
    inner: Arc<Inner>,
}

struct Inner {
    config: kube::Config,
    published: Option<PublishedMembers>,
    storage: Option<Arc<dyn ReleaseStorage>>,
    runtime: LiveRuntime,
    client: OnceLock<std::result::Result<kube::Client, StoreError>>,
    resources: Mutex<BTreeMap<(String, String), Option<kube::core::ApiResource>>>,
}

/// The runtime the cluster is read on. Reads block the caller, which may be
/// inside another runtime, so each one runs on a thread of its own.
struct LiveRuntime(Option<tokio::runtime::Runtime>);

impl Drop for LiveRuntime {
    fn drop(&mut self) {
        if let Some(runtime) = self.0.take() {
            // Dropping a runtime inside an async context panics; this never does.
            runtime.shutdown_background();
        }
    }
}

impl LiveLookup {
    /// The cluster `config` reaches, with the objects `published` names as members
    /// of a Release beside those its recorded revision applied.
    pub fn new(config: kube::Config, published: Option<PublishedMembers>) -> crate::Result<Self> {
        // The cluster client needs a process-level TLS provider; one already
        // installed stays.
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("nyl-lookup-live")
            .enable_all()
            .build()?;
        Ok(Self {
            inner: Arc::new(Inner {
                config,
                published,
                storage: None,
                runtime: LiveRuntime(Some(runtime)),
                client: OnceLock::new(),
                resources: Mutex::new(BTreeMap::new()),
            }),
        })
    }

    /// Read Release revisions from `storage` instead of the cluster, for tests.
    #[must_use]
    pub fn with_release_storage(self, storage: Arc<dyn ReleaseStorage>) -> Self {
        let inner =
            Arc::try_unwrap(self.inner).unwrap_or_else(|_| panic!("storage is set before the lookup is shared"));
        Self {
            inner: Arc::new(Inner {
                storage: Some(storage),
                ..inner
            }),
        }
    }
}

impl LookupBackend for LiveLookup {
    fn describe(&self) -> String {
        "the live cluster".to_owned()
    }

    fn store(&self, scope: &ReleaseScope) -> Arc<dyn ObjectStore> {
        Arc::new(LiveStore {
            inner: self.inner.clone(),
            scope: scope.clone(),
            members: OnceLock::new(),
        })
    }

    /// What the cluster holds changes without Nyl observing it.
    fn cacheable(&self) -> bool {
        false
    }
}

struct LiveStore {
    inner: Arc<Inner>,
    scope: ReleaseScope,
    members: OnceLock<std::result::Result<BTreeSet<ObjectIdentity>, StoreError>>,
}

fn unavailable(message: impl std::fmt::Display) -> StoreError {
    StoreError::new(
        LIVE_UNAVAILABLE_CODE,
        format!(
            "the live cluster cannot be read for a chart's lookup: {message}; check the cluster connection, or render \
             without --lookup=live to read the target's last publication"
        ),
    )
}

impl Inner {
    /// Run `make` to completion on the cluster runtime, from a thread that is
    /// in no runtime, whatever the caller is in.
    fn block_on<F, Fut, T>(&self, make: F) -> T
    where
        F: FnOnce() -> Fut + Send,
        Fut: Future<Output = T>,
        T: Send,
    {
        let runtime = self
            .runtime
            .0
            .as_ref()
            .expect("the runtime lives as long as the lookup");
        std::thread::scope(|scope| {
            scope
                .spawn(|| runtime.block_on(make()))
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
        })
    }

    fn client(&self) -> std::result::Result<kube::Client, StoreError> {
        self.client
            .get_or_init(|| {
                let config = self.config.clone();
                self.block_on(move || async move { kube::Client::try_from(config) })
                    .map_err(unavailable)
            })
            .clone()
    }

    /// The objects that belong to the Release `scope` names.
    fn members(&self, scope: &ReleaseScope) -> std::result::Result<BTreeSet<ObjectIdentity>, StoreError> {
        let mut members = BTreeSet::new();
        if let Some(release) = &scope.release {
            let storage: Arc<dyn ReleaseStorage> = match &self.storage {
                Some(storage) => storage.clone(),
                None => Arc::new(KubernetesReleaseStorage::new(self.client()?)),
            };
            let storage = &storage;
            let latest = self
                .block_on(|| async move { storage.get_latest_release(&release.name, &release.namespace).await })
                .map_err(unavailable)?;
            for key in latest.into_iter().flat_map(|revision| revision.resource_keys) {
                members.insert(ObjectIdentity {
                    api_version: crate::helm::lookup::join_api_version(&key.gvk.group, &key.gvk.version),
                    kind: key.gvk.kind,
                    namespace: key.namespace,
                    name: key.name,
                });
            }
        }
        if let Some(published) = &self.published {
            members.extend(published(scope)?);
        }
        Ok(members)
    }

    /// How the cluster names `kind`, or `None` when it serves no such kind.
    fn resource(&self, kind: &KindInfo) -> std::result::Result<Option<kube::core::ApiResource>, StoreError> {
        let key = (kind.api_version(), kind.kind.clone());
        if let Some(known) = self.resources.lock().expect("the resources are not poisoned").get(&key) {
            return Ok(known.clone());
        }
        let client = self.client()?;
        let gvk = kube::core::GroupVersionKind::gvk(&kind.group, &kind.version, &kind.kind);
        let (client_ref, gvk_ref) = (&client, &gvk);
        let found = match self.block_on(|| async move { kube::discovery::pinned_kind(client_ref, gvk_ref).await }) {
            Ok((resource, _)) => Some(resource),
            Err(kube::Error::Discovery(kube::error::DiscoveryError::MissingKind(_))) => None,
            Err(error) => return Err(unavailable(error)),
        };
        self.resources
            .lock()
            .expect("the resources are not poisoned")
            .insert(key, found.clone());
        Ok(found)
    }

    fn api(
        &self,
        resource: &kube::core::ApiResource,
        namespaced: bool,
        namespace: Option<&str>,
    ) -> std::result::Result<Api<DynamicObject>, StoreError> {
        let client = self.client()?;
        Ok(match (namespaced, namespace) {
            (true, Some(namespace)) => Api::namespaced_with(client, namespace, resource),
            _ => Api::all_with(client, resource),
        })
    }
}

impl LiveStore {
    fn members(&self) -> std::result::Result<&BTreeSet<ObjectIdentity>, StoreError> {
        if self.scope.is_none() {
            static NONE: BTreeSet<ObjectIdentity> = BTreeSet::new();
            return Ok(&NONE);
        }
        self.members
            .get_or_init(|| self.inner.members(&self.scope))
            .as_ref()
            .map_err(Clone::clone)
    }

    fn is_member(&self, kind: &KindInfo, namespace: Option<&str>, name: &str) -> std::result::Result<bool, StoreError> {
        Ok(self.members()?.contains(&ObjectIdentity {
            api_version: kind.api_version(),
            kind: kind.kind.clone(),
            namespace: namespace.map(ToOwned::to_owned),
            name: name.to_owned(),
        }))
    }
}

impl ObjectStore for LiveStore {
    fn kinds(&self) -> std::result::Result<Vec<KindInfo>, StoreError> {
        Ok(self
            .members()?
            .iter()
            .map(|member| KindInfo::new(&member.api_version, &member.kind, member.namespace.is_some()))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect())
    }

    fn get(
        &self,
        kind: &KindInfo,
        namespace: Option<&str>,
        name: &str,
    ) -> std::result::Result<Option<Value>, StoreError> {
        if !self.is_member(kind, namespace, name)? {
            return Ok(None);
        }
        let Some(resource) = self.inner.resource(kind)? else {
            return Ok(None);
        };
        let api = self.inner.api(&resource, kind.namespaced, namespace)?;
        let api = &api;
        let found = self
            .inner
            .block_on(|| async move { api.get_opt(name).await })
            .map_err(unavailable)?;
        found.map(|object| object_value(&resource, &object)).transpose()
    }

    fn list(&self, kind: &KindInfo, namespace: Option<&str>) -> std::result::Result<Vec<Value>, StoreError> {
        let Some(resource) = self.inner.resource(kind)? else {
            return Ok(Vec::new());
        };
        let api = self.inner.api(&resource, kind.namespaced, namespace)?;
        let api = &api;
        let listed = self
            .inner
            .block_on(|| async move { api.list(&ListParams::default()).await })
            .map_err(unavailable)?;
        let mut objects = Vec::new();
        for object in listed.items {
            let object_namespace = object.metadata.namespace.as_deref();
            let name = object.metadata.name.as_deref().unwrap_or_default();
            if self.is_member(kind, object_namespace, name)? {
                objects.push(object_value(&resource, &object)?);
            }
        }
        objects.sort_by_key(|object| {
            (
                object
                    .pointer("/metadata/namespace")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                object
                    .pointer("/metadata/name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            )
        });
        Ok(objects)
    }
}

/// An object as the API serves it: list items carry no `apiVersion` and `kind`.
fn object_value(resource: &kube::core::ApiResource, object: &DynamicObject) -> std::result::Result<Value, StoreError> {
    let mut value = serde_json::to_value(object).map_err(unavailable)?;
    if let Some(fields) = value.as_object_mut() {
        fields
            .entry("apiVersion")
            .or_insert_with(|| Value::String(resource.api_version.clone()));
        fields
            .entry("kind")
            .or_insert_with(|| Value::String(resource.kind.clone()));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use serde_json::json;

    use super::*;
    use crate::helm::lookup::api::{ApiCapabilities, ApiState};
    use crate::helm::lookup::{ApiServer, ObjectSet, ReleaseIdentity};
    use crate::kubernetes::state::{ReleaseInfo, ReleaseState, ReleaseStatus};
    use crate::kubernetes::{GroupVersionKind, ResourceKey};
    use crate::Result;

    /// A recorded revision of one Release, the only thing the lookup reads from storage.
    struct Revision(Vec<ResourceKey>);

    #[async_trait]
    impl ReleaseStorage for Revision {
        async fn save_release(&self, _: &ReleaseState) -> Result<()> {
            unreachable!("a lookup never writes")
        }
        async fn complete_release(&self, _: &ReleaseState) -> Result<()> {
            unreachable!("a lookup never writes")
        }
        async fn get_latest_release(&self, name: &str, namespace: &str) -> Result<Option<ReleaseState>> {
            Ok(Some(ReleaseState {
                release_name: name.to_owned(),
                release_namespace: namespace.to_owned(),
                revision: 3,
                resource_keys: self.0.clone(),
                manifest: String::new(),
                status: ReleaseStatus::Deployed,
                rendered_at: chrono::Utc::now(),
                applied_at: None,
                error: None,
                meta: crate::kubernetes::state::ReleaseMeta::default(),
            }))
        }
        async fn get_release(&self, _: &str, _: &str, _: u32) -> Result<Option<ReleaseState>> {
            unreachable!("a lookup reads the latest revision")
        }
        async fn list_revisions(&self, _: &str, _: &str) -> Result<Vec<u32>> {
            unreachable!("a lookup reads the latest revision")
        }
        async fn update_release_status(
            &self,
            _: &str,
            _: &str,
            _: u32,
            _: ReleaseStatus,
            _: Option<String>,
        ) -> Result<()> {
            unreachable!("a lookup never writes")
        }
        async fn list_releases(&self, _: Option<&str>) -> Result<Vec<ReleaseInfo>> {
            unreachable!("a lookup reads the latest revision")
        }
        async fn delete_release(&self, _: &str, _: &str, _: u32) -> Result<()> {
            unreachable!("a lookup never writes")
        }
        async fn delete_all_revisions(&self, _: &str, _: &str) -> Result<u32> {
            unreachable!("a lookup never writes")
        }
    }

    fn key(kind: &str, namespace: &str, name: &str) -> ResourceKey {
        ResourceKey {
            gvk: GroupVersionKind::from_api_version_kind("v1", kind).unwrap(),
            namespace: Some(namespace.to_owned()),
            name: name.to_owned(),
        }
    }

    /// A fake cluster: the lookup API, serving what a cluster would hold.
    fn cluster() -> ApiServer {
        let objects = ObjectSet::from_documents(
            vec![
                json!({"apiVersion": "v1", "kind": "Secret", "metadata": {"name": "my-ca", "namespace": "demo"}, "data": {"ca": "Y2E="}}),
                json!({"apiVersion": "v1", "kind": "Secret", "metadata": {"name": "foreign", "namespace": "demo"}, "data": {"x": "eA=="}}),
                json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "published", "namespace": "demo"}}),
                json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "foreign-cm", "namespace": "demo"}}),
            ],
            &|_, _| Some(true),
            "demo",
        )
        .unwrap();
        let capabilities = ApiCapabilities::new("1.30.0", ["v1".to_owned()], []).unwrap();
        ApiServer::start(
            Arc::new(ApiState::new(capabilities, Arc::new(objects), "token".to_owned())),
            "token".to_owned(),
        )
        .unwrap()
    }

    async fn config(server: &ApiServer) -> kube::Config {
        let kubeconfig = kube::config::Kubeconfig::from_yaml(&server.kubeconfig()).unwrap();
        let mut config = kube::Config::from_custom_kubeconfig(kubeconfig, &kube::config::KubeConfigOptions::default())
            .await
            .unwrap();
        // The fake cluster is on loopback, whatever proxy the environment names.
        config.proxy_url = None;
        config
    }

    fn scope() -> ReleaseScope {
        ReleaseScope {
            directory: "group/web".to_owned(),
            namespace: "demo".to_owned(),
            release: Some(ReleaseIdentity {
                name: "web".to_owned(),
                namespace: "demo".to_owned(),
            }),
        }
    }

    fn names(objects: Vec<Value>) -> Vec<String> {
        objects
            .iter()
            .map(|object| object["metadata"]["name"].as_str().unwrap().to_owned())
            .collect()
    }

    /// Catches `lookup` reading beyond its Release: the cluster holds more than
    /// the Release owns, and only the Release's own objects, from its recorded
    /// revision and its publication, are visible.
    #[tokio::test]
    async fn test_live_lookup_reads_only_the_objects_of_the_release() {
        let server = cluster();
        let published: PublishedMembers = Arc::new(|_| {
            Ok(vec![ObjectIdentity {
                api_version: "v1".to_owned(),
                kind: "ConfigMap".to_owned(),
                namespace: Some("demo".to_owned()),
                name: "published".to_owned(),
            }])
        });
        let live = LiveLookup::new(config(&server).await, Some(published))
            .unwrap()
            .with_release_storage(Arc::new(Revision(vec![key("Secret", "demo", "my-ca")])));
        let store = live.store(&scope());
        let secret = KindInfo::new("v1", "Secret", true);
        let config_map = KindInfo::new("v1", "ConfigMap", true);

        let found = store.get(&secret, Some("demo"), "my-ca").unwrap().unwrap();
        assert_eq!(found["data"]["ca"], "Y2E=");
        assert_eq!(found["kind"], "Secret");
        assert!(
            store.get(&secret, Some("demo"), "foreign").unwrap().is_none(),
            "an object of the cluster that no revision or publication of the Release holds is absent"
        );
        assert!(store.get(&secret, Some("demo"), "missing").unwrap().is_none());
        assert_eq!(names(store.list(&secret, Some("demo")).unwrap()), ["my-ca"]);
        assert_eq!(names(store.list(&config_map, None).unwrap()), ["published"]);
        let kinds = store.kinds().unwrap();
        assert_eq!(
            kinds.iter().map(|kind| kind.kind.as_str()).collect::<Vec<_>>(),
            ["ConfigMap", "Secret"]
        );
        assert!(
            !live.cacheable(),
            "what a cluster holds changes without Nyl observing it"
        );
    }

    /// Catches an unreachable cluster reading as an absent object, which would
    /// regenerate what the cluster still holds.
    #[tokio::test]
    async fn test_an_unreachable_cluster_is_an_error_never_an_absent_object() {
        let server = cluster();
        let config = config(&server).await;
        drop(server);
        let live = LiveLookup::new(config, None)
            .unwrap()
            .with_release_storage(Arc::new(Revision(vec![key("Secret", "demo", "my-ca")])));
        let error = live
            .store(&scope())
            .get(&KindInfo::new("v1", "Secret", true), Some("demo"), "my-ca")
            .unwrap_err();
        assert_eq!(error.code, Some(LIVE_UNAVAILABLE_CODE));
        assert!(error.message.contains("without --lookup=live"), "{error}");
    }

    /// A render that belongs to no Release reads nothing, whatever the cluster holds.
    #[tokio::test]
    async fn test_a_render_without_a_release_finds_nothing() {
        let server = cluster();
        let live = LiveLookup::new(config(&server).await, None).unwrap();
        let store = live.store(&ReleaseScope::none());
        assert!(store.kinds().unwrap().is_empty());
        assert!(store
            .get(&KindInfo::new("v1", "Secret", true), Some("demo"), "my-ca")
            .unwrap()
            .is_none());
    }
}
