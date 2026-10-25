//! What `plan`, `reconcile`, and `status` share: locating the project and
//! its environment, building the shell's request and context, and rendering
//! the one typed [`RunReport`] as a table or JSON with its exit category.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::Path;

use nyl_core::orchestration::codes::{Code, Finding};
use nyl_core::orchestration::ids::{CommitId, EnvironmentName, UnitName};
use nyl_core::orchestration::time::Duration;
use nyl_core::resources::environment::{effective_environment, Environment};
use nyl_orchestration::run_report::{EnvironmentRunReport, ReportKind, RunReport, UnitReport};
use nyl_orchestration::shell::run::RunRequest;
use nyl_orchestration::shell::{Context, Settings};
use nyl_state::git::{CommitSigner, GitStateStore};
use nyl_state::source::GitSourceResolver;

use crate::cli::commands::reconcile::RunOutput;
use crate::cli::table::{render as render_table, Cell};
use crate::gitops::units::UnitSourceRenderer;
use crate::gitops::{discover_gitops_inventory, GitOpsInventory};
use crate::orchestration::project::{
    declared, open_store, requested_by as requested_by_default, resolve_environment, ResolvedEnvironment,
};
use crate::orchestration::run::{CommandOutcome, Wiring};
use crate::NylError;

/// Why a command stopped before the shell produced a report.
pub enum Stop {
    /// A coded finding, reported in the command's typed report (exit 1, or
    /// the finding's own category).
    Finding(Finding),
    /// Anything else, such as a project that cannot be discovered.
    Error(NylError),
}

impl From<Finding> for Stop {
    fn from(finding: Finding) -> Self {
        Self::Finding(finding)
    }
}

impl From<NylError> for Stop {
    fn from(error: NylError) -> Self {
        Self::Error(error)
    }
}

/// The project and the environment a command works on.
pub struct Session {
    pub inventory: GitOpsInventory,
    pub resolved: ResolvedEnvironment,
}

impl Session {
    /// Discover the project at `path` and resolve `environment` in it.
    pub fn open(path: &Path, environment: &str) -> Result<Self, Stop> {
        let inventory = discover_gitops_inventory(path, None)?;
        let resolved = resolve_environment(&inventory, environment)?;
        Ok(Self { inventory, resolved })
    }

    /// The Environment as the entry worktree declares it, with its group's
    /// defaults applied.
    pub fn entry(&self) -> Result<Environment, Finding> {
        let (_, groups) = declared(&self.inventory);
        effective_environment(&self.resolved.environment, &groups)
    }

    /// The commit the clone's `HEAD` names: S when the Environment names no
    /// source.
    pub fn entry_commit(&self) -> Result<CommitId, Finding> {
        let unavailable = |message: String| {
            Finding::new(
                Code::SourceUnavailable,
                message,
                "run Nyl in a clone of the project's repository with a checked-out commit",
            )
        };
        let repository = git2::Repository::open(&self.resolved.git_dir)
            .map_err(|error| unavailable(format!("cannot open the project's clone: {}", error.message())))?;
        let commit = repository
            .head()
            .and_then(|head| head.peel_to_commit())
            .map_err(|error| unavailable(format!("the clone has no HEAD commit: {}", error.message())))?;
        CommitId::new(commit.id().to_string()).map_err(|error| unavailable(error.to_string()))
    }

    /// The worktree-rooted directory of `nyl.toml`, such as `/nyl`, or `/`
    /// at the worktree root.
    pub fn entry_project_dir(&self) -> String {
        let root = self
            .inventory
            .project_root
            .canonicalize()
            .unwrap_or_else(|_| self.inventory.project_root.clone());
        let relative = root
            .strip_prefix(&self.inventory.worktree_root)
            .map(|relative| {
                relative
                    .components()
                    .map(|component| component.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/")
            })
            .unwrap_or_default();
        format!("/{relative}")
    }

    /// `[project] previous_paths` of the entry `nyl.toml`.
    pub fn previous_paths(&self) -> Vec<String> {
        self.inventory.project_config.config.project.previous_paths.clone()
    }

    /// The state store, writing with `signer` or read-only without one.
    pub fn store(&self, signer: Option<CommitSigner>) -> Result<GitStateStore, NylError> {
        open_store(&self.inventory, &self.resolved, signer)
    }

    /// Sources in the clone, through the project's Git credentials.
    pub fn sources(&self) -> Result<GitSourceResolver, NylError> {
        let auth = crate::cli::git_auth::project_git_auth(&self.inventory.project_config.config)?;
        Ok(GitSourceResolver::new(&self.resolved.git_dir, auth))
    }

    /// The renderer of the environment's units at a source commit.
    pub fn renderer(&self) -> UnitSourceRenderer {
        UnitSourceRenderer::new(self.entry_project_dir()).with_previous_paths(self.previous_paths())
    }

    /// The shell context of one command.
    pub fn context<'a>(
        &self,
        store: &'a GitStateStore,
        wiring: &'a Wiring<'_>,
        requested_by: Option<String>,
    ) -> Context<'a> {
        Context {
            store,
            clock: wiring.clock.as_ref(),
            ids: wiring.ids,
            runner: wiring.runner.clone(),
            requested_by: requested_by.unwrap_or_else(|| requested_by_default(&self.resolved.git_dir, &wiring.runner)),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            settings: Settings::default(),
        }
    }

    /// Add the warnings of resolving the environment to `report`.
    pub fn add_warnings(&self, report: &mut RunReport) {
        for environment in &mut report.environments {
            let mut warnings = self.resolved.warnings.clone();
            warnings.append(&mut environment.warnings);
            environment.warnings = warnings;
        }
    }

    /// The request of one `plan` or `reconcile`.
    pub fn request(&self, options: &RunOptions<'_>) -> Result<RunRequest, Finding> {
        let units = if options.units.is_empty() {
            None
        } else {
            let mut names = BTreeSet::new();
            for unit in options.units {
                names.insert(UnitName::new(unit).map_err(|error| {
                    Finding::new(
                        Code::UnitUnknown,
                        format!("--unit {unit:?} is not a unit name: {error}"),
                        "name units as `nyl get units -e <env>` lists them",
                    )
                })?);
            }
            Some(names)
        };
        Ok(RunRequest {
            environment: self.resolved.name.clone(),
            entry: self.entry()?,
            location: self.resolved.location.clone(),
            entry_commit: self.entry_commit()?,
            entry_project_dir: self.entry_project_dir(),
            units,
            concurrency: options.concurrency.max(1),
            source_override: options.source.map(str::to_owned),
            wait: options.wait,
        })
    }
}

/// The options of a `plan` or `reconcile` request.
pub struct RunOptions<'a> {
    pub units: &'a [String],
    pub concurrency: usize,
    pub source: Option<&'a str>,
    pub wait: Option<Duration>,
}

/// A report of `kind` for `environment` that holds only `finding`, for a
/// command that stopped before the shell ran.
pub fn finding_report(kind: ReportKind, environment: &str, finding: Finding) -> RunReport {
    let mut report = RunReport::new(kind);
    // An environment name the project cannot hold is reported under a
    // placeholder; the finding names the given one.
    let name = EnvironmentName::new(environment)
        .or_else(|_| EnvironmentName::new("unknown"))
        .expect("a valid placeholder name");
    let mut environment = EnvironmentRunReport::new(name);
    environment.errors.push(finding);
    environment.finish(kind);
    report.environments.push(environment);
    report
}

/// Render `report` in `output` as the command's outcome. `exit` is the
/// category the command exits with, which is the report's own unless a
/// command option, such as `plan --fail-on-leaving`, raises it.
pub fn outcome(report: &RunReport, output: RunOutput, exit: u8) -> CommandOutcome {
    let json = serde_json::to_value(report).expect("reports serialize");
    let stdout = match output {
        RunOutput::Json => format!("{}\n", serde_json::to_string_pretty(&json).expect("JSON prints")),
        RunOutput::Table => table(report),
    };
    CommandOutcome {
        exit,
        report: json,
        stdout,
        stderr: diagnostics(report),
    }
}

/// The outcome of a command that stopped: a finding in a typed report, or
/// an error.
pub fn stopped(kind: ReportKind, environment: &str, output: RunOutput, stop: Stop) -> CommandOutcome {
    match stop {
        Stop::Finding(finding) => {
            let report = finding_report(kind, environment, finding);
            let exit = report.exit().code();
            outcome(&report, output, exit)
        }
        Stop::Error(error) => CommandOutcome::error(error),
    }
}

/// The table form of a report: the leaving section first, then one row per unit.
fn table(report: &RunReport) -> String {
    let mut text = String::new();
    for environment in &report.environments {
        if !environment.leaving.is_empty() {
            let _ = writeln!(
                text,
                "Units leaving the ownership set of {} ({}):",
                environment.name,
                environment.leaving.len()
            );
            let rows = environment
                .leaving
                .iter()
                .map(|leaving| {
                    vec![
                        Cell::plain(&leaving.unit),
                        Cell::plain(&leaving.kind),
                        Cell::plain(&leaving.reason),
                        Cell::plain(leaving.consequences.join("; ")),
                    ]
                })
                .collect::<Vec<_>>();
            for line in render_table(&["UNIT", "KIND", "REASON", "CONSEQUENCES"], &rows).lines() {
                let _ = writeln!(text, "  {line}");
            }
            text.push('\n');
        }
        let rows = environment
            .units
            .iter()
            .map(|(name, unit)| unit_row(report.kind, name, unit))
            .collect::<Vec<_>>();
        let headers: &[&str] = match report.kind {
            ReportKind::PlanReport => &["UNIT", "KIND", "STATE", "RECEIPT", "CHANGES", "DETAIL"],
            ReportKind::ReconcileReport => &["UNIT", "KIND", "RESULT", "DETAIL"],
            ReportKind::StatusReport => &["UNIT", "KIND", "STATE", "RECEIPT", "LAST RUN", "DETAIL"],
        };
        if rows.is_empty() {
            let _ = writeln!(text, "{}: no units", environment.name);
        } else {
            let _ = writeln!(text, "{}", render_table(headers, &rows));
        }
        if let Some(run) = &environment.run {
            let _ = writeln!(text, "run: {run}");
        }
        if let Some(source) = &environment.source_commit {
            let _ = writeln!(text, "source: {source}");
        }
        for (side, commit) in &environment.commits {
            let _ = writeln!(text, "{side}: {commit}");
        }
    }
    text
}

fn unit_row(kind: ReportKind, name: &UnitName, unit: &UnitReport) -> Vec<Cell> {
    let dash = || "-".to_owned();
    let outcome = unit.outcome_name().map_or_else(dash, str::to_owned);
    let receipt = unit
        .receipt
        .and_then(|receipt| serde_json::to_value(receipt).ok())
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(dash);
    let detail = detail(unit);
    let cells = match kind {
        ReportKind::PlanReport => {
            let changes = unit
                .changes
                .and_then(|change| serde_json::to_value(change).ok())
                .and_then(|value| value.as_str().map(str::to_owned))
                .unwrap_or_else(dash);
            vec![name.to_string(), unit.kind.clone(), outcome, receipt, changes, detail]
        }
        ReportKind::ReconcileReport => vec![name.to_string(), unit.kind.clone(), outcome, detail],
        ReportKind::StatusReport => {
            let last_run = unit.last_run.as_ref().map_or_else(dash, ToString::to_string);
            vec![name.to_string(), unit.kind.clone(), outcome, receipt, last_run, detail]
        }
    };
    cells.into_iter().map(Cell::plain).collect()
}

/// The most useful single line about a unit: its plan error, blockers,
/// failure message, or driver summary.
fn detail(unit: &UnitReport) -> String {
    if let Some(error) = &unit.plan_error {
        return format!("plan failed: {error}");
    }
    if !unit.blocked_by.is_empty() {
        return unit
            .blocked_by
            .iter()
            .map(|blocked| match &blocked.unit {
                Some(producer) => format!("{} waits for {producer} ({})", blocked.pointer, blocked.reason),
                None => format!("{}: {}", blocked.pointer, blocked.reason),
            })
            .collect::<Vec<_>>()
            .join("; ");
    }
    if let Some(nyl_orchestration::run_report::UnitResult::Blocked { reason }) = &unit.result {
        return reason.clone();
    }
    unit.message
        .clone()
        .or_else(|| unit.summary.clone())
        .map(|text| text.lines().next().unwrap_or_default().to_owned())
        .unwrap_or_default()
}

/// Warnings and errors of a report, as printed to stderr.
fn diagnostics(report: &RunReport) -> String {
    let mut text = String::new();
    for environment in &report.environments {
        for finding in &environment.warnings {
            write_finding(&mut text, "warning", None, finding);
        }
        for (name, unit) in &environment.units {
            for finding in &unit.warnings {
                write_finding(&mut text, "warning", Some(name), finding);
            }
        }
        for finding in &environment.errors {
            write_finding(&mut text, "error", None, finding);
        }
    }
    text
}

/// Print one finding with its code and fix.
pub fn write_finding(text: &mut String, level: &str, unit: Option<&UnitName>, finding: &Finding) {
    let unit = unit.map(|unit| format!(" ({unit})")).unwrap_or_default();
    let _ = writeln!(
        text,
        "{level} {}{unit}: {}\n  fix: {}",
        finding.code, finding.message, finding.fix
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use nyl_orchestration::run_report::UnitResult;

    /// Catches the table and the JSON telling different stories: both come
    /// from one report, and the exit is the one the command passes.
    #[test]
    fn test_outcome_renders_table_and_json_from_one_report() {
        let mut report = RunReport::new(ReportKind::ReconcileReport);
        let mut environment = EnvironmentRunReport::new(EnvironmentName::new("dev").unwrap());
        environment.units.insert(
            UnitName::new("web").unwrap(),
            UnitReport {
                result: Some(UnitResult::Blocked {
                    reason: "network has no current receipt".to_owned(),
                }),
                ..UnitReport::new("Command")
            },
        );
        environment.finish(ReportKind::ReconcileReport);
        report.environments.push(environment);
        let table = outcome(&report, RunOutput::Table, 2);
        assert_eq!(table.exit, 2);
        assert!(table.stdout.contains("web"), "{}", table.stdout);
        assert!(table.stdout.contains("blocked"), "{}", table.stdout);
        assert!(
            table.stdout.contains("network has no current receipt"),
            "{}",
            table.stdout
        );
        let json = outcome(&report, RunOutput::Json, 2);
        assert_eq!(json.report["environments"][0]["units"]["web"]["result"], "blocked");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&json.stdout).unwrap(),
            json.report
        );
    }

    /// Catches a finding before the shell ran losing its code or exit category.
    #[test]
    fn test_stopped_reports_the_finding_with_its_category() {
        let held = Finding::new(Code::LeaseHeld, "held by ci", "wait");
        let outcome = stopped(ReportKind::ReconcileReport, "dev", RunOutput::Json, Stop::Finding(held));
        assert_eq!(outcome.exit, 2);
        assert_eq!(outcome.report["environments"][0]["errors"][0]["code"], "NYL-LEASE-HELD");
        assert!(outcome.stderr.contains("NYL-LEASE-HELD"), "{}", outcome.stderr);
    }
}
