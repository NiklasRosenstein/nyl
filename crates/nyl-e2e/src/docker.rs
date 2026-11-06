//! Docker and the host's network: the few operations the infrastructure needs
//! beyond starting a container.

use std::net::TcpListener;
use std::time::Duration;

use crate::cmd::Cmd;
use crate::error::{Context, Error, Result};
use crate::wait::{Poll, Waiter};

pub fn docker() -> Cmd {
    Cmd::new("docker")
}

/// A loopback port nothing listens on right now.
pub fn free_port() -> Result<u16> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).context("find a free loopback port")?;
    Ok(listener.local_addr().context("read the free port")?.port())
}

/// Whether the container exists and runs.
pub fn is_running(container: &str) -> bool {
    docker()
        .args(["inspect", "--format", "{{.State.Running}}", container])
        .run()
        .map(|output| output.success() && output.stdout_trimmed() == "true")
        .unwrap_or(false)
}

/// The container's IPv4 address on `network`, which pods and kind nodes use to
/// reach it without any DNS.
pub fn ip_on_network(container: &str, network: &str) -> Result<String> {
    let template = format!("{{{{(index .NetworkSettings.Networks \"{network}\").IPAddress}}}}");
    let output = docker().args(["inspect", "--format", &template, container]).run_ok()?;
    let address = output.stdout_trimmed().to_owned();
    if address.is_empty() || address.contains("no value") {
        return Err(Error::new(format!(
            "container {container} has no address on the {network} network"
        )));
    }
    Ok(address)
}

/// Remove the container, whether or not it exists.
pub fn remove(container: &str) {
    let _ = docker().args(["rm", "--force", "--volumes", container]).run();
}

/// Wait until `url` answers a GET with success.
pub fn wait_for_http(what: &str, url: &str, timeout: Duration) -> Result<()> {
    Waiter::new(what, timeout)
        .backoff(Duration::from_millis(500), Duration::from_secs(5))
        .until(|| {
            let output = Cmd::new("curl")
                .args([
                    "--silent",
                    "--show-error",
                    "--fail",
                    "--max-time",
                    "10",
                    "--output",
                    "/dev/null",
                    url,
                ])
                .run()?;
            Ok(if output.success() {
                Poll::Ready(())
            } else {
                Poll::pending(
                    output
                        .stderr
                        .lines()
                        .last()
                        .map_or_else(|| "no answer".to_owned(), str::to_owned),
                )
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_a_free_port_can_be_bound_right_after() {
        let port = free_port().unwrap();
        assert!(port > 0);
        assert!(TcpListener::bind(("127.0.0.1", port)).is_ok());
    }
}
