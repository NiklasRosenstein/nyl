use std::collections::HashSet;
use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use reqwest::{Client, Method, Url};
use serde_json::{json, Value};

use super::context::{append, Context, Provider};
use super::error;
use crate::{NylError, Result};

pub(super) struct UpsertResult {
    pub(super) outcome: &'static str,
    pub(super) url: Url,
}

struct Api {
    context: Context,
    client: Client,
}

struct ApiFailure {
    error: NylError,
    uncertain: bool,
}

impl From<ApiFailure> for NylError {
    fn from(value: ApiFailure) -> Self {
        value.error
    }
}

enum Account {
    GithubLogin(String),
    Id(u64),
}

struct Comment {
    id: u64,
    body: String,
}

pub(super) async fn upsert(
    context: Context,
    token: String,
    marker: &str,
    body: &str,
    dry_run: bool,
) -> Result<UpsertResult> {
    let api = Api::new(context, token)?;
    let account = api.account().await?;
    let request_url = api.request_url().await?;
    if let Some(comment) = api.find(&account, marker).await? {
        return api.update(comment, body, dry_run, &request_url).await;
    }
    if dry_run {
        return Ok(UpsertResult {
            outcome: "created",
            url: request_url,
        });
    }
    for attempt in 0..2 {
        match api
            .send(
                Method::POST,
                api.comments_url(),
                Some(json!({"body": body})),
                "create comment",
            )
            .await
            .and_then(|value| comment_id(&value).map_err(|error| ApiFailure { error, uncertain: true }))
        {
            Ok(id) => {
                return Ok(UpsertResult {
                    outcome: "created",
                    url: api.comment_url(&request_url, id),
                })
            }
            Err(failure) if failure.uncertain => {
                // A failed response does not prove that the server rejected the write.
                tokio::time::sleep(Duration::from_millis(250)).await;
                let found = api.find(&account, marker).await.map_err(|_| error(
                    "Create outcome is uncertain and rechecking comments failed. Inspect the request before rerunning; a comment may already exist."
                ))?;
                if let Some(comment) = found {
                    return api.update(comment, body, false, &request_url).await;
                }
                if attempt == 1 {
                    return Err(error("Create outcome is uncertain after one retry. No matching comment is visible yet. Inspect the request before rerunning."));
                }
            }
            Err(failure) => return Err(failure.into()),
        }
    }
    unreachable!("both create attempts return or retry")
}

impl Api {
    fn new(context: Context, token: String) -> Result<Self> {
        let mut headers = HeaderMap::new();
        let (name, value) = match context.provider {
            Provider::Gitlab => (reqwest::header::HeaderName::from_static("private-token"), token),
            Provider::Github => (AUTHORIZATION, format!("Bearer {token}")),
            Provider::Forgejo => (AUTHORIZATION, format!("token {token}")),
        };
        let mut value = HeaderValue::from_str(&value).map_err(|_| {
            error("Credential contains invalid HTTP header characters. Check the token environment variable.")
        })?;
        value.set_sensitive(true);
        headers.insert(name, value);
        let client = Client::builder()
            .default_headers(headers)
            .user_agent(concat!("nyl/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .build()
            .map_err(|_| error("Cannot initialize forge HTTP client. Check TLS and proxy configuration."))?;
        Ok(Self { context, client })
    }

    async fn send(
        &self,
        method: Method,
        url: Url,
        body: Option<Value>,
        operation: &str,
    ) -> std::result::Result<Value, ApiFailure> {
        let mut request = self.client.request(method, url);
        if let Some(body) = body {
            request = request.header(CONTENT_TYPE, "application/json").body(body.to_string());
        }
        // Response bodies and transport errors may echo credentials or Markdown.
        let response = request.send().await.map_err(|_| ApiFailure {
            error: error(format!("Cannot {operation}: connection failed or timed out. Check --server-url, TLS certificates, proxy settings, and connectivity.")),
            uncertain: true,
        })?;
        let status = response.status();
        if !status.is_success() {
            let hint = match status.as_u16() {
                401 | 403 => self.context.provider.credential_hint(),
                404 => "Check --repository, --request, and --server-url, and confirm the token can access this repository.",
                413 => "Content is too large for this server. Shorten the report and link to a CI artifact.",
                400 | 422 => "The server rejected the content. Check its comment size limit, shorten the report, and verify the request is writable.",
                429 => "API rate limit reached. Wait for the server's rate limit to reset before rerunning.",
                300..=399 => "Redirects are disabled to protect credentials. Set --server-url to the canonical forge web URL.",
                500..=599 => "The forge is unavailable. Check its service status before rerunning.",
                _ => "Check forge access permissions and server availability.",
            };
            return Err(ApiFailure {
                error: error(format!("Cannot {operation}: HTTP {}. {hint}", status.as_u16())),
                uncertain: status.is_server_error() || status.as_u16() == 408,
            });
        }
        let bytes = response.bytes().await.map_err(|_| ApiFailure {
            error: error(format!(
                "Cannot {operation}: incomplete API response. Check server connectivity."
            )),
            uncertain: true,
        })?;
        serde_json::from_slice(&bytes).map_err(|_| ApiFailure {
            error: error(format!(
                "Cannot {operation}: invalid JSON response. Verify --server-url and forge API compatibility."
            )),
            uncertain: true,
        })
    }

    async fn account(&self) -> Result<Account> {
        if self.context.provider == Provider::Github {
            // GraphQL viewer also identifies GitHub App installation tokens, including GITHUB_TOKEN.
            let value = self
                .send(
                    Method::POST,
                    self.context.graphql_url(),
                    Some(json!({"query": "query { viewer { login } }"})),
                    "identify authenticated account",
                )
                .await?;
            if value
                .get("errors")
                .is_some_and(|v| v.as_array().is_none_or(|v| !v.is_empty()))
            {
                return Err(error(format!(
                    "Cannot identify authenticated GitHub account. {}",
                    self.context.provider.credential_hint()
                )));
            }
            let login = value
                .pointer("/data/viewer/login")
                .and_then(Value::as_str)
                .filter(|v| !v.is_empty())
                .ok_or_else(|| {
                    error("GitHub did not return the authenticated account. Check the token and GraphQL API access.")
                })?;
            Ok(Account::GithubLogin(login.to_owned()))
        } else {
            let value = self
                .send(
                    Method::GET,
                    append(&self.context.api_url(), &["user"]),
                    None,
                    "identify authenticated account",
                )
                .await?;
            Ok(Account::Id(comment_id(&value)?))
        }
    }

    fn comments_url(&self) -> Url {
        let number = self.context.request.to_string();
        if self.context.provider == Provider::Gitlab {
            append(
                &self.context.api_url(),
                &["projects", &self.context.repository, "merge_requests", &number, "notes"],
            )
        } else {
            let mut parts = vec!["repos"];
            parts.extend(self.context.repository.split('/'));
            parts.extend(["issues", &number, "comments"]);
            append(&self.context.api_url(), &parts)
        }
    }

    fn update_url(&self, id: u64) -> Url {
        let id = id.to_string();
        if self.context.provider == Provider::Gitlab {
            append(&self.comments_url(), &[&id])
        } else {
            let mut parts = vec!["repos"];
            parts.extend(self.context.repository.split('/'));
            parts.extend(["issues", "comments", &id]);
            append(&self.context.api_url(), &parts)
        }
    }

    async fn request_url(&self) -> Result<Url> {
        if self.context.provider == Provider::Gitlab {
            let url = append(
                &self.context.api_url(),
                &[
                    "projects",
                    &self.context.repository,
                    "merge_requests",
                    &self.context.request.to_string(),
                ],
            );
            let value = self.send(Method::GET, url, None, "resolve merge request URL").await?;
            let url = value["web_url"].as_str().and_then(|v| Url::parse(v).ok()).filter(|url| {
                url.origin() == self.context.server.origin() && url.username().is_empty() && url.password().is_none()
                    && url.query().is_none() && url.fragment().is_none()
            }).ok_or_else(|| error("GitLab returned an invalid merge request web URL. Check the instance's external URL configuration."))?;
            Ok(url)
        } else {
            let number = self.context.request.to_string();
            let mut parts: Vec<_> = self.context.repository.split('/').collect();
            parts.extend([
                if self.context.provider == Provider::Github {
                    "pull"
                } else {
                    "pulls"
                },
                &number,
            ]);
            Ok(append(&self.context.server, &parts))
        }
    }

    fn comment_url(&self, request: &Url, id: u64) -> Url {
        let mut url = request.clone();
        let fragment = if self.context.provider == Provider::Gitlab {
            format!("note_{id}")
        } else {
            format!("issuecomment-{id}")
        };
        url.set_fragment(Some(&fragment));
        url
    }

    async fn find(&self, account: &Account, marker: &str) -> Result<Option<Comment>> {
        let mut found = None;
        let mut seen = HashSet::new();
        // Stop only at an empty page: self-hosted instances can cap page sizes below 100.
        for page in 1..=10_000 {
            let mut url = self.comments_url();
            let size = if self.context.provider == Provider::Forgejo {
                "limit"
            } else {
                "per_page"
            };
            url.query_pairs_mut()
                .append_pair(size, "100")
                .append_pair("page", &page.to_string());
            let value = self.send(Method::GET, url, None, "list comments").await?;
            let comments = value
                .as_array()
                .ok_or_else(|| error("Invalid comment list response. Check forge API compatibility."))?;
            if comments.is_empty() {
                return Ok(found);
            }
            for value in comments {
                let id = comment_id(value)?;
                if !seen.insert(id) {
                    return Err(error("Comment pagination repeated an ID. The request may be changing concurrently; rerun after other comment jobs finish."));
                }
                let owned = match account {
                    Account::GithubLogin(login) => value
                        .pointer("/user/login")
                        .and_then(Value::as_str)
                        .is_some_and(|v| v.eq_ignore_ascii_case(login)),
                    Account::Id(id) => {
                        let field = if self.context.provider == Provider::Gitlab {
                            "/author/id"
                        } else {
                            "/user/id"
                        };
                        value.pointer(field).and_then(Value::as_u64) == Some(*id)
                    }
                };
                if !owned || value["system"].as_bool() == Some(true) {
                    continue;
                }
                if let Some(body) = value["body"]
                    .as_str()
                    .filter(|body| body.lines().next() == Some(marker))
                {
                    if found.is_some() {
                        return Err(error("Multiple comments owned by this account have the same key. Remove duplicate comments manually and serialize CI jobs for this request/key."));
                    }
                    found = Some(Comment {
                        id,
                        body: body.to_owned(),
                    });
                }
            }
        }
        Err(error(
            "Comment pagination exceeded 10,000 pages. Check the server's pagination support; no comment was written.",
        ))
    }

    async fn update(&self, comment: Comment, body: &str, dry_run: bool, request_url: &Url) -> Result<UpsertResult> {
        let outcome = if comment.body == body { "unchanged" } else { "updated" };
        if outcome == "updated" && !dry_run {
            let method = if self.context.provider == Provider::Gitlab {
                Method::PUT
            } else {
                Method::PATCH
            };
            self.send(
                method,
                self.update_url(comment.id),
                Some(json!({"body": body})),
                "update comment",
            )
            .await?;
        }
        Ok(UpsertResult {
            outcome,
            url: self.comment_url(request_url, comment.id),
        })
    }
}

fn comment_id(value: &Value) -> Result<u64> {
    value["id"]
        .as_u64()
        .filter(|id| *id > 0)
        .ok_or_else(|| error("Forge API response is missing a positive numeric ID. Check API compatibility."))
}
