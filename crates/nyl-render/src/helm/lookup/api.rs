//! The request handling of the read-only Kubernetes API that serves Helm `lookup`.
//!
//! Helm (`template --dry-run=server`) discovers the API, reads the objects a
//! chart looks up, and reads each object it renders. This module answers all of
//! it from an [`ObjectStore`], and nothing else:
//!
//! - a request without the render's token is a 401, and any method but GET a 405;
//! - an object that does not exist is a 404, and nothing else is: a store that
//!   fails, a path that is not an object path, a query the API does not
//!   implement, an API version nobody declared, or an object the store holds at
//!   another version is a 500, so Helm fails instead of reading "absent" and
//!   generating what the object would have held;
//! - discovery lists the API versions and kinds the render declares and nothing the
//!   store holds, so what a chart sees in `.Capabilities` never depends on the objects
//!   it is about to look up.
//!
//! Responses and their fingerprints share [`evaluate`], so a cached render
//! revalidates a recorded lookup with exactly the answer Helm got.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use super::model::{split_api_version, KindInfo, ObjectStore, StoreError};
use super::{declare_api_version_fix, API_VERSION_UNDECLARED_CODE, VERSION_MISMATCH_CODE};
use crate::render::cache::LookupFingerprint;

const JSON: &str = "application/json";
const AGGREGATED_DISCOVERY: &str = "application/json;g=apidiscovery.k8s.io;v=v2;as=APIGroupDiscoveryList";
const VERBS: [&str; 2] = ["get", "list"];

/// What the API tells Helm about the cluster, before any object is listed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiCapabilities {
    git_version: String,
    major: String,
    minor: String,
    group_versions: BTreeSet<String>,
    /// The kinds discovery lists: one per kind of an API version.
    kinds: BTreeMap<(String, String), KindInfo>,
}

impl ApiCapabilities {
    /// Capabilities for the declared Kubernetes version, the declared API
    /// versions (each `group/version`, or the core `version`), and the kinds
    /// that discovery must list because Helm maps every object it renders
    /// through discovery and cannot map a kind it does not list. Of two entries
    /// for one kind of an API version the later wins.
    pub fn new(
        kube_version: &str,
        group_versions: impl IntoIterator<Item = String>,
        kinds: impl IntoIterator<Item = KindInfo>,
    ) -> Result<Self, String> {
        let trimmed = kube_version.trim().trim_start_matches('v');
        let mut parts = trimmed.split('.');
        let digits = |part: Option<&str>| -> Option<String> {
            let digits = part?.chars().take_while(char::is_ascii_digit).collect::<String>();
            (!digits.is_empty()).then_some(digits)
        };
        let (Some(major), Some(minor)) = (digits(parts.next()), digits(parts.next())) else {
            return Err(format!(
                "Kubernetes version {kube_version:?} is not of the form major.minor[.patch]"
            ));
        };
        // Helm compares the version with semver ranges, so it needs a patch.
        let git_version = if trimmed.split('.').count() < 3 {
            format!("v{trimmed}.0")
        } else {
            format!("v{trimmed}")
        };
        Ok(Self {
            git_version,
            major,
            minor,
            group_versions: group_versions.into_iter().collect(),
            kinds: kinds
                .into_iter()
                .map(|kind| ((kind.api_version(), kind.kind.clone()), kind))
                .collect(),
        })
    }

    /// The version document `/version` serves.
    pub fn version_document(&self) -> Value {
        json!({
            "major": self.major,
            "minor": self.minor,
            "gitVersion": self.git_version,
            "gitCommit": "",
            "gitTreeState": "",
            "buildDate": "1970-01-01T00:00:00Z",
            "goVersion": "go1.22",
            "compiler": "gc",
            "platform": "linux/amd64",
        })
    }

    /// Every API version discovery lists: the declared ones and those of its kinds.
    fn all_group_versions(&self) -> BTreeSet<String> {
        let mut versions = self.group_versions.clone();
        versions.extend(self.kinds.values().map(KindInfo::api_version));
        versions.insert("v1".to_owned());
        versions
    }

    /// The kinds discovery lists under `api_version`.
    fn kinds_of(&self, api_version: &str) -> Vec<KindInfo> {
        self.kinds
            .values()
            .filter(|kind| kind.api_version() == api_version)
            .cloned()
            .collect()
    }
}

/// A request the API answers.
#[derive(Clone, Copy, Debug)]
pub struct RequestHead<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub query: Option<&'a str>,
    pub accept: Option<&'a str>,
    pub authorization: Option<&'a str>,
}

/// The answer to a request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reply {
    pub status: u16,
    pub content_type: String,
    pub body: Vec<u8>,
    /// The `Allow` header of a 405.
    pub allow: Option<&'static str>,
}

impl Reply {
    fn json(status: u16, content_type: &str, body: &Value) -> Self {
        Self {
            status,
            content_type: content_type.to_owned(),
            body: serde_json::to_vec(body).expect("a JSON value serializes"),
            allow: None,
        }
    }

    fn status(code: u16, reason: &str, message: &str) -> Self {
        Self::json(
            code,
            JSON,
            &json!({
                "kind": "Status",
                "apiVersion": "v1",
                "metadata": {},
                "status": "Failure",
                "message": message,
                "reason": reason,
                "code": code,
            }),
        )
    }

    /// A 500 for a failure of the API itself.
    pub fn internal_error(message: &str) -> Self {
        Self::status(500, "InternalError", message)
    }

    /// The JSON of the body, for tests.
    pub fn body_json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
}

/// One object request the API answered, as a cached render must revalidate it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Observation {
    /// [`LookupRequest::key`] of the request.
    pub request: String,
    pub fingerprint: LookupFingerprint,
}

/// The state one render's API answers from.
pub struct ApiState {
    capabilities: ApiCapabilities,
    store: Arc<dyn ObjectStore>,
    token: String,
    observations: Mutex<BTreeMap<String, LookupFingerprint>>,
    failures: Mutex<Vec<StoreError>>,
}

impl ApiState {
    pub fn new(capabilities: ApiCapabilities, store: Arc<dyn ObjectStore>, token: String) -> Self {
        Self {
            capabilities,
            store,
            token,
            observations: Mutex::new(BTreeMap::new()),
            failures: Mutex::new(Vec::new()),
        }
    }

    /// The object requests answered so far.
    pub fn observations(&self) -> Vec<Observation> {
        self.observations
            .lock()
            .expect("observations are not poisoned")
            .iter()
            .map(|(request, fingerprint)| Observation {
                request: request.clone(),
                fingerprint: fingerprint.clone(),
            })
            .collect()
    }

    /// The requests the API answered with a 500 because the store failed.
    pub fn failures(&self) -> Vec<StoreError> {
        self.failures.lock().expect("failures are not poisoned").clone()
    }

    fn fail(&self, error: StoreError) -> Reply {
        let reply = Reply::status(500, "InternalError", &error.to_string());
        self.failures.lock().expect("failures are not poisoned").push(error);
        reply
    }

    /// Answer one request.
    pub fn respond(&self, request: &RequestHead<'_>) -> Reply {
        if !self.authorized(request.authorization) {
            return Reply::status(401, "Unauthorized", "Unauthorized");
        }
        if request.method != "GET" {
            let mut reply = Reply::status(
                405,
                "MethodNotAllowed",
                "the lookup API is read-only and serves GET requests only",
            );
            reply.allow = Some("GET");
            return reply;
        }
        if let Some(unsupported) = unsupported_query(request.query) {
            return self.fail(StoreError::internal(format!(
                "the lookup API does not implement the {unsupported} query parameter"
            )));
        }
        match self.route(request) {
            Ok(reply) => reply,
            Err(error) => self.fail(error),
        }
    }

    fn authorized(&self, authorization: Option<&str>) -> bool {
        let expected = format!("Bearer {}", self.token);
        authorization.is_some_and(|given| constant_time_equal(given.as_bytes(), expected.as_bytes()))
    }

    fn route(&self, request: &RequestHead<'_>) -> Result<Reply, StoreError> {
        let path = request.path.trim_end_matches('/');
        let wants_aggregated = request.accept.is_some_and(|accept| {
            accept.contains("application/json") && accept.contains("g=apidiscovery.k8s.io") && accept.contains("v=v2")
        });
        match path {
            "/version" => Ok(Reply::json(200, JSON, &self.capabilities.version_document())),
            "/api" => Ok(self.discover_core(wants_aggregated)?),
            "/apis" => Ok(self.discover_groups(wants_aggregated)?),
            "/api/v1" => self.resource_list("v1"),
            _ => {
                if let Some(api_version) = group_version_path(path) {
                    return self.resource_list(&api_version);
                }
                match LookupRequest::from_path(path) {
                    Some(lookup) => self.answer_lookup(&lookup),
                    None => Err(StoreError::internal(format!(
                        "the lookup API does not serve {path}; it serves discovery and GET/LIST of objects only"
                    ))),
                }
            }
        }
    }

    fn discover_core(&self, aggregated: bool) -> Result<Reply, StoreError> {
        if aggregated {
            let item = aggregated_item("", &["v1".to_owned()], |version| self.capabilities.kinds_of(version));
            return Ok(Reply::json(
                200,
                AGGREGATED_DISCOVERY,
                &json!({
                    "kind": "APIGroupDiscoveryList",
                    "apiVersion": "apidiscovery.k8s.io/v2",
                    "metadata": {},
                    "items": [item],
                }),
            ));
        }
        Ok(Reply::json(
            200,
            JSON,
            &json!({
                "kind": "APIVersions",
                "versions": ["v1"],
                "serverAddressByClientCIDRs": [{"clientCIDR": "0.0.0.0/0", "serverAddress": "127.0.0.1"}],
            }),
        ))
    }

    fn discover_groups(&self, aggregated: bool) -> Result<Reply, StoreError> {
        let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for api_version in self.capabilities.all_group_versions() {
            let (group, version) = split_api_version(&api_version);
            if !group.is_empty() {
                groups.entry(group.to_owned()).or_default().push(version.to_owned());
            }
        }
        if aggregated {
            let items = groups
                .iter()
                .map(|(group, versions)| {
                    aggregated_item(group, versions, |version| {
                        self.capabilities.kinds_of(&format!("{group}/{version}"))
                    })
                })
                .collect::<Vec<_>>();
            return Ok(Reply::json(
                200,
                AGGREGATED_DISCOVERY,
                &json!({
                    "kind": "APIGroupDiscoveryList",
                    "apiVersion": "apidiscovery.k8s.io/v2",
                    "metadata": {},
                    "items": items,
                }),
            ));
        }
        let groups = groups
            .iter()
            .map(|(group, versions)| {
                let versions = versions
                    .iter()
                    .map(|version| json!({"groupVersion": format!("{group}/{version}"), "version": version}))
                    .collect::<Vec<_>>();
                json!({"name": group, "versions": versions, "preferredVersion": versions[0]})
            })
            .collect::<Vec<_>>();
        Ok(Reply::json(
            200,
            JSON,
            &json!({"kind": "APIGroupList", "apiVersion": "v1", "groups": groups}),
        ))
    }

    fn resource_list(&self, api_version: &str) -> Result<Reply, StoreError> {
        if !self.capabilities.all_group_versions().contains(api_version) {
            // Only a chart's own `lookup` asks for an API version discovery did not list. Helm
            // would fail with a bare "not found" and the render with "helm template failed", so
            // the failure is recorded with its code and its fix.
            return Err(StoreError::new(
                API_VERSION_UNDECLARED_CODE,
                format!(
                    "a chart looked up an object of API version {api_version}, which is not declared, so a render \
                     with lookup cannot serve it; {}",
                    declare_api_version_fix(api_version)
                ),
            ));
        }
        let resources = self
            .capabilities
            .kinds_of(api_version)
            .iter()
            .map(|kind| {
                json!({
                    "name": kind.plural,
                    "singularName": kind.singular,
                    "namespaced": kind.namespaced,
                    "kind": kind.kind,
                    "verbs": VERBS,
                })
            })
            .collect::<Vec<_>>();
        Ok(Reply::json(
            200,
            JSON,
            &json!({"kind": "APIResourceList", "apiVersion": "v1", "groupVersion": api_version, "resources": resources}),
        ))
    }

    fn answer_lookup(&self, lookup: &LookupRequest) -> Result<Reply, StoreError> {
        let evaluation = evaluate(self.store.as_ref(), lookup)?;
        self.observations
            .lock()
            .expect("observations are not poisoned")
            .insert(lookup.key(), evaluation.fingerprint());
        Ok(match evaluation {
            Evaluation::UnknownResource => {
                Reply::status(404, "NotFound", "the server could not find the requested resource")
            }
            Evaluation::Absent { plural, name } => Reply::json(
                404,
                JSON,
                &json!({
                    "kind": "Status",
                    "apiVersion": "v1",
                    "metadata": {},
                    "status": "Failure",
                    "message": format!("{plural} {name:?} not found"),
                    "reason": "NotFound",
                    "details": {"name": name, "kind": plural},
                    "code": 404,
                }),
            ),
            Evaluation::Object(object) => Reply::json(200, JSON, &object),
            Evaluation::List { kind, items } => Reply::json(
                200,
                JSON,
                &json!({
                    "kind": format!("{}List", kind.kind),
                    "apiVersion": kind.api_version(),
                    "metadata": {"resourceVersion": ""},
                    "items": items,
                }),
            ),
        })
    }
}

fn aggregated_item(group: &str, versions: &[String], kinds_of: impl Fn(&str) -> Vec<KindInfo>) -> Value {
    let versions = versions
        .iter()
        .map(|version| {
            let resources = kinds_of(version)
                .iter()
                .map(|kind| {
                    json!({
                        "resource": kind.plural,
                        "singularResource": kind.singular,
                        "responseKind": {"group": group, "version": version, "kind": kind.kind},
                        "scope": if kind.namespaced { "Namespaced" } else { "Cluster" },
                        "verbs": VERBS,
                    })
                })
                .collect::<Vec<_>>();
            json!({"version": version, "freshness": "Current", "resources": resources})
        })
        .collect::<Vec<_>>();
    json!({"metadata": {"name": group, "creationTimestamp": null}, "versions": versions})
}

/// `/apis/<group>/<version>`, as the `apiVersion` it names.
fn group_version_path(path: &str) -> Option<String> {
    let rest = path.strip_prefix("/apis/")?;
    let (group, version) = rest.split_once('/')?;
    (!group.is_empty() && !version.is_empty() && !version.contains('/')).then(|| format!("{group}/{version}"))
}

/// A query parameter the API does not implement, which would make an answer
/// silently wrong.
fn unsupported_query(query: Option<&str>) -> Option<&'static str> {
    let query = query?;
    for pair in query.split('&') {
        let name = pair.split('=').next().unwrap_or_default();
        match name {
            "watch" if !pair.ends_with("=false") && pair != "watch" => return Some("watch"),
            "labelSelector" if pair.len() > "labelSelector=".len() => return Some("labelSelector"),
            "fieldSelector" if pair.len() > "fieldSelector=".len() => return Some("fieldSelector"),
            "continue" if pair.len() > "continue=".len() => return Some("continue"),
            _ => {}
        }
    }
    None
}

fn constant_time_equal(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .fold(0, |difference, (a, b)| difference | (a ^ b))
            == 0
}

/// A read of one object, or a list of one kind, as a request path names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LookupRequest {
    pub api_version: String,
    pub plural: String,
    /// `None` reads across all namespaces, or a cluster-scoped kind.
    pub namespace: Option<String>,
    /// `None` lists.
    pub name: Option<String>,
}

impl LookupRequest {
    /// The request an object path names: `/api/v1` or `/apis/<g>/<v>`, then
    /// optionally `/namespaces/<ns>`, then `/<plural>` and optionally `/<name>`.
    /// `None` for any other path, and for subresources.
    pub fn from_path(path: &str) -> Option<Self> {
        let (api_version, rest) = if let Some(rest) = path.strip_prefix("/api/v1/") {
            ("v1".to_owned(), rest)
        } else {
            let (group, rest) = path.strip_prefix("/apis/")?.split_once('/')?;
            let (version, rest) = rest.split_once('/')?;
            (format!("{group}/{version}"), rest)
        };
        let segments = rest.split('/').collect::<Vec<_>>();
        if segments.iter().any(|segment| segment.is_empty()) {
            return None;
        }
        let (namespace, tail) = match segments.as_slice() {
            // `/namespaces/<name>` is the Namespace object itself.
            ["namespaces", _name] => (None, &segments[..]),
            ["namespaces", namespace, tail @ ..] => (Some((*namespace).to_owned()), tail),
            tail => (None, tail),
        };
        let (plural, name) = match tail {
            [plural] => ((*plural).to_owned(), None),
            [plural, name] => ((*plural).to_owned(), Some((*name).to_owned())),
            _ => return None,
        };
        Some(Self {
            api_version,
            plural,
            namespace,
            name,
        })
    }

    /// The request as a cache dependency names it, within a Release.
    pub fn key(&self) -> String {
        format!(
            "{}|{}|{}|{}",
            self.api_version,
            self.plural,
            self.namespace.as_deref().unwrap_or_default(),
            self.name.as_deref().unwrap_or_default()
        )
    }

    /// The request [`Self::key`] names.
    pub fn from_key(key: &str) -> Option<Self> {
        let mut parts = key.split('|');
        let (api_version, plural, namespace, name) = (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
        let optional = |text: &str| (!text.is_empty()).then(|| text.to_owned());
        (parts.next().is_none() && !api_version.is_empty() && !plural.is_empty()).then(|| Self {
            api_version: api_version.to_owned(),
            plural: plural.to_owned(),
            namespace: optional(namespace),
            name: optional(name),
        })
    }
}

/// What a [`LookupRequest`] found in a store.
#[derive(Clone, Debug, PartialEq)]
pub enum Evaluation {
    /// No kind of that API version has that plural.
    UnknownResource,
    Absent {
        plural: String,
        name: String,
    },
    Object(Value),
    List {
        kind: KindInfo,
        items: Vec<Value>,
    },
}

impl Evaluation {
    /// The result as a cache dependency: Secret data is kept for the recorder
    /// to fingerprint with its key.
    pub fn fingerprint(&self) -> LookupFingerprint {
        let (is_secret, value) = match self {
            Self::UnknownResource | Self::Absent { .. } => return LookupFingerprint::Absent,
            Self::Object(object) => (is_secret(object), object.clone()),
            Self::List { kind, items } => (
                kind.kind == "Secret" && kind.group.is_empty(),
                Value::Array(items.clone()),
            ),
        };
        let bytes = nyl_core::digest::canonical_json_bytes(&value).expect("a JSON value serializes");
        if is_secret {
            LookupFingerprint::Secret(bytes)
        } else {
            LookupFingerprint::Plain(nyl_core::digest::sha256_hex(&bytes))
        }
    }
}

fn is_secret(object: &Value) -> bool {
    object.get("kind").and_then(Value::as_str) == Some("Secret")
        && object
            .get("apiVersion")
            .and_then(Value::as_str)
            .is_some_and(|api_version| !api_version.contains('/'))
}

/// Evaluate `request` against `store`.
///
/// An object the store holds at another version of its kind than the request names is
/// an error, never absent: Helm would read "absent" and generate what the object holds.
pub fn evaluate(store: &dyn ObjectStore, request: &LookupRequest) -> Result<Evaluation, StoreError> {
    let (group, version) = split_api_version(&request.api_version);
    let found = store.kind_of(&request.api_version, &request.plural)?;
    let evaluation = match &found {
        None => Evaluation::UnknownResource,
        Some(kind) => {
            // A cluster-scoped kind has no namespace to read it in.
            if request.namespace.is_some() && !kind.namespaced {
                return Ok(Evaluation::UnknownResource);
            }
            // A namespaced kind is read in the namespace its path names.
            let namespace = request.namespace.as_deref().filter(|_| kind.namespaced);
            match &request.name {
                Some(name) => match store.get(kind, namespace, name)? {
                    Some(object) => Evaluation::Object(object),
                    None => Evaluation::Absent {
                        plural: kind.plural.clone(),
                        name: name.clone(),
                    },
                },
                None => Evaluation::List {
                    kind: kind.clone(),
                    items: store.list(kind, namespace)?,
                },
            }
        }
    };
    let found_nothing = match &evaluation {
        Evaluation::UnknownResource | Evaluation::Absent { .. } => true,
        Evaluation::List { items, .. } => items.is_empty(),
        Evaluation::Object(_) => false,
    };
    if found_nothing {
        for other in store
            .kinds()?
            .iter()
            .filter(|kind| kind.group == group && kind.plural == request.plural && kind.version != version)
        {
            let namespace = request.namespace.as_deref().filter(|_| other.namespaced);
            let held = match &request.name {
                Some(name) => store.get(other, namespace, name)?.is_some(),
                None => !store.list(other, namespace)?.is_empty(),
            };
            if held {
                return Err(version_mismatch(
                    request.namespace.as_deref(),
                    request.name.as_deref(),
                    &request.api_version,
                    other,
                ));
            }
        }
    }
    Ok(evaluation)
}

/// The error for an object a chart looks up at `requested` that the source holds as `held`.
pub(crate) fn version_mismatch(
    namespace: Option<&str>,
    name: Option<&str>,
    requested: &str,
    held: &KindInfo,
) -> StoreError {
    let what = match (name, namespace) {
        (Some(name), Some(namespace)) => format!("{} {namespace}/{name}", held.kind),
        (Some(name), None) => format!("{} {name}", held.kind),
        (None, _) => format!("{} objects", held.kind),
    };
    StoreError::new(
        VERSION_MISMATCH_CODE,
        format!(
            "a chart looked up {what} as {requested}, but the lookup source holds it at {} (held at {}); change the \
             chart's lookup to apiVersion {}, or pass --refresh-lookup for it to regenerate it deliberately at the \
             new version",
            held.api_version(),
            held.version,
            held.api_version()
        ),
    )
}

/// A digest of what discovery lists for `store`: its kinds and where they live.
pub fn discovery_fingerprint(store: &dyn ObjectStore) -> Result<String, StoreError> {
    let kinds = store.kinds()?;
    let description = kinds
        .iter()
        .map(|kind| {
            format!(
                "{}|{}|{}|{}",
                kind.api_version(),
                kind.kind,
                kind.plural,
                kind.namespaced
            )
        })
        .collect::<Vec<_>>();
    let bytes = nyl_core::digest::canonical_json_bytes(&description).expect("strings serialize");
    Ok(nyl_core::digest::sha256_hex(&bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::helm::lookup::model::{FilteredStore, ObjectSet};

    const TOKEN: &str = "per-render-token";

    fn store() -> ObjectSet {
        ObjectSet::from_documents(
            vec![
                json!({"apiVersion": "v1", "kind": "Secret", "metadata": {"name": "my-ca", "namespace": "demo"}, "data": {"tls.crt": "Q0VSVA=="}}),
                json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "cm-a", "namespace": "demo"}}),
                json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "cm-b", "namespace": "other"}}),
                json!({"apiVersion": "rbac.authorization.k8s.io/v1", "kind": "ClusterRole", "metadata": {"name": "reader"}}),
            ],
            &|_, kind| Some(kind != "ClusterRole"),
            "demo",
        )
        .unwrap()
    }

    fn state(store: Arc<dyn ObjectStore>) -> ApiState {
        let group_versions = ["v1", "apps/v1", "foo.example.com/v1", "rbac.authorization.k8s.io/v1"]
            .map(ToOwned::to_owned)
            .into_iter()
            .collect::<BTreeSet<_>>();
        let kinds = crate::helm::lookup::builtin::builtin_kinds_of(&group_versions)
            .chain([KindInfo::new("monitoring.coreos.com/v1", "ServiceMonitor", true)])
            .collect::<Vec<_>>();
        let capabilities = ApiCapabilities::new("1.30", group_versions, kinds).unwrap();
        ApiState::new(capabilities, store, TOKEN.to_owned())
    }

    fn get(state: &ApiState, path: &str) -> Reply {
        request(state, "GET", path, None)
    }

    fn request(state: &ApiState, method: &str, path: &str, accept: Option<&str>) -> Reply {
        let (path, query) = path
            .split_once('?')
            .map_or((path, None), |(path, query)| (path, Some(query)));
        state.respond(&RequestHead {
            method,
            path,
            query,
            accept,
            authorization: Some(&format!("Bearer {TOKEN}")),
        })
    }

    #[test]
    fn test_version_reports_the_declared_kubernetes_version() {
        let reply = get(&state(Arc::new(store())), "/version");
        let version = reply.body_json();
        assert_eq!(version["gitVersion"], "v1.30.0");
        assert_eq!(
            (version["major"].as_str(), version["minor"].as_str()),
            (Some("1"), Some("30"))
        );
        let vendor = ApiCapabilities::new("v1.31.4-gke.100", [], []).unwrap();
        assert_eq!(vendor.version_document()["gitVersion"], "v1.31.4-gke.100");
        assert_eq!(vendor.version_document()["minor"], "31");
        assert!(ApiCapabilities::new("latest", [], []).is_err());
    }

    #[test]
    fn test_requests_without_the_render_token_are_unauthorized() {
        let state = state(Arc::new(store()));
        for authorization in [None, Some("Bearer wrong"), Some(TOKEN)] {
            let reply = state.respond(&RequestHead {
                method: "GET",
                path: "/version",
                query: None,
                accept: None,
                authorization,
            });
            assert_eq!(reply.status, 401, "{authorization:?}");
            assert_eq!(reply.body_json()["reason"], "Unauthorized");
        }
        assert!(state.observations().is_empty());
    }

    #[test]
    fn test_writes_are_method_not_allowed() {
        let state = state(Arc::new(store()));
        for method in ["POST", "PUT", "PATCH", "DELETE", "HEAD"] {
            let reply = request(&state, method, "/api/v1/namespaces/demo/secrets", None);
            assert_eq!(reply.status, 405, "{method}");
            assert_eq!(reply.allow, Some("GET"));
            assert_eq!(reply.body_json()["reason"], "MethodNotAllowed");
        }
    }

    const AGGREGATED_ACCEPT: &str = "application/json;g=apidiscovery.k8s.io;v=v2;as=APIGroupDiscoveryList,application/json;g=apidiscovery.k8s.io;v=v2beta1;as=APIGroupDiscoveryList,application/json";

    #[test]
    fn test_aggregated_discovery_lists_declared_versions_and_their_kinds() {
        let state = state(Arc::new(store()));
        let core = request(&state, "GET", "/api", Some(AGGREGATED_ACCEPT));
        assert_eq!(core.content_type, AGGREGATED_DISCOVERY);
        let core = core.body_json();
        assert_eq!(core["apiVersion"], "apidiscovery.k8s.io/v2");
        let kinds = core["items"][0]["versions"][0]["resources"]
            .as_array()
            .unwrap()
            .iter()
            .map(|resource| resource["responseKind"]["kind"].as_str().unwrap())
            .collect::<BTreeSet<_>>();
        assert!(
            ["ConfigMap", "Secret", "Namespace", "Service"]
                .iter()
                .all(|kind| kinds.contains(kind)),
            "the built-in kinds of v1 are listed: {kinds:?}"
        );

        let groups = request(&state, "GET", "/apis", Some(AGGREGATED_ACCEPT)).body_json();
        let described = groups["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| {
                let versions = item["versions"].as_array().unwrap();
                (
                    item["metadata"]["name"].as_str().unwrap().to_owned(),
                    versions[0]["resources"].as_array().unwrap().len(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            described,
            [
                ("apps".to_owned(), 5),
                ("foo.example.com".to_owned(), 0),
                ("monitoring.coreos.com".to_owned(), 1),
                ("rbac.authorization.k8s.io".to_owned(), 4),
            ],
            "built-in kinds come with their API version, and a custom API version lists a kind only when it is declared"
        );
    }

    /// Catches `.Capabilities` depending on what the lookup source holds, which would
    /// make the first render of a Release differ from every one after it.
    #[test]
    fn test_discovery_does_not_depend_on_what_the_store_holds() {
        let held = state(Arc::new(store()));
        let empty = state(Arc::new(crate::helm::lookup::model::EmptyStore));
        let broken = state(Arc::new(Broken));
        for path in ["/api", "/apis", "/api/v1", "/apis/apps/v1", "/apis/foo.example.com/v1"] {
            for accept in [Some(AGGREGATED_ACCEPT), Some("application/json")] {
                let expected = request(&held, "GET", path, accept);
                assert_eq!(expected.status, 200, "{path}");
                assert_eq!(request(&empty, "GET", path, accept), expected, "{path}");
                assert_eq!(request(&broken, "GET", path, accept), expected, "{path}");
            }
        }
        assert!(broken.failures().is_empty(), "discovery never reads the source");
    }

    #[test]
    fn test_legacy_discovery_answers_without_the_aggregated_accept() {
        let state = state(Arc::new(store()));
        let groups = request(&state, "GET", "/apis", Some("application/json")).body_json();
        assert_eq!(groups["kind"], "APIGroupList");
        assert_eq!(
            groups["groups"]
                .as_array()
                .unwrap()
                .iter()
                .map(|group| group["name"].as_str().unwrap())
                .collect::<Vec<_>>(),
            [
                "apps",
                "foo.example.com",
                "monitoring.coreos.com",
                "rbac.authorization.k8s.io"
            ]
        );
        let core = get(&state, "/api/v1").body_json();
        assert_eq!(core["kind"], "APIResourceList");
        let resource = core["resources"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["kind"] == "Secret")
            .unwrap();
        assert_eq!(
            (resource["name"].as_str(), resource["namespaced"].as_bool()),
            (Some("secrets"), Some(true))
        );
        let apps = get(&state, "/apis/apps/v1").body_json();
        assert_eq!(apps["resources"].as_array().unwrap().len(), 5);
        assert!(state.failures().is_empty());
    }

    /// Catches an undeclared API version surfacing as Helm's bare "not found": only a chart's
    /// own `lookup` asks for one, and it is recorded with its code and fix.
    #[test]
    fn test_a_lookup_of_an_undeclared_api_version_is_a_coded_failure_with_its_fix() {
        let state = state(Arc::new(store()));
        let reply = get(&state, "/apis/undeclared.example.com/v1");
        assert_eq!(reply.status, 500);
        let failures = state.failures();
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].code, Some("NYL-HELM-LOOKUP-API-VERSION-UNDECLARED"));
        assert!(
            failures[0].message.contains("undeclared.example.com/v1")
                && failures[0].message.contains("spec.kubernetes.apiVersions"),
            "{}",
            failures[0].message
        );
    }

    #[test]
    fn test_a_missing_object_is_not_found_and_a_present_one_is_returned() {
        let state = state(Arc::new(store()));
        let found = get(&state, "/api/v1/namespaces/demo/secrets/my-ca");
        assert_eq!(found.status, 200);
        assert_eq!(found.body_json()["data"]["tls.crt"], "Q0VSVA==");
        let missing = get(&state, "/api/v1/namespaces/demo/secrets/other");
        assert_eq!(missing.status, 404);
        assert_eq!(missing.body_json()["reason"], "NotFound");
        // The same name in another namespace is a different object.
        assert_eq!(get(&state, "/api/v1/namespaces/other/secrets/my-ca").status, 404);
        // Cluster-scoped objects and the Namespace-style path.
        assert_eq!(
            get(&state, "/apis/rbac.authorization.k8s.io/v1/clusterroles/reader").status,
            200
        );
        assert_eq!(get(&state, "/api/v1/namespaces/demo").status, 404);
        // A kind the store holds nothing of is absent, like an object that does not exist.
        assert_eq!(get(&state, "/apis/apps/v1/namespaces/demo/deployments/web").status, 404);
        assert!(state.failures().is_empty());
    }

    #[test]
    fn test_lists_are_scoped_to_a_namespace_or_span_all_of_them() {
        let state = state(Arc::new(store()));
        let names = |path: &str| -> Vec<String> {
            get(&state, path).body_json()["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|item| item["metadata"]["name"].as_str().unwrap().to_owned())
                .collect()
        };
        assert_eq!(names("/api/v1/namespaces/demo/configmaps"), ["cm-a"]);
        assert_eq!(names("/api/v1/configmaps"), ["cm-a", "cm-b"]);
        let list = get(&state, "/api/v1/namespaces/demo/configmaps").body_json();
        assert_eq!(list["kind"], "ConfigMapList");
    }

    struct Broken;

    impl ObjectStore for Broken {
        fn kinds(&self) -> Result<Vec<KindInfo>, StoreError> {
            Err(StoreError::new("NYL-TEST-BROKEN", "the snapshot is corrupt"))
        }

        fn get(&self, _: &KindInfo, _: Option<&str>, _: &str) -> Result<Option<Value>, StoreError> {
            unreachable!("kinds fail first")
        }

        fn list(&self, _: &KindInfo, _: Option<&str>) -> Result<Vec<Value>, StoreError> {
            unreachable!("kinds fail first")
        }
    }

    #[test]
    fn test_an_unreadable_store_is_a_server_error_never_not_found() {
        let state = state(Arc::new(Broken));
        for path in [
            "/api/v1/namespaces/demo/secrets/my-ca",
            "/api/v1/namespaces/demo/secrets",
        ] {
            let reply = get(&state, path);
            assert_eq!(reply.status, 500, "{path}");
            assert!(reply.body_json()["message"]
                .as_str()
                .unwrap()
                .contains("NYL-TEST-BROKEN"));
        }
        assert_eq!(state.failures().len(), 2);
        assert!(state.observations().is_empty(), "a failed read is no observation");
    }

    /// Catches a lookup at a version the object is not held at reading as absent, which
    /// would regenerate what the object holds.
    #[test]
    fn test_an_object_held_at_another_version_is_a_coded_failure_never_not_found() {
        let store = ObjectSet::from_documents(
            vec![
                json!({"apiVersion": "autoscaling/v2", "kind": "HorizontalPodAutoscaler", "metadata": {"name": "web", "namespace": "demo"}}),
                json!({"apiVersion": "autoscaling/v1", "kind": "HorizontalPodAutoscaler", "metadata": {"name": "api", "namespace": "demo"}}),
            ],
            &|_, _| Some(true),
            "demo",
        )
        .unwrap();
        let request = |path: &str| LookupRequest::from_path(path).unwrap();

        // Held at v2, asked at v1: the kind exists at v1 (held for `api`), the object does not.
        let error = evaluate(
            &store,
            &request("/apis/autoscaling/v1/namespaces/demo/horizontalpodautoscalers/web"),
        )
        .unwrap_err();
        assert_eq!(error.code, Some("NYL-HELM-LOOKUP-VERSION-MISMATCH"));
        assert!(
            error.message.contains("autoscaling/v2") && error.message.contains("held at v2"),
            "{}",
            error.message
        );
        // Held at v1, asked at v2.
        assert!(evaluate(
            &store,
            &request("/apis/autoscaling/v2/namespaces/demo/horizontalpodautoscalers/api")
        )
        .is_err());
        // The right version finds it, and a name held nowhere is plainly absent.
        assert!(matches!(
            evaluate(
                &store,
                &request("/apis/autoscaling/v2/namespaces/demo/horizontalpodautoscalers/web")
            ),
            Ok(Evaluation::Object(_))
        ));
        assert!(matches!(
            evaluate(
                &store,
                &request("/apis/autoscaling/v2/namespaces/demo/horizontalpodautoscalers/none")
            ),
            Ok(Evaluation::Absent { .. })
        ));
        // A list at a version that holds nothing, where another version holds objects.
        let only_v2 = ObjectSet::from_documents(
            vec![json!({"apiVersion": "autoscaling/v2", "kind": "HorizontalPodAutoscaler", "metadata": {"name": "web", "namespace": "demo"}})],
            &|_, _| Some(true),
            "demo",
        )
        .unwrap();
        let error = evaluate(
            &only_v2,
            &request("/apis/autoscaling/v1/namespaces/demo/horizontalpodautoscalers"),
        )
        .unwrap_err();
        assert_eq!(error.code, Some("NYL-HELM-LOOKUP-VERSION-MISMATCH"));
        // Through the API it is a 500 that Helm cannot read as absent.
        let state = state(Arc::new(only_v2));
        let reply = get(
            &state,
            "/apis/autoscaling/v1/namespaces/demo/horizontalpodautoscalers/web",
        );
        assert_eq!(reply.status, 500);
        assert_eq!(state.failures().len(), 1);
    }

    #[test]
    fn test_paths_that_are_not_object_paths_are_server_errors_not_absent() {
        let state = state(Arc::new(store()));
        for path in [
            "/openapi/v2",
            "/healthz",
            "/api/v1/namespaces/demo/pods/web/log",
            "/apis/apps/v1/namespaces/demo/deployments/web/scale",
            "/api/v1/namespaces/demo/secrets/my-ca?watch=true",
            "/api/v1/secrets?labelSelector=app%3Dweb",
        ] {
            assert_eq!(get(&state, path).status, 500, "{path}");
        }
        // Parameters Helm sends are ignored.
        assert_eq!(get(&state, "/api/v1/configmaps?timeout=32s&limit=500").status, 200);
        assert_eq!(get(&state, "/api/v1/configmaps?watch=false").status, 200);
    }

    #[test]
    fn test_the_scope_filter_hides_objects_that_belong_to_no_admitted_release() {
        let filtered = FilteredStore::new(store(), |object| {
            object.pointer("/metadata/namespace").and_then(Value::as_str) != Some("other")
        });
        let state = state(Arc::new(filtered));
        assert_eq!(get(&state, "/api/v1/namespaces/other/configmaps/cm-b").status, 404);
        assert_eq!(
            get(&state, "/api/v1/configmaps").body_json()["items"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(get(&state, "/api/v1/namespaces/demo/configmaps/cm-a").status, 200);
    }

    #[test]
    fn test_observations_fingerprint_what_helm_read() {
        let state = state(Arc::new(store()));
        get(&state, "/api/v1/namespaces/demo/secrets/my-ca");
        get(&state, "/api/v1/namespaces/demo/secrets/absent");
        get(&state, "/api/v1/namespaces/demo/configmaps");
        let observations = state
            .observations()
            .into_iter()
            .map(|observation| (observation.request, observation.fingerprint))
            .collect::<BTreeMap<_, _>>();
        assert!(matches!(
            observations["v1|secrets|demo|my-ca"],
            LookupFingerprint::Secret(_)
        ));
        assert_eq!(observations["v1|secrets|demo|absent"], LookupFingerprint::Absent);
        assert!(matches!(
            observations["v1|configmaps|demo|"],
            LookupFingerprint::Plain(_)
        ));
    }

    #[test]
    fn test_request_keys_round_trip() {
        for path in [
            "/api/v1/namespaces/demo/secrets/my-ca",
            "/api/v1/configmaps",
            "/apis/apps/v1/namespaces/demo/deployments",
            "/apis/rbac.authorization.k8s.io/v1/clusterroles/reader",
            "/api/v1/namespaces/demo",
        ] {
            let request = LookupRequest::from_path(path).unwrap();
            assert_eq!(LookupRequest::from_key(&request.key()), Some(request), "{path}");
        }
        assert_eq!(LookupRequest::from_path("/apis/apps/v1"), None);
        assert_eq!(LookupRequest::from_key("v1|secrets"), None);
    }
}
