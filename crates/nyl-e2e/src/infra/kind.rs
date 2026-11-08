//! The kind cluster: a pinned node image, containerd set up to read a
//! per-registry `hosts.toml` (the standard kind local-registry pattern), and,
//! for Argo CD, one NodePort published on the host's loopback.

use std::path::Path;
use std::time::Duration;

use crate::cmd::Cmd;
use crate::error::Result;

/// The node image, pinned by digest (the multi-architecture index of
/// `kindest/node:v1.36.4`), so every run gets the same Kubernetes.
pub const NODE_IMAGE: &str =
    "kindest/node:v1.36.4@sha256:099e049362a1526b2db71494e1947aae99bd16290d7c895f2b7ea312e3cbfaed";

/// The Kubernetes version of [`NODE_IMAGE`], for `Cluster.spec.kubernetes`.
pub const KUBE_VERSION: &str = "1.36.4";

/// The node port Argo CD's API server is published on, and the one kind maps
/// to the host's loopback.
pub const ARGOCD_NODE_PORT: u16 = 30080;

/// The kind configuration: registry configuration read from
/// `/etc/containerd/certs.d`, and the Argo CD port mapping when asked for.
pub fn config(argocd_host_port: Option<u16>) -> String {
    let mut config = String::from(
        r#"kind: Cluster
apiVersion: kind.x-k8s.io/v1alpha4
containerdConfigPatches:
  - |-
    [plugins."io.containerd.grpc.v1.cri".registry]
      config_path = "/etc/containerd/certs.d"
nodes:
  - role: control-plane
"#,
    );
    if let Some(host_port) = argocd_host_port {
        config.push_str(&format!(
            r#"    extraPortMappings:
      - containerPort: {ARGOCD_NODE_PORT}
        hostPort: {host_port}
        listenAddress: "127.0.0.1"
        protocol: TCP
"#
        ));
    }
    config
}

/// Create the cluster and write its kubeconfig to `kubeconfig` (not the
/// user's `~/.kube/config`).
pub fn create(name: &str, config_file: &Path, kubeconfig: &Path) -> Result<()> {
    Cmd::new("kind")
        .args([
            "create", "cluster", "--name", name, "--image", NODE_IMAGE, "--wait", "300s",
        ])
        .arg("--config")
        .arg(config_file)
        .arg("--kubeconfig")
        .arg(kubeconfig)
        .timeout(Duration::from_secs(15 * 60))
        .echo()
        .run_ok()?;
    Ok(())
}

/// Delete the cluster; absent is fine.
pub fn delete(name: &str) {
    let _ = Cmd::new("kind")
        .args(["delete", "cluster", "--name", name])
        .timeout(Duration::from_secs(5 * 60))
        .run();
}

/// Whether a cluster of this name exists.
pub fn exists(name: &str) -> Result<bool> {
    let output = Cmd::new("kind").args(["get", "clusters"]).run_ok()?;
    Ok(output.stdout.lines().any(|line| line.trim() == name))
}

/// The node containers of the cluster.
pub fn nodes(name: &str) -> Result<Vec<String>> {
    let output = Cmd::new("kind").args(["get", "nodes", "--name", name]).run_ok()?;
    Ok(output
        .stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_the_node_image_is_pinned_by_digest_to_the_declared_version() {
        assert!(NODE_IMAGE.contains(&format!("v{KUBE_VERSION}@sha256:")), "{NODE_IMAGE}");
    }

    #[test]
    fn test_the_argocd_port_is_mapped_to_the_loopback_only_when_asked_for() {
        let without = config(None);
        assert!(without.contains("config_path = \"/etc/containerd/certs.d\""));
        assert!(!without.contains("extraPortMappings"));
        let with = config(Some(41234));
        assert!(with.contains("containerPort: 30080\n        hostPort: 41234\n        listenAddress: \"127.0.0.1\""));
        assert!(with.starts_with(&without), "the mapping extends the base configuration");
    }
}
