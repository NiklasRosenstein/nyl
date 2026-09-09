use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::json;
use tempfile::TempDir;

#[test]
fn validation_failure_prevents_resource_and_release_writes() {
    let temp = TempDir::new().unwrap();
    git2::Repository::init(temp.path()).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let server = format!("http://{}", listener.local_addr().unwrap());
    let done = Arc::new(AtomicBool::new(false));
    let server_done = done.clone();
    let worker = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(40);
        let mut requests = Vec::new();
        while !server_done.load(Ordering::Relaxed) && Instant::now() < deadline {
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
            let path = first.split_whitespace().nth(1).unwrap();
            let body = match path {
                "/api" => json!({"kind":"APIVersions","apiVersion":"v1","versions":["v1"],"serverAddressByClientCIDRs":[]}),
                "/apis" => json!({"kind":"APIGroupList","apiVersion":"v1","groups":[]}),
                "/api/v1" => json!({"kind":"APIResourceList","apiVersion":"v1","groupVersion":"v1","resources":[
                    {"name":"configmaps","singularName":"","kind":"ConfigMap","namespaced":true,"verbs":["get","list","create","patch"]}
                ]}),
                _ => json!({"kind":"Status","apiVersion":"v1","status":"Failure","reason":"NotFound","code":404}),
            }.to_string();
            requests.push(first);
            write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
        }
        requests
    });
    std::fs::write(
        temp.path().join("nyl.toml"),
        "[validation]\nenabled=true\n[validation.kubeconform]\nschema_locations=['schema.json']\n",
    )
    .unwrap();
    std::fs::write(
        temp.path().join("schema.json"),
        r#"{"type":"object","properties":{"data":{"type":"object","additionalProperties":{"type":"string"}}}}"#,
    )
    .unwrap();
    std::fs::write(
        temp.path().join("cluster.yaml"),
        format!(
            r#"apiVersion: k8s.gitops.nyl/v1
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
"#
        ),
    )
    .unwrap();
    std::fs::write(
        temp.path().join("kubeconfig"),
        format!(
            r#"apiVersion: v1
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
"#
        ),
    )
    .unwrap();
    std::fs::write(
        temp.path().join("manifest.yaml"),
        "apiVersion: v1\nkind: ConfigMap\nmetadata: {name: invalid, namespace: default}\ndata: {count: 123}\n",
    )
    .unwrap();
    let output = Command::new(assert_cmd::cargo::cargo_bin!("nyl"))
        .current_dir(temp.path())
        .env("KUBECONFIG", temp.path().join("kubeconfig"))
        .args([
            "apply",
            "manifest.yaml",
            "--no-cache",
            "--target",
            "test",
            "--name",
            "test",
            "--namespace",
            "default",
        ])
        .timeout(Duration::from_secs(30))
        .output()
        .unwrap();
    done.store(true, Ordering::Relaxed);
    let requests = worker.join().unwrap();
    assert_cmd::assert::Assert::new(output)
        .failure()
        .stderr(predicate::str::contains("1 resource(s) failed kubeconform validation"));
    assert!(!requests.is_empty(), "test must reach API discovery");
    assert!(
        requests.iter().all(|request| request.starts_with("GET ")),
        "unexpected API write: {requests:?}"
    );
}
