//! Argo CD: the upstream install manifest of one pinned version, configured
//! for a test cluster, and a read-only API token for the `api` access method.
//!
//! The configuration changes what a test needs and nothing else: the
//! reconciliation poll drops from three minutes to 15 seconds (so a push is
//! seen quickly), the API server serves plain HTTP (Nyl allows `http` for a
//! loopback address, and the NodePort is published on the host's loopback
//! only), dex and the notifications controller are not started (nothing uses
//! them), and an `e2e` account may read Applications.

use std::path::Path;
use std::time::Duration;

use crate::checks::parse_token;
use crate::cmd::Cmd;
use crate::error::{Context, Result};
use crate::fixture::ARGOCD_NAMESPACE;
use crate::kubectl::Kubectl;
use crate::wait::{Poll, Waiter};

use super::kind::ARGOCD_NODE_PORT;

/// The Argo CD release, installed from its version-pinned upstream manifest.
pub const VERSION: &str = "v3.5.7";

/// The account that gets the API token, with read-only access.
pub const READ_ONLY_ACCOUNT: &str = "e2e";

/// The Deployments and StatefulSet the suite waits for.
const DEPLOYMENTS: [&str; 4] = [
    "argocd-redis",
    "argocd-repo-server",
    "argocd-server",
    "argocd-applicationset-controller",
];

/// Where the pinned install manifest comes from.
pub fn manifest_url() -> String {
    format!("https://raw.githubusercontent.com/argoproj/argo-cd/{VERSION}/manifests/install.yaml")
}

/// The API server as the host reaches it.
pub fn api_url(host_port: u16) -> String {
    format!("http://127.0.0.1:{host_port}")
}

/// A NodePort Service in front of the API server, which kind publishes on
/// the host's loopback.
fn node_port_service() -> String {
    format!(
        r#"apiVersion: v1
kind: Service
metadata:
  name: argocd-server-nodeport
  namespace: {ARGOCD_NAMESPACE}
spec:
  type: NodePort
  selector:
    app.kubernetes.io/name: argocd-server
  ports:
    - name: http
      port: 8080
      targetPort: 8080
      nodePort: {ARGOCD_NODE_PORT}
"#
    )
}

fn merge_patch(kubectl: &Kubectl, configmap: &str, data: serde_json::Value) -> Result<()> {
    let patch = serde_json::json!({ "data": data }).to_string();
    kubectl.run(&[
        "patch",
        "configmap",
        configmap,
        "--namespace",
        ARGOCD_NAMESPACE,
        "--type",
        "merge",
        "--patch",
        &patch,
    ])?;
    Ok(())
}

/// Install Argo CD and wait until its API answers on `host_port`.
pub fn install(kubectl: &Kubectl, directory: &Path, host_port: u16) -> Result<()> {
    let manifest = directory.join(format!("argocd-{VERSION}-install.yaml"));
    if !manifest.is_file() {
        Cmd::new("curl")
            .args([
                "--fail",
                "--silent",
                "--show-error",
                "--location",
                "--retry",
                "5",
                "--retry-all-errors",
            ])
            .arg("--output")
            .arg(&manifest)
            .arg(manifest_url())
            .timeout(Duration::from_secs(180))
            .echo()
            .run_ok()?;
    }

    // CoreDNS and the proxy must run before Argo CD's pods can find each other.
    kubectl.run(&[
        "wait",
        "--for=condition=Ready",
        "pods",
        "--selector=k8s-app=kube-dns",
        "--namespace=kube-system",
        "--timeout=180s",
    ])?;

    kubectl.apply(
        &format!("apiVersion: v1\nkind: Namespace\nmetadata:\n  name: {ARGOCD_NAMESPACE}\n"),
        false,
    )?;
    // Server-side: the CRDs are larger than client-side apply's annotation allows.
    kubectl
        .cmd(&[
            "apply",
            "--server-side",
            "--force-conflicts",
            "--namespace",
            ARGOCD_NAMESPACE,
            "--filename",
        ])
        .arg(&manifest)
        .timeout(Duration::from_secs(300))
        .echo()
        .run_ok()?;
    // `kubectl wait --for=condition=Established` fails at once, instead of
    // waiting, on a CRD the API server has not given conditions yet.
    for crd in ["applications.argoproj.io", "appprojects.argoproj.io"] {
        Waiter::new(format!("CRD {crd} to be established"), Duration::from_secs(120))
            .backoff(Duration::from_millis(500), Duration::from_secs(5))
            .until(|| {
                Ok(match kubectl.get_optional(&["crd", crd])? {
                    Some(object) if crd_established(&object) => Poll::Ready(()),
                    Some(_) => Poll::pending("not established yet"),
                    None => Poll::pending("not found"),
                })
            })?;
    }

    merge_patch(
        kubectl,
        "argocd-cm",
        serde_json::json!({
            "timeout.reconciliation": "15s",
            "timeout.reconciliation.jitter": "0s",
            format!("accounts.{READ_ONLY_ACCOUNT}"): "apiKey",
        }),
    )?;
    merge_patch(
        kubectl,
        "argocd-cmd-params-cm",
        serde_json::json!({ "server.insecure": "true" }),
    )?;
    merge_patch(
        kubectl,
        "argocd-rbac-cm",
        serde_json::json!({ "policy.csv": format!("g, {READ_ONLY_ACCOUNT}, role:readonly\n") }),
    )?;
    kubectl.run(&[
        "scale",
        "deployment/argocd-dex-server",
        "deployment/argocd-notifications-controller",
        "--replicas=0",
        "--namespace",
        ARGOCD_NAMESPACE,
    ])?;
    // The configuration is read at start; the pods are still starting.
    kubectl.run(&[
        "rollout",
        "restart",
        "deployment/argocd-server",
        "deployment/argocd-repo-server",
        "statefulset/argocd-application-controller",
        "--namespace",
        ARGOCD_NAMESPACE,
    ])?;
    kubectl.apply(&node_port_service(), false)?;

    for deployment in DEPLOYMENTS {
        rollout(kubectl, &format!("deployment/{deployment}"))?;
    }
    rollout(kubectl, "statefulset/argocd-application-controller")?;

    crate::docker::wait_for_http(
        "the Argo CD API server to answer on the host",
        &format!("{}/api/version", api_url(host_port)),
        Duration::from_secs(180),
    )
}

/// Whether a CustomResourceDefinition has the condition `Established` set to
/// `True`. A CRD the API server has not processed yet has no conditions.
fn crd_established(crd: &serde_json::Value) -> bool {
    crd.pointer("/status/conditions")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|conditions| {
            conditions
                .iter()
                .any(|condition| condition["type"] == "Established" && condition["status"] == "True")
        })
}

fn rollout(kubectl: &Kubectl, workload: &str) -> Result<()> {
    kubectl
        .cmd(&[
            "rollout",
            "status",
            workload,
            "--namespace",
            ARGOCD_NAMESPACE,
            "--timeout=300s",
        ])
        .timeout(Duration::from_secs(330))
        .run_ok()
        .map_err(|error| error.context(format!("Argo CD's {workload} did not become ready")))?;
    Ok(())
}

/// One request to the Argo CD API through `curl`. The bearer token goes in a
/// configuration read from stdin, so it is in no argument list; `secrets` are
/// hidden from the logs.
fn api(host_port: u16, method: &str, path: &str, body: &str, bearer: Option<&str>, secrets: &[&str]) -> Result<String> {
    let mut command = Cmd::new("curl")
        .args(["--silent", "--show-error", "--fail-with-body", "--max-time", "30"])
        .args(["--request", method])
        .args(["--header", "Content-Type: application/json"])
        .arg(format!("{}{path}", api_url(host_port)));
    if !body.is_empty() {
        command = command.args(["--data", body]);
    }
    if let Some(token) = bearer {
        command = command
            .args(["--config", "-"])
            .stdin(format!("header = \"Authorization: Bearer {token}\"\n"))
            .secret(token);
    }
    for secret in secrets {
        command = command.secret(*secret);
    }
    Ok(command.run_ok()?.stdout)
}

/// Mint a read-only API token: sign in as `admin` with the initial password,
/// create a token for the `e2e` account (which `install` gave read access to
/// Applications), and wait until it works.
pub fn mint_token(kubectl: &Kubectl, host_port: u16) -> Result<String> {
    let password = Waiter::new("Argo CD's initial admin password", Duration::from_secs(180)).until(|| {
        let output = kubectl
            .cmd(&[
                "get",
                "secret",
                "argocd-initial-admin-secret",
                "--namespace",
                ARGOCD_NAMESPACE,
                "--output",
                "go-template={{.data.password | base64decode}}",
            ])
            .run()?;
        Ok(if output.success() && !output.stdout.trim().is_empty() {
            Poll::Ready(output.stdout.trim().to_owned())
        } else {
            Poll::pending("the secret does not exist yet")
        })
    })?;
    crate::log::mask_in_ci(&password);

    let login = serde_json::json!({"username": "admin", "password": password}).to_string();
    let session = Waiter::new("an admin session on the Argo CD API", Duration::from_secs(120)).until(|| {
        Ok(
            match api(host_port, "POST", "/api/v1/session", &login, None, &[&password, &login]) {
                Ok(body) => Poll::Ready(parse_token(&body)?),
                Err(error) => Poll::pending(error.to_string()),
            },
        )
    })?;
    crate::log::mask_in_ci(&session);

    let body = api(
        host_port,
        "POST",
        &format!("/api/v1/account/{READ_ONLY_ACCOUNT}/token"),
        "{}",
        Some(&session),
        &[],
    )
    .context(format!("create an API token for the {READ_ONLY_ACCOUNT} account"))?;
    let token = parse_token(&body)?;
    crate::log::mask_in_ci(&token);

    Waiter::new("the API token to read Applications", Duration::from_secs(120)).until(|| {
        Ok(
            match api(host_port, "GET", "/api/v1/applications", "", Some(&token), &[]) {
                Ok(_) => Poll::Ready(()),
                Err(error) => Poll::pending(error.to_string()),
            },
        )
    })?;
    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_the_install_manifest_is_the_pinned_upstream_one() {
        assert_eq!(
            manifest_url(),
            "https://raw.githubusercontent.com/argoproj/argo-cd/v3.5.7/manifests/install.yaml"
        );
        assert!(VERSION.starts_with('v'), "an exact release, never a branch");
    }

    #[test]
    fn test_the_node_port_service_matches_the_port_kind_maps() {
        let service = node_port_service();
        assert!(service.contains(&format!("nodePort: {ARGOCD_NODE_PORT}")));
        assert!(service.contains("app.kubernetes.io/name: argocd-server"));
    }

    #[test]
    fn test_a_crd_is_established_only_once_its_condition_is_true() {
        let crd = |status: serde_json::Value| serde_json::json!({"kind": "CustomResourceDefinition", "status": status});
        assert!(!crd_established(&crd(serde_json::json!({}))));
        assert!(!crd_established(&crd(serde_json::json!({"conditions": null}))));
        assert!(!crd_established(&crd(serde_json::json!({
            "conditions": [{"type": "Established", "status": "False"}]
        }))));
        assert!(crd_established(&crd(serde_json::json!({
            "conditions": [
                {"type": "NamesAccepted", "status": "True"},
                {"type": "Established", "status": "True"}
            ]
        }))));
    }
}
