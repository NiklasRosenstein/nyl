//! Reading Argo CD: the one driver-owned seam between the publication
//! driver and the Applications it observes, with two implementations.
//!
//! Contract: [Kubernetes publications](../../../../../design/promotion.md#kubernetes-publications)
//! (observer, access methods) and [Credentials](../../../../../design/orchestration-core.md#credentials).
//!
//! An [`ArgoReader`] answers one question: which Applications exist in a
//! namespace, as Argo CD serves them. Everything after that, the running
//! revision, the match to a publication, health, and the attestations, is
//! shared and lives in [`live`] and [`evaluate`], so it is written and tested
//! once whichever way Argo CD is read:
//!
//! - [`KubeApiReader`] lists `applications.argoproj.io` objects from the
//!   Kubernetes API of the instance's control-plane Cluster, through the
//!   kubeconfig the unit admits as `KUBECONFIG` and the Cluster's declared
//!   `spec.live.context`.
//! - [`ApiServerReader`] lists them from the Argo CD API server the
//!   instance declares, with the token the unit admits (named by the
//!   declaration's `tokenEnv`).
//!
//! Which one, and the API server's address, come from the ArgoCDInstance in
//! Git. Credentials come only from the unit's admitted environment: a
//! missing one is a [`ReadErrorKind::Credentials`] error that names the
//! variable and the `env` entry that admits it, never a fallback to the
//! process environment, the current kube context, or `~/.kube/config`.
//!
//! Each reading runs on a thread with a runtime of its own, so a driver
//! calls it from synchronous code and from inside an asynchronous runtime
//! alike, and ends by its deadline.

use std::collections::BTreeMap;
use std::time::Duration;

use nyl_render::gitops::observation::ArgoCdObservation;
use nyl_render::resources::ArgoCDAccessMethod;
use serde_json::Value;

mod api_server;
pub mod evaluate;
mod kube_api;
pub mod live;

pub use api_server::ApiServerReader;
pub use kube_api::KubeApiReader;

/// Why Argo CD could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadErrorKind {
    /// A credential the declaration needs is not admitted to the unit.
    Credentials,
    /// The declaration cannot be read as it stands, such as a Cluster
    /// without the local context the Kubernetes method needs.
    Declaration,
    /// Argo CD or its API did not answer: connection, timeout, server error.
    Unreachable,
    /// The credential was rejected (HTTP 401): invalid, or expired.
    Rejected,
    /// The credential is accepted but may not read Applications (HTTP 403).
    Forbidden,
    /// The answer is not the Application list it should be.
    Malformed,
}

/// A failed reading, with the fix attached to the message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadError {
    pub kind: ReadErrorKind,
    pub message: String,
}

impl ReadError {
    pub fn new(kind: ReadErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ReadError {}

/// Reads the Applications of one Argo CD instance.
pub trait ArgoReader {
    /// The Applications in `namespace` by name, each as Argo CD serves it.
    /// An Application the credential may not read is absent, as Argo CD's own
    /// API answers; a namespace without any is an empty map.
    fn applications(&self, namespace: &str) -> Result<BTreeMap<String, Value>, ReadError>;

    /// Whether the credential may `get` the Application Argo CD's policy
    /// names `rbac_name` (`<project>/<name>`, or `<project>/<namespace>/<name>`
    /// outside the instance's own namespace). A listing silently leaves out
    /// what the credential may not get and still answers success, so an
    /// Application's absence from it proves nothing until this says the
    /// credential could have seen it. A reader whose listing fails instead of
    /// filtering (the Kubernetes API, whose RBAC denies the whole list) has
    /// nothing to add.
    fn can_get(&self, rbac_name: &str) -> Result<bool, ReadError> {
        let _ = rbac_name;
        Ok(true)
    }
}

/// The name Argo CD's policy and its `can-i` endpoint know an Application by:
/// `<project>/<name>` in the instance's own namespace, else
/// `<project>/<namespace>/<name>`.
pub fn rbac_name(project: &str, namespace: &str, name: &str, default_namespace: &str) -> String {
    if namespace.is_empty() || namespace == default_namespace {
        format!("{project}/{name}")
    } else {
        format!("{project}/{namespace}/{name}")
    }
}

/// The reader the declaration asks for, built from the unit's admitted
/// environment, finishing each reading within `budget`.
pub fn reader(
    observation: &ArgoCdObservation,
    env: &BTreeMap<String, String>,
    budget: Duration,
) -> Result<Box<dyn ArgoReader>, ReadError> {
    let access = &observation.location.access;
    match access.method {
        ArgoCDAccessMethod::Kubernetes => Ok(Box::new(KubeApiReader::new(
            &observation.location.cluster,
            observation.location.context.as_deref(),
            env.get(KUBECONFIG_ENV).map(String::as_str),
            budget,
        )?)),
        ArgoCDAccessMethod::Api => {
            let api = access.api.as_ref().ok_or_else(|| {
                ReadError::new(
                    ReadErrorKind::Declaration,
                    format!(
                        "ArgoCDInstance {} declares access method api without spec.access.api",
                        observation.location.instance
                    ),
                )
            })?;
            Ok(Box::new(ApiServerReader::new(
                &api.server,
                &api.token_env,
                env.get(&api.token_env).map(String::as_str),
                env.get("HTTPS_PROXY")
                    .or_else(|| env.get("https_proxy"))
                    .map(String::as_str),
                budget,
            )?))
        }
    }
}

/// The admitted variable naming the kubeconfig file(s) the Kubernetes method reads.
pub const KUBECONFIG_ENV: &str = "KUBECONFIG";

/// Run `future` on a thread with a current-thread runtime of its own.
fn block_on<T: Send>(future: impl std::future::Future<Output = T> + Send) -> Result<T, ReadError> {
    // Both clients build a TLS configuration, which needs a process-level
    // crypto provider; the binary installs one at start, and a library caller
    // may not have. Installing is idempotent: it fails only when one exists.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map(|runtime| runtime.block_on(future))
                    .map_err(|error| {
                        ReadError::new(
                            ReadErrorKind::Unreachable,
                            format!("cannot start a runtime to read Argo CD: {error}"),
                        )
                    })
            })
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
    })
}
