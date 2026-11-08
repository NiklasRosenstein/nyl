//! Running the `nyl` binary the way a CI job does: from a checkout of the
//! project, with an empty environment of its own and only the variables the
//! project's units admit. The core principle applies to the harness too:
//! nothing reaches Nyl through the suite's own process environment, so a
//! credential that is not admitted by a unit fails the run here exactly as it
//! would in production.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

use crate::cmd::{Cmd, Output};
use crate::error::{Context, Error, Result};

/// How long a `nyl` command may run unless the caller says otherwise: a
/// reconcile builds an image and applies OpenTofu.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(25 * 60);

/// The Docker client variables a unit may admit, as the tier-2 harness
/// passes them.
const DOCKER_SELECTORS: [&str; 5] = [
    "DOCKER_HOST",
    "DOCKER_CONTEXT",
    "DOCKER_CONFIG",
    "DOCKER_TLS_VERIFY",
    "DOCKER_CERT_PATH",
];

/// The Docker client variables the image unit admits, with their values: the
/// ones the suite's environment sets, and `DOCKER_CONFIG` pointing at
/// `~/.docker` when it exists and nothing says otherwise. A unit admits only
/// names that have a value, because an admitted variable that is missing fails
/// the unit.
pub fn docker_selectors(
    lookup: impl Fn(&str) -> Option<String>,
    home: Option<&Path>,
    is_dir: impl Fn(&Path) -> bool,
) -> BTreeMap<String, String> {
    let mut selectors = DOCKER_SELECTORS
        .iter()
        .filter_map(|name| lookup(name).map(|value| ((*name).to_owned(), value)))
        .collect::<BTreeMap<_, _>>();
    if !selectors.contains_key("DOCKER_CONFIG") {
        if let Some(config) = home.map(|home| home.join(".docker")).filter(|path| is_dir(path)) {
            selectors.insert("DOCKER_CONFIG".to_owned(), config.to_string_lossy().into_owned());
        }
    }
    selectors
}

/// The `nyl` executable, the project checkout it runs in, and the variables
/// the units admit.
#[derive(Clone, Debug)]
pub struct Nyl {
    binary: PathBuf,
    checkout: PathBuf,
    home: PathBuf,
    path: String,
    admitted: BTreeMap<String, String>,
}

impl Nyl {
    /// `path` is the `PATH` the tools Nyl runs (`git`, `tofu`, `docker`) are
    /// found on; `home` is an empty directory that holds no configuration.
    pub fn new(binary: PathBuf, checkout: PathBuf, home: PathBuf, path: String) -> Self {
        Self {
            binary,
            checkout,
            home,
            path,
            admitted: BTreeMap::new(),
        }
    }

    /// Make `value` available to Nyl as `name`, as a job that holds the
    /// credential would. Only variables a unit admits reach a tool.
    pub fn admit(&mut self, name: impl Into<String>, value: impl Into<String>) {
        self.admitted.insert(name.into(), value.into());
    }

    /// The whole environment of a `nyl` process: a search path, an empty home,
    /// no ambient Git configuration, plain output, and what was admitted.
    pub fn environment(&self) -> BTreeMap<String, String> {
        let home = self.home.to_string_lossy().into_owned();
        let mut environment = BTreeMap::from([
            ("PATH".to_owned(), self.path.clone()),
            ("HOME".to_owned(), home.clone()),
            ("XDG_CONFIG_HOME".to_owned(), home),
            ("GIT_CONFIG_NOSYSTEM".to_owned(), "1".to_owned()),
            ("NO_COLOR".to_owned(), "1".to_owned()),
        ]);
        environment.extend(self.admitted.clone());
        environment
    }

    /// The command `nyl <args>` in the checkout. Long-running ones (a reconcile
    /// builds an image and applies OpenTofu) get a generous timeout.
    pub fn command(&self, args: &[&str]) -> Cmd {
        Cmd::new(&self.binary)
            .args(args)
            .cwd(&self.checkout)
            .env_clear()
            .envs(self.environment())
            .timeout(DEFAULT_TIMEOUT)
            .echo()
            .secrets_from(self.admitted_secret_values())
    }

    /// Admitted values that are credentials and must never be printed.
    fn admitted_secret_values(&self) -> Vec<String> {
        self.admitted
            .iter()
            .filter(|(name, _)| name.contains("TOKEN") || name.contains("SECRET") || name.contains("PASSWORD"))
            .map(|(_, value)| value.clone())
            .collect()
    }

    /// Run `nyl <args> -o json`, requiring one of the exit codes, and parse the
    /// report on stdout. Stderr, where Nyl prints progress, is echoed.
    pub fn report(&self, args: &[&str], exits: &[i32]) -> Result<Report> {
        self.report_within(args, exits, DEFAULT_TIMEOUT)
    }

    /// [`report`](Self::report) with a timeout of the caller's choosing, so a
    /// command that waits for the cluster fails soon after its own bound.
    pub fn report_within(&self, args: &[&str], exits: &[i32], timeout: Duration) -> Result<Report> {
        let mut arguments = args.to_vec();
        arguments.extend(["-o", "json"]);
        let output = self.command(&arguments).timeout(timeout).run_expect(exits)?;
        Report::parse(output)
    }

    /// Run `nyl <args>` and return its trimmed stdout; for `nyl get`.
    pub fn text(&self, args: &[&str]) -> Result<String> {
        Ok(self.command(args).run_ok()?.stdout_trimmed().to_owned())
    }
}

/// A `-o json` report and the exit code that came with it.
#[derive(Clone, Debug)]
pub struct Report {
    pub exit: i32,
    pub json: Value,
    pub command: String,
}

impl Report {
    fn parse(output: Output) -> Result<Self> {
        let json = serde_json::from_str::<Value>(&output.stdout).map_err(|error| {
            Error::new(format!(
                "`{}` printed no JSON report ({error}); stdout was:\n{}",
                output.command,
                crate::cmd::truncate_tail(&output.stdout, 40)
            ))
        })?;
        Ok(Self {
            exit: output.code.context("the process ended without an exit code")?,
            json,
            command: output.command,
        })
    }

    /// One unit's entry in the report of the first environment.
    pub fn unit(&self, name: &str) -> Result<&Value> {
        unit(&self.json, name).with_report(self)
    }

    /// A unit's `result`, as `reconcile` and `teardown` report it.
    pub fn result(&self, name: &str) -> Result<&str> {
        self.unit(name)?.get("result").and_then(Value::as_str).with_report(self)
    }
}

trait WithReport<T> {
    fn with_report(self, report: &Report) -> Result<T>;
}

impl<T> WithReport<T> for Option<T> {
    fn with_report(self, report: &Report) -> Result<T> {
        self.ok_or_else(|| {
            Error::new(format!(
                "the report of `{}` lacks the expected field:\n{}",
                report.command,
                serde_json::to_string_pretty(&report.json).unwrap_or_default()
            ))
        })
    }
}

/// `environments[0].units[name]` of a report.
pub fn unit<'a>(report: &'a Value, name: &str) -> Option<&'a Value> {
    report.get("environments")?.get(0)?.get("units")?.get(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nyl() -> Nyl {
        Nyl::new(
            PathBuf::from("/bin/nyl"),
            PathBuf::from("/work/clone"),
            PathBuf::from("/work/home"),
            "/usr/bin:/bin".to_owned(),
        )
    }

    #[test]
    fn test_nyl_sees_only_what_is_admitted_and_nothing_ambient() {
        let mut nyl = nyl();
        nyl.admit("KUBECONFIG", "/work/kubeconfig");
        let environment = nyl.environment();
        let names = environment.keys().map(String::as_str).collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                "GIT_CONFIG_NOSYSTEM",
                "HOME",
                "KUBECONFIG",
                "NO_COLOR",
                "PATH",
                "XDG_CONFIG_HOME"
            ]
        );
        assert_eq!(environment["HOME"], "/work/home");
        assert_eq!(environment["KUBECONFIG"], "/work/kubeconfig");
    }

    #[test]
    fn test_the_command_never_prints_an_admitted_token() {
        let mut nyl = nyl();
        nyl.admit("ARGOCD_AUTH_TOKEN", "s3cr3t-token");
        nyl.admit("KUBECONFIG", "/work/kubeconfig");
        assert_eq!(nyl.admitted_secret_values(), vec!["s3cr3t-token".to_owned()]);
    }

    #[test]
    fn test_docker_selectors_take_the_environment_and_fall_back_to_the_home_config() {
        let environment = |name: &str| match name {
            "DOCKER_HOST" => Some("unix:///run/docker.sock".to_owned()),
            _ => None,
        };
        let with_home = docker_selectors(environment, Some(Path::new("/home/ci")), |path| {
            path == Path::new("/home/ci/.docker")
        });
        assert_eq!(with_home.len(), 2);
        assert_eq!(with_home["DOCKER_HOST"], "unix:///run/docker.sock");
        assert_eq!(
            Path::new(&with_home["DOCKER_CONFIG"]),
            Path::new("/home/ci").join(".docker")
        );
        let without_config = docker_selectors(environment, Some(Path::new("/home/ci")), |_| false);
        assert_eq!(without_config.keys().collect::<Vec<_>>(), ["DOCKER_HOST"]);
        let explicit = docker_selectors(
            |name| (name == "DOCKER_CONFIG").then(|| "/elsewhere".to_owned()),
            Some(Path::new("/home/ci")),
            |_| true,
        );
        assert_eq!(explicit["DOCKER_CONFIG"], "/elsewhere");
    }

    #[test]
    fn test_unit_reads_the_first_environments_units() {
        let report = serde_json::json!({"environments": [{"units": {"database": {"result": "executed"}}}]});
        assert_eq!(unit(&report, "database").unwrap()["result"], "executed");
        assert!(unit(&report, "missing").is_none());
        assert!(unit(&serde_json::json!({}), "database").is_none());
    }
}
