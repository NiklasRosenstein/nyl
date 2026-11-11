//! Helm `lookup` from a declared source.
//!
//! A HelmChart that declares `spec.lookup` renders with Helm's `lookup` reading
//! objects instead of finding nothing. Helm never gets a cluster: Nyl starts a
//! read-only Kubernetes API on 127.0.0.1 for the duration of the chart's
//! render, points Helm at it with a temporary kubeconfig, and answers from the
//! objects of the chart's Release in a [`LookupBackend`]. A chart such as
//! Cilium's then finds the CA it generated earlier and keeps it, so the same
//! chart, values, and snapshot render the same bytes however cold the cache is.
//!
//! - [`api`] answers the requests; [`server`] serves them over TLS.
//! - [`LookupBackend`] says where the objects come from: the target's last
//!   publication ([`crate::gitops::lookup`]).
//! - Every lookup a render makes enters the shared dependency recorder with
//!   its result, and a cached render is reused only when each one still
//!   returns the same result ([`LookupReplay`]).
//!
//! `lookup` sees only the objects of the Release being rendered. Anything else
//! is absent.

pub mod api;
pub mod capabilities;
mod model;
mod run;
mod server;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

pub use model::{join_api_version, EmptyStore, FilteredStore, KindInfo, ObjectRef, ObjectSet, ObjectStore, StoreError};
pub use run::{render_with_lookup, LookupRender};
pub use server::{ApiServer, ServerReport};

use crate::render::cache::{DependencyRecorder, LookupFingerprint, LookupReplay};
use crate::{NylError, Result};

/// Where the Helm `lookup` of a chart that declares `spec.lookup` reads from.
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum LookupSource {
    /// The target's last publication, restricted to the Release's objects.
    Snapshot,
    /// The live cluster, through Nyl's own client, restricted to the Release's objects.
    /// Only `render`, `diff`, and `apply` read it.
    Live,
}

/// The options every command that renders Helm charts takes for their `lookup`.
#[derive(clap::Args, Debug, Clone, Default)]
pub struct LookupArgs {
    /// Where a chart's `lookup` reads from. `snapshot` (the default with a target) is the
    /// target's last publication; `live` reads the cluster instead, and is the default for
    /// `--context` without GitOps configuration. Tree and publication commands refuse `live`.
    #[arg(long = "lookup", value_enum, value_name = "SOURCE")]
    pub lookup: Option<LookupSource>,

    /// Treat this object as absent for this render, so a chart's `lookup` regenerates what it
    /// would have reused from it, such as a CA, for rotation. Written as `Kind/namespace/name`, or
    /// `Kind/name` for a cluster-scoped object, such as `Secret/kube-system/cilium-ca`. Repeatable.
    #[arg(long = "refresh-lookup", value_name = "KIND/NAMESPACE/NAME")]
    pub refresh_lookup: Vec<ObjectRef>,
}

impl LookupArgs {
    /// Refuse `--lookup=live` for a command that renders what Git publishes: a
    /// tree or publication must not depend on what a cluster holds today.
    pub fn refuse_live(&self, command: &str) -> Result<()> {
        if self.lookup == Some(LookupSource::Live) {
            return Err(lookup_error(
                LIVE_REFUSED_CODE,
                format!(
                    "{command} does not accept --lookup=live: tree and publication commands render what Git declares \
                     and publishes, so what a cluster holds today must not decide it. Use `nyl render`, `nyl diff`, or \
                     `nyl apply` for live lookups, or drop the flag to read the target's last publication"
                ),
            ));
        }
        Ok(())
    }

    /// The objects to treat as absent.
    pub fn refresh(&self) -> BTreeSet<ObjectRef> {
        self.refresh_lookup.iter().cloned().collect()
    }
}

/// The code of the error for a lookup source that cannot be read.
pub const SNAPSHOT_UNAVAILABLE_CODE: &str = "NYL-HELM-LOOKUP-SNAPSHOT-UNAVAILABLE";
/// The code of the error for a rendered kind whose API version is not declared.
pub const API_VERSION_UNDECLARED_CODE: &str = "NYL-HELM-LOOKUP-API-VERSION-UNDECLARED";
/// The code of the error for a lookup request the API could not answer.
pub const SERVER_FAILED_CODE: &str = "NYL-HELM-LOOKUP-FAILED";
/// The code of the refusal of `--lookup=live` by a tree or publication command.
pub const LIVE_REFUSED_CODE: &str = "NYL-HELM-LOOKUP-LIVE-REFUSED";
/// The code of `--lookup=live` where no cluster is selected.
pub const LIVE_NEEDS_CLUSTER_CODE: &str = "NYL-HELM-LOOKUP-LIVE-NEEDS-CLUSTER";
/// The code of the error for a live cluster that cannot be read.
pub const LIVE_UNAVAILABLE_CODE: &str = "NYL-HELM-LOOKUP-LIVE-UNAVAILABLE";

const REQUEST_PREFIX: &str = "lookup:";
const DISCOVERY_PREFIX: &str = "lookup-discovery:";

/// The Release whose objects a lookup reads: where its tree lives, and the
/// namespace Argo CD gives its namespaced objects that name none.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ReleaseScope {
    /// The Release's directory in a publication, `<group output>/<release>`.
    /// Empty when the render belongs to no published Release, and then nothing
    /// is found.
    pub directory: String,
    pub namespace: String,
    /// The Release itself, which a live cluster identifies its objects by.
    pub release: Option<ReleaseIdentity>,
}

impl ReleaseScope {
    /// The scope of a render that belongs to no published Release.
    pub fn none() -> Self {
        Self {
            directory: String::new(),
            namespace: String::new(),
            release: None,
        }
    }

    pub fn is_none(&self) -> bool {
        self.directory.is_empty()
    }

    /// The scope as a cache dependency names it. A directory holds neither
    /// `|` nor a leading `/`: every segment is a validated path segment.
    fn key(&self) -> String {
        format!("{}|{}", self.directory, self.namespace)
    }

    fn from_key(key: &str) -> Option<Self> {
        let (directory, namespace) = key.split_once('|')?;
        Some(Self {
            directory: directory.to_owned(),
            namespace: namespace.to_owned(),
            release: None,
        })
    }
}

/// Where a Release's lookups read from.
pub trait LookupBackend: Send + Sync {
    /// What the source is, for messages.
    fn describe(&self) -> String;

    /// What the source holds for `scope`. The store reads lazily, so a source
    /// that cannot be read fails the first request that needs it, and Helm
    /// sees an error, never an absent object.
    fn store(&self, scope: &ReleaseScope) -> Arc<dyn ObjectStore>;

    /// Whether a render that used the source can be cached. A source that can
    /// change under the cache without Nyl observing it says no.
    fn cacheable(&self) -> bool {
        true
    }
}

/// The backend of a render that declares no snapshot: nothing is found.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoSnapshot;

impl LookupBackend for NoSnapshot {
    fn describe(&self) -> String {
        "no snapshot".to_owned()
    }

    fn store(&self, _: &ReleaseScope) -> Arc<dyn ObjectStore> {
        Arc::new(EmptyStore)
    }
}

/// How a render answers lookups: a backend, and the objects the user asked
/// to treat as absent for this render.
#[derive(Clone)]
pub struct LookupConfig {
    backend: Arc<dyn LookupBackend>,
    refresh: Arc<BTreeSet<ObjectRef>>,
}

impl std::fmt::Debug for LookupConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LookupConfig")
            .field("backend", &self.backend.describe())
            .field("refresh", &self.refresh)
            .finish()
    }
}

impl Default for LookupConfig {
    fn default() -> Self {
        Self::new(Arc::new(NoSnapshot), BTreeSet::new())
    }
}

impl LookupConfig {
    pub fn new(backend: Arc<dyn LookupBackend>, refresh: BTreeSet<ObjectRef>) -> Self {
        Self {
            backend,
            refresh: Arc::new(refresh),
        }
    }

    pub fn describe(&self) -> String {
        self.backend.describe()
    }

    pub fn cacheable(&self) -> bool {
        self.backend.cacheable()
    }

    /// The same backend with another set of objects treated as absent.
    #[must_use]
    pub fn with_refresh(&self, refresh: BTreeSet<ObjectRef>) -> Self {
        Self {
            backend: self.backend.clone(),
            refresh: Arc::new(refresh),
        }
    }

    fn store(&self, scope: &ReleaseScope) -> Arc<dyn ObjectStore> {
        let store = self.backend.store(scope);
        if self.refresh.is_empty() {
            store
        } else {
            Arc::new(FilteredStore::refreshing(store, (*self.refresh).clone()))
        }
    }
}

impl LookupReplay for LookupConfig {
    fn evaluate(&self, name: &str) -> Option<LookupFingerprint> {
        // A source that changes without Nyl observing it cannot confirm what a
        // cached render looked up, so nothing cached from another source is
        // reused for it.
        if !self.cacheable() {
            return None;
        }
        if let Some(rest) = name.strip_prefix(DISCOVERY_PREFIX) {
            let scope = ReleaseScope::from_key(rest)?;
            let store = self.store(&scope);
            return api::discovery_fingerprint(store.as_ref())
                .ok()
                .map(LookupFingerprint::Plain);
        }
        let (scope, request) = split_request_name(name.strip_prefix(REQUEST_PREFIX)?)?;
        let request = api::LookupRequest::from_key(&request)?;
        let store = self.store(&scope);
        api::evaluate(store.as_ref(), &request)
            .ok()
            .map(|evaluation| evaluation.fingerprint())
    }
}

/// Split `<directory>|<namespace>|<request key>` into the scope and the key.
fn split_request_name(rest: &str) -> Option<(ReleaseScope, String)> {
    let mut parts = rest.splitn(3, '|');
    let (directory, namespace, request) = (parts.next()?, parts.next()?, parts.next()?);
    Some((
        ReleaseScope {
            directory: directory.to_owned(),
            namespace: namespace.to_owned(),
            release: None,
        },
        request.to_owned(),
    ))
}

/// The dependency name of a lookup request within a Release.
pub fn request_dependency_name(scope: &ReleaseScope, request: &str) -> String {
    format!("{REQUEST_PREFIX}{}|{request}", scope.key())
}

/// The dependency name of the discovery a render of a Release saw.
pub fn discovery_dependency_name(scope: &ReleaseScope) -> String {
    format!("{DISCOVERY_PREFIX}{}", scope.key())
}

/// The Helm release a render belongs to, as its own file declares it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ReleaseIdentity {
    pub name: String,
    pub namespace: String,
}

/// Decides, once the Release a render belongs to is known, where its tree is
/// published. `None` means the render has no published Release.
#[derive(Clone)]
pub struct ScopeSource(Arc<ScopeFn>);

type ScopeFn = dyn Fn(&ReleaseIdentity) -> Result<Option<ReleaseScope>> + Send + Sync;

impl ScopeSource {
    pub fn new(source: impl Fn(&ReleaseIdentity) -> Result<Option<ReleaseScope>> + Send + Sync + 'static) -> Self {
        Self(Arc::new(source))
    }

    /// The scope of the Release `release`.
    pub fn resolve(&self, release: &ReleaseIdentity) -> Result<Option<ReleaseScope>> {
        (self.0)(release)
    }
}

impl std::fmt::Debug for ScopeSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ScopeSource")
    }
}

/// The lookups of one Release's render: how they are answered, and what they
/// returned, so every cache layer above the Helm render can record them.
#[derive(Clone)]
pub struct LookupHandle {
    config: LookupConfig,
    release: Option<ReleaseIdentity>,
    scope_source: Option<ScopeSource>,
    observed: Arc<Mutex<BTreeMap<String, LookupFingerprint>>>,
}

impl std::fmt::Debug for LookupHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LookupHandle")
            .field("config", &self.config)
            .field("release", &self.release)
            .finish_non_exhaustive()
    }
}

impl LookupHandle {
    pub fn new(config: LookupConfig, release: Option<ReleaseIdentity>, scope_source: Option<ScopeSource>) -> Self {
        Self {
            config,
            release,
            scope_source,
            observed: Arc::default(),
        }
    }

    pub fn config(&self) -> &LookupConfig {
        &self.config
    }

    /// Where this Release is published, or [`ReleaseScope::none`].
    pub fn scope(&self) -> Result<ReleaseScope> {
        let (Some(release), Some(source)) = (&self.release, &self.scope_source) else {
            return Ok(ReleaseScope::none());
        };
        Ok(source.resolve(release)?.unwrap_or_else(ReleaseScope::none))
    }

    /// Where the Release is published, as a cache input: the scope, or the
    /// reason there is none. A render for another group never reuses this one.
    pub fn scope_input(&self) -> serde_json::Value {
        match self.scope() {
            Ok(scope) => serde_json::json!({"directory": scope.directory, "namespace": scope.namespace}),
            Err(error) => serde_json::json!({"error": error.to_string()}),
        }
    }

    /// Note what a lookup (or the discovery of a render) returned.
    pub fn observe(&self, name: String, fingerprint: LookupFingerprint) {
        self.observed
            .lock()
            .expect("observed lookups are not poisoned")
            .insert(name, fingerprint);
    }

    /// Note what a render looked up.
    pub fn observe_all(&self, lookups: &BTreeMap<String, LookupFingerprint>) {
        self.observed.lock().expect("observed lookups are not poisoned").extend(
            lookups
                .iter()
                .map(|(name, fingerprint)| (name.clone(), fingerprint.clone())),
        );
    }

    /// Everything the Release's lookups returned so far.
    pub fn observed(&self) -> BTreeMap<String, LookupFingerprint> {
        self.observed.lock().expect("observed lookups are not poisoned").clone()
    }
}

/// Record `observed` lookups in `recorder`.
pub fn record_observed(recorder: &mut DependencyRecorder, observed: &BTreeMap<String, LookupFingerprint>) {
    for (name, fingerprint) in observed {
        match fingerprint {
            LookupFingerprint::Plain(digest) if name.starts_with(DISCOVERY_PREFIX) => {
                recorder.record_lookup_discovery(name.clone(), digest.clone());
            }
            _ => recorder.record_lookup(name.clone(), fingerprint),
        }
    }
}

/// The error for an unreadable lookup source, with its fix.
pub fn snapshot_unavailable(message: impl std::fmt::Display) -> StoreError {
    StoreError::new(SNAPSHOT_UNAVAILABLE_CODE, message.to_string())
}

pub(crate) fn lookup_error(code: &str, message: impl std::fmt::Display) -> NylError {
    NylError::Other(format!("{code}: {message}"))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    struct Fixed(ObjectSet);

    impl LookupBackend for Fixed {
        fn describe(&self) -> String {
            "fixed".to_owned()
        }

        fn store(&self, scope: &ReleaseScope) -> Arc<dyn ObjectStore> {
            if scope.is_none() {
                Arc::new(EmptyStore)
            } else {
                Arc::new(self.0.clone())
            }
        }
    }

    fn config(refresh: &[&str]) -> LookupConfig {
        let objects = ObjectSet::from_documents(
            vec![json!({"apiVersion": "v1", "kind": "Secret", "metadata": {"name": "ca", "namespace": "demo"}, "data": {"k": "v"}})],
            &|_, _| Some(true),
            "demo",
        )
        .unwrap();
        LookupConfig::new(
            Arc::new(Fixed(objects)),
            refresh.iter().map(|name| name.parse().unwrap()).collect(),
        )
    }

    fn scope() -> ReleaseScope {
        ReleaseScope {
            directory: "platform/web".to_owned(),
            namespace: "demo".to_owned(),
            release: None,
        }
    }

    #[test]
    fn test_a_recorded_lookup_replays_to_what_the_source_returns_now() {
        let name = request_dependency_name(&scope(), "v1|secrets|demo|ca");
        assert!(matches!(
            config(&[]).evaluate(&name),
            Some(LookupFingerprint::Secret(_))
        ));
        // Treating the object as absent changes what the lookup returns, which
        // is how a rotation invalidates what rendered from the old object.
        assert_eq!(
            config(&["Secret/demo/ca"]).evaluate(&name),
            Some(LookupFingerprint::Absent)
        );
        let none = request_dependency_name(&ReleaseScope::none(), "v1|secrets|demo|ca");
        assert_eq!(config(&[]).evaluate(&none), Some(LookupFingerprint::Absent));
        assert_eq!(config(&[]).evaluate("lookup:not|a|valid"), None);
    }

    #[test]
    fn test_discovery_replays_to_a_digest_of_the_held_kinds() {
        let name = discovery_dependency_name(&scope());
        let held = config(&[]).evaluate(&name);
        assert!(matches!(held, Some(LookupFingerprint::Plain(_))));
        let empty = LookupConfig::default().evaluate(&name);
        assert_ne!(held, empty);
    }
}
