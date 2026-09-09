use clap::ValueEnum;
use reqwest::Url;
use serde_json::Value;

use super::{error, UpsertArgs};
use crate::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(super) enum Provider {
    Github,
    Gitlab,
    Forgejo,
}

impl Provider {
    pub(super) fn body_limit(self) -> usize {
        match self {
            Self::Gitlab => 1_000_000,
            Self::Github | Self::Forgejo => 65_536,
        }
    }

    pub(super) fn credential_hint(self) -> &'static str {
        match self {
            Self::Github => "Set GH_TOKEN or GITHUB_TOKEN with pull-requests: write (or issues: write) for this repository.",
            Self::Gitlab => "Set GITLAB_TOKEN with api scope and a role allowed to comment on this project; CI_JOB_TOKEN cannot create notes.",
            Self::Forgejo => "Set FORGEJO_TOKEN with read:user and write:issue and access to this repository.",
        }
    }

    pub(super) fn token(self, env: impl Fn(&str) -> Option<String>) -> Result<String> {
        let names: &[&str] = match self {
            Self::Github => &["GH_TOKEN", "GITHUB_TOKEN"],
            Self::Gitlab => &["GITLAB_TOKEN"],
            Self::Forgejo => &["FORGEJO_TOKEN"],
        };
        names
            .iter()
            .find_map(|name| env(name).filter(|v| !v.is_empty()))
            .ok_or_else(|| error(format!("Missing credentials. {}", self.credential_hint())))
    }
}

pub(super) struct Context {
    pub(super) provider: Provider,
    pub(super) server: Url,
    pub(super) repository: String,
    pub(super) request: u64,
}

impl Context {
    pub(super) fn resolve(args: &UpsertArgs, env: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let env = |name: &str| env(name).filter(|v| !v.is_empty());
        let active = |name| env(name).is_some_and(|v| v != "false" && v != "0");
        let provider = args
            .provider
            .or_else(|| {
                if active("FORGEJO_ACTIONS") || active("GITEA_ACTIONS") || env("FORGEJO_SERVER_URL").is_some() {
                    Some(Provider::Forgejo)
                } else if active("GITLAB_CI") {
                    Some(Provider::Gitlab)
                } else if active("GITHUB_ACTIONS") {
                    Some(Provider::Github)
                } else {
                    None
                }
            })
            .ok_or_else(|| error("Cannot detect forge provider. Pass --provider github, gitlab, or forgejo."))?;

        let ci_value = |suffix: &str| match provider {
            Provider::Github => env(&format!("GITHUB_{suffix}")),
            Provider::Forgejo => env(&format!("FORGEJO_{suffix}"))
                .or_else(|| env(&format!("GITEA_{suffix}")))
                .or_else(|| env(&format!("GITHUB_{suffix}"))),
            Provider::Gitlab => None,
        };
        let server = args
            .server_url
            .clone()
            .or_else(|| {
                if provider == Provider::Gitlab {
                    env("CI_SERVER_URL")
                } else {
                    ci_value("SERVER_URL")
                }
            })
            .or_else(|| match provider {
                Provider::Github => Some("https://github.com".into()),
                Provider::Gitlab => Some("https://gitlab.com".into()),
                Provider::Forgejo => None,
            })
            .ok_or_else(|| error("Cannot detect forge server. Pass --server-url with the instance's web URL."))?;
        let server = server_url(&server)?;

        // Explicit coordinates must work even when a CI event file is unavailable.
        let event = if provider != Provider::Gitlab && (args.repository.is_none() || args.request.is_none()) {
            ci_value("EVENT_PATH")
                .map(|path| {
                    let contents = std::fs::read(path).map_err(|_| {
                        error("Cannot read CI event file. Supply --repository and --request explicitly.")
                    })?;
                    serde_json::from_slice::<Value>(&contents).map_err(|_| {
                        error("Invalid JSON in CI event file. Supply --repository and --request explicitly.")
                    })
                })
                .transpose()?
                .unwrap_or(Value::Null)
        } else {
            Value::Null
        };
        let repository = args
            .repository
            .clone()
            .or_else(|| {
                if provider == Provider::Gitlab {
                    env("CI_MERGE_REQUEST_PROJECT_ID")
                        .or_else(|| env("CI_PROJECT_PATH"))
                        .or_else(|| env("CI_PROJECT_ID"))
                } else {
                    event
                        .pointer("/pull_request/base/repo/full_name")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .or_else(|| ci_value("REPOSITORY"))
                }
            })
            .ok_or_else(|| {
                error("Cannot detect repository. Pass --repository owner/repo (or a GitLab project path/ID).")
            })?;
        validate_repository(provider, &repository)?;

        let request = if let Some(number) = args.request {
            number
        } else if provider == Provider::Gitlab {
            env("CI_MERGE_REQUEST_IID").and_then(|v| v.parse().ok()).unwrap_or(0)
        } else {
            event_request(&event, ci_value("REF").as_deref()).unwrap_or(0)
        };
        if request == 0 {
            return Err(error(
                "Cannot detect a positive PR/MR number. Run in a pull/merge-request pipeline or pass --request NUMBER.",
            ));
        }
        Ok(Self {
            provider,
            server,
            repository,
            request,
        })
    }

    pub(super) fn api_url(&self) -> Url {
        match self.provider {
            Provider::Github if self.server.host_str() == Some("github.com") => {
                Url::parse("https://api.github.com/").unwrap()
            }
            Provider::Github => append(&self.server, &["api", "v3"]),
            Provider::Gitlab => append(&self.server, &["api", "v4"]),
            Provider::Forgejo => append(&self.server, &["api", "v1"]),
        }
    }

    pub(super) fn graphql_url(&self) -> Url {
        if self.server.host_str() == Some("github.com") {
            Url::parse("https://api.github.com/graphql").unwrap()
        } else {
            append(&self.server, &["api", "graphql"])
        }
    }
}

fn event_request(event: &Value, git_ref: Option<&str>) -> Option<u64> {
    event
        .pointer("/pull_request/number")
        .and_then(Value::as_u64)
        .or_else(|| {
            event
                .get("pull_request")
                .filter(|v| v.is_object())
                .and_then(|_| event["number"].as_u64())
        })
        .or_else(|| {
            event
                .pointer("/issue/pull_request")
                .filter(|v| v.is_object())
                .and_then(|_| event["issue"]["number"].as_u64())
        })
        .or_else(|| {
            let mut parts = git_ref?.strip_prefix("refs/pull/")?.split('/');
            let number = parts.next()?.parse().ok()?;
            (matches!(parts.next(), Some("head" | "merge")) && parts.next().is_none()).then_some(number)
        })
}

fn validate_repository(provider: Provider, repository: &str) -> Result<()> {
    let parts: Vec<_> = repository.split('/').collect();
    let numeric_project = provider == Provider::Gitlab && repository.parse::<u64>().is_ok_and(|id| id > 0);
    if !numeric_project && (parts.len() < 2 || (provider != Provider::Gitlab && parts.len() != 2))
        || parts.iter().any(|part| {
            part.is_empty()
                || matches!(*part, "." | "..")
                || part
                    .chars()
                    .any(|c| c.is_control() || c.is_whitespace() || "\\?#%:".contains(c))
        })
    {
        return Err(error(
            "Invalid --repository. Use owner/repo, a GitLab group/project path, or a positive GitLab project ID.",
        ));
    }
    Ok(())
}

fn server_url(value: &str) -> Result<Url> {
    let mut url = Url::parse(value).map_err(|_| error("Invalid --server-url. Supply an absolute HTTP(S) web URL."))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(error(
            "--server-url must be an HTTP(S) web URL without credentials, query parameters, or a fragment.",
        ));
    }
    let path = format!("{}/", url.path().trim_end_matches('/'));
    url.set_path(&path);
    Ok(url)
}

pub(super) fn append(base: &Url, parts: &[&str]) -> Url {
    let mut url = base.clone();
    url.path_segments_mut()
        .expect("validated HTTP URL")
        .pop_if_empty()
        .extend(parts);
    url
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> UpsertArgs {
        UpsertArgs {
            key: "key".into(),
            body_file: "-".into(),
            provider: None,
            server_url: None,
            repository: None,
            request: None,
            dry_run: false,
        }
    }

    fn env<'a>(values: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            values
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.to_string())
        }
    }

    #[test]
    fn test_context_detects_each_ci_and_self_hosted_prefixes() {
        for (vars, expected, api) in [
            (
                vec![
                    ("GITHUB_ACTIONS", "true"),
                    ("GITHUB_REPOSITORY", "a/b"),
                    ("GITHUB_REF", "refs/pull/12/merge"),
                ],
                Provider::Github,
                "https://api.github.com/",
            ),
            (
                vec![
                    ("GITLAB_CI", "true"),
                    ("CI_SERVER_URL", "https://git.example/gl"),
                    ("CI_PROJECT_PATH", "fork/repo"),
                    ("CI_MERGE_REQUEST_PROJECT_ID", "23"),
                    ("CI_MERGE_REQUEST_IID", "12"),
                ],
                Provider::Gitlab,
                "https://git.example/gl/api/v4",
            ),
            (
                vec![
                    ("GITEA_ACTIONS", "true"),
                    ("GITHUB_ACTIONS", "true"),
                    ("GITHUB_SERVER_URL", "https://git.example/forge"),
                    ("GITHUB_REPOSITORY", "a/b"),
                    ("GITHUB_REF", "refs/pull/12/head"),
                ],
                Provider::Forgejo,
                "https://git.example/forge/api/v1",
            ),
        ] {
            let context = Context::resolve(&args(), env(&vars)).unwrap();
            assert_eq!(context.provider, expected);
            assert_eq!(context.request, 12);
            assert_eq!(context.api_url().as_str(), api);
            assert_eq!(
                context.repository,
                if expected == Provider::Gitlab { "23" } else { "a/b" }
            );
        }
    }

    #[test]
    fn test_event_uses_base_repository_and_explicit_overrides_win() {
        let temp = tempfile::TempDir::new().unwrap();
        let event = temp.path().join("event.json");
        std::fs::write(&event, r#"{"number":14,"pull_request":{"base":{"repo":{"full_name":"base/repo"}},"head":{"repo":{"full_name":"fork/repo"}}}}"#).unwrap();
        let vars = [
            ("GITHUB_ACTIONS", "true"),
            ("GITHUB_EVENT_PATH", event.to_str().unwrap()),
            ("GITHUB_REPOSITORY", "fork/repo"),
        ];
        let context = Context::resolve(&args(), env(&vars)).unwrap();
        assert_eq!(context.repository, "base/repo");
        assert_eq!(context.request, 14);
        let explicit = UpsertArgs {
            provider: Some(Provider::Github),
            server_url: Some("https://ghe.example".into()),
            repository: Some("explicit/repo".into()),
            request: Some(45),
            ..args()
        };
        let context = Context::resolve(&explicit, env(&[("GITHUB_EVENT_PATH", "/unavailable")])).unwrap();
        assert_eq!(context.repository, "explicit/repo");
        assert_eq!(context.request, 45);
        assert_eq!(context.api_url().as_str(), "https://ghe.example/api/v3");
        assert_eq!(context.graphql_url().as_str(), "https://ghe.example/api/graphql");
    }

    #[test]
    fn test_context_rejects_missing_or_invalid_coordinates() {
        assert!(Context::resolve(&args(), |_| None).is_err());
        for repo in ["a/../b", "a/b?token=secret", "a%2fb", "/repo", "a/b/c"] {
            assert!(validate_repository(Provider::Github, repo).is_err());
        }
        assert!(validate_repository(Provider::Gitlab, "group/subgroup/repo").is_ok());
        for server in [
            "file:///tmp/a",
            "https://token@git.example",
            "https://git.example?token=secret",
        ] {
            assert!(server_url(server).is_err());
        }
        let explicit = UpsertArgs {
            provider: Some(Provider::Github),
            repository: Some("a/b".into()),
            ..args()
        };
        assert!(Context::resolve(&explicit, env(&[("GITHUB_REF", "refs/heads/main")])).is_err());
    }

    #[test]
    fn test_credentials_are_provider_specific() {
        let vars = [
            ("GH_TOKEN", "gh"),
            ("GITHUB_TOKEN", "actions"),
            ("GITLAB_TOKEN", "gl"),
            ("FORGEJO_TOKEN", "fj"),
        ];
        assert_eq!(Provider::Github.token(env(&vars)).unwrap(), "gh");
        assert_eq!(Provider::Gitlab.token(env(&vars)).unwrap(), "gl");
        assert_eq!(Provider::Forgejo.token(env(&vars)).unwrap(), "fj");
        assert!(Provider::Gitlab.token(env(&[("CI_JOB_TOKEN", "job")])).is_err());
    }
}
