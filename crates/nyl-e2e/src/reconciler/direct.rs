//! The `direct` variant: Nyl's own reconciler applies the published tree
//! through the mechanics of `nyl apply`, with no Argo CD in the cluster.
//!
//! The DeploymentTarget declares `reconciler: {direct: {}}`, with no
//! Argo CD options and no catalog fields, and deploys to a Cluster of its own whose
//! `destination.server` is the kind API server the kubeconfig names (Nyl
//! verifies that the context's server is the declared one) and whose
//! `live.context` is the kubeconfig context `e2e-admin`. The fixture's `dev`
//! Cluster, which the Argo CD variants use with the in-cluster alias, stays
//! unused. The publication unit admits only `KUBECONFIG`.
//!
//! `nyl reconcile` publishes the tree, applies every Release from the pushed
//! bytes, and, with `observe.waitHealthy`, reads the release state and the
//! rollout status until the workloads are healthy, so no operator is needed. A
//! teardown deletes the Releases' objects, waits (`observe`) until none remain,
//! and then deletes the release state and the Namespace (`deletePolicy:
//! Automatic`, which the fixture sets with `prunePolicy`).

use std::collections::BTreeMap;
use std::sync::atomic::AtomicBool;

use super::Reconciler;
use crate::cmd::Cmd;
use crate::env::Env;
use crate::error::Result;
use crate::fixture::{cluster_document, Declarations};
use crate::infra::kind::KUBE_VERSION;
use crate::infra::{Infra, KUBE_CONTEXT};
use crate::variant::Variant;

/// The Cluster resource the direct target deploys to.
pub const CLUSTER: &str = "dev-direct";

pub struct Direct;

impl Reconciler for Direct {
    fn variant(&self) -> Variant {
        Variant::Direct
    }

    fn declarations(&self, infra: &Infra) -> Result<Declarations> {
        Ok(declarations(&infra.api_server))
    }

    fn credentials(&self, _infra: &Infra) -> Result<BTreeMap<String, String>> {
        Ok(BTreeMap::new())
    }

    fn needs_operator(&self) -> bool {
        false
    }

    fn published_files(&self) -> Vec<String> {
        // A direct target generates no Application, AppProject, or catalog.
        Vec::new()
    }

    fn deploy(&self, _env: &Env, _stop: &AtomicBool) -> Result<()> {
        Ok(())
    }

    fn after_teardown(&self, _env: &Env) -> Result<()> {
        Ok(())
    }

    fn diagnostics(&self, infra: &Infra) -> Vec<Cmd> {
        // The release state Nyl keeps in the cluster, with the workload's Namespace.
        vec![infra
            .kubectl()
            .cmd(&["get", "secrets", "--all-namespaces", "--selector", "k8s.nyl/release"])]
    }
}

/// The direct reconciler's part of the project, for the kind API server `api_server`.
fn declarations(api_server: &str) -> Declarations {
    Declarations {
        instance_documents: cluster_document(CLUSTER, api_server, KUBE_CONTEXT, KUBE_VERSION),
        target_cluster: CLUSTER.to_owned(),
        target_fields: "reconciler:\n  direct: {}".to_owned(),
        publication_env: vec!["KUBECONFIG".to_owned()],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::{render, Params};

    fn platform() -> String {
        let params = Params {
            registry_host: "localhost:5001".to_owned(),
            repo_url: "http://172.18.0.4:8080/git/shop.git".to_owned(),
            publish_url: "http://127.0.0.1:40123/git/shop.git".to_owned(),
            state_dir: "/work/tofu".to_owned(),
            kube_context: KUBE_CONTEXT.to_owned(),
            kube_version: KUBE_VERSION.to_owned(),
            revision: 1,
            docker_env: Vec::new(),
        };
        let files = render(&params, &declarations("https://127.0.0.1:43210")).unwrap();
        files
            .into_iter()
            .find(|file| file.path == "nyl/platform.yaml")
            .unwrap()
            .content
    }

    #[test]
    fn test_the_target_chooses_the_direct_reconciler_with_no_argocd_fields() {
        let platform = platform();
        assert!(
            platform.contains(
                "\nspec:\n  clusterRef: {name: dev-direct}\n  reconciler:\n    direct: {}\n  applicationGroupSelector:"
            ),
            "{platform}"
        );
        assert!(!platform.contains("argocd:"), "{platform}");
        assert!(!platform.contains("ArgoCDInstance"), "{platform}");
        assert!(!platform.contains("catalogApplication"), "{platform}");
    }

    #[test]
    fn test_the_direct_cluster_names_the_kind_api_server_and_the_suites_context() {
        let platform = platform();
        assert!(
            platform.contains(
                "kind: Cluster\nmetadata:\n  name: dev-direct\nspec:\n  destination:\n    server: https://127.0.0.1:43210\n  live: {context: e2e-admin}\n  kubernetes:\n    kubeVersion: 1.36.4\n"
            ),
            "{platform}"
        );
        // The documents stay separate: the Cluster ends before the target starts.
        assert!(platform.contains(
            "  clusterScopedKinds: [core/Namespace]\n---\napiVersion: k8s.gitops.nyl/v1\nkind: DeploymentTarget"
        ));
    }

    #[test]
    fn test_the_publication_unit_admits_only_the_kubeconfig() {
        assert_eq!(declarations("https://127.0.0.1:1").publication_env, vec!["KUBECONFIG"]);
        assert!(!Direct.needs_operator());
        assert!(Direct.published_files().is_empty());
    }
}
