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

    /// The OS may hand a port out again right after it is released, so the
    /// probe is only checked against ports that are held right now: a port
    /// that is in use is never the answer.
    #[test]
    fn test_a_free_port_is_never_one_that_is_in_use() {
        let held = (0..8)
            .map(|_| TcpListener::bind(("127.0.0.1", 0)).unwrap())
            .collect::<Vec<_>>();
        let in_use = held
            .iter()
            .map(|listener| listener.local_addr().unwrap().port())
            .collect::<Vec<_>>();
        for _ in 0..16 {
            let port = free_port().unwrap();
            assert!(port > 0);
            assert!(!in_use.contains(&port), "{port} is held by a listener");
        }
    }
}
