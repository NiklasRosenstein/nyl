//! The Git server: Git's own smart-HTTP backend (`git http-backend`) behind
//! lighttpd, in a container on the kind network. One address serves both
//! sides: the host reaches it on a published loopback port, where Nyl pushes
//! the trees and its state; Argo CD's repo-server reads the same repository
//! from inside the cluster by the container's address on the kind network.
//!
//! Why this and not `git daemon`: Argo CD speaks HTTP(S) and SSH to Git, not
//! the `git://` protocol, and a plain HTTP server needs no credentials, no
//! TLS, and no DNS. Why not Gitea or similar: a dozen lines of configuration
//! and a 30 MB Alpine image, with the real `git-http-backend` that hosting
//! services wrap, instead of a service to configure and wait for.

use std::time::Duration;

use super::Endpoint;
use crate::cmd::Cmd;
use crate::docker::{docker, free_port, ip_on_network, remove};
use crate::error::{Context, Result};
use crate::wait::{Poll, Waiter};

/// The image the server runs; the tag changes with the Dockerfile or the
/// configuration, so a stale local image is never reused.
pub const IMAGE: &str = "nyl-e2e-git:2";

/// The port the server listens on inside the container.
pub const PORT: u16 = 8080;

/// The one repository the project lives in.
pub const REPOSITORY: &str = "shop";

/// Alpine pinned to a release; `git-daemon` carries `git-http-backend`.
const DOCKERFILE: &str = r#"FROM alpine:3.22.8
RUN apk add --no-cache git git-daemon lighttpd \
 && git config --system safe.directory '*' \
 && mkdir -p /srv/git /srv/www
COPY lighttpd.conf /etc/lighttpd/lighttpd.conf
EXPOSE 8080
CMD ["lighttpd", "-D", "-f", "/etc/lighttpd/lighttpd.conf"]
"#;

/// lighttpd runs `git-http-backend` as a CGI for every path under `/git`,
/// serving the repositories in `/srv/git`.
const LIGHTTPD_CONF: &str = r#"server.document-root = "/srv/www"
server.port = 8080
server.modules = ( "mod_accesslog", "mod_alias", "mod_cgi", "mod_setenv" )
server.errorlog = "/dev/stderr"
accesslog.filename = "/dev/stderr"
alias.url = ( "/git" => "/usr/libexec/git-core/git-http-backend" )
$HTTP["url"] =~ "^/git" {
    cgi.assign = ( "" => "" )
    setenv.add-environment = (
        "GIT_PROJECT_ROOT" => "/srv/git",
        "GIT_HTTP_EXPORT_ALL" => ""
    )
}
"#;

/// Build the server image unless it exists.
fn ensure_image() -> Result<()> {
    if docker().args(["image", "inspect", IMAGE]).run()?.success() {
        return Ok(());
    }
    let context = tempfile::tempdir().context("create the build context of the Git server image")?;
    std::fs::write(context.path().join("Dockerfile"), DOCKERFILE).context("write the Dockerfile")?;
    std::fs::write(context.path().join("lighttpd.conf"), LIGHTTPD_CONF).context("write lighttpd.conf")?;
    docker()
        .args(["build", "--tag", IMAGE])
        .arg(context.path())
        .timeout(Duration::from_secs(10 * 60))
        .echo()
        .run_ok()
        .map_err(|error| error.context("build the Git server image (it needs to reach the Alpine mirrors)"))?;
    Ok(())
}

/// The URL the host pushes to.
pub fn publish_url(host_port: u16) -> String {
    format!("http://127.0.0.1:{host_port}/git/{REPOSITORY}.git")
}

/// The URL Argo CD reads from inside the cluster.
pub fn cluster_url(ip: &str) -> String {
    format!("http://{ip}:{PORT}/git/{REPOSITORY}.git")
}

/// Start the server on `network`, published on a free loopback port, with an
/// empty repository ready.
pub fn start(container: &str, network: &str) -> Result<Endpoint> {
    ensure_image()?;
    remove(container);
    let host_port = free_port()?;
    docker()
        .args(["run", "--detach", "--name", container, "--network", network])
        .args(["--publish", &format!("127.0.0.1:{host_port}:{PORT}"), IMAGE])
        .run_ok()?;
    let endpoint = Endpoint {
        container: container.to_owned(),
        host_port,
        ip: ip_on_network(container, network)?,
    };
    reset_repository(container)?;
    wait_ready(&endpoint)?;
    Ok(endpoint)
}

/// Replace the repository with an empty one that accepts pushes over HTTP.
pub fn reset_repository(container: &str) -> Result<()> {
    let path = format!("/srv/git/{REPOSITORY}.git");
    docker()
        .args(["exec", container, "sh", "-c"])
        .arg(format!(
            "rm -rf {path} && git init --quiet --bare --initial-branch=main {path} \
             && git -C {path} config http.receivepack true"
        ))
        .run_ok()?;
    Ok(())
}

/// Wait until the host can list the repository over HTTP, which proves the
/// published port, the CGI, and the repository together.
fn wait_ready(endpoint: &Endpoint) -> Result<()> {
    let url = publish_url(endpoint.host_port);
    Waiter::new("the Git server to serve the repository", Duration::from_secs(60))
        .backoff(Duration::from_millis(500), Duration::from_secs(5))
        .until(|| {
            let output = Cmd::new("git")
                .args(["ls-remote", &url])
                .env("GIT_TERMINAL_PROMPT", "0")
                .run()?;
            Ok(if output.success() {
                Poll::Ready(())
            } else {
                Poll::pending(output.stderr.lines().last().unwrap_or("no answer").to_owned())
            })
        })
        .map_err(|error| {
            let logs = docker()
                .args(["logs", "--tail", "40", &endpoint.container])
                .run()
                .map(|output| format!("{}{}", output.stdout, output.stderr))
                .unwrap_or_default();
            error.context(format!("the Git server's container logs are:\n{logs}\nso"))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_the_host_and_the_cluster_reach_one_repository_by_different_addresses() {
        assert_eq!(publish_url(40123), "http://127.0.0.1:40123/git/shop.git");
        assert_eq!(cluster_url("172.18.0.4"), "http://172.18.0.4:8080/git/shop.git");
    }

    #[test]
    fn test_the_backend_serves_the_path_the_urls_use() {
        // The alias prefix and the repository root are what `git/shop.git` resolves through.
        assert!(LIGHTTPD_CONF.contains("alias.url = ( \"/git\" =>"));
        assert!(LIGHTTPD_CONF.contains("\"GIT_PROJECT_ROOT\" => \"/srv/git\""));
        assert!(publish_url(1).contains("/git/shop.git"));
        assert!(
            DOCKERFILE.contains("git-daemon"),
            "git-http-backend ships in git-daemon on Alpine"
        );
    }
}
