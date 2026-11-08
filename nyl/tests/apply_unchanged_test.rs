//! `nyl apply` of a release that changed nothing applies again but records no
//! new revision, and says so.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::{json, Value};
use tempfile::TempDir;

/// The Secrets (release state) the stub holds, by namespace and name.
type Secrets = Arc<Mutex<BTreeMap<(String, String), Value>>>;

/// A Kubernetes stub with a memory for Secrets, which is where release state
/// lives. Everything else it serves is what `apply` of a ConfigMap needs.
fn serve(listener: TcpListener, done: &Arc<AtomicBool>, secrets: &Secrets) -> std::thread::JoinHandle<()> {
    let done = done.clone();
    let secrets = secrets.clone();
    std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(60);
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
            let mut length = 0;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse().unwrap();
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            let mut parts = first.split_whitespace();
            let method = parts.next().unwrap_or_default().to_owned();
            let target = parts.next().unwrap_or_default().to_owned();
            let (path, query) = target.split_once('?').unwrap_or((target.as_str(), ""));

            let (code, body) = response(&method, path, query, &body, &mut secrets.lock().unwrap());
            let status = match code {
                200 => "200 OK",
                _ => "404 Not Found",
            };
            write!(
                socket,
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        }
    })
}

fn not_found(message: &str) -> (u16, String) {
    (
        404,
        json!({"kind": "Status", "apiVersion": "v1", "status": "Failure", "message": message,
               "reason": "NotFound", "code": 404})
        .to_string(),
    )
}

fn response(
    method: &str,
    path: &str,
    query: &str,
    body: &[u8],
    secrets: &mut BTreeMap<(String, String), Value>,
) -> (u16, String) {
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
                     "verbs": ["get", "list", "create", "patch", "delete"]}
                ]})
                .to_string(),
            )
        }
        _ => {}
    }

    let Some(rest) = path.strip_prefix("/api/v1/namespaces/") else {
        return not_found(&format!("{path} not found"));
    };
    let mut segments = rest.splitn(3, '/');
    let namespace = segments.next().unwrap_or_default().to_owned();
    let (Some(resource), name) = (segments.next(), segments.next()) else {
        // The Namespace object: it always exists, and applying it succeeds.
        return (
            200,
            json!({"apiVersion": "v1", "kind": "Namespace",
                   "metadata": {"name": namespace, "resourceVersion": "1"}})
            .to_string(),
        );
    };
    match (resource, name, method) {
        ("secrets", None, "GET") => {
            // The release label selector is `<label>=<release>`, or a bare label.
            let selector = query
                .split('&')
                .find_map(|pair| pair.strip_prefix("labelSelector="))
                .unwrap_or_default()
                .replace("%3D", "=")
                .replace("%2F", "/");
            let items: Vec<&Value> = secrets
                .iter()
                .filter(|((secret_namespace, _), secret)| {
                    secret_namespace == &namespace && matches_selector(secret, &selector)
                })
                .map(|(_, secret)| secret)
                .collect();
            (
                200,
                json!({"apiVersion": "v1", "kind": "SecretList", "metadata": {"resourceVersion": "1"}, "items": items})
                    .to_string(),
            )
        }
        ("secrets", None, "POST") => {
            let secret: Value = serde_json::from_slice(body).unwrap();
            let name = secret["metadata"]["name"].as_str().unwrap().to_owned();
            secrets.insert((namespace, name), secret.clone());
            (200, secret.to_string())
        }
        ("secrets", Some(name), "GET") => secrets.get(&(namespace, name.to_owned())).map_or_else(
            || not_found(&format!("secrets \"{name}\" not found")),
            |secret| (200, secret.to_string()),
        ),
        ("secrets", Some(name), "PUT") => {
            let secret: Value = serde_json::from_slice(body).unwrap();
            secrets.insert((namespace, name.to_owned()), secret.clone());
            (200, secret.to_string())
        }
        ("secrets", Some(name), "DELETE") => match secrets.remove(&(namespace, name.to_owned())) {
            Some(secret) => (200, secret.to_string()),
            None => not_found(&format!("secrets \"{name}\" not found")),
        },
        ("configmaps", None, "GET") => (
            200,
            json!({"apiVersion": "v1", "kind": "ConfigMapList", "metadata": {"resourceVersion": "1"}, "items": []})
                .to_string(),
        ),
        ("configmaps", Some(name), "GET") => not_found(&format!("configmaps \"{name}\" not found")),
        ("configmaps", Some(name), _) => (
            200,
            json!({"apiVersion": "v1", "kind": "ConfigMap",
                   "metadata": {"name": name, "namespace": namespace, "resourceVersion": "1"}})
            .to_string(),
        ),
        _ => not_found(&format!("{path} not found")),
    }
}

/// Whether a Secret's labels satisfy `key=value` or a bare `key`.
fn matches_selector(secret: &Value, selector: &str) -> bool {
    let labels = &secret["metadata"]["labels"];
    match selector.split_once('=') {
        Some((key, value)) => labels[key].as_str() == Some(value),
        None => labels.get(selector).is_some(),
    }
}

fn project(server: &str) -> TempDir {
    let temp = TempDir::new().unwrap();
    git2::Repository::init(temp.path()).unwrap();
    std::fs::write(temp.path().join("nyl.toml"), "[project]\n").unwrap();
    std::fs::write(
        temp.path().join("cluster.yaml"),
        format!(
            r"apiVersion: k8s.nyl/v1
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
  reconciler: {{argocd: {{}}}}
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
    write_manifest(&temp, "b");
    temp
}

fn write_manifest(temp: &TempDir, value: &str) {
    std::fs::write(
        temp.path().join("manifest.yaml"),
        format!(
            "apiVersion: k8s.nyl/v1\nkind: Release\nmetadata: {{name: example, namespace: traefik}}\n---\napiVersion: v1\nkind: ConfigMap\nmetadata: {{name: example}}\ndata: {{a: {value}}}\n"
        ),
    )
    .unwrap();
}

/// Runs `nyl apply` against the stub and returns its stdout.
fn apply(temp: &TempDir) -> String {
    let mut command = Command::new(assert_cmd::cargo::cargo_bin!("nyl"));
    // The stub cluster is local; an ambient proxy must not intercept it.
    for variable in ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"] {
        command.env_remove(variable);
    }
    let output = command
        .current_dir(temp.path())
        .env("KUBECONFIG", temp.path().join("kubeconfig"))
        .args([
            "apply",
            "manifest.yaml",
            "--no-cache",
            "--target",
            "test",
            "--color",
            "never",
        ])
        .timeout(Duration::from_secs(30))
        .output()
        .unwrap();
    let assert = assert_cmd::assert::Assert::new(output).success();
    String::from_utf8(assert.get_output().stdout.clone()).unwrap()
}

fn revisions(secrets: &Secrets) -> Vec<String> {
    secrets.lock().unwrap().keys().map(|(_, name)| name.clone()).collect()
}

/// A second apply of the same release applies every resource again, reports the
/// release unchanged at its revision, and leaves the history alone; a changed
/// manifest records revision 2.
#[test]
fn reports_an_unchanged_release_and_records_no_revision() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let server = format!("http://{}", listener.local_addr().unwrap());
    let done = Arc::new(AtomicBool::new(false));
    let secrets = Secrets::default();
    let worker = serve(listener, &done, &secrets);
    let temp = project(&server);

    let first = apply(&temp);
    assert!(
        predicate::str::contains("Release: example revision 1 deployed successfully to namespace traefik").eval(&first),
        "{first}"
    );
    assert_eq!(revisions(&secrets), ["nyl.release.v1.example.1"]);

    let second = apply(&temp);
    assert!(
        predicate::str::contains("Release: traefik/example unchanged at revision 1").eval(&second),
        "{second}"
    );
    // The apply itself still ran.
    assert!(
        predicate::str::contains("ConfigMap traefik/example").eval(&second),
        "{second}"
    );
    assert_eq!(revisions(&secrets), ["nyl.release.v1.example.1"]);

    write_manifest(&temp, "changed");
    let third = apply(&temp);
    assert!(
        predicate::str::contains("Release: example revision 2 deployed successfully to namespace traefik").eval(&third),
        "{third}"
    );
    assert_eq!(
        revisions(&secrets),
        ["nyl.release.v1.example.1", "nyl.release.v1.example.2"]
    );

    done.store(true, Ordering::Relaxed);
    worker.join().unwrap();
}
