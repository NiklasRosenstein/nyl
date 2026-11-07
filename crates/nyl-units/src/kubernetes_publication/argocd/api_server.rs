//! Reading Applications from the Argo CD API server.
//!
//! Contract: [Kubernetes publications](../../../../../design/promotion.md#kubernetes-publications),
//! access method `api`. `GET <server>/api/v1/applications?appNamespace=<ns>`
//! with `Authorization: Bearer <token>` lists the Applications of a
//! namespace the token may read. The token is an Argo CD API token (an
//! account's or a project role's, with `get` on the instance's Applications)
//! that the unit admits through its `env`; it is sent only to the server the
//! ArgoCDInstance declares, and appears in no message.
//!
//! Argo CD answers HTTP 401 for a token that is invalid or has expired and
//! 403 for one that may not read, and omits Applications the token may not
//! `get` rather than saying so: a listing cannot tell an Application that
//! does not exist from one the token cannot see.

use std::collections::BTreeMap;
use std::time::Duration;

use reqwest::{StatusCode, Url};
use serde_json::Value;

use super::{ArgoQueries, ReadError, ReadErrorKind, ReadRequest, Reading, ReconcilerReader};
use crate::kubernetes_publication::reconciler::{block_on, Proof};

/// Reads Applications from the Argo CD API server.
pub struct ApiServerReader {
    server: Url,
    token: String,
    /// The proxy the unit admits as `HTTPS_PROXY`, if any.
    proxy: Option<String>,
    budget: Duration,
    /// The instance's namespace, where its AppProjects live and whose
    /// Applications Argo CD's policy names without a namespace.
    namespace: String,
}

impl ApiServerReader {
    /// A reader of `server` (the declared base URL) with the admitted
    /// `token`, which `token_env` names and `admitted` holds if the unit
    /// admits it. Each reading ends within `budget`.
    ///
    /// `proxy` is the proxy URL the unit admits as `HTTPS_PROXY`: the
    /// process's own proxy settings are never read, so only what the unit
    /// declares can steer where the token goes.
    pub fn new(
        server: &str,
        token_env: &str,
        admitted: Option<&str>,
        proxy: Option<&str>,
        budget: Duration,
    ) -> Result<Self, ReadError> {
        let token = admitted
            .map(str::trim)
            .filter(|token| !token.is_empty())
            .ok_or_else(|| {
                ReadError::new(
                    ReadErrorKind::Credentials,
                    format!(
                    "the Argo CD API token is not admitted: ArgoCDInstance spec.access.api.tokenEnv is {token_env}, \
                     so add {token_env} to the unit's env.passthrough (to take it from the runner) or env.secrets \
                     (to take it from the project's secrets provider)"
                ),
                )
            })?;
        let server = Url::parse(server).map_err(|error| {
            ReadError::new(
                ReadErrorKind::Declaration,
                format!("ArgoCDInstance spec.access.api.server {server:?} is not a URL: {error}"),
            )
        })?;
        Ok(Self {
            server,
            token: token.to_owned(),
            proxy: proxy
                .map(str::trim)
                .filter(|proxy| !proxy.is_empty())
                .map(str::to_owned),
            budget,
            namespace: super::DEFAULT_NAMESPACE.to_owned(),
        })
    }

    /// Read the instance that lives in `namespace`.
    #[must_use]
    pub fn in_namespace(mut self, namespace: &str) -> Self {
        namespace.clone_into(&mut self.namespace);
        self
    }

    /// The list endpoint for `namespace`: `<server>/api/v1/applications`
    /// (a path prefix on the server is kept), asking for its namespace.
    fn list_url(&self, namespace: &str) -> Url {
        let mut url = self.server.clone();
        let path = format!("{}/api/v1/applications", url.path().trim_end_matches('/'));
        url.set_path(&path);
        url.query_pairs_mut().clear().append_pair("appNamespace", namespace);
        url
    }
}

impl ApiServerReader {
    /// `GET <server>/api/v1/projects/<name>`: visible on success; Argo CD
    /// answers 404, or 403 for a token that may not get it, otherwise.
    pub fn project_exists(&self, _namespace: &str, name: &str) -> Result<bool, ReadError> {
        let mut url = self.server.clone();
        let path = format!("{}/api/v1/projects/{name}", url.path().trim_end_matches('/'));
        url.set_path(&path);
        let server = self.server.as_str().to_owned();
        let (token, budget, proxy) = (self.token.clone(), self.budget, self.proxy.clone());
        block_on(async move {
            let client = client(budget, proxy.as_deref())?;
            let unreachable = |error: reqwest::Error| {
                ReadError::new(
                    ReadErrorKind::Unreachable,
                    format!(
                        "cannot ask Argo CD at {server} for project {name}: {}",
                        error.without_url()
                    ),
                )
            };
            let response = client
                .get(url)
                .bearer_auth(&token)
                .header(reqwest::header::ACCEPT, "application/json")
                .send()
                .await
                .map_err(unreachable)?;
            let status = response.status();
            let body = response.text().await.map_err(unreachable)?;
            match status {
                status if status.is_success() => Ok(true),
                StatusCode::NOT_FOUND | StatusCode::FORBIDDEN => Ok(false),
                status => Err(failure(status, &body, &server)),
            }
        })?
    }

    /// Whether the token may `get` the Application Argo CD names `rbac_name`.
    pub fn can_get(&self, rbac_name: &str) -> Result<bool, ReadError> {
        let url = self.can_i_url(rbac_name);
        let server = self.server.as_str().to_owned();
        let (token, budget, proxy) = (self.token.clone(), self.budget, self.proxy.clone());
        block_on(async move {
            let client = client(budget, proxy.as_deref())?;
            let unreachable = |error: reqwest::Error| {
                ReadError::new(
                    ReadErrorKind::Unreachable,
                    format!(
                        "cannot ask Argo CD at {server} whether the token may get {rbac_name}: {}",
                        error.without_url()
                    ),
                )
            };
            let response = client
                .get(url)
                .bearer_auth(&token)
                .header(reqwest::header::ACCEPT, "application/json")
                .send()
                .await
                .map_err(unreachable)?;
            let status = response.status();
            let body = response.text().await.map_err(unreachable)?;
            if !status.is_success() {
                return Err(failure(status, &body, &server));
            }
            match serde_json::from_str::<Value>(&body)
                .ok()
                .and_then(|body| body.get("value").and_then(Value::as_str).map(str::to_owned))
            {
                Some(value) if value == "yes" => Ok(true),
                Some(value) if value == "no" => Ok(false),
                _ => Err(ReadError::new(
                    ReadErrorKind::Malformed,
                    format!("Argo CD at {server} answered the can-i question for {rbac_name} with neither yes nor no"),
                )),
            }
        })?
    }

    /// The Applications in `namespace` by name, as Argo CD serves them. One
    /// the token may not read is absent, as Argo CD's own API answers.
    pub fn applications(&self, namespace: &str) -> Result<BTreeMap<String, Value>, ReadError> {
        let url = self.list_url(namespace);
        let server = self.server.as_str().to_owned();
        let token = self.token.clone();
        let budget = self.budget;
        let proxy = self.proxy.clone();
        block_on(async move {
            let client = client(budget, proxy.as_deref())?;
            let unreachable = |error: reqwest::Error| {
                // `without_url` keeps the message free of anything but the cause.
                let error = error.without_url();
                ReadError::new(
                    ReadErrorKind::Unreachable,
                    format!(
                        "cannot read Argo CD at {server}: {error}; check the ArgoCDInstance's spec.access.api.server and \
                         that the runner can reach it"
                    ),
                )
            };
            let response = client
                .get(url)
                .bearer_auth(&token)
                .header(reqwest::header::ACCEPT, "application/json")
                .send()
                .await
                .map_err(unreachable)?;
            let status = response.status();
            let body = response.text().await.map_err(unreachable)?;
            if status.is_success() {
                applications(&body, &server)
            } else {
                Err(failure(status, &body, &server))
            }
        })?
    }
}

impl ArgoQueries for ApiServerReader {
    fn applications(&self, namespace: &str) -> Result<BTreeMap<String, Value>, ReadError> {
        ApiServerReader::applications(self, namespace)
    }

    fn can_get(&self, rbac_name: &str) -> Result<bool, ReadError> {
        ApiServerReader::can_get(self, rbac_name)
    }

    fn project_exists(&self, namespace: &str, name: &str) -> Result<bool, ReadError> {
        ApiServerReader::project_exists(self, namespace, name)
    }
}

impl ReconcilerReader for ApiServerReader {
    /// Argo CD omits what a token may not `get` from a listing and still
    /// answers success, so a missing Application means something only once
    /// `can-i` says the token could have seen it.
    fn read(&self, request: &ReadRequest<'_>) -> Result<Reading, ReadError> {
        super::read_argocd(self, &self.namespace, Proof::PerDeployment, request)
    }
}

impl ApiServerReader {
    /// `GET <server>/api/v1/account/can-i/applications/get/<rbac name>`: Argo
    /// CD's own answer, `yes` or `no`, about the token.
    fn can_i_url(&self, rbac_name: &str) -> Url {
        let mut url = self.server.clone();
        let path = format!(
            "{}/api/v1/account/can-i/applications/get/{rbac_name}",
            url.path().trim_end_matches('/')
        );
        url.set_path(&path);
        url
    }
}

/// The HTTP client of one reading: no ambient proxy, an admitted
/// `HTTPS_PROXY` only, no redirects (the token goes to the declared server or
/// nowhere), and the reading's budget.
fn client(budget: Duration, proxy: Option<&str>) -> Result<reqwest::Client, ReadError> {
    let mut builder = reqwest::Client::builder()
        // Ambient proxy settings are not read: only an admitted HTTPS_PROXY applies.
        .no_proxy();
    if let Some(proxy) = proxy {
        builder = builder.proxy(reqwest::Proxy::all(proxy).map_err(|error| {
            ReadError::new(
                ReadErrorKind::Declaration,
                format!("the admitted HTTPS_PROXY is not a proxy URL: {}", error.without_url()),
            )
        })?);
    }
    builder
        .timeout(budget)
        .connect_timeout(budget.min(Duration::from_secs(10)))
        .user_agent("nyl")
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| {
            ReadError::new(
                ReadErrorKind::Unreachable,
                format!("cannot build an HTTP client: {error}"),
            )
        })
}

/// The error of an unsuccessful answer: 401 (invalid or expired), 403, or any other status.
fn failure(status: StatusCode, body: &str, server: &str) -> ReadError {
    match status {
        StatusCode::UNAUTHORIZED => ReadError::new(
            ReadErrorKind::Rejected,
            format!(
                "Argo CD at {server} rejected the API token (HTTP 401{}): it is invalid or has expired; \
                 issue a new one and update the secret or variable the unit admits",
                server_message(body)
            ),
        ),
        StatusCode::FORBIDDEN => ReadError::new(
            ReadErrorKind::Forbidden,
            format!(
                "Argo CD at {server} denied the API token read access to Applications (HTTP 403{}): \
                 grant its account or project role `get` on applications",
                server_message(body)
            ),
        ),
        status => ReadError::new(
            ReadErrorKind::Unreachable,
            format!("Argo CD at {server} answered HTTP {status}{}", server_message(body)),
        ),
    }
}

/// `: <message>` from an Argo CD error body (`{"error": …, "message": …}`).
fn server_message(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|body| {
            body.get("error")
                .or_else(|| body.get("message"))
                .and_then(Value::as_str)
                .map(|message| format!(": {}", message.chars().take(200).collect::<String>()))
        })
        .unwrap_or_default()
}

/// The Applications of an `ApplicationList` body, by name. A body that is
/// not a list, or an item without a name, is malformed: a partial reading
/// never stands for a complete one.
fn applications(body: &str, server: &str) -> Result<BTreeMap<String, Value>, ReadError> {
    let malformed = |detail: String| {
        ReadError::new(
            ReadErrorKind::Malformed,
            format!("Argo CD at {server} answered something other than an Application list: {detail}"),
        )
    };
    let list: Value = serde_json::from_str(body).map_err(|error| malformed(error.to_string()))?;
    let items = match list.get("items") {
        // Argo CD serves an empty list as `"items": null`.
        Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items.clone(),
        _ => return Err(malformed("it has no items".to_owned())),
    };
    items
        .into_iter()
        .map(|item| {
            let name = item
                .pointer("/metadata/name")
                .and_then(Value::as_str)
                .ok_or_else(|| malformed("an item has no metadata.name".to_owned()))?
                .to_owned();
            Ok((name, item))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_the_list_endpoint_keeps_a_path_prefix_and_names_the_namespace() {
        let reader = ApiServerReader::new(
            "https://example.invalid/argocd/",
            "T",
            Some(" token \n"),
            None,
            Duration::from_secs(1),
        )
        .unwrap();
        assert_eq!(
            reader.list_url("team-a").as_str(),
            "https://example.invalid/argocd/api/v1/applications?appNamespace=team-a"
        );
        assert_eq!(reader.token, "token");
    }

    /// Catches a missing token falling back to anything but an error that
    /// names the variable and the unit entry that admits it.
    #[test]
    fn test_a_missing_token_names_the_variable_and_how_to_admit_it() {
        for admitted in [None, Some(""), Some("  \n")] {
            let error = ApiServerReader::new(
                "https://example.invalid",
                "MY_TOKEN",
                admitted,
                None,
                Duration::from_secs(1),
            )
            .err()
            .unwrap();
            assert_eq!(error.kind, ReadErrorKind::Credentials);
            assert!(
                error.message.contains("MY_TOKEN") && error.message.contains("env.passthrough"),
                "{error}"
            );
        }
    }

    /// Catches a partial list standing for a complete reading.
    #[test]
    fn test_a_body_that_is_not_a_complete_application_list_is_malformed() {
        let apps = applications(
            r#"{"items": [{"metadata": {"name": "web"}}, {"metadata": {"name": "api"}}]}"#,
            "s",
        )
        .unwrap();
        assert_eq!(apps.keys().collect::<Vec<_>>(), ["api", "web"]);
        assert!(applications(r#"{"items": null}"#, "s").unwrap().is_empty());
        for body in [
            "not json",
            "{}",
            r#"{"items": [{"metadata": {}}]}"#,
            r#"{"items": "x"}"#,
        ] {
            assert_eq!(
                applications(body, "s").unwrap_err().kind,
                ReadErrorKind::Malformed,
                "{body}"
            );
        }
    }
}
