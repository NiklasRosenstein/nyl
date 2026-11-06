//! The `direct` variant: Nyl's own reconciler applies the published tree
//! through the mechanics of `nyl apply`, with no Argo CD in the cluster.
//!
//! PENDING. The reconciler is designed and implemented on the
//! `m5/10-direct-reconciler` branch, and its declaration syntax is not fixed
//! yet, so this variant fails at once with a pending error instead of guessing.
//! Wiring it takes these changes, all in this file (the scenario, the fixture,
//! and the infrastructure need none):
//!
//! 1. [`Direct::declarations`]: return what the target and its Cluster declare
//!    in m5/10's syntax:
//!    - `instance_documents`: a Cluster resource of its own for the direct
//!      target (not the fixture's `dev` Cluster, which Argo CD's targets use
//!      with the in-cluster alias `https://kubernetes.default.svc`). It names
//!      the same kind cluster by a server URL Nyl verifies against the
//!      kubeconfig context: `spec.destination.server: <infra.api_server>` and
//!      `spec.live.context: e2e-admin` ([`crate::infra::KUBE_CONTEXT`]). Any
//!      resource the reconciler needs besides, such as an instance, goes here too.
//!    - `target_cluster`: that Cluster's name.
//!    - `target_fields`: the DeploymentTarget lines that choose the direct
//!      reconciler instead of `argocdRef`.
//!    - `publication_env`: what the publication unit admits to apply and read:
//!      `["KUBECONFIG"]` unless m5/10 reads something else.
//! 2. [`Direct::credentials`]: none, unless the declaration names a variable.
//! 3. [`Direct::deploy`]: nothing to do if the reconcile itself applies the
//!    tree; otherwise run what m5/10 requires after a publication.
//! 4. [`Direct::after_teardown`]: the teardown's removal is the reconciler's
//!    own; check what m5/10 promises (the generic checks already assert that
//!    the Namespace, the Deployment, and the Service are gone, and that the
//!    Namespace's `deletePolicy: Automatic` let the teardown remove it).
//!
//! The behaviours the scenario then asserts for it, as for Argo CD: one
//! `nyl reconcile` publishes and applies; the workload becomes ready with the
//! image digest and the OpenTofu output; `nyl attest` reports `accepted` and
//! `healthy` (the scenario polls it, because a reconcile attests nothing);
//! `nyl observe` reads the live values; a changed input rolls out; and a
//! teardown leaves the cluster empty of the workload and the state held.
//! If m5/10's reconciler attests differently, the one place to adapt is
//! `scenario::attest`.

use std::collections::BTreeMap;

use super::Reconciler;
use crate::cmd::Cmd;
use crate::env::Env;
use crate::error::{Error, Result};
use crate::fixture::Declarations;
use crate::infra::Infra;
use crate::variant::Variant;

pub struct Direct;

fn pending() -> Error {
    Error::pending(
        "the direct variant is pending: it waits for the direct reconciler of m5/10-direct-reconciler; \
         see crates/nyl-e2e/src/reconciler/direct.rs for what wiring it takes",
    )
}

impl Reconciler for Direct {
    fn variant(&self) -> Variant {
        Variant::Direct
    }

    fn availability(&self) -> Result<()> {
        Err(pending())
    }

    fn declarations(&self, _infra: &Infra) -> Result<Declarations> {
        Err(pending())
    }

    fn credentials(&self, _infra: &Infra) -> Result<BTreeMap<String, String>> {
        Err(pending())
    }

    fn deploy(&self, _env: &Env) -> Result<()> {
        Err(pending())
    }

    fn after_teardown(&self, _env: &Env) -> Result<()> {
        Err(pending())
    }

    fn diagnostics(&self, _infra: &Infra) -> Vec<Cmd> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_the_direct_variant_is_reported_as_pending_not_as_a_failure() {
        let error = pending();
        assert!(error.is_pending());
        assert!(error.to_string().contains("m5/10-direct-reconciler"));
    }
}
