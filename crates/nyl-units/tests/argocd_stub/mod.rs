//! A local HTTP server standing in for Argo CD in tests: the Argo CD API
//! server, or the Kubernetes API of the control-plane Cluster. It listens on
//! port 0 on the loopback address, answers each request through a handler,
//! and records every request. No test reaches a real cluster.

#![allow(dead_code)] // Each test binary uses part of the stub.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

/// One request the stub received.
#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    /// The path and query, as sent.
    pub target: String,
    /// Header names lowercased.
    pub headers: BTreeMap<String, String>,
}

impl Request {
    pub fn path(&self) -> &str {
        self.target.split('?').next().unwrap_or_default()
    }

    pub fn query(&self, name: &str) -> Option<String> {
        let query = self.target.split_once('?')?.1;
        query.split('&').find_map(|pair| {
            let (key, value) = pair.split_once('=')?;
            (key == name).then(|| value.to_owned())
        })
    }
}

/// A canned answer.
pub struct Response {
    pub status: u16,
    pub body: String,
}

impl Response {
    pub fn json(status: u16, body: &Value) -> Self {
        Self {
            status,
            body: body.to_string(),
        }
    }

    pub fn text(status: u16, body: &str) -> Self {
        Self {
            status,
            body: body.to_owned(),
        }
    }
}

type Handler = dyn Fn(&Request) -> Response + Send + Sync;

pub struct Stub {
    address: SocketAddr,
    requests: Arc<Mutex<Vec<Request>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Stub {
    pub fn start(handler: impl Fn(&Request) -> Response + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let handler: Arc<Handler> = Arc::new(handler);
        let thread = {
            let (requests, stop) = (requests.clone(), stop.clone());
            std::thread::spawn(move || {
                for connection in listener.incoming() {
                    if stop.load(Ordering::SeqCst) {
                        return;
                    }
                    let Ok(connection) = connection else { continue };
                    let (requests, handler) = (requests.clone(), handler.clone());
                    std::thread::spawn(move || serve(connection, &requests, &*handler));
                }
            })
        };
        Self {
            address,
            requests,
            stop,
            thread: Some(thread),
        }
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.address)
    }

    pub fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for Stub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the accept loop.
        let _ = TcpStream::connect(self.address);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn serve(mut connection: TcpStream, requests: &Mutex<Vec<Request>>, handler: &Handler) {
    let _ = connection.set_read_timeout(Some(std::time::Duration::from_secs(5)));
    let mut reader = BufReader::new(connection.try_clone().unwrap());
    let mut first = String::new();
    if reader.read_line(&mut first).unwrap_or(0) == 0 {
        return;
    }
    let mut headers = BTreeMap::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.trim_end().split_once(':') {
            headers.insert(name.to_ascii_lowercase(), value.trim().to_owned());
        }
    }
    if let Some(length) = headers
        .get("content-length")
        .and_then(|length| length.parse::<usize>().ok())
    {
        let mut body = vec![0; length];
        let _ = reader.read_exact(&mut body);
    }
    let mut parts = first.split_whitespace();
    let request = Request {
        method: parts.next().unwrap_or_default().to_owned(),
        target: parts.next().unwrap_or_default().to_owned(),
        headers,
    };
    requests.lock().unwrap().push(request.clone());
    let response = handler(&request);
    let reason = match response.status {
        200 => "OK",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "Status",
    };
    let _ = write!(
        connection,
        "HTTP/1.1 {} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        response.status,
        response.body.len(),
        response.body
    );
}

/// An Application as Argo CD serves it: the directory it sources, the
/// revisions its full syncs deployed (oldest first), and its live health.
pub fn application(name: &str, namespace: &str, path: &str, revisions: &[&str], health: &str) -> Value {
    let source = json!({"repoURL": "https://example.invalid/deploy.git", "path": path});
    application_with_source(name, namespace, &source, revisions, health)
}

/// [`application`] sourcing exactly `source` (its `path` names the directory).
pub fn application_with_source(name: &str, namespace: &str, source: &Value, revisions: &[&str], health: &str) -> Value {
    let history = revisions
        .iter()
        .enumerate()
        .map(|(index, revision)| {
            json!({
                "id": index,
                "revision": revision,
                "deployedAt": format!("2026-01-0{}T00:00:00Z", index + 1),
                "deployStartedAt": format!("2026-01-0{}T00:00:00Z", index + 1),
                "source": source,
            })
        })
        .collect::<Vec<_>>();
    json!({
        "apiVersion": "argoproj.io/v1alpha1",
        "kind": "Application",
        "metadata": {"name": name, "namespace": namespace},
        "spec": {"project": "default", "source": source},
        "status": {
            "sync": {"status": "OutOfSync"},
            "health": {"status": health},
            "history": history,
        },
    })
}

/// The Applications a stub serves, which a test may change while it runs.
pub type Applications = Arc<Mutex<Vec<Value>>>;

pub fn applications(applications: Vec<Value>) -> Applications {
    Arc::new(Mutex::new(applications))
}

/// The Argo CD API server: lists `applications` of the namespace in
/// `appNamespace` for requests that carry `Bearer <token>`, else HTTP 401.
pub fn argocd_api(token: &'static str, applications: Applications) -> Stub {
    argocd_api_with_policy(token, applications, applications_none())
}

/// The policy names (`<project>/<name>`, `<project>/<namespace>/<name>`) the
/// token may not get, whatever the list holds; `*` denies every one.
pub type Denied = Arc<Mutex<Vec<String>>>;

pub fn applications_none() -> Denied {
    Arc::new(Mutex::new(Vec::new()))
}

/// [`argocd_api`] with a policy: like Argo CD, the list answers success with
/// whatever `applications` holds (a token that sees nothing gets an empty one),
/// and `can-i` answers `no` for the names in `denied`.
pub fn argocd_api_with_policy(token: &'static str, applications: Applications, denied: Denied) -> Stub {
    Stub::start(move |request| {
        if request.headers.get("authorization").map(String::as_str) != Some(&format!("Bearer {token}")) {
            return Response::json(
                401,
                &json!({"error": "invalid session: token has invalid claims: token is expired", "code": 16}),
            );
        }
        if let Some(name) = request.path().strip_prefix("/api/v1/account/can-i/applications/get/") {
            let no = denied
                .lock()
                .unwrap()
                .iter()
                .any(|denied| denied == "*" || denied == name);
            return Response::json(200, &json!({"value": if no { "no" } else { "yes" }}));
        }
        if request.path() != "/api/v1/applications" {
            return Response::json(404, &json!({"error": "not found", "code": 5}));
        }
        let namespace = request.query("appNamespace");
        let items = applications
            .lock()
            .unwrap()
            .iter()
            .filter(|application| {
                namespace.as_deref().is_none_or(|namespace| {
                    application.pointer("/metadata/namespace").and_then(Value::as_str) == Some(namespace)
                })
            })
            .cloned()
            .collect::<Vec<_>>();
        Response::json(200, &json!({"metadata": {}, "items": items}))
    })
}

/// The Kubernetes API of a cluster that runs Argo CD: serves the
/// `applications.argoproj.io` list of each namespace.
pub fn kube_api(applications: Applications) -> Stub {
    Stub::start(move |request| {
        let Some(namespace) = request
            .path()
            .strip_prefix("/apis/argoproj.io/v1alpha1/namespaces/")
            .and_then(|rest| rest.strip_suffix("/applications"))
        else {
            return Response::json(
                404,
                &json!({"kind": "Status", "apiVersion": "v1", "status": "Failure", "reason": "NotFound", "code": 404,
                        "message": format!("{} not found", request.path())}),
            );
        };
        let items = applications
            .lock()
            .unwrap()
            .iter()
            .filter(|application| application.pointer("/metadata/namespace").and_then(Value::as_str) == Some(namespace))
            .cloned()
            .collect::<Vec<_>>();
        Response::json(
            200,
            &json!({"apiVersion": "argoproj.io/v1alpha1", "kind": "ApplicationList", "metadata": {"resourceVersion": "1"}, "items": items}),
        )
    })
}

/// A kubeconfig with context `context` pointing at `server`, authenticating
/// with a static token, written to `path`.
pub fn kubeconfig(path: &Path, context: &str, server: &str) {
    std::fs::write(
        path,
        format!(
            "apiVersion: v1\nkind: Config\nclusters:\n- name: stub\n  cluster: {{server: '{server}'}}\n\
             users:\n- name: stub\n  user: {{token: stub-token}}\n\
             contexts:\n- name: {context}\n  context: {{cluster: stub, user: stub}}\ncurrent-context: other\n"
        ),
    )
    .unwrap();
}
