//! The two-environment project of the cross-environment tests: `dev` observes
//! a value and `prod` reads it through a `fromUnit` reference.

use std::sync::Arc;

use serde_json::json;

use super::observing::{value, Observing, Probe, Read};
use super::shell::{self, project, NYL_TOML};
use nyl_units::command::CommandDriver;
use serde_json::Value;

fn unit<'a>(report: &'a Value, name: &str) -> &'a Value {
    &report["environments"][0]["units"][name]
}

/// `dev` observes the `ip` of `lb`; `dns` of `prod` reads it through
/// `fromUnit: {environment: dev, …}`.
pub const CROSS_UNITS: &str = "apiVersion: units.gitops.nyl/v1
kind: Command
metadata:
  name: lb
  labels: {dev: 'true'}
spec:
  command: [sh, -c, 'true']
  observations:
    ip: {type: string, from: {service: web}}
---
apiVersion: units.gitops.nyl/v1
kind: Command
metadata:
  name: dns
  labels: {prod: 'true'}
spec:
  command: [sh, -c, 'true']
  values:
    address: {fromUnit: {environment: dev, unit: lb, observation: ip}}
";

pub fn environment(name: &str, state: &str) -> String {
    format!(
        "apiVersion: gitops.nyl/v1\nkind: Environment\nmetadata:\n  name: {name}\nspec:\n  unitSelector:\n    matchLabels: {{{name}: 'true'}}\n  allowUnprotectedSource: true\n{state}"
    )
}

/// A project with `dev` and `prod`, both initialized, `lb` observed as
/// 203.0.113.10 and `dns` reconciled with it.
pub struct Cross {
    pub shell: shell::Harness,
    pub probe: Arc<Probe>,
    _project: tempfile::TempDir,
}

impl Cross {
    pub fn new() -> Self {
        let project = project(&[
            NYL_TOML,
            ("nyl/dev.yaml", &environment("dev", "")),
            ("nyl/prod.yaml", &environment("prod", "")),
            ("nyl/units.yaml", CROSS_UNITS),
        ]);
        let mut shell = shell::Harness::new(project.path(), true);
        let probe = Arc::new(Probe::default());
        shell.drivers.register(Arc::new(Observing {
            inner: CommandDriver,
            probe: probe.clone(),
        }));
        let cross = Self {
            shell,
            probe,
            _project: project,
        };
        for name in ["dev", "prod"] {
            assert_eq!(cross.shell.invoke(&["state", "init", "-e", name]).exit, 0);
        }
        cross.read("203.0.113.10");
        assert_eq!(cross.shell.invoke(&["reconcile", "-e", "dev"]).exit, 0);
        let ran = cross.shell.invoke(&["reconcile", "-e", "prod", "-o", "json"]);
        assert_eq!(ran.exit, 0, "{}", ran.stderr);
        assert_eq!(unit(&ran.report, "dns")["result"], "executed");
        cross
    }

    /// The next read of `lb` finds `ip`.
    pub fn read(&self, ip: &'static str) {
        self.probe.script("lb", [Read::Values(vec![value("ip", json!(ip))])]);
    }

    /// Commit a changed declaration of an environment to the project's branch.
    pub fn declare(&self, name: &str, state: &str) {
        self.shell.world.commit_files(
            "main",
            "change the state of an environment",
            &[(&format!("nyl/{name}.yaml"), &environment(name, state))],
        );
    }

    pub fn status_of_prod(&self) -> nyl::orchestration::run::CommandOutcome {
        self.shell.invoke(&["status", "-e", "prod", "-o", "json"])
    }
}
