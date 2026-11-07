//! The infrastructure a variant runs on, set up once and shared by every step:
//!
//! - a kind cluster created from a pinned node image;
//! - a registry container on the kind network that the host pushes to and the
//!   nodes pull from;
//! - a Git server on the kind network that the host pushes to and Argo CD
//!   reads from;
//! - Argo CD from a pinned manifest, for the variants that use it;
//! - a kubeconfig whose context matches the project's
//!   `Cluster.spec.live.context`.
//!
//! Everything a variant creates is named after it, so `up` and `down` find
//! their own leftovers without a state file, and a state file lets `run` reuse
//! what `setup` created.

pub mod argocd;
pub mod git_server;
pub mod kind;
pub mod registry;

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::cmd::Cmd;
use crate::docker::{docker, free_port, is_running, remove};
use crate::error::{Context, Result};
use crate::kubectl::Kubectl;
use crate::log;
use crate::variant::Variant;
use crate::wait::{Poll, Waiter};

/// The Docker network kind creates for its nodes.
pub const KIND_NETWORK: &str = "kind";

/// The kubeconfig context of the cluster, which the fixture's
/// `Cluster.spec.live.context` names.
pub const KUBE_CONTEXT: &str = "e2e-admin";

/// A container the suite runs: its published loopback port and its address on
/// the kind network.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Endpoint {
    pub container: String,
    pub host_port: u16,
    pub ip: String,
}

/// What `up` created, saved to `infra.json` in the variant's directory.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Infra {
    pub variant: Variant,
    /// The directory of the variant's files: kubeconfig, checkout, logs.
    pub directory: PathBuf,
    pub kubeconfig: PathBuf,
    /// The API server URL of the kubeconfig's cluster, as the host reaches it.
    pub api_server: String,
    pub registry: Endpoint,
    pub git: Endpoint,
    /// The host's loopback port of Argo CD's API server; `None` without Argo CD.
    pub argocd_port: Option<u16>,
}

fn cluster_name(variant: Variant) -> String {
    variant.resource_prefix()
}

fn registry_container(variant: Variant) -> String {
    format!("{}-registry", variant.resource_prefix())
}

fn git_container(variant: Variant) -> String {
    format!("{}-git", variant.resource_prefix())
}

fn state_file(directory: &Path) -> PathBuf {
    directory.join("infra.json")
}

impl Infra {
    /// The registry as the host's Docker and the nodes' containerd name it.
    pub fn registry_host(&self) -> String {
        format!("localhost:{}", self.registry.host_port)
    }

    /// Where Nyl pushes to, from the host.
    pub fn git_publish_url(&self) -> String {
        git_server::publish_url(self.git.host_port)
    }

    /// Where Argo CD reads from, inside the cluster.
    pub fn git_repo_url(&self) -> String {
        git_server::cluster_url(&self.git.ip)
    }

    /// The Argo CD API server as the host reaches it.
    pub fn argocd_url(&self) -> Option<String> {
        self.argocd_port.map(argocd::api_url)
    }

    pub fn kubectl(&self) -> Kubectl {
        Kubectl::new(self.kubeconfig.clone(), KUBE_CONTEXT.to_owned())
    }

    /// Read the state `up` saved in `directory`.
    pub fn load(directory: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(state_file(directory)).ok()?;
        serde_json::from_str(&text).ok()
    }

    fn save(&self) -> Result<()> {
        let text = serde_json::to_string_pretty(self).context("serialize infra.json")?;
        std::fs::write(state_file(&self.directory), text).context("write infra.json")
    }

    /// Whether everything the saved state names still exists and runs.
    fn alive(&self) -> bool {
        kind::exists(&cluster_name(self.variant)).unwrap_or(false)
            && is_running(&self.registry.container)
            && is_running(&self.git.container)
    }

    /// Create the infrastructure of `variant` in `directory`, or reuse what an
    /// earlier `setup` left there. The flag says whether it was reused, in
    /// which case the scenario's own leftovers need a [`reset`](Self::reset).
    pub fn up(variant: Variant, directory: &Path) -> Result<(Self, bool)> {
        std::fs::create_dir_all(directory).context(format!("create {}", directory.display()))?;
        if let Some(existing) = Self::load(directory) {
            if existing.alive() {
                log::line(format!(
                    "reusing the {} infrastructure from {}",
                    variant.name(),
                    directory.display()
                ));
                return Ok((existing, true));
            }
            log::line("the saved infrastructure is gone or stopped; creating it again");
        }
        Self::down(variant, directory);
        let infra = Self::create(variant, directory)?;
        infra.save()?;
        Ok((infra, false))
    }

    fn create(variant: Variant, directory: &Path) -> Result<Self> {
        let name = cluster_name(variant);
        let kubeconfig = directory.join("kubeconfig");
        let argocd_port = variant.needs_argocd().then(free_port).transpose()?;

        let _group = log::Group::open(&format!("Create the kind cluster {name}"));
        let config_file = directory.join("kind-config.yaml");
        std::fs::write(&config_file, kind::config(argocd_port)).context("write the kind configuration")?;
        kind::create(&name, &config_file, &kubeconfig)?;
        let kube_name = format!("kind-{name}");
        Cmd::new("kubectl")
            .arg("--kubeconfig")
            .arg(&kubeconfig)
            .args(["config", "rename-context", &kube_name, KUBE_CONTEXT])
            .run_ok()?;
        let kubectl = Kubectl::new(kubeconfig.clone(), KUBE_CONTEXT.to_owned());
        let api_server = kubectl
            .run(&[
                "config",
                "view",
                "--minify",
                "--output",
                "jsonpath={.clusters[0].cluster.server}",
            ])?
            .stdout_trimmed()
            .to_owned();

        log::line("starting the registry and the Git server on the kind network");
        let registry = registry::start(&registry_container(variant), KIND_NETWORK)?;
        registry::configure_nodes(&kind::nodes(&name)?, &registry)?;
        let git = git_server::start(&git_container(variant), KIND_NETWORK)?;

        if let Some(port) = argocd_port {
            let _group = log::Group::open(&format!("Install Argo CD {}", argocd::VERSION));
            argocd::install(&kubectl, directory, port)?;
        }
        Ok(Self {
            variant,
            directory: directory.to_owned(),
            kubeconfig,
            api_server,
            registry,
            git,
            argocd_port,
        })
    }

    /// Remove everything `up` creates for `variant`; what is absent is fine.
    pub fn down(variant: Variant, directory: &Path) {
        let _ = std::fs::remove_file(state_file(directory));
        remove(&registry_container(variant));
        remove(&git_container(variant));
        kind::delete(&cluster_name(variant));
    }

    /// Clear what an earlier scenario left in reused infrastructure: the
    /// repository, Argo CD's Applications and Projects, and the workload's
    /// Namespace.
    pub fn reset(&self) -> Result<()> {
        log::line("resetting the scenario's leftovers in the reused infrastructure");
        git_server::reset_repository(&self.git.container)?;
        let kubectl = self.kubectl();
        if self.argocd_port.is_some() {
            for kind in ["applications.argoproj.io", "appprojects.argoproj.io"] {
                let names = kubectl.run(&[
                    "get",
                    kind,
                    "--namespace",
                    crate::fixture::ARGOCD_NAMESPACE,
                    "--output",
                    "jsonpath={.items[*].metadata.name}",
                ])?;
                for name in names.stdout.split_whitespace().filter(|name| *name != "default") {
                    // A finalizer would wait for a controller that may be gone.
                    let _ = kubectl.run(&[
                        "patch",
                        kind,
                        name,
                        "--namespace",
                        crate::fixture::ARGOCD_NAMESPACE,
                        "--type",
                        "merge",
                        "--patch",
                        r#"{"metadata":{"finalizers":null}}"#,
                    ]);
                    kubectl.run(&[
                        "delete",
                        kind,
                        name,
                        "--namespace",
                        crate::fixture::ARGOCD_NAMESPACE,
                        "--ignore-not-found",
                        "--wait=false",
                    ])?;
                }
            }
        }
        kubectl
            .cmd(&[
                "delete",
                "namespace",
                crate::fixture::NAMESPACE,
                "--ignore-not-found",
                "--timeout=180s",
            ])
            .timeout(Duration::from_secs(200))
            .run_ok()?;
        Ok(())
    }

    /// Wait until the nodes are Ready, for a cluster that was just reused.
    pub fn wait_nodes_ready(&self) -> Result<()> {
        let kubectl = self.kubectl();
        Waiter::new("the kind nodes to be Ready", Duration::from_secs(120)).until(|| {
            let output = kubectl.cmd(&["get", "nodes", "--no-headers"]).run()?;
            let ready = output.success()
                && !output.stdout.trim().is_empty()
                && output
                    .stdout
                    .lines()
                    .all(|line| line.split_whitespace().nth(1) == Some("Ready"));
            Ok(if ready {
                Poll::Ready(())
            } else {
                Poll::pending(output.stdout.trim().to_owned())
            })
        })
    }

    /// `docker ps` of the suite's containers, for diagnostics.
    pub fn containers(&self) -> Result<String> {
        Ok(docker().args(["ps", "--all", "--filter", "name=nyl-e2e"]).run()?.stdout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn infra() -> Infra {
        Infra {
            variant: Variant::ArgocdApi,
            directory: PathBuf::from("/work/argocd-api"),
            kubeconfig: PathBuf::from("/work/argocd-api/kubeconfig"),
            api_server: "https://127.0.0.1:40001".to_owned(),
            registry: Endpoint {
                container: "nyl-e2e-argocd-api-registry".to_owned(),
                host_port: 40002,
                ip: "172.18.0.3".to_owned(),
            },
            git: Endpoint {
                container: "nyl-e2e-argocd-api-git".to_owned(),
                host_port: 40003,
                ip: "172.18.0.4".to_owned(),
            },
            argocd_port: Some(40004),
        }
    }

    #[test]
    fn test_the_host_and_the_cluster_get_their_own_addresses_for_each_service() {
        let infra = infra();
        assert_eq!(infra.registry_host(), "localhost:40002");
        assert_eq!(infra.git_publish_url(), "http://127.0.0.1:40003/git/shop.git");
        assert_eq!(infra.git_repo_url(), "http://172.18.0.4:8080/git/shop.git");
        assert_eq!(infra.argocd_url().as_deref(), Some("http://127.0.0.1:40004"));
    }

    #[test]
    fn test_saved_state_round_trips_and_a_missing_file_is_no_state() {
        let directory = tempfile::tempdir().unwrap();
        assert!(Infra::load(directory.path()).is_none());
        let mut infra = infra();
        infra.directory = directory.path().to_owned();
        infra.save().unwrap();
        let loaded = Infra::load(directory.path()).unwrap();
        assert_eq!(loaded.registry, infra.registry);
        assert_eq!(loaded.git, infra.git);
        assert_eq!(loaded.variant, Variant::ArgocdApi);
        assert_eq!(loaded.argocd_port, Some(40004));
    }

    #[test]
    fn test_each_variant_names_its_own_cluster_and_containers() {
        let names = Variant::ALL
            .iter()
            .flat_map(|variant| {
                [
                    cluster_name(*variant),
                    registry_container(*variant),
                    git_container(*variant),
                ]
            })
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(names.len(), 9);
        assert!(names.iter().all(|name| name.starts_with("nyl-e2e-")));
    }
}
