//! Resolve approval identity before effects; authorization belongs to the CI gate.
//!
//! Contract: `design/orchestration-core.md#approval`.

use clap::Args;
use nyl_core::orchestration::codes::{Code, Finding};
use nyl_core::orchestration::ids::{ChangeDigest, UnitName};
use nyl_core::orchestration::ports::CommitIdentity;
use nyl_core::orchestration::state::{ApprovalIdentity, ApprovalRecord};
use nyl_orchestration::rules::approval::Approvals;
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};

use super::run::Wiring;
use super::session::Session;

/// Approval flags shared by every command that can execute units.
#[derive(Debug, Clone, Args, Default)]
pub struct ApprovalArgs {
    /// Approve a unit, optionally binding it to a reviewed change digest (repeatable).
    #[arg(long = "approve", value_name = "UNIT[=DIGEST]", value_delimiter = ',')]
    pub approve: Vec<String>,
    /// Approve all candidates without reviewed digests; requireDigest units still wait.
    #[arg(long)]
    pub approve_all: bool,
    /// Assert the approver identities (repeatable, or comma-separated).
    #[arg(long, value_delimiter = ',')]
    pub approved_by: Vec<String>,
    /// Assert where the approval was given, such as a CI run URL.
    #[arg(long)]
    pub approval_source: Option<String>,
}

fn failure(message: impl Into<String>) -> Finding {
    Finding::new(
        Code::ApprovalInvalid,
        message,
        "correct the approval flags or lookup configuration before retrying",
    )
}

impl ApprovalArgs {
    /// Parse approval targets, reject excluded units, and resolve their audit identity.
    pub fn resolve(
        &self,
        session: &Session,
        wiring: &Wiring<'_>,
        identity: &CommitIdentity,
        selected: Option<&BTreeSet<UnitName>>,
    ) -> Result<Approvals, Finding> {
        let units = self.units(selected)?;
        if units.is_empty() && !self.approve_all {
            return Ok(Approvals::default());
        }
        let record = if !self.approved_by.is_empty() || self.approval_source.is_some() {
            if self.approved_by.iter().any(|name| name.trim().is_empty()) {
                return Err(failure("--approved-by must name an approver"));
            }
            ApprovalRecord {
                by: self.approved_by.clone(),
                identity: ApprovalIdentity::Asserted,
                source: self.approval_source.clone(),
                digest: None,
                reviewed: false,
            }
        } else if !session.inventory.project_config.config.approval.lookup.is_empty() {
            hook(session, wiring, &units)?
        } else if let Some(record) = github(&wiring.process_env, session.resolved.name.as_str())? {
            record
        } else if is_ci(&wiring.process_env) {
            ApprovalRecord {
                by: Vec::new(),
                identity: ApprovalIdentity::Asserted,
                source: Some(wiring.runner.clone()),
                digest: None,
                reviewed: false,
            }
        } else {
            ApprovalRecord {
                by: vec![identity.to_string()],
                identity: ApprovalIdentity::Local,
                source: Some("local".into()),
                digest: None,
                reviewed: false,
            }
        };
        Ok(Approvals {
            units,
            all: self.approve_all,
            identity: Some(record),
        })
    }

    fn units(
        &self,
        selected: Option<&BTreeSet<UnitName>>,
    ) -> Result<BTreeMap<UnitName, Option<ChangeDigest>>, Finding> {
        let mut units = BTreeMap::new();
        for argument in &self.approve {
            let (name, digest) = argument
                .split_once('=')
                .map_or((argument.as_str(), None), |(name, digest)| (name, Some(digest)));
            let name =
                UnitName::new(name).map_err(|error| failure(format!("invalid --approve {argument:?}: {error}")))?;
            if selected.is_some_and(|selected| !selected.contains(&name)) {
                return Err(failure(format!("--approve {name} names a unit excluded by --unit")));
            }
            let digest = digest
                .map(ChangeDigest::new)
                .transpose()
                .map_err(|error| failure(format!("invalid approval digest: {error}")))?;
            if let Some(previous) = units.insert(name.clone(), digest.clone()) {
                if previous != digest {
                    return Err(failure(format!("conflicting approvals for {name}")));
                }
            }
        }
        Ok(units)
    }
}

fn is_ci(env: &BTreeMap<String, String>) -> bool {
    ["CI", "GITHUB_ACTIONS", "GITLAB_CI"]
        .iter()
        .any(|key| env.get(*key).is_some_and(|value| value == "true" || value == "1"))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Lookup {
    by: Vec<String>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    url: Option<String>,
}

fn hook(
    session: &Session,
    wiring: &Wiring<'_>,
    units: &BTreeMap<UnitName, Option<ChangeDigest>>,
) -> Result<ApprovalRecord, Finding> {
    let lookup = &session.inventory.project_config.config.approval.lookup;
    if lookup[0].trim().is_empty() {
        return Err(failure("[approval] lookup must name a command"));
    }
    let mut env = wiring.process_env.clone();
    env.insert("NYL_ENVIRONMENT".into(), session.resolved.name.to_string());
    env.insert(
        "NYL_APPROVAL_UNITS".into(),
        serde_json::to_string(&units.keys().collect::<Vec<_>>()).expect("names serialize"),
    );
    env.insert(
        "NYL_APPROVAL_DIGESTS".into(),
        serde_json::to_string(units).expect("digests serialize"),
    );
    let outcome = wiring
        .tools
        .run(nyl_core::orchestration::driver::ToolInvocation {
            program: lookup[0].clone().into(),
            args: lookup[1..].iter().map(Into::into).collect(),
            working_dir: session.inventory.project_root.clone(),
            env,
            stdin: None,
            timeout: Some(std::time::Duration::from_secs(30)),
            masked: Vec::new(),
        })
        .map_err(|_| failure(format!("approval lookup {:?} could not start", lookup[0])))?;
    match outcome {
        nyl_core::orchestration::driver::ToolExit::Exited { code: 0, stdout } => lookup_record(&stdout, &lookup[0]),
        _ => Err(failure(format!(
            "approval lookup {:?} failed, timed out, or was cancelled",
            lookup[0]
        ))),
    }
}

fn lookup_record(bytes: &[u8], command: &str) -> Result<ApprovalRecord, Finding> {
    let lookup: Lookup = serde_json::from_slice(bytes)
        .map_err(|_| failure("approval lookup must print {\"by\": [...], \"source\": \"...\", \"url\": \"...\"}"))?;
    if lookup.by.iter().any(|name| name.trim().is_empty()) {
        return Err(failure("approval lookup returned an empty approver"));
    }
    let source = [Some(format!("hook {command}")), lookup.source, lookup.url]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ");
    Ok(ApprovalRecord {
        by: lookup.by,
        identity: ApprovalIdentity::Verified,
        source: Some(source),
        digest: None,
        reviewed: false,
    })
}

fn github(env: &BTreeMap<String, String>, environment: &str) -> Result<Option<ApprovalRecord>, Finding> {
    if env.get("GITHUB_ACTIONS").is_none_or(|value| value != "true") {
        return Ok(None);
    }
    let Some(token) = env
        .get("GITHUB_TOKEN")
        .or_else(|| env.get("GH_TOKEN"))
        .filter(|token| !token.is_empty())
    else {
        return Ok(None);
    };
    let Some(repository) = env.get("GITHUB_REPOSITORY") else {
        return Ok(None);
    };
    let Some(run) = env.get("GITHUB_RUN_ID") else {
        return Ok(None);
    };
    if repository.split('/').count() != 2
        || repository
            .split('/')
            .any(|part| part.is_empty() || !part.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)))
        || !run.chars().all(|c| c.is_ascii_digit())
        || run.is_empty()
    {
        return Err(failure(
            "GitHub approval lookup requires a repository owner/name and numeric run ID",
        ));
    }
    let api = env
        .get("GITHUB_API_URL")
        .map_or("https://api.github.com", String::as_str);
    let url = format!(
        "{}/repos/{repository}/actions/runs/{run}/approvals",
        api.trim_end_matches('/')
    );
    // CLI commands may already run inside Tokio; the adapter owns its runtime on a separate thread.
    let reviews = std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|_| failure("cannot initialize GitHub approval lookup"))?;
                runtime.block_on(async {
                    let client = reqwest::Client::builder()
                        .timeout(std::time::Duration::from_secs(30))
                        .redirect(reqwest::redirect::Policy::none())
                        .build()
                        .map_err(|_| failure("cannot initialize GitHub approval client"))?;
                    let response = client
                        .get(url)
                        .bearer_auth(token)
                        .header("User-Agent", "nyl")
                        .header("Accept", "application/vnd.github+json")
                        .send()
                        .await
                        .map_err(|_| failure("GitHub approval lookup failed to connect"))?;
                    if !response.status().is_success() {
                        return Err(failure(format!(
                            "GitHub approval lookup returned HTTP {}",
                            response.status()
                        )));
                    }
                    let bytes = response
                        .bytes()
                        .await
                        .map_err(|_| failure("cannot read GitHub approval lookup response"))?;
                    serde_json::from_slice::<Vec<Review>>(&bytes)
                        .map_err(|_| failure("invalid GitHub approval lookup response"))
                })
            })
            .join()
            .expect("approval lookup thread panicked")
    })?;
    let by = github_approvers(&reviews, environment);
    let server = env
        .get("GITHUB_SERVER_URL")
        .map_or("https://github.com", String::as_str);
    Ok(Some(ApprovalRecord {
        by,
        identity: ApprovalIdentity::Verified,
        source: Some(format!(
            "github-environment {environment} {server}/{repository}/actions/runs/{run}"
        )),
        digest: None,
        reviewed: false,
    }))
}

#[derive(Deserialize)]
struct Review {
    state: String,
    user: User,
    environments: Vec<GitHubEnvironment>,
}
#[derive(Deserialize)]
struct User {
    login: String,
}
#[derive(Deserialize)]
struct GitHubEnvironment {
    name: String,
}

fn github_approvers(reviews: &[Review], environment: &str) -> Vec<String> {
    reviews
        .iter()
        .filter(|review| {
            review.state == "approved" && review.environments.iter().any(|entry| entry.name == environment)
        })
        .map(|review| review.user.login.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_approval_parser_rejects_excluded_and_conflicting_units() {
        let mut args = ApprovalArgs {
            approve: vec!["db".into()],
            ..ApprovalArgs::default()
        };
        assert!(args.units(Some(&BTreeSet::new())).is_err());
        args.approve.push(format!("db=sha256:{}", "a".repeat(64)));
        assert!(args.units(None).is_err());
        args.approve = vec!["db=invalid".into()];
        assert!(args.units(None).is_err());
    }
    #[test]
    fn test_hook_record_is_verified_and_invalid_output_fails() {
        let record = lookup_record(
            br#"{"by":["alice","bob"],"source":"gate","url":"https://ci.invalid/1"}"#,
            "./approvers",
        )
        .unwrap();
        assert_eq!(record.identity, ApprovalIdentity::Verified);
        assert_eq!(record.by, ["alice", "bob"]);
        assert_eq!(
            record.source.as_deref(),
            Some("hook ./approvers gate https://ci.invalid/1")
        );
        assert!(lookup_record(br#"{"by":[""]}"#, "hook").is_err());
        assert!(lookup_record(b"not JSON", "hook").is_err());
    }
    #[test]
    fn test_github_lookup_records_only_approvers_of_this_environment() {
        let reviews: Vec<Review> = serde_json::from_value(serde_json::json!([
            {"state":"approved","user":{"login":"alice"},"environments":[{"name":"prod"}]},
            {"state":"rejected","user":{"login":"bob"},"environments":[{"name":"prod"}]},
            {"state":"approved","user":{"login":"carol"},"environments":[{"name":"dev"}]}
        ]))
        .unwrap();
        assert_eq!(github_approvers(&reviews, "prod"), ["alice"]);
    }
    #[test]
    fn test_github_adapter_reads_run_reviews_with_the_injected_token() {
        use std::io::{BufRead, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(10)))
                .unwrap();
            let mut reader = std::io::BufReader::new(&stream);
            let mut request = String::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" || line.is_empty() {
                    break;
                }
                request.push_str(&line);
            }
            assert!(request.starts_with("GET /repos/acme/infra/actions/runs/42/approvals "));
            assert!(request
                .to_ascii_lowercase()
                .contains("authorization: bearer test-token"));
            let body = r#"[{"state":"approved","user":{"login":"alice"},"environments":[{"name":"prod"}]}]"#;
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        });
        let env = BTreeMap::from([
            ("GITHUB_ACTIONS".into(), "true".into()),
            ("GITHUB_TOKEN".into(), "test-token".into()),
            ("GITHUB_REPOSITORY".into(), "acme/infra".into()),
            ("GITHUB_RUN_ID".into(), "42".into()),
            ("GITHUB_API_URL".into(), format!("http://{address}")),
        ]);
        let record = github(&env, "prod").unwrap().unwrap();
        server.join().unwrap();
        assert_eq!(record.identity, ApprovalIdentity::Verified);
        assert_eq!(record.by, ["alice"]);
    }
}
