//! The objects and kinds the lookup API serves.
//!
//! A lookup store holds the objects of one Release. Kinds are identified the
//! way Kubernetes clients guess them (`kubectl` falls back to the same rule for
//! kinds discovery does not list), so the plural a [`KindInfo`] advertises is the
//! plural Helm requests, whether it learned it from discovery or guessed it.

use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use serde_json::Value;

/// The plural and singular resource names of `kind`, by the rule of
/// `meta.UnsafeGuessKindToResource`: lowercase, then `s` appends `es`, `y`
/// becomes `ies`, `endpoints` stays, and anything else appends `s`.
pub fn guess_resource_names(kind: &str) -> (String, String) {
    let singular = kind.to_ascii_lowercase();
    if singular.is_empty() || singular.ends_with("endpoints") {
        return (singular.clone(), singular);
    }
    let plural = if singular.ends_with('s') {
        format!("{singular}es")
    } else if let Some(stem) = singular.strip_suffix('y') {
        format!("{stem}ies")
    } else {
        format!("{singular}s")
    };
    (plural, singular)
}

/// One kind of one API version, as discovery lists it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct KindInfo {
    /// The API group; empty for the core group.
    pub group: String,
    pub version: String,
    pub kind: String,
    pub plural: String,
    pub singular: String,
    pub namespaced: bool,
}

impl KindInfo {
    /// The kind `kind` of `api_version` with the guessed resource names.
    pub fn new(api_version: &str, kind: &str, namespaced: bool) -> Self {
        let (group, version) = split_api_version(api_version);
        let (plural, singular) = guess_resource_names(kind);
        Self {
            group: group.to_owned(),
            version: version.to_owned(),
            kind: kind.to_owned(),
            plural,
            singular,
            namespaced,
        }
    }

    /// `group/version`, or `version` for the core group.
    pub fn api_version(&self) -> String {
        join_api_version(&self.group, &self.version)
    }
}

/// Split `group/version` (or the core `version`) into its parts.
pub fn split_api_version(api_version: &str) -> (&str, &str) {
    api_version.split_once('/').unwrap_or(("", api_version))
}

/// Join a group and version into an `apiVersion`.
pub fn join_api_version(group: &str, version: &str) -> String {
    if group.is_empty() {
        version.to_owned()
    } else {
        format!("{group}/{version}")
    }
}

/// An object the user names to treat as absent, as `Kind/namespace/name`, or
/// `Kind/name` for a cluster-scoped object.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ObjectRef {
    pub kind: String,
    pub namespace: Option<String>,
    pub name: String,
}

impl ObjectRef {
    /// Whether `object` is the one this reference names.
    pub fn matches(&self, object: &Value) -> bool {
        object.get("kind").and_then(Value::as_str) == Some(&self.kind)
            && object.pointer("/metadata/name").and_then(Value::as_str) == Some(&self.name)
            && object
                .pointer("/metadata/namespace")
                .and_then(Value::as_str)
                .filter(|namespace| !namespace.is_empty())
                == self.namespace.as_deref()
    }
}

impl ObjectRef {
    /// Whether this reference names the object of `kind` in `namespace` called `name`.
    pub fn names(&self, kind: &str, namespace: Option<&str>, name: &str) -> bool {
        self.kind == kind && self.name == name && self.namespace.as_deref() == namespace.filter(|ns| !ns.is_empty())
    }
}

impl FromStr for ObjectRef {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, String> {
        let parts = text.split('/').collect::<Vec<_>>();
        let (kind, namespace, name) = match parts.as_slice() {
            [kind, name] => (*kind, None, *name),
            [kind, namespace, name] => (*kind, Some(*namespace), *name),
            _ => {
                return Err(format!(
                    "{text:?} is not Kind/namespace/name or, for a cluster-scoped object, Kind/name"
                ))
            }
        };
        if kind.is_empty() || name.is_empty() || namespace == Some("") {
            return Err(format!("{text:?} has an empty Kind, namespace, or name"));
        }
        Ok(Self {
            kind: kind.to_owned(),
            namespace: namespace.map(ToOwned::to_owned),
            name: name.to_owned(),
        })
    }
}

impl std::fmt::Display for ObjectRef {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.namespace {
            Some(namespace) => write!(formatter, "{}/{namespace}/{}", self.kind, self.name),
            None => write!(formatter, "{}/{}", self.kind, self.name),
        }
    }
}

impl serde::Serialize for ObjectRef {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

/// A store failed to answer. The API reports it as a 500, never as an absent
/// object, so Helm fails instead of generating what the object would have held.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoreError {
    /// The stable code of the failure, when it has one.
    pub code: Option<&'static str>,
    pub message: String,
}

impl StoreError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code: Some(code),
            message: message.into(),
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            code: None,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.code {
            Some(code) => write!(formatter, "{code}: {}", self.message),
            None => formatter.write_str(&self.message),
        }
    }
}

/// What a lookup can read: the objects of one Release.
///
/// Calls may block; the API runs them off its async threads.
pub trait ObjectStore: Send + Sync {
    /// The kinds the store holds objects of.
    fn kinds(&self) -> Result<Vec<KindInfo>, StoreError>;

    /// The kind of `api_version` that a request names by its `plural`, when the
    /// store can tell. A store that holds objects answers for the kinds it holds; one
    /// that reads from a server may know more kinds than it holds objects of.
    fn kind_of(&self, api_version: &str, plural: &str) -> Result<Option<KindInfo>, StoreError> {
        Ok(self
            .kinds()?
            .into_iter()
            .find(|kind| kind.api_version() == api_version && kind.plural == plural))
    }

    /// One object, or `None` when it does not exist.
    fn get(&self, kind: &KindInfo, namespace: Option<&str>, name: &str) -> Result<Option<Value>, StoreError>;

    /// The objects of one kind, in one namespace or in all of them, ordered by
    /// namespace and name.
    fn list(&self, kind: &KindInfo, namespace: Option<&str>) -> Result<Vec<Value>, StoreError>;
}

impl<T: ObjectStore + ?Sized> ObjectStore for std::sync::Arc<T> {
    fn kinds(&self) -> Result<Vec<KindInfo>, StoreError> {
        (**self).kinds()
    }

    fn kind_of(&self, api_version: &str, plural: &str) -> Result<Option<KindInfo>, StoreError> {
        (**self).kind_of(api_version, plural)
    }

    fn get(&self, kind: &KindInfo, namespace: Option<&str>, name: &str) -> Result<Option<Value>, StoreError> {
        (**self).get(kind, namespace, name)
    }

    fn list(&self, kind: &KindInfo, namespace: Option<&str>) -> Result<Vec<Value>, StoreError> {
        (**self).list(kind, namespace)
    }
}

/// A store without objects: what a lookup finds when no snapshot is declared.
#[derive(Clone, Copy, Debug, Default)]
pub struct EmptyStore;

impl ObjectStore for EmptyStore {
    fn kinds(&self) -> Result<Vec<KindInfo>, StoreError> {
        Ok(Vec::new())
    }

    fn get(&self, _: &KindInfo, _: Option<&str>, _: &str) -> Result<Option<Value>, StoreError> {
        Ok(None)
    }

    fn list(&self, _: &KindInfo, _: Option<&str>) -> Result<Vec<Value>, StoreError> {
        Ok(Vec::new())
    }
}

/// Whether the object of a kind, in a namespace, with a name, is hidden.
type NamePredicate = Box<dyn Fn(&KindInfo, Option<&str>, &str) -> bool + Send + Sync>;

/// Hides objects from another store: those outside its scope, and those the
/// user asked to treat as absent.
pub struct FilteredStore<S> {
    inner: S,
    admits: Box<dyn Fn(&Value) -> bool + Send + Sync>,
    /// Whether an object is hidden by its name alone, which spares the store a read
    /// (and a failure) for an object the user asked to treat as absent.
    hides_name: NamePredicate,
}

impl<S: ObjectStore> FilteredStore<S> {
    pub fn new(inner: S, admits: impl Fn(&Value) -> bool + Send + Sync + 'static) -> Self {
        Self {
            inner,
            admits: Box::new(admits),
            hides_name: Box::new(|_, _, _| false),
        }
    }

    /// `inner` without the objects the audit's references name.
    pub fn refreshing(inner: S, audit: Arc<RefreshAudit>) -> Self {
        let by_name = audit.clone();
        Self {
            inner,
            admits: Box::new(move |object| !audit.hides(object)),
            hides_name: Box::new(move |kind, namespace, name| by_name.hides_name(&kind.kind, namespace, name)),
        }
    }
}

/// The `--refresh-lookup` references of a command, and which of them hid an object
/// from a chart's lookup. A reference that never did (a typo, or an object no chart
/// looks up) is reported once the command is done with its lookups, so a rotation
/// that did nothing is not silent.
#[derive(Debug, Default)]
pub struct RefreshAudit {
    refs: BTreeSet<ObjectRef>,
    hit: Mutex<BTreeSet<ObjectRef>>,
}

/// References that hid an object in any audit of this process. A direct command
/// renders through two lookup configurations, and the reference counts as matched
/// when either one hid its object.
static MATCHED: Mutex<BTreeSet<ObjectRef>> = Mutex::new(BTreeSet::new());
static REPORTED: Mutex<BTreeSet<ObjectRef>> = Mutex::new(BTreeSet::new());

impl RefreshAudit {
    pub fn new(refs: BTreeSet<ObjectRef>) -> Self {
        Self {
            refs,
            hit: Mutex::default(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.refs.is_empty()
    }

    /// Whether the user asked to treat `object` as absent, noting the reference that did.
    fn hides(&self, object: &Value) -> bool {
        self.note(self.refs.iter().filter(|reference| reference.matches(object)))
    }

    /// Whether the user asked to treat the object of this kind, namespace, and name as
    /// absent, noting the reference that did.
    fn hides_name(&self, kind: &str, namespace: Option<&str>, name: &str) -> bool {
        self.note(
            self.refs
                .iter()
                .filter(|reference| reference.names(kind, namespace, name)),
        )
    }

    fn note<'a>(&self, matched: impl Iterator<Item = &'a ObjectRef>) -> bool {
        let mut hidden = false;
        for reference in matched {
            hidden = true;
            self.hit
                .lock()
                .expect("refresh hits are not poisoned")
                .insert(reference.clone());
            MATCHED
                .lock()
                .expect("refresh hits are not poisoned")
                .insert(reference.clone());
        }
        hidden
    }

    /// The references that have hidden no object so far.
    pub fn unmatched(&self) -> Vec<ObjectRef> {
        let hit = self.hit.lock().expect("refresh hits are not poisoned");
        self.refs
            .iter()
            .filter(|reference| !hit.contains(*reference))
            .cloned()
            .collect()
    }
}

impl Drop for RefreshAudit {
    fn drop(&mut self) {
        let matched = MATCHED.lock().expect("refresh hits are not poisoned").clone();
        let mut reported = REPORTED.lock().expect("refresh reports are not poisoned");
        for reference in self.unmatched() {
            if matched.contains(&reference) || !reported.insert(reference.clone()) {
                continue;
            }
            tracing::warn!(
                "{}: --refresh-lookup {reference} matched no object that a chart looked up, so it changed nothing. \
                 Check the spelling: Kind/namespace/name, or Kind/name for a cluster-scoped object, such as \
                 Secret/kube-system/cilium-ca; an object the source does not hold yet is not found by it either",
                super::REFRESH_UNMATCHED_CODE
            );
        }
    }
}

impl<S: ObjectStore> ObjectStore for FilteredStore<S> {
    fn kinds(&self) -> Result<Vec<KindInfo>, StoreError> {
        self.inner.kinds()
    }

    fn kind_of(&self, api_version: &str, plural: &str) -> Result<Option<KindInfo>, StoreError> {
        self.inner.kind_of(api_version, plural)
    }

    fn get(&self, kind: &KindInfo, namespace: Option<&str>, name: &str) -> Result<Option<Value>, StoreError> {
        if (self.hides_name)(kind, namespace, name) {
            return Ok(None);
        }
        Ok(self
            .inner
            .get(kind, namespace, name)?
            .filter(|object| (self.admits)(object)))
    }

    fn list(&self, kind: &KindInfo, namespace: Option<&str>) -> Result<Vec<Value>, StoreError> {
        Ok(self
            .inner
            .list(kind, namespace)?
            .into_iter()
            .filter(|object| (self.admits)(object))
            .collect())
    }
}

/// The objects of one Release, held in memory.
#[derive(Clone, Debug, Default)]
pub struct ObjectSet {
    objects: Vec<StoredObject>,
}

#[derive(Clone, Debug)]
struct StoredObject {
    kind: KindInfo,
    namespace: Option<String>,
    name: String,
    value: Value,
}

impl ObjectSet {
    /// Index `documents`. `namespaced` says whether a kind of `(group, kind)` is
    /// namespaced; a namespaced object that names no namespace lives in
    /// `default_namespace`, where its Application puts it.
    ///
    /// A kind has one scope across its objects, so discovery lists it once: what
    /// `namespaced` says, and a kind it does not know is taken to be namespaced, as Argo CD
    /// applies an unnamespaced custom resource into the destination namespace. An object
    /// that names a namespace of a kind `namespaced` says is cluster-scoped is read without
    /// one.
    pub fn from_documents(
        documents: Vec<Value>,
        namespaced: &dyn Fn(&str, &str) -> Option<bool>,
        default_namespace: &str,
    ) -> Result<Self, String> {
        let text = |document: &Value, pointer: &str| {
            document
                .pointer(pointer)
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
                .map(ToOwned::to_owned)
        };
        let mut identities = Vec::with_capacity(documents.len());
        for document in &documents {
            let (Some(api_version), Some(kind), Some(name)) = (
                text(document, "/apiVersion"),
                text(document, "/kind"),
                text(document, "/metadata/name"),
            ) else {
                return Err("a document has no apiVersion, kind, or metadata.name".to_owned());
            };
            identities.push((api_version, kind, name, text(document, "/metadata/namespace")));
        }
        let mut scopes: BTreeMap<String, bool> = BTreeMap::new();
        let mut by_identity = BTreeMap::new();
        for (mut document, (api_version, kind, name, mut namespace)) in documents.into_iter().zip(identities) {
            let group = split_api_version(&api_version).0;
            let group_kind = format!("{group}/{kind}");
            let is_namespaced = *scopes
                .entry(group_kind)
                .or_insert_with(|| namespaced(group, &kind).unwrap_or(true));
            if is_namespaced && namespace.is_none() {
                namespace = Some(default_namespace.to_owned());
                if let Some(metadata) = document.get_mut("metadata").and_then(Value::as_object_mut) {
                    metadata.insert("namespace".to_owned(), Value::String(default_namespace.to_owned()));
                }
            } else if !is_namespaced && namespace.is_some() {
                // A cluster-scoped kind has no namespace to read it in.
                namespace = None;
            }
            let kind_info = KindInfo::new(&api_version, &kind, is_namespaced);
            by_identity.insert(
                (kind_info.group.clone(), kind.clone(), namespace.clone(), name.clone()),
                StoredObject {
                    kind: kind_info,
                    namespace,
                    name,
                    value: document,
                },
            );
        }
        Ok(Self {
            objects: by_identity.into_values().collect(),
        })
    }

    pub fn is_empty(&self) -> bool {
        self.objects.is_empty()
    }

    pub fn len(&self) -> usize {
        self.objects.len()
    }

    /// Every object's `(apiVersion, kind, namespace, name)`.
    pub fn identities(&self) -> impl Iterator<Item = (String, String, Option<String>, String)> + '_ {
        self.objects.iter().map(|object| {
            (
                object.kind.api_version(),
                object.kind.kind.clone(),
                object.namespace.clone(),
                object.name.clone(),
            )
        })
    }

    /// The `apiVersion`s of the objects, which discovery must list.
    pub fn api_versions(&self) -> BTreeSet<String> {
        self.objects.iter().map(|object| object.kind.api_version()).collect()
    }
}

impl ObjectStore for ObjectSet {
    fn kinds(&self) -> Result<Vec<KindInfo>, StoreError> {
        Ok(self
            .objects
            .iter()
            .map(|object| object.kind.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect())
    }

    fn get(&self, kind: &KindInfo, namespace: Option<&str>, name: &str) -> Result<Option<Value>, StoreError> {
        Ok(self
            .objects
            .iter()
            .find(|object| {
                same_kind(&object.kind, kind) && object.namespace.as_deref() == namespace && object.name == name
            })
            .map(|object| object.value.clone()))
    }

    fn list(&self, kind: &KindInfo, namespace: Option<&str>) -> Result<Vec<Value>, StoreError> {
        Ok(self
            .objects
            .iter()
            .filter(|object| {
                same_kind(&object.kind, kind)
                    && namespace.is_none_or(|namespace| object.namespace.as_deref() == Some(namespace))
            })
            .map(|object| object.value.clone())
            .collect())
    }
}

fn same_kind(left: &KindInfo, right: &KindInfo) -> bool {
    left.group == right.group && left.version == right.version && left.kind == right.kind
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn test_guessed_names_follow_the_kubernetes_rule() {
        let names = |kind: &str| guess_resource_names(kind).0;
        assert_eq!(names("Secret"), "secrets");
        assert_eq!(names("Ingress"), "ingresses");
        assert_eq!(names("NetworkPolicy"), "networkpolicies");
        assert_eq!(names("Endpoints"), "endpoints");
        assert_eq!(names("CiliumNode"), "ciliumnodes");
    }

    #[test]
    fn test_object_refs_parse_namespaced_and_cluster_scoped_names() {
        let secret: ObjectRef = "Secret/kube-system/cilium-ca".parse().unwrap();
        assert!(
            secret.matches(&json!({"kind": "Secret", "metadata": {"name": "cilium-ca", "namespace": "kube-system"}}))
        );
        assert!(!secret.matches(&json!({"kind": "Secret", "metadata": {"name": "cilium-ca"}})));
        let role: ObjectRef = "ClusterRole/admin".parse().unwrap();
        assert!(role.matches(&json!({"kind": "ClusterRole", "metadata": {"name": "admin"}})));
        assert!("Secret".parse::<ObjectRef>().is_err());
        assert!("Secret//x".parse::<ObjectRef>().is_err());
    }

    /// Catches one kind being listed twice because one of its objects names a namespace and
    /// another does not, and an unknown scope reading as cluster-scoped.
    #[test]
    fn test_a_kind_has_one_scope_and_an_unknown_one_is_namespaced_in_the_destination() {
        let set = ObjectSet::from_documents(
            vec![
                json!({"apiVersion": "example.com/v1", "kind": "Widget", "metadata": {"name": "a", "namespace": "demo"}}),
                json!({"apiVersion": "example.com/v1", "kind": "Widget", "metadata": {"name": "b"}}),
                json!({"apiVersion": "example.com/v1", "kind": "Gadget", "metadata": {"name": "c"}}),
                json!({"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": "demo", "namespace": "stray"}}),
            ],
            &|group, kind| (group.is_empty() && kind == "Namespace").then_some(false),
            "apps",
        )
        .unwrap();
        let kinds = set.kinds().unwrap();
        assert_eq!(
            kinds
                .iter()
                .map(|kind| (kind.kind.as_str(), kind.namespaced))
                .collect::<Vec<_>>(),
            [("Namespace", false), ("Gadget", true), ("Widget", true)],
            "{kinds:?}"
        );
        let widget = KindInfo::new("example.com/v1", "Widget", true);
        assert!(set.get(&widget, Some("apps"), "b").unwrap().is_some());
        assert!(set.get(&widget, Some("demo"), "a").unwrap().is_some());
        let gadget = KindInfo::new("example.com/v1", "Gadget", true);
        assert!(set.get(&gadget, Some("apps"), "c").unwrap().is_some());
        let namespace = KindInfo::new("v1", "Namespace", false);
        assert!(set.get(&namespace, None, "demo").unwrap().is_some());
    }

    #[test]
    fn test_a_refresh_reference_that_hid_nothing_is_reported_as_unmatched() {
        let audit = Arc::new(RefreshAudit::new(BTreeSet::from([
            "Secret/demo/ca".parse().unwrap(),
            "Secret/demo/typo".parse().unwrap(),
        ])));
        let store = FilteredStore::refreshing(
            ObjectSet::from_documents(
                vec![json!({"apiVersion": "v1", "kind": "Secret", "metadata": {"name": "ca", "namespace": "demo"}})],
                &|_, _| Some(true),
                "demo",
            )
            .unwrap(),
            audit.clone(),
        );
        let secret = KindInfo::new("v1", "Secret", true);
        assert!(store.get(&secret, Some("demo"), "ca").unwrap().is_none());
        assert_eq!(
            audit.unmatched().iter().map(ToString::to_string).collect::<Vec<_>>(),
            ["Secret/demo/typo"]
        );
    }

    #[test]
    fn test_unnamespaced_objects_of_namespaced_kinds_live_in_the_destination_namespace() {
        let set = ObjectSet::from_documents(
            vec![
                json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "settings"}}),
                json!({"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": "demo"}}),
            ],
            &|_, kind| Some(kind != "Namespace"),
            "apps",
        )
        .unwrap();
        let config_map = KindInfo::new("v1", "ConfigMap", true);
        let found = set.get(&config_map, Some("apps"), "settings").unwrap().unwrap();
        assert_eq!(found.pointer("/metadata/namespace"), Some(&json!("apps")));
        assert!(set.get(&config_map, None, "settings").unwrap().is_none());
        let namespace = KindInfo::new("v1", "Namespace", false);
        assert!(set.get(&namespace, None, "demo").unwrap().is_some());
    }
}
