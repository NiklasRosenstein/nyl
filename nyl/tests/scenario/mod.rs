//! The scenario harness of `design/reference-scenarios.md`.
//!
//! [`format`] parses and validates scenario files against the documented
//! format; [`world`] is the Git test world they run in. Scenarios run in
//! tier 1, in-process with fakes, once the orchestration commands exist;
//! until then a scenario whose milestone the harness has reached is listed
//! as pending, by name and with the reason, so none is skipped silently.

#![allow(dead_code)] // Each test binary uses part of the harness.

pub mod format;
#[cfg(feature = "tier2")]
pub mod native;
pub mod runner;
pub mod shell;
pub mod world;

use std::path::{Path, PathBuf};

use format::{Milestone, Scenario};

/// The milestone this harness has reached.
pub const REACHED: Milestone = Milestone(4);

/// A scenario file and its parsed content.
pub struct Loaded {
    pub name: String,
    pub directory: PathBuf,
    pub scenario: Scenario,
}

/// Explicit Docker selector admission for real-tool fixtures. Values never enter
/// committed fixture YAML; the isolated CLI inherits only these declared keys.
pub fn docker_selectors() -> std::collections::BTreeMap<String, String> {
    #[cfg(feature = "tier2")]
    {
        let mut values: std::collections::BTreeMap<_, _> = [
            "DOCKER_HOST",
            "DOCKER_CONTEXT",
            "DOCKER_CONFIG",
            "DOCKER_TLS_VERIFY",
            "DOCKER_CERT_PATH",
        ]
        .into_iter()
        .filter_map(|name| std::env::var(name).ok().map(|value| (name.to_owned(), value)))
        .collect();
        if !values.contains_key("DOCKER_CONFIG") {
            if let Some(home) = std::env::var_os("HOME") {
                let config = PathBuf::from(home).join(".docker");
                if config.is_dir() {
                    values.insert("DOCKER_CONFIG".to_owned(), config.to_string_lossy().into_owned());
                }
            }
        }
        values
    }
    #[cfg(not(feature = "tier2"))]
    std::collections::BTreeMap::new()
}

/// `nyl <args>` through the binary in `checkout`, with a home that holds no
/// Git configuration, so only `--author` or the clone's own configuration
/// can name an identity, and without CI variables, a test clock, or colour.
pub fn nyl_binary(checkout: &Path, home: &Path, args: &[&str]) -> std::process::Command {
    let mut command = std::process::Command::new(assert_cmd::cargo::cargo_bin("nyl"));
    command
        .current_dir(checkout)
        .args(args)
        .env_remove("GITHUB_RUN_ID")
        .env_remove("CI_JOB_URL")
        .env_remove("NYL_TEST_NOW")
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        // CI forces colour; the assertions read plain text.
        .env("NO_COLOR", "1")
        .envs(docker_selectors());
    command
}

/// [`nyl_binary`] with `--author` [`runner::AUTHOR`] and a home of its own.
pub fn nyl_as_author(checkout: &Path, args: &[&str]) -> std::process::Command {
    let home = checkout.with_extension("home");
    std::fs::create_dir_all(&home).unwrap();
    let mut command = nyl_binary(checkout, &home, args);
    command.args(["--author", runner::AUTHOR]);
    command
}

pub fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

pub fn scenarios_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/scenarios")
}

/// Every scenario under `tests/scenarios/<group>/<name>/scenario.yaml`,
/// named `<group>/<name>`, with the problems of those that do not load.
pub fn load_all() -> (Vec<Loaded>, Vec<String>) {
    let mut loaded = Vec::new();
    let mut problems = Vec::new();
    let mut files = walkdir::WalkDir::new(scenarios_root())
        .min_depth(3)
        .max_depth(3)
        .into_iter()
        .map(|entry| entry.unwrap().into_path())
        .filter(|path| path.file_name().is_some_and(|name| name == "scenario.yaml"))
        .collect::<Vec<_>>();
    files.sort();
    for file in files {
        let directory = file.parent().unwrap().to_path_buf();
        // Names use `/` on every platform, as scenarios refer to each other.
        let name = directory
            .strip_prefix(scenarios_root())
            .unwrap()
            .components()
            .map(|component| component.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        let text = std::fs::read_to_string(&file).unwrap();
        let result = serde_saphyr::from_str::<Scenario>(&text)
            .map_err(|error| error.to_string())
            .and_then(|scenario| format::validate(&scenario, &directory, &repository_root()).map(|()| scenario));
        match result {
            Ok(scenario) => loaded.push(Loaded {
                name,
                directory,
                scenario,
            }),
            Err(error) => problems.push(format!("{name}: {error}")),
        }
    }
    (loaded, problems)
}
