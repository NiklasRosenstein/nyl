//! Argo CD as the reconciler, read two ways: through the Kubernetes API of its
//! control-plane Cluster (`kubernetes`, credential `KUBECONFIG`) or through its
//! API server (`api`, credential `ARGOCD_AUTH_TOKEN`). The ArgoCDInstance
//! declares which, once, for the whole project.
//!
//! Argo CD does not run a published tree by itself here: the catalog
//! Application is applied once by an operator (the guide's "apply the
//! catalogs once"), which then creates the workload Application, whose sync is
//! manual by default. The publication unit waits for health inside
//! `nyl reconcile` (`observe.waitHealthy`), so [`ArgoCd::deploy`] does both
//! while the reconcile runs, and returns once the Application is Synced to the
//! published commit and Healthy.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use super::Reconciler;
use crate::checks::{application_converged, needs_sync};
use crate::cmd::Cmd;
use crate::env::Env;
use crate::error::{Context, Error, Result};
use crate::fixture::{
    Declarations, ARGOCD_NAMESPACE, CATALOG_APPLICATION, CATALOG_MANIFEST, PUBLICATION_BRANCH, WORKLOAD_APPLICATION,
};
use crate::infra::{argocd as install, Infra};
use crate::kubectl::Kubectl;
use crate::variant::Variant;
use crate::wait::{Poll, Waiter};

/// The variable the ArgoCDInstance names for the API token (its default).
pub const TOKEN_ENV: &str = "ARGOCD_AUTH_TOKEN";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    Kubernetes,
    Api,
}

pub struct ArgoCd {
    access: Access,
}

impl ArgoCd {
    pub fn new(access: Access) -> Self {
        Self { access }
    }
}

/// The ArgoCDInstance document: Applications are listed in the `argocd`
/// namespace of the cluster, and the catalog prunes removed Applications
/// (which a teardown needs) while syncing itself.
fn instance_document(access: Access, api_server: Option<&str>) -> String {
    let mut document = [
        "---",
        "apiVersion: k8s.gitops.nyl/v1",
        "kind: ArgoCDInstance",
        "metadata:",
        "  name: dev",
        "spec:",
        "  clusterRef: {name: dev}",
        "  namespace: argocd",
        "  catalogApplicationDefaults:",
        "    syncPolicy: {automated: {prune: true}}",
        "",
    ]
    .join("\n");
    if access == Access::Api {
        document.push_str(&format!(
            "  access:\n    method: api\n    api:\n      server: {}\n      tokenEnv: {TOKEN_ENV}\n",
            api_server.unwrap_or("http://127.0.0.1:0")
        ));
    }
    document
}

impl Reconciler for ArgoCd {
    fn variant(&self) -> Variant {
        match self.access {
            Access::Kubernetes => Variant::ArgocdKubernetes,
            Access::Api => Variant::ArgocdApi,
        }
    }

    fn declarations(&self, infra: &Infra) -> Result<Declarations> {
        let api_server = infra.argocd_url();
        Ok(declarations(self.access, api_server.as_deref()))
    }

    fn credentials(&self, infra: &Infra) -> Result<BTreeMap<String, String>> {
        match self.access {
            Access::Kubernetes => Ok(BTreeMap::new()),
            Access::Api => {
                let port = infra
                    .argocd_port
                    .context("the api variant needs Argo CD's published port")?;
                let token = install::mint_token(&infra.kubectl(), port)?;
                Ok(BTreeMap::from([(TOKEN_ENV.to_owned(), token)]))
            }
        }
    }

    fn needs_operator(&self) -> bool {
        true
    }

    fn published_files(&self) -> Vec<String> {
        vec![CATALOG_MANIFEST.to_owned()]
    }

    fn deploy(&self, env: &Env, stop: &AtomicBool) -> Result<()> {
        env.project.fetch()?;
        let catalog = env
            .project
            .show(PUBLICATION_BRANCH, CATALOG_MANIFEST)
            .map_err(|error| {
                error.context(format!(
                    "read the catalog Application {CATALOG_MANIFEST} from the {PUBLICATION_BRANCH} branch"
                ))
            })?;
        env.kubectl.apply(&catalog, false)?;
        let head = env
            .project
            .remote_head(PUBLICATION_BRANCH)?
            .context(format!("the {PUBLICATION_BRANCH} branch does not exist on the server"))?;
        converge(&env.kubectl, WORKLOAD_APPLICATION, &head, stop)
    }

    fn after_teardown(&self, env: &Env) -> Result<()> {
        Waiter::new(
            format!("Argo CD to delete the Application {WORKLOAD_APPLICATION}"),
            Duration::from_secs(300),
        )
        .until(|| {
            let found = env.kubectl.get_optional(&[
                "applications.argoproj.io",
                WORKLOAD_APPLICATION,
                "--namespace",
                ARGOCD_NAMESPACE,
            ])?;
            Ok(match found {
                None => Poll::Ready(()),
                Some(_) => Poll::pending("the Application still exists"),
            })
        })
    }

    fn diagnostics(&self, infra: &Infra) -> Vec<Cmd> {
        let kubectl = infra.kubectl();
        let mut commands = vec![
            kubectl.cmd(&[
                "get",
                "applications.argoproj.io,appprojects.argoproj.io",
                "--all-namespaces",
                "--output",
                "wide",
            ]),
            kubectl.cmd(&[
                "get",
                "applications.argoproj.io",
                "--namespace",
                ARGOCD_NAMESPACE,
                "--output",
                "yaml",
            ]),
            kubectl.cmd(&["get", "pods", "--namespace", ARGOCD_NAMESPACE, "--output", "wide"]),
        ];
        for workload in [
            "deployment/argocd-repo-server",
            "statefulset/argocd-application-controller",
            "deployment/argocd-server",
        ] {
            commands.push(kubectl.cmd(&["logs", workload, "--namespace", ARGOCD_NAMESPACE, "--tail=150"]));
        }
        commands
    }
}

/// The reconciler's part of the project, for `api_server` (the API server's
/// URL as the host reaches it, for the `api` method).
fn declarations(access: Access, api_server: Option<&str>) -> Declarations {
    Declarations {
        instance_documents: instance_document(access, api_server),
        target_cluster: "dev".to_owned(),
        target_fields: "argocdRef: {name: dev}".to_owned(),
        publication_env: match access {
            // The kubeconfig reads the Applications, and the workload's live fields.
            Access::Kubernetes => vec!["KUBECONFIG".to_owned()],
            // The token reads the Applications; the kubeconfig still reads the live fields.
            Access::Api => vec![TOKEN_ENV.to_owned(), "KUBECONFIG".to_owned()],
        },
    }
}

/// Argo CD notices a push at its next poll, which the installation shortens;
/// the suite asks for a refresh as well so a test never waits for it.
fn refresh(kubectl: &Kubectl, application: &str) {
    let _ = kubectl
        .cmd(&[
            "annotate",
            "applications.argoproj.io",
            application,
            "--namespace",
            ARGOCD_NAMESPACE,
            "argocd.argoproj.io/refresh=hard",
            "--overwrite",
        ])
        .run();
}

/// Sync the Application to `revision` and prune what it no longer renders: a
/// manual sync is an `operation` on the Application object.
fn start_sync(kubectl: &Kubectl, application: &str, revision: &str) -> Result<()> {
    let patch = serde_json::json!({
        "operation": {
            "initiatedBy": {"username": "nyl-e2e"},
            "sync": {"revision": revision, "prune": true, "syncOptions": ["ServerSideApply=true"]},
        }
    })
    .to_string();
    kubectl.run(&[
        "patch",
        "applications.argoproj.io",
        application,
        "--namespace",
        ARGOCD_NAMESPACE,
        "--type",
        "merge",
        "--patch",
        &patch,
    ])?;
    Ok(())
}

/// Wait until the Application exists (the catalog creates it), is Synced to
/// `head`, and is Healthy, starting a sync whenever none is under way.
fn converge(kubectl: &Kubectl, application: &str, head: &str, stop: &AtomicBool) -> Result<()> {
    Waiter::new(
        format!("Application {application} to be Synced to {head} and Healthy"),
        Duration::from_secs(10 * 60),
    )
    .backoff(Duration::from_secs(2), Duration::from_secs(10))
    .until(|| {
        if stop.load(Ordering::SeqCst) {
            return Err(Error::new("the reconcile failed, so nothing waits for the Application"));
        }
        refresh(kubectl, CATALOG_APPLICATION);
        let Some(found) =
            kubectl.get_optional(&["applications.argoproj.io", application, "--namespace", ARGOCD_NAMESPACE])?
        else {
            return Ok(Poll::pending(format!(
                "the Application does not exist yet (the catalog {CATALOG_APPLICATION} has to sync first)"
            )));
        };
        refresh(kubectl, application);
        match application_converged(&found, head) {
            Poll::Ready(()) => Ok(Poll::Ready(())),
            Poll::Pending(reason) => {
                if needs_sync(&found, head) {
                    crate::log::line(format!("starting a sync of {application} to {head}"));
                    start_sync(kubectl, application, head)?;
                }
                Ok(Poll::Pending(reason))
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::{render, Params};

    fn params() -> Params {
        Params {
            registry_host: "localhost:5001".to_owned(),
            repo_url: "http://172.18.0.4:8080/git/shop.git".to_owned(),
            publish_url: "http://127.0.0.1:40123/git/shop.git".to_owned(),
            state_dir: "/work/tofu".to_owned(),
            kube_context: "e2e-admin".to_owned(),
            kube_version: "1.36.4".to_owned(),
            revision: 1,
            docker_env: Vec::new(),
        }
    }

    fn platform(access: Access) -> String {
        let declarations = declarations(access, Some("http://127.0.0.1:41000"));
        let files = render(&params(), &declarations).unwrap();
        files
            .into_iter()
            .find(|file| file.path == "nyl/platform.yaml")
            .unwrap()
            .content
    }

    fn units(access: Access) -> String {
        let declarations = declarations(access, Some("http://127.0.0.1:41000"));
        let files = render(&params(), &declarations).unwrap();
        files
            .into_iter()
            .find(|file| file.path == "nyl/units.yaml")
            .unwrap()
            .content
    }

    #[test]
    fn test_the_kubernetes_method_declares_no_access_and_admits_only_the_kubeconfig() {
        let platform = platform(Access::Kubernetes);
        assert!(platform.contains("kind: ArgoCDInstance"), "{platform}");
        assert!(
            platform.contains("  catalogApplicationDefaults:\n    syncPolicy: {automated: {prune: true}}\n---\n"),
            "{platform}"
        );
        assert!(!platform.contains("access:"), "{platform}");
        assert!(platform.contains("\n  argocdRef: {name: dev}\n"), "{platform}");
        assert!(units(Access::Kubernetes).contains("env: {passthrough: [\"KUBECONFIG\"]}"));
    }

    #[test]
    fn test_the_api_method_names_the_server_and_the_token_variable_the_unit_admits() {
        let platform = platform(Access::Api);
        assert!(
            platform.contains(
                "  access:\n    method: api\n    api:\n      server: http://127.0.0.1:41000\n      tokenEnv: ARGOCD_AUTH_TOKEN\n---\n"
            ),
            "{platform}"
        );
        // The token reads Argo CD; the kubeconfig still reads the live fields.
        assert!(units(Access::Api).contains("env: {passthrough: [\"ARGOCD_AUTH_TOKEN\",\"KUBECONFIG\"]}"));
    }

    #[test]
    fn test_both_methods_share_everything_but_the_instance_access_and_the_admitted_env() {
        let (kubernetes, api) = (platform(Access::Kubernetes), platform(Access::Api));
        let strip = |text: &str| {
            text.lines()
                .filter(|line| {
                    !line.contains("access:")
                        && !line.contains("method:")
                        && !line.contains("server: http://127")
                        && !line.trim_start().starts_with("api:")
                        && !line.contains("tokenEnv")
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        assert_eq!(strip(&kubernetes), strip(&api));
    }
}
