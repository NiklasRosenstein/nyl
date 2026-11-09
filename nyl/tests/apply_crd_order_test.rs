//! `nyl apply` must apply CustomResourceDefinitions and Namespaces before the
//! resources that need them, whatever order the manifest lists them in.

use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::{json, Value};
use tempfile::TempDir;

/// What the stub cluster holds, as a real API server would after each write.
#[derive(Default)]
struct Cluster {
    namespaces: HashSet<String>,
    /// The CustomResourceDefinition was applied, so its kind is served.
    widgets_served: bool,
}

/// A Kubernetes stub that, like a real API server, rejects writes into a
/// namespace that does not exist and serves a custom kind only once its
/// CustomResourceDefinition was applied. Returns the writes it served, in order.
fn serve(listener: TcpListener, done: &Arc<AtomicBool>) -> std::thread::JoinHandle<Vec<String>> {
    let done = done.clone();
    std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(40);
        let mut writes = Vec::new();
        let mut cluster = Cluster::default();
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

            let (code, body) = response(&method, &path, &mut cluster);
            if method == "PATCH" && code == 200 {
                writes.push(path);
            }
            let status = if code == 200 { "200 OK" } else { "404 Not Found" };
            write!(
                socket,
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        }
        writes
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

fn ok(body: &Value) -> (u16, String) {
    (200, body.to_string())
}

fn resource_list(group_version: &str, resources: &Value) -> (u16, String) {
    ok(
        &json!({"kind": "APIResourceList", "apiVersion": "v1", "groupVersion": group_version,
               "resources": resources}),
    )
}

fn group(name: &str) -> Value {
    let version = json!({"groupVersion": format!("{name}/v1"), "version": "v1"});
    json!({"name": name, "versions": [version], "preferredVersion": version})
}

fn response(method: &str, path: &str, cluster: &mut Cluster) -> (u16, String) {
    let verbs = json!(["get", "list", "create", "patch"]);
    match path {
        "/version" => {
            return ok(
                &json!({"major": "1", "minor": "31", "gitVersion": "v1.31.4", "platform": "test",
                        "buildDate": "2025-01-01T00:00:00Z", "compiler": "rustc", "gitCommit": "0",
                        "gitTreeState": "clean", "goVersion": "go1.23"}),
            )
        }
        "/api" => {
            return ok(
                &json!({"kind": "APIVersions", "apiVersion": "v1", "versions": ["v1"], "serverAddressByClientCIDRs": []}),
            )
        }
        "/apis" => {
            let mut groups = vec![group("apiextensions.k8s.io")];
            if cluster.widgets_served {
                groups.push(group("example.com"));
            }
            return ok(&json!({"kind": "APIGroupList", "apiVersion": "v1", "groups": groups}));
        }
        "/api/v1" => {
            return resource_list(
                "v1",
                &json!([
                    {"name": "namespaces", "singularName": "", "kind": "Namespace", "namespaced": false, "verbs": verbs},
                    {"name": "configmaps", "singularName": "", "kind": "ConfigMap", "namespaced": true, "verbs": verbs},
                    {"name": "secrets", "singularName": "", "kind": "Secret", "namespaced": true, "verbs": verbs}
                ]),
            )
        }
        "/apis/apiextensions.k8s.io/v1" => {
            return resource_list(
                "apiextensions.k8s.io/v1",
                &json!([{"name": "customresourcedefinitions", "singularName": "", "kind": "CustomResourceDefinition",
                         "namespaced": false, "verbs": verbs}]),
            )
        }
        "/apis/example.com/v1" if cluster.widgets_served => {
            return resource_list(
                "example.com/v1",
                &json!([{"name": "widgets", "singularName": "", "kind": "Widget", "namespaced": true, "verbs": verbs}]),
            )
        }
        "/apis/apiextensions.k8s.io/v1/customresourcedefinitions" => {
            return ok(
                &json!({"apiVersion": "apiextensions.k8s.io/v1", "kind": "CustomResourceDefinitionList",
                              "metadata": {"resourceVersion": "1"}, "items": []}),
            )
        }
        _ => {}
    }

    if let Some(name) = path.strip_prefix("/apis/apiextensions.k8s.io/v1/customresourcedefinitions/") {
        if method != "PATCH" {
            return not_found(&format!("customresourcedefinitions \"{name}\" not found"));
        }
        cluster.widgets_served = true;
        return ok(
            &json!({"apiVersion": "apiextensions.k8s.io/v1", "kind": "CustomResourceDefinition",
                          "metadata": {"name": name, "resourceVersion": "1"}}),
        );
    }

    let (api_version, rest) = match path.strip_prefix("/apis/example.com/v1/namespaces/") {
        Some(rest) if cluster.widgets_served => ("example.com/v1", rest),
        Some(_) => return not_found(&format!("{path} not found")),
        None => match path.strip_prefix("/api/v1/namespaces/") {
            Some(rest) => ("v1", rest),
            None => return not_found(&format!("{path} not found")),
        },
    };
    let mut segments = rest.splitn(2, '/');
    let namespace = segments.next().unwrap_or_default().to_owned();
    let Some(subpath) = segments.next() else {
        // The Namespace object itself.
        if method == "PATCH" {
            cluster.namespaces.insert(namespace.clone());
        } else if !cluster.namespaces.contains(&namespace) {
            return not_found(&format!("namespaces \"{namespace}\" not found"));
        }
        return ok(&json!({"apiVersion": "v1", "kind": "Namespace",
                          "metadata": {"name": namespace, "resourceVersion": "1"}}));
    };

    if !cluster.namespaces.contains(&namespace) {
        return not_found(&format!("namespaces \"{namespace}\" not found"));
    }
    let mut subsegments = subpath.splitn(2, '/');
    let resource = subsegments.next().unwrap_or_default().to_owned();
    let kind = match resource.as_str() {
        "configmaps" => "ConfigMap",
        "secrets" => "Secret",
        "widgets" => "Widget",
        _ => return not_found(&format!("{path} not found")),
    };
    match subsegments.next() {
        // A collection request is either a list or a create.
        None if method == "GET" => ok(&json!({"apiVersion": api_version, "kind": format!("{kind}List"),
                                              "metadata": {"resourceVersion": "1"}, "items": []})),
        None => ok(&json!({"apiVersion": api_version, "kind": kind,
                           "metadata": {"name": "created", "namespace": namespace, "resourceVersion": "1"}})),
        Some(name) if method == "GET" => not_found(&format!("{resource} \"{name}\" not found")),
        Some(name) => ok(&json!({"apiVersion": api_version, "kind": kind,
                                 "metadata": {"name": name, "namespace": namespace, "resourceVersion": "1"}})),
    }
}

/// A project whose manifest lists the custom resource and the object in the new
/// Namespace before the CustomResourceDefinition and the Namespace they need.
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
    std::fs::write(
        temp.path().join("manifest.yaml"),
        r"apiVersion: k8s.nyl/v1
kind: Release
metadata: {name: example, namespace: release-ns}
spec: {additionalNamespaces: [app]}
---
apiVersion: example.com/v1
kind: Widget
metadata: {name: first}
spec: {size: 1}
---
apiVersion: v1
kind: ConfigMap
metadata: {name: settings, namespace: app}
data: {a: b}
---
apiVersion: v1
kind: Namespace
metadata: {name: app}
---
apiVersion: apiextensions.k8s.io/v1
kind: CustomResourceDefinition
metadata: {name: widgets.example.com}
spec:
  group: example.com
  scope: Namespaced
  names: {kind: Widget, plural: widgets}
  versions:
  - name: v1
    served: true
    storage: true
    schema: {openAPIV3Schema: {type: object, x-kubernetes-preserve-unknown-fields: true}}
",
    )
    .unwrap();
    temp
}

#[test]
fn applies_crds_and_namespaces_before_the_resources_that_need_them() {
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
        // CI sets colored output on; the assertions below read plain text.
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
    done.store(true, Ordering::Relaxed);
    let writes = worker.join().unwrap();

    // The custom resource without a namespace takes the Release's, which its CRD
    // in the manifest declares it belongs in.
    assert_cmd::assert::Assert::new(output)
        .success()
        .stdout(predicate::str::contains("+ Widget release-ns/first"))
        .stdout(predicate::str::contains("✗").not());

    let position = |path: &str| {
        writes
            .iter()
            .position(|write| write == path)
            .unwrap_or_else(|| panic!("{path} was never applied: {writes:?}"))
    };
    let crd = position("/apis/apiextensions.k8s.io/v1/customresourcedefinitions/widgets.example.com");
    let namespace = position("/api/v1/namespaces/app");
    let config_map = position("/api/v1/namespaces/app/configmaps/settings");
    let widget = position("/apis/example.com/v1/namespaces/release-ns/widgets/first");
    assert!(
        crd < namespace && namespace < config_map && namespace < widget,
        "the CRD and the Namespace come first: {writes:?}"
    );
}
