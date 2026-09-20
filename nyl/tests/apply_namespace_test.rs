//! `nyl apply` must create the release namespace before the resources in it.

use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::json;
use tempfile::TempDir;

/// A Kubernetes stub that, like a real API server, rejects writes into a
/// namespace that does not exist yet. Returns the requests it served, in order.
fn serve(listener: TcpListener, done: &Arc<AtomicBool>) -> std::thread::JoinHandle<Vec<String>> {
    let done = done.clone();
    std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(40);
        let mut requests = Vec::new();
        let mut namespaces: HashSet<String> = HashSet::new();
        while !done.load(Ordering::Relaxed) && Instant::now() < deadline {
            let (mut socket, _) = match listener.accept() {
                Ok(connection) => connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(error) => panic!("{error}"),
            };
            // Windows sockets inherit the listener's nonblocking mode.
            socket.set_nonblocking(false).unwrap();
            socket.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
            let mut reader = BufReader::new(&socket);
            let mut first = String::new();
            reader.read_line(&mut first).unwrap();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                    break;
                }
            }
            let mut parts = first.split_whitespace();
            let method = parts.next().unwrap_or_default().to_owned();
            let target = parts.next().unwrap_or_default().to_owned();
            let path = target.split('?').next().unwrap_or_default().to_owned();

            let (code, body) = response(&method, &path, &mut namespaces);
            requests.push(format!("{method} {path}"));
            let status = if code == 200 { "200 OK" } else { "404 Not Found" };
            write!(
                socket,
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        }
        requests
    })
}

fn response(method: &str, path: &str, namespaces: &mut HashSet<String>) -> (u16, String) {
    let not_found = |message: String| {
        (
            404,
            json!({"kind": "Status", "apiVersion": "v1", "status": "Failure", "message": message,
                   "reason": "NotFound", "code": 404})
            .to_string(),
        )
    };
    match path {
        "/version" => {
            return (
                200,
                json!({"major": "1", "minor": "31", "gitVersion": "v1.31.4", "platform": "test",
                       "buildDate": "2025-01-01T00:00:00Z", "compiler": "rustc", "gitCommit": "0",
                       "gitTreeState": "clean", "goVersion": "go1.23"})
                .to_string(),
            )
        }
        "/api" => return (
            200,
            json!({"kind": "APIVersions", "apiVersion": "v1", "versions": ["v1"], "serverAddressByClientCIDRs": []})
                .to_string(),
        ),
        "/apis" => {
            return (
                200,
                json!({"kind": "APIGroupList", "apiVersion": "v1", "groups": []}).to_string(),
            )
        }
        "/api/v1" => {
            return (
                200,
                json!({"kind": "APIResourceList", "apiVersion": "v1", "groupVersion": "v1", "resources": [
                    {"name": "namespaces", "singularName": "", "kind": "Namespace", "namespaced": false,
                     "verbs": ["get", "list", "create", "patch"]},
                    {"name": "configmaps", "singularName": "", "kind": "ConfigMap", "namespaced": true,
                     "verbs": ["get", "list", "create", "patch"]},
                    {"name": "secrets", "singularName": "", "kind": "Secret", "namespaced": true,
                     "verbs": ["get", "list", "create", "patch"]}
                ]})
                .to_string(),
            )
        }
        _ => {}
    }

    let Some(rest) = path.strip_prefix("/api/v1/namespaces/") else {
        return not_found(format!("{path} not found"));
    };
    let mut segments = rest.splitn(2, '/');
    let namespace = segments.next().unwrap_or_default().to_owned();
    let Some(subpath) = segments.next() else {
        // The Namespace object itself.
        if method == "PATCH" {
            namespaces.insert(namespace.clone());
            return (
                200,
                json!({"apiVersion": "v1", "kind": "Namespace",
                       "metadata": {"name": namespace, "resourceVersion": "1"}})
                .to_string(),
            );
        }
        if namespaces.contains(&namespace) {
            return (
                200,
                json!({"apiVersion": "v1", "kind": "Namespace",
                       "metadata": {"name": namespace, "resourceVersion": "1"}})
                .to_string(),
            );
        }
        return not_found(format!("namespaces \"{namespace}\" not found"));
    };

    if !namespaces.contains(&namespace) {
        return not_found(format!("namespaces \"{namespace}\" not found"));
    }
    let mut subsegments = subpath.splitn(2, '/');
    let resource = subsegments.next().unwrap_or_default().to_owned();
    let kind = match resource.as_str() {
        "configmaps" => "ConfigMap",
        "secrets" => "Secret",
        _ => return not_found(format!("{path} not found")),
    };
    match subsegments.next() {
        // A collection request is either a list or a create.
        None if method == "GET" => (
            200,
            json!({"apiVersion": "v1", "kind": format!("{kind}List"),
                   "metadata": {"resourceVersion": "1"}, "items": []})
            .to_string(),
        ),
        None => (
            200,
            json!({"apiVersion": "v1", "kind": kind,
                   "metadata": {"name": "created", "namespace": namespace, "resourceVersion": "1"}})
            .to_string(),
        ),
        Some(name) if method == "GET" => not_found(format!("{resource} \"{name}\" not found")),
        Some(name) => (
            200,
            json!({"apiVersion": "v1", "kind": kind,
                   "metadata": {"name": name, "namespace": namespace, "resourceVersion": "1"}})
            .to_string(),
        ),
    }
}

fn project(server: &str) -> TempDir {
    let temp = TempDir::new().unwrap();
    git2::Repository::init(temp.path()).unwrap();
    std::fs::write(temp.path().join("nyl.toml"), "[project]\n").unwrap();
    std::fs::write(
        temp.path().join("cluster.yaml"),
        format!(
            r"apiVersion: k8s.gitops.nyl/v1
kind: Cluster
metadata: {{name: test}}
spec:
  destination: {{server: '{server}'}}
  kubernetes: {{kubeVersion: 1.31.4, apiVersions: [v1]}}
---
apiVersion: k8s.gitops.nyl/v1
kind: DeploymentTarget
metadata: {{name: test}}
spec:
  clusterRef: {{name: test}}
  publication:
    repository: {{repoURL: https://example.invalid/deploy.git}}
    revision: deploy/test
"
        ),
    )
    .unwrap();
    std::fs::write(
        temp.path().join("kubeconfig"),
        format!(
            r"apiVersion: v1
kind: Config
clusters:
- name: test
  cluster: {{server: '{server}'}}
contexts:
- name: test
  context: {{cluster: test, user: test}}
current-context: test
users:
- name: test
  user: {{token: test}}
"
        ),
    )
    .unwrap();
    std::fs::write(
        temp.path().join("manifest.yaml"),
        "apiVersion: k8s.gitops.nyl/v1\nkind: Release\nmetadata: {name: example, namespace: traefik}\n---\napiVersion: v1\nkind: ConfigMap\nmetadata: {name: example}\ndata: {a: b}\n",
    )
    .unwrap();
    temp
}

#[test]
fn creates_the_release_namespace_before_the_resources_that_live_in_it() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let server = format!("http://{}", listener.local_addr().unwrap());
    let done = Arc::new(AtomicBool::new(false));
    let worker = serve(listener, &done);
    let temp = project(&server);

    let mut command = Command::new(assert_cmd::cargo::cargo_bin!("nyl"));
    // The stub cluster is local; an ambient proxy must not intercept it.
    for variable in ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"] {
        command.env_remove(variable);
    }
    let output = command
        .current_dir(temp.path())
        .env("KUBECONFIG", temp.path().join("kubeconfig"))
        .args(["apply", "manifest.yaml", "--no-cache", "--target", "test"])
        .timeout(Duration::from_secs(30))
        .output()
        .unwrap();
    done.store(true, Ordering::Relaxed);
    let requests = worker.join().unwrap();

    assert_cmd::assert::Assert::new(output)
        .success()
        .stdout(predicate::str::contains("+ ConfigMap traefik/example"))
        .stdout(predicate::str::contains("✗").not());

    let namespace_write = requests
        .iter()
        .position(|request| request == "PATCH /api/v1/namespaces/traefik")
        .unwrap_or_else(|| panic!("the namespace was never created: {requests:?}"));
    let configmap_write = requests
        .iter()
        .position(|request| request == "PATCH /api/v1/namespaces/traefik/configmaps/example")
        .unwrap_or_else(|| panic!("the ConfigMap was never applied: {requests:?}"));
    assert!(
        namespace_write < configmap_write,
        "the namespace must be created first: {requests:?}"
    );
}
