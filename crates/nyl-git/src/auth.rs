//! Optional dedicated credentials for remote Git operations.
//!
//! Without configuration, `git` authenticates as it does for the user: SSH
//! config and agent, credential helpers. Runners without those, such as CI
//! jobs holding a deploy key or an access token in a secret, configure
//! `[[git.credentials]]` in `nyl.toml`. An entry applies to every repository
//! URL it prefixes, the longest prefix winning, and names where the secret
//! is, never the secret itself:
//!
//! ```toml
//! [[git.credentials]]
//! url = "https://gitlab.example.com/platform/"
//! token_env = "NYL_STATE_TOKEN"      # HTTPS token from this variable
//! username = "oauth2"              # default x-access-token
//!
//! [[git.credentials]]
//! url = "git@github.com:acme/"
//! ssh_key_file = "/run/secrets/deploy-key"   # or ssh_key_env with the key itself
//! ```
//!
//! A token reaches `git` through a credential helper that reads it from the
//! child process's environment, so it never appears on a command line or in
//! Git configuration. A key from `ssh_key_env` is written to a temporary file
//! readable only by the user and removed after the invocation.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::{GitError, Result};

/// The `[git]` table of `nyl.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct GitSettings {
    /// Dedicated credentials for state pushes and publication, by repository URL prefix.
    pub credentials: Vec<CredentialEntry>,
}

/// One dedicated credential. Set exactly one of `ssh_key_file`, `ssh_key_env`, and `token_env`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct CredentialEntry {
    /// Prefix of the repository URLs this credential applies to, such as `https://gitlab.example.com/platform/`.
    pub url: String,
    /// Private SSH key file used instead of the SSH agent and config identities.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_key_file: Option<PathBuf>,
    /// Environment variable holding a private SSH key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_key_env: Option<String>,
    /// Environment variable holding an HTTPS access token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_env: Option<String>,
    /// HTTPS username sent with the token. Default `x-access-token`; GitLab expects `oauth2`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
}

impl CredentialEntry {
    fn validate(&self) -> Result<()> {
        let set = [
            self.ssh_key_file.is_some(),
            self.ssh_key_env.is_some(),
            self.token_env.is_some(),
        ]
        .into_iter()
        .filter(|set| *set)
        .count();
        if self.url.is_empty() || set != 1 {
            return Err(GitError::Credentials(format!(
                "git.credentials entry for {:?} needs a url and exactly one of ssh_key_file, ssh_key_env, and token_env",
                self.url
            )));
        }
        if self.username.is_some() && self.token_env.is_none() {
            return Err(GitError::Credentials(format!(
                "git.credentials entry for {:?}: username applies only to token_env",
                self.url
            )));
        }
        Ok(())
    }
}

/// The secret material behind one credential, read when Nyl starts.
#[derive(Clone)]
enum Secret {
    KeyFile(PathBuf),
    KeyContent(String),
    Token { username: String, token: String },
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::KeyFile(path) => formatter.debug_tuple("KeyFile").field(path).finish(),
            Self::KeyContent(_) => formatter.write_str("KeyContent(***)"),
            Self::Token { username, .. } => formatter.debug_struct("Token").field("username", username).finish(),
        }
    }
}

/// Resolved credentials: what to add to a `git` invocation for a URL.
#[derive(Debug, Clone, Default)]
pub struct GitAuth {
    entries: BTreeMap<String, Secret>,
}

/// What [`GitAuth::apply`] set up, kept alive for the invocation.
pub(crate) struct Applied {
    _key: Option<tempfile::NamedTempFile>,
}

const TOKEN_VARIABLE: &str = "NYL_GIT_HELPER_TOKEN";
const USERNAME_VARIABLE: &str = "NYL_GIT_HELPER_USERNAME";
const PROTOCOL_VARIABLE: &str = "NYL_GIT_HELPER_PROTOCOL";
const HOST_VARIABLE: &str = "NYL_GIT_HELPER_HOST";

/// The protocol and host, with any port, that git's credential protocol
/// names for `url`.
fn protocol_and_host(url: &str) -> (&str, &str) {
    let (protocol, rest) = url.split_once("://").unwrap_or(("", url));
    let authority = rest.split('/').next().unwrap_or_default();
    let host = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
    (protocol, host)
}

impl GitAuth {
    /// No dedicated credentials: `git` authenticates as configured for the user.
    pub fn none() -> Self {
        Self::default()
    }

    /// Resolve the configured entries, reading secrets through `variable`.
    /// A variable an entry names but the environment lacks is an error, so a
    /// misconfigured runner fails before it tries anything.
    pub fn from_settings(settings: &GitSettings, variable: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let mut entries = BTreeMap::new();
        for entry in &settings.credentials {
            entry.validate()?;
            let read = |name: &str| {
                variable(name).filter(|value| !value.is_empty()).ok_or_else(|| {
                    GitError::Credentials(format!(
                        "git.credentials for {} reads the environment variable {name}, which is not set",
                        entry.url
                    ))
                })
            };
            let secret = if let Some(path) = &entry.ssh_key_file {
                Secret::KeyFile(path.clone())
            } else if let Some(name) = &entry.ssh_key_env {
                Secret::KeyContent(read(name)?)
            } else {
                let name = entry.token_env.as_deref().expect("validated");
                Secret::Token {
                    username: entry.username.clone().unwrap_or_else(|| "x-access-token".to_owned()),
                    token: read(name)?,
                }
            };
            if entries.insert(entry.url.clone(), secret).is_some() {
                return Err(GitError::Credentials(format!(
                    "git.credentials lists {} twice",
                    entry.url
                )));
            }
        }
        Ok(Self { entries })
    }

    /// Whether any credential applies to `url`.
    pub fn applies_to(&self, url: &str) -> bool {
        self.secret_for(url).is_some()
    }

    fn secret_for(&self, url: &str) -> Option<&Secret> {
        self.entries
            .iter()
            .filter(|(prefix, _)| url.starts_with(prefix.as_str()))
            .max_by_key(|(prefix, _)| prefix.len())
            .map(|(_, secret)| secret)
    }

    /// Add the credential for `url`, if any, to a `git` command.
    pub(crate) fn apply(&self, url: &str, command: &mut Command) -> Result<Applied> {
        match self.secret_for(url) {
            None => Ok(Applied { _key: None }),
            Some(Secret::KeyFile(path)) => {
                command.env("GIT_SSH_COMMAND", ssh_command(path));
                Ok(Applied { _key: None })
            }
            Some(Secret::KeyContent(content)) => {
                let key = write_private_key(content)?;
                command.env("GIT_SSH_COMMAND", ssh_command(key.path()));
                Ok(Applied { _key: Some(key) })
            }
            Some(Secret::Token { username, token }) => {
                // The empty helper drops the user's helpers for this
                // invocation; the next one answers from the child's
                // environment, so the token is never on a command line. It
                // answers only for the URL's own protocol and host, never
                // for a redirect or another repository git asks about.
                let (protocol, host) = protocol_and_host(url);
                command
                    .args(["-c", "credential.helper="])
                    .args([
                        "-c",
                        &format!(
                            "credential.helper=!f() {{ test \"$1\" = get || exit 0; p=; h=; \
                             while IFS='=' read -r k v; do test -n \"$k\" || break; case \"$k\" in protocol) p=$v;; host) h=$v;; esac; done; \
                             test \"$p\" = \"${PROTOCOL_VARIABLE}\" && test \"$h\" = \"${HOST_VARIABLE}\" || exit 0; \
                             echo username=\"${USERNAME_VARIABLE}\"; echo password=\"${TOKEN_VARIABLE}\"; }}; f"
                        ),
                    ])
                    .env(PROTOCOL_VARIABLE, protocol)
                    .env(HOST_VARIABLE, host)
                    .env(USERNAME_VARIABLE, username)
                    .env(TOKEN_VARIABLE, token);
                Ok(Applied { _key: None })
            }
        }
    }
}

fn ssh_command(key: &Path) -> String {
    let quoted = key.display().to_string().replace('\'', "'\\''");
    format!("ssh -i '{quoted}' -o IdentitiesOnly=yes -o IdentityAgent=none")
}

fn write_private_key(content: &str) -> Result<tempfile::NamedTempFile> {
    let mut file = tempfile::Builder::new()
        .prefix("nyl-git-key-")
        .tempfile()
        .map_err(|error| GitError::Credentials(format!("cannot write the SSH key to a temporary file: {error}")))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|error| GitError::Credentials(format!("cannot restrict the SSH key file: {error}")))?;
    }
    let mut content = content.to_owned();
    if !content.ends_with('\n') {
        content.push('\n');
    }
    file.write_all(content.as_bytes())
        .map_err(|error| GitError::Credentials(format!("cannot write the SSH key to a temporary file: {error}")))?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(entries: &str) -> GitSettings {
        let value: serde_json::Value = serde_json::from_str(entries).unwrap();
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn test_longest_prefix_wins_and_unrelated_urls_get_nothing() {
        let settings = settings(
            r#"{"credentials": [
                {"url": "https://git.example.com/", "token_env": "BROAD"},
                {"url": "https://git.example.com/platform/", "token_env": "NARROW", "username": "oauth2"}
            ]}"#,
        );
        let auth = GitAuth::from_settings(&settings, |name| Some(format!("{name}-value"))).unwrap();
        match auth.secret_for("https://git.example.com/platform/state.git") {
            Some(Secret::Token { username, token }) => {
                assert_eq!(username, "oauth2");
                assert_eq!(token, "NARROW-value");
            }
            other => panic!("{other:?}"),
        }
        assert!(!auth.applies_to("https://other.example.com/state.git"));
    }

    #[test]
    fn test_a_missing_variable_fails_before_any_git_command() {
        let settings = settings(r#"{"credentials": [{"url": "git@github.com:acme/", "ssh_key_env": "DEPLOY_KEY"}]}"#);
        let error = GitAuth::from_settings(&settings, |_| None).unwrap_err().to_string();
        assert!(error.contains("DEPLOY_KEY"), "{error}");
    }

    #[test]
    fn test_an_entry_names_exactly_one_secret() {
        let settings =
            settings(r#"{"credentials": [{"url": "git@github.com:acme/", "ssh_key_env": "A", "token_env": "B"}]}"#);
        assert!(GitAuth::from_settings(&settings, |_| Some("x".into())).is_err());
    }

    #[test]
    fn test_secrets_never_reach_the_command_line() {
        let settings = settings(r#"{"credentials": [{"url": "https://git.example.com/", "token_env": "TOKEN"}]}"#);
        let auth = GitAuth::from_settings(&settings, |_| Some("s3cr3t".into())).unwrap();
        let mut command = Command::new("git");
        let _applied = auth.apply("https://git.example.com/state.git", &mut command).unwrap();
        let arguments = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(!arguments.contains("s3cr3t"), "{arguments}");
        assert!(format!("{auth:?}").contains("Token") && !format!("{auth:?}").contains("s3cr3t"));
    }

    /// What `git credential fill` gets from the helper for `protocol`/`host`.
    fn filled(auth: &GitAuth, protocol: &str, host: &str) -> String {
        let mut command = Command::new("git");
        let _applied = auth
            .apply("https://user@git.example.com:8443/state.git", &mut command)
            .unwrap();
        let mut child = command
            .args(["credential", "fill"])
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ASKPASS", "true")
            .env_remove("SSH_ASKPASS")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(format!("protocol={protocol}\nhost={host}\n\n").as_bytes())
            .unwrap();
        String::from_utf8(child.wait_with_output().unwrap().stdout).unwrap()
    }

    #[test]
    fn test_the_token_answers_only_for_its_own_protocol_and_host() {
        let settings =
            settings(r#"{"credentials": [{"url": "https://user@git.example.com:8443/", "token_env": "TOKEN"}]}"#);
        let auth = GitAuth::from_settings(&settings, |_| Some("s3cr3t".into())).unwrap();
        assert!(filled(&auth, "https", "git.example.com:8443").contains("password=s3cr3t"));
        assert!(!filled(&auth, "https", "elsewhere.example.com").contains("s3cr3t"));
        assert!(!filled(&auth, "http", "git.example.com:8443").contains("s3cr3t"));
    }
}
