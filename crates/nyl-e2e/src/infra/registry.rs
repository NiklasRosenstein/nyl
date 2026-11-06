//! The local registry, the standard kind pattern: a `registry:2` container on
//! the kind network, published on the host's loopback so the host's Docker
//! pushes to `localhost:<port>`, and named in every node's containerd
//! configuration so the nodes pull `localhost:<port>/...` from the container.
//! Nodes reach it by its address on the kind network, never by name.

use std::time::Duration;

use super::Endpoint;
use crate::cmd::Cmd;
use crate::docker::{docker, free_port, ip_on_network, remove, wait_for_http};
use crate::error::Result;

/// The registry image, pinned to a release.
pub const IMAGE: &str = "registry:2.8.3";

/// Start the registry on `network`, published on a free loopback port.
pub fn start(container: &str, network: &str) -> Result<Endpoint> {
    remove(container);
    let host_port = free_port()?;
    docker()
        .args(["run", "--detach", "--name", container, "--network", network])
        .args(["--publish", &format!("127.0.0.1:{host_port}:5000"), IMAGE])
        .timeout(Duration::from_secs(5 * 60))
        .run_ok()?;
    let endpoint = Endpoint {
        container: container.to_owned(),
        host_port,
        ip: ip_on_network(container, network)?,
    };
    wait_for_http(
        "the registry to answer",
        &format!("http://127.0.0.1:{host_port}/v2/"),
        Duration::from_secs(60),
    )?;
    Ok(endpoint)
}

/// The `hosts.toml` that sends `localhost:<host_port>` to the registry
/// container at its address on the kind network.
pub fn hosts_toml(registry_ip: &str) -> String {
    format!("[host.\"http://{registry_ip}:5000\"]\n")
}

/// Tell every node's containerd where `localhost:<host_port>` is. containerd
/// reads the file when it resolves an image, so no restart is needed.
pub fn configure_nodes(nodes: &[String], endpoint: &Endpoint) -> Result<()> {
    let directory = format!("/etc/containerd/certs.d/localhost:{}", endpoint.host_port);
    for node in nodes {
        docker()
            .args(["exec", "--interactive", node, "sh", "-c"])
            .arg(format!("mkdir -p '{directory}' && cat > '{directory}/hosts.toml'"))
            .stdin(hosts_toml(&endpoint.ip))
            .run_ok()?;
    }
    Ok(())
}

/// Whether the registry holds `repository` at `reference` (a tag or digest).
pub fn has_manifest(host_port: u16, repository: &str, reference: &str) -> Result<bool> {
    let accept = [
        "application/vnd.oci.image.index.v1+json",
        "application/vnd.oci.image.manifest.v1+json",
        "application/vnd.docker.distribution.manifest.list.v2+json",
        "application/vnd.docker.distribution.manifest.v2+json",
    ]
    .join(", ");
    let output = Cmd::new("curl")
        .args([
            "--silent",
            "--output",
            "/dev/null",
            "--write-out",
            "%{http_code}",
            "--max-time",
            "20",
        ])
        .args(["--header", &format!("Accept: {accept}")])
        .arg(format!(
            "http://127.0.0.1:{host_port}/v2/{repository}/manifests/{reference}"
        ))
        .run_ok()?;
    Ok(output.stdout_trimmed() == "200")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hosts_toml_names_the_registry_by_address_not_by_name() {
        assert_eq!(hosts_toml("172.18.0.3"), "[host.\"http://172.18.0.3:5000\"]\n");
    }
}
