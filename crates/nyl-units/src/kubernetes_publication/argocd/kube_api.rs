//! Reading Applications from the Kubernetes API of the Argo CD control-plane
//! Cluster.
//!
//! Contract: [Kubernetes publications](../../../../../design/promotion.md#kubernetes-publications),
//! access method `kubernetes`. The Applications are `applications.argoproj.io`
//! objects in the instance's namespace (or the namespace an ApplicationGroup
//! names). The connection is explicit end to end: the kubeconfig is the file
//! the unit admits as `KUBECONFIG` (a list of files, as for `kubectl`, is
//! merged), and the context is the control-plane Cluster's declared
//! `spec.live.context`. Nothing falls back to the kubeconfig's current
//! context, `~/.kube/config`, or in-cluster configuration, so the same
//! declaration reads the same cluster wherever it runs. The kubeconfig's own
//! credentials, including its exec plugins, authenticate as it defines them.
//! A proxy comes only from the kubeconfig (`proxy-url`), never from the
//! process's `HTTPS_PROXY`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use kube::api::{ApiResource, DynamicObject, ListParams};
use kube::config::{KubeConfigOptions, Kubeconfig};
use kube::{Api, Client};
use serde_json::Value;

use super::{ArgoQueries, ReadError, ReadErrorKind, ReadRequest, Reading, ReconcilerReader, KUBECONFIG_ENV};
use crate::kubernetes_publication::reconciler::{block_on, Proof};

/// An explicit connection to one Cluster: a kubeconfig context, read from the
/// files the unit admits as `KUBECONFIG`. Nothing falls back to the current
/// context, `~/.kube/config`, or in-cluster configuration. Shared by the
/// Argo CD reader and the reader of a workload Cluster's resources.
pub struct Connection {
    cluster: String,
    context: String,
    kubeconfig: Vec<PathBuf>,
    pub(crate) budget: Duration,
}

impl Connection {
    /// A connection to the Cluster named `cluster` through `context` (its
    /// `spec.live.context`), reading the files of `kubeconfig` (the
    /// admitted `KUBECONFIG`). `without_context` is the advice a missing
    /// context gets. Each reading ends within `budget`.
    pub fn new(
        cluster: &str,
        context: Option<&str>,
        kubeconfig: Option<&str>,
        budget: Duration,
        without_context: &str,
    ) -> Result<Self, ReadError> {
        let context = context.filter(|context| !context.is_empty()).ok_or_else(|| {
            ReadError::new(
                ReadErrorKind::Declaration,
                format!(
                    "no local kubeconfig context is declared for Cluster {cluster}: set its spec.live.context \
                     (Nyl never uses the current context){without_context}"
                ),
            )
        })?;
        let files = kubeconfig
            .map(|paths| {
                std::env::split_paths(paths)
                    .filter(|path| !path.as_os_str().is_empty())
                    .collect::<Vec<_>>()
            })
            .filter(|files| !files.is_empty())
            .ok_or_else(|| {
                ReadError::new(
                    ReadErrorKind::Credentials,
                    format!(
                        "no kubeconfig is admitted to read Cluster {cluster} (context {context}): add {KUBECONFIG_ENV} \
                         to the unit's env.passthrough so the runner's kubeconfig file is used (the project's \
                         kube context and ~/.kube/config are never read implicitly)"
                    ),
                )
            })?;
        Ok(Self {
            cluster: cluster.to_owned(),
            context: context.to_owned(),
            kubeconfig: files,
            budget,
            namespace: super::DEFAULT_NAMESPACE.to_owned(),
        })
    }

    /// The context's name, for messages.
    pub fn context(&self) -> &str {
        &self.context
    }

    /// The Cluster's name, for messages.
    pub fn cluster(&self) -> &str {
        &self.cluster
    }

    pub(crate) async fn client(&self) -> Result<Client, ReadError> {
        let declaration = |detail: String| ReadError::new(ReadErrorKind::Declaration, detail);
        let mut merged: Option<Kubeconfig> = None;
        for path in &self.kubeconfig {
            let file = Kubeconfig::read_from(path)
                .map_err(|error| declaration(format!("cannot read the kubeconfig {}: {error}", path.display())))?;
            merged = Some(match merged {
                Some(merged) => merged
                    .merge(file)
                    .map_err(|error| declaration(format!("cannot merge the kubeconfig {}: {error}", path.display())))?,
                None => file,
            });
        }
        let kubeconfig = merged.ok_or_else(|| declaration("no kubeconfig file".to_owned()))?;
        if !kubeconfig.contexts.iter().any(|named| named.name == self.context) {
            return Err(declaration(format!(
                "the admitted kubeconfig has no context {:?}, which Cluster {} declares as spec.live.context",
                self.context, self.cluster
            )));
        }
        let declares_proxy = kubeconfig
            .contexts
            .iter()
            .find(|named| named.name == self.context)
            .and_then(|named| named.context.as_ref())
            .and_then(|context| {
                kubeconfig
                    .clusters
                    .iter()
                    .find(|cluster| cluster.name == context.cluster)
            })
            .and_then(|named| named.cluster.as_ref())
            .is_some_and(|cluster| cluster.proxy_url.as_deref().is_some_and(|proxy| !proxy.is_empty()));
        let mut config = kube::Config::from_custom_kubeconfig(
            kubeconfig,
            &KubeConfigOptions {
                context: Some(self.context.clone()),
                ..KubeConfigOptions::default()
            },
        )
        .await
        .map_err(|error| {
            ReadError::new(
                ReadErrorKind::Credentials,
                format!(
                    "cannot use context {:?} of the admitted kubeconfig: {error}",
                    self.context
                ),
            )
        })?;
        // kube reads HTTPS_PROXY from the process environment when the
        // kubeconfig names no proxy; nothing outside the declaration and the
        // admitted environment may steer the connection, so only a proxy the
        // kubeconfig itself declares is kept.
        if !declares_proxy {
            config.proxy_url = None;
        }
        config.connect_timeout = Some(self.budget.min(Duration::from_secs(10)));
        config.read_timeout = Some(self.budget);
        Client::try_from(config).map_err(|error| {
            ReadError::new(
                ReadErrorKind::Unreachable,
                format!("cannot connect through context {:?}: {error}", self.context),
            )
        })
    }
}

/// Reads Applications through a kubeconfig context.
pub struct KubeApiReader {
    connection: Connection,
}

impl KubeApiReader {
    /// A reader of the Cluster named `cluster` through `context` (its
    /// `spec.live.context`), reading the files of `kubeconfig` (the
    /// admitted `KUBECONFIG`). Each reading ends within `budget`.
    pub fn new(
        cluster: &str,
        context: Option<&str>,
        kubeconfig: Option<&str>,
        budget: Duration,
    ) -> Result<Self, ReadError> {
        Ok(Self {
            connection: Connection::new(
                cluster,
                context,
                kubeconfig,
                budget,
                ", or declare the ArgoCDInstance's spec.access with method api",
            )?,
        })
    }
}

impl KubeApiReader {
    /// The `plural` objects (of `kind`) of `namespace`, by name, each as served.
    fn list(&self, kind: &str, plural: &str, namespace: &str) -> Result<BTreeMap<String, Value>, ReadError> {
        let budget = self.connection.budget;
        block_on(async {
            tokio::time::timeout(budget, async {
                let client = self.connection.client().await?;
                let resource = ApiResource {
                    group: "argoproj.io".to_owned(),
                    version: "v1alpha1".to_owned(),
                    api_version: "argoproj.io/v1alpha1".to_owned(),
                    kind: kind.to_owned(),
                    plural: plural.to_owned(),
                };
                let api: Api<DynamicObject> = Api::namespaced_with(client, namespace, &resource);
                let list = api
                    .list(&ListParams::default())
                    .await
                    .map_err(|error| self.list_error(&error, plural, namespace))?;
                list.items
                    .into_iter()
                    .map(|object| {
                        let name = object.metadata.name.clone().ok_or_else(|| {
                            ReadError::new(ReadErrorKind::Malformed, format!("a {kind} in the list has no name"))
                        })?;
                        let value = serde_json::to_value(&object)
                            .map_err(|error| ReadError::new(ReadErrorKind::Malformed, error.to_string()))?;
                        Ok((name, value))
                    })
                    .collect()
            })
            .await
            .unwrap_or_else(|_| {
                Err(ReadError::new(
                    ReadErrorKind::Unreachable,
                    format!(
                        "reading {plural} of namespace {namespace} through context {:?} took longer than {}s",
                        self.connection.context(),
                        budget.as_secs()
                    ),
                ))
            })
        })?
    }
}

impl KubeApiReader {
    /// The Applications in `namespace` by name, each as Argo CD serves it.
    pub fn applications(&self, namespace: &str) -> Result<BTreeMap<String, Value>, ReadError> {
        self.list("Application", "applications", namespace)
    }

    /// Whether the AppProject `name` of `namespace` is in the listing.
    pub fn project_exists(&self, namespace: &str, name: &str) -> Result<bool, ReadError> {
        Ok(self.list("AppProject", "appprojects", namespace)?.contains_key(name))
    }
}

impl ArgoQueries for KubeApiReader {
    fn applications(&self, namespace: &str) -> Result<BTreeMap<String, Value>, ReadError> {
        KubeApiReader::applications(self, namespace)
    }

    /// The Kubernetes API fails a list the context may not make instead of
    /// filtering it, so there is no per-Application question to ask.
    fn can_get(&self, _rbac_name: &str) -> Result<bool, ReadError> {
        Ok(true)
    }

    fn project_exists(&self, namespace: &str, name: &str) -> Result<bool, ReadError> {
        KubeApiReader::project_exists(self, namespace, name)
    }
}

impl ReconcilerReader for KubeApiReader {
    /// A missing Application means something only when the reading shows what
    /// only the Argo CD that runs the target has: the listing of a context for
    /// another Argo CD is an empty success.
    fn read(&self, request: &ReadRequest<'_>) -> Result<Reading, ReadError> {
        super::read_argocd(self, &self.namespace, Proof::Anchors, request)
    }
}

impl KubeApiReader {
    fn list_error(&self, error: &kube::Error, plural: &str, namespace: &str) -> ReadError {
        let context = self.connection.context();
        let cluster = self.connection.cluster();
        match error {
            kube::Error::Api(status) if status.code == 401 => ReadError::new(
                ReadErrorKind::Rejected,
                format!(
                    "the Kubernetes API of context {context:?} rejected the kubeconfig's credentials (HTTP 401): \
                     they are invalid or have expired; log in again or refresh the admitted kubeconfig"
                ),
            ),
            kube::Error::Api(status) if status.code == 403 => ReadError::new(
                ReadErrorKind::Forbidden,
                format!(
                    "context {context:?} may not list {plural}.argoproj.io in namespace {namespace} (HTTP 403): \
                     grant its user `list` on {plural} there"
                ),
            ),
            kube::Error::Api(status) if status.code == 404 => ReadError::new(
                ReadErrorKind::Declaration,
                format!(
                    "context {context:?} serves no {plural}.argoproj.io: it is not the Argo CD control-plane Cluster \
                     {cluster}; fix Cluster {cluster}'s spec.live.context"
                ),
            ),
            other => ReadError::new(
                ReadErrorKind::Unreachable,
                format!("cannot list Applications through context {context:?}: {other}"),
            ),
        }
    }
}
