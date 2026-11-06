//! What differs between the variants. The scenario is one sequence of steps;
//! a [`Reconciler`] supplies the three things that are not shared: how the
//! DeploymentTarget declares it, how the cluster is made to run a published
//! tree, and what to check after a teardown.

mod argocd;
mod direct;

use std::collections::BTreeMap;

use crate::cmd::Cmd;
use crate::env::Env;
use crate::error::Result;
use crate::fixture::Declarations;
use crate::infra::Infra;
use crate::variant::Variant;

pub trait Reconciler {
    fn variant(&self) -> Variant;

    /// `Err` with a pending error while the variant is declared but not wired,
    /// checked before any infrastructure is created.
    fn availability(&self) -> Result<()> {
        Ok(())
    }

    /// The reconciler's part of the project: its control resources and the
    /// lines of the DeploymentTarget that choose it.
    fn declarations(&self, infra: &Infra) -> Result<Declarations>;

    /// The credentials `nyl` needs besides the kubeconfig, which the
    /// publication unit admits by name (see [`Declarations::publication_env`]).
    fn credentials(&self, infra: &Infra) -> Result<BTreeMap<String, String>>;

    /// After a reconcile published a tree: make the cluster run it, and return
    /// once the reconciler says it does. For Argo CD that is applying the
    /// catalog Application (a no-op the second time) and syncing the workload
    /// Application, because the catalog's default sync is manual.
    fn deploy(&self, env: &Env) -> Result<()>;

    /// After a teardown completed: what the reconciler must have removed.
    fn after_teardown(&self, env: &Env) -> Result<()>;

    /// Commands whose output explains a failure, beyond the generic ones.
    fn diagnostics(&self, infra: &Infra) -> Vec<Cmd>;
}

/// The reconciler of `variant`.
pub fn reconciler_for(variant: Variant) -> Box<dyn Reconciler> {
    match variant {
        Variant::Direct => Box::new(direct::Direct),
        Variant::ArgocdKubernetes => Box::new(argocd::ArgoCd::new(argocd::Access::Kubernetes)),
        Variant::ArgocdApi => Box::new(argocd::ArgoCd::new(argocd::Access::Api)),
    }
}
