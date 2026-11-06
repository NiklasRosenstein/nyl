//! The two ways of reading Argo CD, each against a local fake: the Argo CD
//! API server (with a token) and the Kubernetes API of the control-plane
//! Cluster (through an admitted kubeconfig and the Cluster's declared
//! context). Both return Applications as Argo CD serves them, so everything
//! after the reading is shared and tested once (`evaluate`, `live`).
//!
//! Contract: [Kubernetes publications](../../../design/promotion.md#kubernetes-publications),
//! access methods.

mod argocd_stub;

use std::collections::BTreeMap;
use std::time::Duration;

use argocd_stub::{
    application, applications, applications_none, argocd_api, argocd_api_with_policy, kube_api, kubeconfig, Response,
    Stub,
};
use nyl_units::kubernetes_publication::argocd::{ApiServerReader, ArgoReader, KubeApiReader, ReadErrorKind};
use serde_json::{json, Value};

const BUDGET: Duration = Duration::from_secs(10);

fn apps() -> Vec<Value> {
    vec![
        application("web", "argocd", "dev/web/web", &["c1"], "Healthy"),
        application("api", "argocd", "dev/api/api", &["c1"], "Degraded"),
        application("other", "team-a", "dev/other", &["c1"], "Healthy"),
    ]
}

fn names(applications: &BTreeMap<String, Value>) -> Vec<&str> {
    applications.keys().map(String::as_str).collect()
}

/// Catches the API method sending no bearer token, asking for another
/// namespace's Applications, or returning ones it was not asked for.
#[test]
fn test_api_server_reader_lists_the_namespace_with_the_bearer_token() {
    let server = argocd_api("s3cret", applications(apps()));
    let reader = ApiServerReader::new(&server.url(), "ARGOCD_AUTH_TOKEN", Some("s3cret"), None, BUDGET).unwrap();
    assert_eq!(names(&reader.applications("argocd").unwrap()), ["api", "web"]);
    assert_eq!(names(&reader.applications("team-a").unwrap()), ["other"]);
    assert!(reader.applications("empty").unwrap().is_empty());
    let requests = server.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0].method, "GET");
    assert_eq!(requests[0].path(), "/api/v1/applications");
    assert_eq!(requests[0].query("appNamespace").as_deref(), Some("argocd"));
    assert_eq!(requests[0].headers["authorization"], "Bearer s3cret");
}

/// Catches token expiry, a token without read access, a failing server, a
/// server that is not there, and a body that is no Application list being
/// reported alike or as success: each is a distinct failure with its fix,
/// and none leaks the token.
#[test]
fn test_api_server_reader_reports_each_failure_with_its_fix_and_never_the_token() {
    let server = argocd_api("s3cret", applications(apps()));
    let expired =
        ApiServerReader::new(&server.url(), "ARGOCD_AUTH_TOKEN", Some("expired-token"), None, BUDGET).unwrap();
    let error = expired.applications("argocd").unwrap_err();
    assert_eq!(error.kind, ReadErrorKind::Rejected);
    assert!(
        error.message.contains("invalid or has expired") && error.message.contains("token is expired"),
        "{error}"
    );
    assert!(!error.message.contains("expired-token"), "{error}");

    let forbidden = Stub::start(|_| Response::json(403, &json!({"error": "permission denied: applications, get"})));
    let error = ApiServerReader::new(&forbidden.url(), "T", Some("tok"), None, BUDGET)
        .unwrap()
        .applications("argocd")
        .unwrap_err();
    assert_eq!(error.kind, ReadErrorKind::Forbidden);
    assert!(error.message.contains("grant its account or project role"), "{error}");

    let broken = Stub::start(|_| Response::text(500, "boom"));
    let error = ApiServerReader::new(&broken.url(), "T", Some("tok"), None, BUDGET)
        .unwrap()
        .applications("argocd")
        .unwrap_err();
    assert_eq!(error.kind, ReadErrorKind::Unreachable);
    assert!(error.message.contains("HTTP 500"), "{error}");

    let wrong = Stub::start(|_| Response::text(200, "<html>login</html>"));
    let error = ApiServerReader::new(&wrong.url(), "T", Some("tok"), None, BUDGET)
        .unwrap()
        .applications("argocd")
        .unwrap_err();
    assert_eq!(error.kind, ReadErrorKind::Malformed);

    let gone = {
        let stub = Stub::start(|_| Response::text(200, "{}"));
        let url = stub.url();
        drop(stub);
        url
    };
    let error = ApiServerReader::new(&gone, "T", Some("tok"), None, BUDGET)
        .unwrap()
        .applications("argocd")
        .unwrap_err();
    assert_eq!(error.kind, ReadErrorKind::Unreachable);
    assert!(error.message.contains("spec.access.api.server"), "{error}");
}

/// Catches the Kubernetes method listing the wrong group or namespace, or
/// using the kubeconfig's current context instead of the declared one:
/// `current-context` names a context that does not exist, so a reader that
/// fell back to it would fail.
#[test]
fn test_kube_api_reader_lists_applications_through_the_declared_context() {
    let server = kube_api(applications(apps()));
    let directory = tempfile::TempDir::new().unwrap();
    let file = directory.path().join("kubeconfig");
    kubeconfig(&file, "argocd-admin", &server.url());
    let reader = KubeApiReader::new("control", Some("argocd-admin"), Some(file.to_str().unwrap()), BUDGET).unwrap();
    assert_eq!(names(&reader.applications("argocd").unwrap()), ["api", "web"]);
    assert_eq!(names(&reader.applications("team-a").unwrap()), ["other"]);
    let paths = server
        .requests()
        .iter()
        .map(|request| request.path().to_owned())
        .collect::<Vec<_>>();
    assert!(
        paths.contains(&"/apis/argoproj.io/v1alpha1/namespaces/argocd/applications".to_owned()),
        "{paths:?}"
    );
    // The listed Application keeps its status for the shared judgement.
    let web = &reader.applications("argocd").unwrap()["web"];
    assert_eq!(web["status"]["health"]["status"], "Healthy");
    assert_eq!(web["status"]["history"][0]["revision"], "c1");
}

/// Catches the Kubernetes method guessing what the declaration did not say:
/// no context, no admitted kubeconfig, an unreadable file, and a context the
/// kubeconfig lacks each fail before any request, naming the fix.
#[test]
fn test_kube_api_reader_never_falls_back_to_ambient_configuration() {
    let directory = tempfile::TempDir::new().unwrap();
    let file = directory.path().join("kubeconfig");
    kubeconfig(&file, "argocd-admin", "http://127.0.0.1:1");
    let path = file.to_str().unwrap();

    let error = KubeApiReader::new("control", None, Some(path), BUDGET).err().unwrap();
    assert_eq!(error.kind, ReadErrorKind::Declaration);
    assert!(
        error.message.contains("spec.live.context") && error.message.contains("never uses the current context"),
        "{error}"
    );

    let error = KubeApiReader::new("control", Some("argocd-admin"), None, BUDGET)
        .err()
        .unwrap();
    assert_eq!(error.kind, ReadErrorKind::Credentials);
    assert!(
        error.message.contains("env.passthrough") && error.message.contains("KUBECONFIG"),
        "{error}"
    );

    let missing = KubeApiReader::new("control", Some("argocd-admin"), Some("/nonexistent/kubeconfig"), BUDGET).unwrap();
    assert_eq!(
        missing.applications("argocd").unwrap_err().kind,
        ReadErrorKind::Declaration
    );

    let other = KubeApiReader::new("control", Some("elsewhere"), Some(path), BUDGET).unwrap();
    let error = other.applications("argocd").unwrap_err();
    assert_eq!(error.kind, ReadErrorKind::Declaration);
    assert!(error.message.contains("has no context \"elsewhere\""), "{error}");
}

/// Catches an RBAC denial, a cluster without Argo CD's CRD, and an
/// unreachable API server being reported as one failure.
#[test]
fn test_kube_api_reader_reports_denial_missing_crd_and_unreachable() {
    let directory = tempfile::TempDir::new().unwrap();
    let file = directory.path().join("kubeconfig");
    let reader = |server: &str| {
        kubeconfig(&file, "ctx", server);
        KubeApiReader::new("control", Some("ctx"), Some(file.to_str().unwrap()), BUDGET).unwrap()
    };

    let denied = Stub::start(|_| {
        Response::json(
            403,
            &json!({"kind": "Status", "apiVersion": "v1", "status": "Failure", "reason": "Forbidden", "code": 403, "message": "forbidden"}),
        )
    });
    let error = reader(&denied.url()).applications("argocd").unwrap_err();
    assert_eq!(error.kind, ReadErrorKind::Forbidden);
    assert!(error.message.contains("`list` on applications"), "{error}");

    let unauthorized = Stub::start(|_| {
        Response::json(
            401,
            &json!({"kind": "Status", "apiVersion": "v1", "status": "Failure", "reason": "Unauthorized", "code": 401, "message": "Unauthorized"}),
        )
    });
    assert_eq!(
        reader(&unauthorized.url()).applications("argocd").unwrap_err().kind,
        ReadErrorKind::Rejected
    );

    let no_crd = kube_api(applications(Vec::new()));
    // The stub answers 404 for any other group.
    let not_argo = Stub::start(|_| {
        Response::json(
            404,
            &json!({"kind": "Status", "apiVersion": "v1", "status": "Failure", "reason": "NotFound", "code": 404, "message": "not found"}),
        )
    });
    let error = reader(&not_argo.url()).applications("argocd").unwrap_err();
    assert_eq!(error.kind, ReadErrorKind::Declaration);
    assert!(error.message.contains("serves no applications.argoproj.io"), "{error}");
    assert!(reader(&no_crd.url()).applications("argocd").unwrap().is_empty());

    let gone = {
        let stub = Stub::start(|_| Response::text(200, "{}"));
        let url = stub.url();
        drop(stub);
        url
    };
    assert_eq!(
        reader(&gone).applications("argocd").unwrap_err().kind,
        ReadErrorKind::Unreachable
    );
}

/// Catches the API reader answering `can_get` from anything but Argo CD's own
/// `can-i`: the question carries the token and the policy name, and `no`,
/// an error, and an unintelligible answer are each told apart.
#[test]
fn test_api_server_reader_asks_argocd_whether_the_token_may_get_an_application() {
    let denied = applications_none();
    let server = argocd_api_with_policy("s3cret", applications(Vec::new()), denied.clone());
    let reader = ApiServerReader::new(&server.url(), "ARGOCD_AUTH_TOKEN", Some("s3cret"), None, BUDGET).unwrap();
    assert!(reader.can_get("default/web").unwrap());
    denied.lock().unwrap().push("default/team-a/web".to_owned());
    assert!(!reader.can_get("default/team-a/web").unwrap());
    assert!(reader.can_get("default/api").unwrap());
    let request = &server.requests()[0];
    assert_eq!(request.path(), "/api/v1/account/can-i/applications/get/default/web");
    assert_eq!(request.headers["authorization"], "Bearer s3cret");

    let expired = ApiServerReader::new(&server.url(), "T", Some("expired"), None, BUDGET).unwrap();
    assert_eq!(
        expired.can_get("default/web").unwrap_err().kind,
        ReadErrorKind::Rejected
    );
    let odd = Stub::start(|_| Response::json(200, &json!({"value": "maybe"})));
    let reader = ApiServerReader::new(&odd.url(), "T", Some("tok"), None, BUDGET).unwrap();
    assert_eq!(
        reader.can_get("default/web").unwrap_err().kind,
        ReadErrorKind::Malformed
    );
}
