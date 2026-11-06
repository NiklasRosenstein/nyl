//! Reading one live resource from a target's workload Cluster.
//!
//! Contract: [Kubernetes publication unit](../../../../design/orchestration-core.md#kubernetes-publication-unit),
//! observed values. The connection is the same explicit one the Argo CD
//! reader makes ([`Connection`]): the kubeconfig the unit admits as
//! `KUBECONFIG` and the workload Cluster's declared `spec.live.context`,
//! never the current context, `~/.kube/config`, or in-cluster configuration.
//! The workload Cluster may differ from the Argo CD control-plane Cluster, so
//! its context is its own; one kubeconfig may hold both.

use std::time::Duration;

use kube::api::DynamicObject;
use kube::core::GroupVersionKind;
use kube::discovery::{pinned_kind, Scope};
use kube::Api;
use nyl_render::gitops::observation::ResourceRef;
use serde_json::Value;

use super::argocd::Connection;
use super::reconciler::{block_on, ReadError, ReadErrorKind};

/// Reads resources of one Cluster.
pub trait ResourceReader {
    /// The resource as the Kubernetes API serves it, `None` when it does not exist.
    fn get(&self, resource: &ResourceRef) -> Result<Option<Value>, ReadError>;
}

/// Reads resources through a kubeconfig context.
pub struct KubeResourceReader {
    connection: Connection,
}

impl KubeResourceReader {
    /// A reader of Cluster `cluster` through `context` (its
    /// `spec.live.context`) and the files of `kubeconfig` (the admitted
    /// `KUBECONFIG`). Each reading ends within `budget`.
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
                ": observed values read the workload Cluster's resources through it",
            )?,
        })
    }

    fn error(&self, error: &kube::Error, resource: &ResourceRef) -> ReadError {
        let context = self.connection.context();
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
                    "context {context:?} may not get {resource} (HTTP 403): grant its user `get` on {} in the \
                     workload Cluster {}",
                    resource.kind,
                    self.connection.cluster()
                ),
            ),
            other => ReadError::new(
                ReadErrorKind::Unreachable,
                format!("cannot read {resource} through context {context:?}: {other}"),
            ),
        }
    }
}

/// The group, version, and kind a resource's `apiVersion` and `kind` name.
fn gvk(resource: &ResourceRef) -> GroupVersionKind {
    let (group, version) = resource
        .api_version
        .split_once('/')
        .unwrap_or(("", resource.api_version.as_str()));
    GroupVersionKind::gvk(group, version, &resource.kind)
}

impl ResourceReader for KubeResourceReader {
    fn get(&self, resource: &ResourceRef) -> Result<Option<Value>, ReadError> {
        let budget = self.connection.budget;
        block_on(async {
            tokio::time::timeout(budget, async {
                let client = self.connection.client().await?;
                // Discovery names the plural and the scope, so a declaration
                // needs neither; a kind the Cluster does not serve has no
                // resource to read.
                let (api_resource, capabilities) = match pinned_kind(&client, &gvk(resource)).await {
                    Ok(found) => found,
                    Err(kube::Error::Api(status)) if status.code == 404 => return Ok(None),
                    Err(kube::Error::Discovery(_)) => return Ok(None),
                    Err(error) => return Err(self.error(&error, resource)),
                };
                let api: Api<DynamicObject> = match (&resource.namespace, capabilities.scope) {
                    (Some(namespace), Scope::Namespaced) => Api::namespaced_with(client, namespace, &api_resource),
                    (None, Scope::Cluster) => Api::all_with(client, &api_resource),
                    (Some(_), Scope::Cluster) => {
                        return Err(ReadError::new(
                            ReadErrorKind::Declaration,
                            format!(
                                "{resource} names a namespace, but {} is cluster-scoped: remove the namespace from the observation's resource",
                                resource.kind
                            ),
                        ))
                    }
                    (None, Scope::Namespaced) => {
                        return Err(ReadError::new(
                            ReadErrorKind::Declaration,
                            format!(
                                "{resource} names no namespace, but {} is namespaced: add the namespace to the observation's resource",
                                resource.kind
                            ),
                        ))
                    }
                };
                let object = api
                    .get_opt(&resource.name)
                    .await
                    .map_err(|error| self.error(&error, resource))?;
                object
                    .map(|object| {
                        serde_json::to_value(&object)
                            .map_err(|error| ReadError::new(ReadErrorKind::Malformed, error.to_string()))
                    })
                    .transpose()
            })
            .await
            .unwrap_or_else(|_| {
                Err(ReadError::new(
                    ReadErrorKind::Unreachable,
                    format!(
                        "reading {resource} through context {:?} took longer than {}s",
                        self.connection.context(),
                        budget.as_secs()
                    ),
                ))
            })
        })?
    }
}
