use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use assert_cmd::Command;
use serde_json::{json, Value};
use tempfile::TempDir;

const TOKEN: &str = "test-token-do-not-print";
const KEY: &str = "gitops/kasoku";

fn isolated_command() -> Command {
    let mut command = Command::new(assert_cmd::cargo::cargo_bin!("nyl"));
    command.env_clear();
    // Winsock needs SystemRoot to load its service providers.
    #[cfg(windows)]
    command.env(
        "SystemRoot",
        std::env::var_os("SystemRoot").expect("Windows must define SystemRoot"),
    );
    command.timeout(Duration::from_secs(20));
    command
}

fn body(key: &str, markdown: &str) -> String {
    format!("<!-- nyl-comment:v1:{} -->\n\n{markdown}", hex::encode(key))
}

fn comment(id: u64, author: u64, body: &str) -> Value {
    json!({"id":id,"body":body,"user":{"id":author,"login":if author == 7 {"ci-bot[bot]"} else {"someone"}},"author":{"id":author}})
}

#[derive(Clone)]
struct Failure {
    method: &'static str,
    status: u16,
    stored: bool,
}

#[derive(Default)]
struct State {
    comments: Vec<Value>,
    requests: Vec<(String, String, Value)>,
    failures: VecDeque<Failure>,
}

struct Forge {
    provider: &'static str,
    url: String,
    state: Arc<Mutex<State>>,
    done: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Forge {
    fn new(provider: &'static str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/forge", listener.local_addr().unwrap());
        let state = Arc::new(Mutex::new(State::default()));
        let done = Arc::new(AtomicBool::new(false));
        let worker_state = state.clone();
        let worker_done = done.clone();
        let web_url = format!("{url}/group/sub/repo/-/merge_requests/12");
        let worker = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(90);
            while !worker_done.load(Ordering::Relaxed) && Instant::now() < deadline {
                let (mut socket, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(e) => panic!("{e}"),
                };
                socket.set_nonblocking(false).unwrap();
                socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                socket.set_write_timeout(Some(Duration::from_secs(2))).unwrap();
                let mut reader = BufReader::new(&socket);
                let mut first = String::new();
                reader.read_line(&mut first).unwrap();
                let mut length = 0;
                let mut authenticated = false;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                    let (name, value) = line.split_once(':').unwrap();
                    if name.eq_ignore_ascii_case("content-length") {
                        length = value.trim().parse().unwrap();
                    }
                    let expected = match provider {
                        "gitlab" => ("private-token", TOKEN.to_owned()),
                        "github" => ("authorization", format!("Bearer {TOKEN}")),
                        _ => ("authorization", format!("token {TOKEN}")),
                    };
                    if name.eq_ignore_ascii_case(expected.0) && value.trim() == expected.1 {
                        authenticated = true;
                    }
                }
                assert!(authenticated, "request must use the provider credential header");
                let mut bytes = vec![0; length];
                reader.read_exact(&mut bytes).unwrap();
                let payload = if bytes.is_empty() {
                    Value::Null
                } else {
                    serde_json::from_slice(&bytes).unwrap()
                };
                let mut words = first.split_whitespace();
                let method = words.next().unwrap();
                let path = words.next().unwrap();
                let mut state = worker_state.lock().unwrap();
                state.requests.push((method.into(), path.into(), payload.clone()));
                let is_create = method == "POST" && (path.ends_with("/comments") || path.ends_with("/notes"));
                let failure = if state
                    .failures
                    .front()
                    .is_some_and(|f| f.method == method && (method != "POST" || is_create))
                {
                    state.failures.pop_front()
                } else {
                    None
                };
                if let Some(failure) = &failure {
                    if failure.stored {
                        let id = state.comments.len() as u64 + 100;
                        state.comments.push(comment(id, 7, payload["body"].as_str().unwrap()));
                    }
                    if failure.status == 0 {
                        continue;
                    }
                }
                let (status, response) = if let Some(failure) = failure {
                    (
                        failure.status,
                        if failure.status == 200 {
                            "not JSON".to_owned()
                        } else {
                            json!({"message":TOKEN}).to_string()
                        },
                    )
                } else {
                    let api = match provider {
                        "github" => "/forge/api/v3",
                        "gitlab" => "/forge/api/v4",
                        _ => "/forge/api/v1",
                    };
                    let list = if provider == "gitlab" {
                        format!("{api}/projects/group%2Fsub%2Frepo/merge_requests/12/notes")
                    } else {
                        format!("{api}/repos/owner/repo/issues/12/comments")
                    };
                    let numeric_list = format!("{api}/projects/23/merge_requests/12/notes");
                    let update = if provider == "gitlab" {
                        format!("{list}/")
                    } else {
                        format!("{api}/repos/owner/repo/issues/comments/")
                    };
                    if method == "POST" && path == "/forge/api/graphql" {
                        assert_eq!(payload["query"], "query { viewer { login } }");
                        (200, json!({"data":{"viewer":{"login":"ci-bot[bot]"}}}).to_string())
                    } else if method == "GET" && path == format!("{api}/user") {
                        (200, json!({"id":7}).to_string())
                    } else if method == "GET"
                        && (path == format!("{api}/projects/group%2Fsub%2Frepo/merge_requests/12")
                            || path == format!("{api}/projects/23/merge_requests/12"))
                    {
                        (200, json!({"web_url":web_url}).to_string())
                    } else if method == "GET"
                        && (path.starts_with(&format!("{list}?")) || path.starts_with(&format!("{numeric_list}?")))
                    {
                        let parsed = reqwest::Url::parse(&format!("http://localhost{path}")).unwrap();
                        let page: usize = parsed
                            .query_pairs()
                            .find(|(k, _)| k == "page")
                            .unwrap()
                            .1
                            .parse()
                            .unwrap();
                        let size = if provider == "forgejo" { "limit" } else { "per_page" };
                        assert!(parsed.query_pairs().any(|(k, v)| k == size && v == "100"));
                        // The server caps pages at two even though the client asks for 100.
                        let comments: Vec<_> = state.comments.iter().skip((page - 1) * 2).take(2).cloned().collect();
                        (200, json!(comments).to_string())
                    } else if is_create && (path == list || path == numeric_list) {
                        let id = state.comments.len() as u64 + 100;
                        let value = comment(id, 7, payload["body"].as_str().unwrap());
                        state.comments.push(value.clone());
                        (201, value.to_string())
                    } else if method == (if provider == "gitlab" { "PUT" } else { "PATCH" })
                        && path.starts_with(&update)
                    {
                        let id: u64 = path.rsplit('/').next().unwrap().parse().unwrap();
                        let value = state.comments.iter_mut().find(|v| v["id"] == id).unwrap();
                        value["body"] = payload["body"].clone();
                        (200, value.to_string())
                    } else {
                        (404, json!({"message":"unexpected endpoint"}).to_string())
                    }
                };
                write!(socket, "HTTP/1.1 {status} Response\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).unwrap();
            }
        });
        Self {
            provider,
            url,
            state,
            done,
            worker: Some(worker),
        }
    }

    fn command(&self, key: &str) -> Command {
        let mut command = isolated_command();
        command
            .env("GH_TOKEN", TOKEN)
            .env("GITLAB_TOKEN", TOKEN)
            .env("FORGEJO_TOKEN", TOKEN)
            .args([
                "comment",
                "upsert",
                "--provider",
                self.provider,
                "--server-url",
                &self.url,
                "--repository",
                if self.provider == "gitlab" {
                    "group/sub/repo"
                } else {
                    "owner/repo"
                },
                "--request",
                "12",
                "--key",
                key,
                "--body-file",
                "-",
            ]);
        command
    }

    fn run(&self, key: &str, markdown: &str, dry_run: bool) -> String {
        let mut command = self.command(key);
        if dry_run {
            command.arg("--dry-run");
        }
        let result = command.write_stdin(markdown).assert().success();
        String::from_utf8(result.get_output().stdout.clone()).unwrap()
    }

    fn writes(&self) -> usize {
        self.state
            .lock()
            .unwrap()
            .requests
            .iter()
            .filter(|(method, path, _)| method != "GET" && !path.ends_with("/graphql"))
            .count()
    }
}

impl Drop for Forge {
    fn drop(&mut self) {
        self.done.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let result = worker.join();
            if !std::thread::panicking() {
                result.unwrap();
            }
        }
    }
}

fn roundtrip(provider: &'static str) {
    let forge = Forge::new(provider);
    let unrelated = vec![
        comment(1, 9, &body(KEY, "someone else's report")),
        comment(2, 7, &body("another-key", "other report")),
        comment(3, 7, &format!("Quoted marker:\n{}", body(KEY, "example"))),
        comment(
            4,
            7,
            "<!-- nyl-comment:v2:6769746f70732f6b61736f6b75 -->\nfuture version",
        ),
    ];
    forge.state.lock().unwrap().comments = unrelated.clone();
    assert!(forge
        .run(KEY, "# Report\n", true)
        .starts_with("dry-run created http://"));
    assert_eq!(forge.writes(), 0);
    let created = forge.run(KEY, "# Report\n", false);
    assert!(created.starts_with("created http://"));
    assert!(created.contains(if provider == "gitlab" {
        "#note_104"
    } else {
        "#issuecomment-104"
    }));
    assert_eq!(forge.writes(), 1);
    assert!(forge.run(KEY, "# Report\n", false).starts_with("unchanged "));
    assert_eq!(forge.writes(), 1);
    assert!(forge.run(KEY, "# Updated\n", true).starts_with("dry-run updated "));
    assert_eq!(forge.writes(), 1);
    assert!(forge.run(KEY, "# Updated\n", false).starts_with("updated "));
    assert_eq!(forge.writes(), 2);
    assert!(forge.run("second-report", "Independent", false).starts_with("created "));
    assert_eq!(forge.writes(), 3);
    let state = forge.state.lock().unwrap();
    assert_eq!(&state.comments[..4], unrelated.as_slice());
    assert_eq!(state.comments[4]["body"], body(KEY, "# Updated\n"));
    assert_eq!(state.comments[5]["body"], body("second-report", "Independent"));
    assert!(state.requests.iter().any(|(_, path, _)| path.ends_with("page=4")));
}

#[test]
fn test_github_roundtrip() {
    roundtrip("github");
}

#[test]
fn test_gitlab_roundtrip() {
    roundtrip("gitlab");
}

#[test]
fn test_forgejo_roundtrip() {
    roundtrip("forgejo");
}

#[test]
fn test_uncertain_create_rechecks_before_retrying_for_every_provider() {
    for provider in ["github", "gitlab", "forgejo"] {
        for (status, stored) in [(503, true), (0, true), (200, true), (503, false)] {
            let forge = Forge::new(provider);
            forge.state.lock().unwrap().failures.push_back(Failure {
                method: "POST",
                status,
                stored,
            });
            let output = forge.run(KEY, "Report", false);
            assert!(output.starts_with(if stored { "unchanged " } else { "created " }));
            assert_eq!(forge.writes(), if stored { 1 } else { 2 });
            let state = forge.state.lock().unwrap();
            assert_eq!(state.comments.len(), 1);
            let writes: Vec<_> = state
                .requests
                .iter()
                .enumerate()
                .filter(|(_, (method, path, _))| method == "POST" && !path.ends_with("/graphql"))
                .map(|(index, _)| index)
                .collect();
            if writes.len() == 2 {
                assert!(state.requests[writes[0] + 1..writes[1]]
                    .iter()
                    .any(|(method, path, _)| method == "GET" && path.contains("page=1")));
            }
        }
    }
}

#[test]
fn test_uncertain_create_stops_when_recheck_fails_or_retry_is_exhausted() {
    for recheck_fails in [true, false] {
        let forge = Forge::new("github");
        forge.state.lock().unwrap().failures.extend([
            Failure {
                method: "POST",
                status: 503,
                stored: false,
            },
            Failure {
                method: if recheck_fails { "GET" } else { "POST" },
                status: 503,
                stored: false,
            },
        ]);
        forge
            .command(KEY)
            .write_stdin("Report")
            .assert()
            .failure()
            .stderr(predicates::str::contains("Create outcome is uncertain"));
        assert_eq!(forge.writes(), if recheck_fails { 1 } else { 2 });
    }
}

#[test]
fn test_http_failures_are_actionable_and_redact_response_bodies() {
    for provider in ["github", "gitlab", "forgejo"] {
        for (status, hint) in [
            (401, "TOKEN"),
            (403, "TOKEN"),
            (404, "--repository"),
            (413, "too large"),
            (422, "size limit"),
            (429, "rate limit"),
            (503, "unavailable"),
            (302, "Redirects are disabled"),
        ] {
            let forge = Forge::new(provider);
            forge.state.lock().unwrap().failures.push_back(Failure {
                method: "GET",
                status,
                stored: false,
            });
            let mut command = forge.command(KEY);
            let result = command.write_stdin("Report").assert().failure();
            let stderr = String::from_utf8_lossy(&result.get_output().stderr);
            assert!(stderr.contains(hint), "missing {hint}: {stderr}");
            assert!(!stderr.contains(TOKEN));
            assert_eq!(forge.writes(), 0);
        }
    }
}

#[test]
fn test_rejected_create_is_not_retried_and_update_errors_propagate() {
    for provider in ["github", "gitlab", "forgejo"] {
        let forge = Forge::new(provider);
        forge.state.lock().unwrap().failures.push_back(Failure {
            method: "POST",
            status: 413,
            stored: false,
        });
        forge.command(KEY).write_stdin("Report").assert().failure();
        assert_eq!(forge.writes(), 1);
        forge
            .state
            .lock()
            .unwrap()
            .comments
            .push(comment(10, 7, &body(KEY, "old")));
        forge.state.lock().unwrap().failures.push_back(Failure {
            method: if provider == "gitlab" { "PUT" } else { "PATCH" },
            status: 403,
            stored: false,
        });
        forge.command(KEY).write_stdin("Report").assert().failure();
        assert_eq!(forge.state.lock().unwrap().comments[0]["body"], body(KEY, "old"));
    }
}

#[test]
fn test_duplicate_owned_markers_fail_without_writing() {
    let forge = Forge::new("github");
    forge.state.lock().unwrap().comments = vec![comment(1, 7, &body(KEY, "old")), comment(2, 7, &body(KEY, "other"))];
    forge
        .command(KEY)
        .write_stdin("Report")
        .assert()
        .failure()
        .stderr(predicates::str::contains("Multiple comments"));
    assert_eq!(forge.writes(), 0);
}

#[test]
fn test_ci_detection_reads_file_without_a_nyl_project() {
    let forge = Forge::new("gitlab");
    let temp = TempDir::new().unwrap();
    let file = temp.path().join("report.md");
    std::fs::write(&file, "# File report\n").unwrap();
    isolated_command()
        .env("GITLAB_CI", "true")
        .env("CI_SERVER_URL", &forge.url)
        .env("CI_MERGE_REQUEST_PROJECT_ID", "23")
        .env("CI_PROJECT_PATH", "fork/repo")
        .env("CI_MERGE_REQUEST_IID", "12")
        .env("GITLAB_TOKEN", TOKEN)
        .current_dir(temp.path())
        .args(["comment", "upsert", "--key", KEY, "--body-file"])
        .arg(file)
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "/group/sub/repo/-/merge_requests/12#note_100",
        ));
    assert_eq!(
        forge.state.lock().unwrap().comments[0]["body"],
        body(KEY, "# File report\n")
    );
}

#[test]
fn test_actions_context_uses_pr_base_repository_and_authenticated_bot() {
    for provider in ["github", "forgejo"] {
        let forge = Forge::new(provider);
        let temp = TempDir::new().unwrap();
        let event = temp.path().join("event.json");
        std::fs::write(
            &event,
            json!({"number":12,"pull_request":{"base":{"repo":{"full_name":"owner/repo"}}}}).to_string(),
        )
        .unwrap();
        let prefix = if provider == "github" { "GITHUB" } else { "FORGEJO" };
        isolated_command()
            .env(format!("{prefix}_ACTIONS"), "true")
            .env(format!("{prefix}_SERVER_URL"), &forge.url)
            .env(format!("{prefix}_REPOSITORY"), "fork/repo")
            .env(format!("{prefix}_EVENT_PATH"), event)
            .env(format!("{prefix}_TOKEN"), TOKEN)
            .env(format!("{prefix}_ACTOR"), "someone")
            .current_dir(temp.path())
            .args(["comment", "upsert", "--key", KEY, "--body-file", "-"])
            .write_stdin("Report")
            .assert()
            .success();
        assert_eq!(forge.state.lock().unwrap().comments[0]["body"], body(KEY, "Report"));
    }
}

#[test]
fn test_oversized_content_and_missing_credentials_do_not_contact_the_forge() {
    let forge = Forge::new("github");
    forge
        .command(KEY)
        .write_stdin("x".repeat(65_536))
        .assert()
        .failure()
        .stderr(predicates::str::contains("Shorten the report"));
    forge
        .command(KEY)
        .env_remove("GH_TOKEN")
        .write_stdin("Report")
        .assert()
        .failure()
        .stderr(predicates::str::contains("Missing credentials"));
    assert!(forge.state.lock().unwrap().requests.is_empty());
}
