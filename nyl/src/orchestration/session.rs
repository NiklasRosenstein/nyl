//! What `plan`, `reconcile`, and `status` share: locating the project and
//! its environment, building the shell's request and context, and rendering
//! the one typed [`RunReport`] as a table or JSON with its exit category.

use colored::Colorize as _;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::Path;

use nyl_core::orchestration::codes::{Code, Finding};
use nyl_core::orchestration::ids::{CommitId, EnvironmentName, UnitName};
use nyl_core::orchestration::ports::{CommitIdentity, StateStore};
use nyl_core::orchestration::time::Duration;
use nyl_core::resources::environment::{effective_environment, Environment};
use nyl_orchestration::run_report::{
    EnvironmentRunReport, LeavingConsequence, LeavingUnit, ReportKind, RunReport, UnitReport,
};
use nyl_orchestration::shell::run::{RunContext, RunRequest};
use nyl_orchestration::shell::{Context, Settings};
use nyl_state::git::{CommitAuthor, GitStateStore};
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
        project_dir(&self.inventory)
    }

    /// `[project] previous_paths` of the entry `nyl.toml`.
    pub fn previous_paths(&self) -> Vec<String> {
        self.inventory.project_config.config.project.previous_paths.clone()
    }

    /// The state store, writing with `author` or read-only without one.
    pub fn store(&self, author: Option<CommitAuthor>) -> Result<GitStateStore, NylError> {
        open_store(&self.inventory, &self.resolved, author)
    }

    /// Sources in the clone, through the project's Git credentials.
    pub fn sources(&self) -> Result<GitSourceResolver, NylError> {
        let auth = crate::cli::git_auth::project_git_auth(&self.inventory.project_config.config)?;
        Ok(GitSourceResolver::new(&self.resolved.git_dir, auth))
    }

    /// Wire explicit working-file capture through the shared source shell.
    pub fn snapshot_worktree(
        &self,
        wiring: &Wiring<'_>,
        author: Option<&str>,
    ) -> Result<nyl_core::orchestration::ports::WorktreeSnapshot, Stop> {
        let identity = wiring.commit_identity(&self.resolved.git_dir, author)?;
        let store = self.store(None)?;
        let sources = self.sources()?;
        nyl_orchestration::shell::sources::snapshot_worktree(
            &store,
            &sources,
            &self.resolved.location,
            &self.entry_commit()?,
            &identity,
            wiring.clock.as_ref(),
        )
        .map_err(|error| {
            Finding::new(
                Code::SourceUnavailable,
                error,
                "check state access and worktree paths; symlink targets must stay inside the repository",
            )
            .into()
        })
    }

    /// The renderer of the environment's units at a source commit.
    pub fn renderer(&self) -> UnitSourceRenderer {
        UnitSourceRenderer::new(self.entry_project_dir()).with_previous_paths(self.previous_paths())
    }

    /// The shell context of one command, working through `store`: the
    /// environment's state store, or a [`LocalStore`](nyl_orchestration::shell::local::LocalStore)
    /// wrapping it for `--local`.
    pub fn context<'a>(
        &self,
        store: &'a dyn StateStore,
        wiring: &'a Wiring<'_>,
        requested_by: Option<String>,
    ) -> Context<'a> {
        Context {
            store,
            clock: wiring.clock.as_ref(),
            ids: wiring.ids,
            faults: wiring.faults,
            runner: wiring.runner.clone(),
            requested_by: requested_by.unwrap_or_else(|| requested_by_default(&self.resolved.git_dir, &wiring.runner)),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            settings: Settings::default(),
        }
    }

    /// Run `operation` of the run shell through `store` as `identity`
    /// (`None` for a command that writes nothing), in a fresh workspace,
    /// with the warnings of resolving the environment added to its report.
    pub fn run(
        &self,
        wiring: &Wiring<'_>,
        store: &dyn StateStore,
        identity: Option<&CommitIdentity>,
        request: &RunRequest,
        operation: fn(&RunContext<'_>, &RunRequest) -> RunReport,
    ) -> Result<RunReport, Stop> {
        let mut report = self.run_with(wiring, store, identity, request, operation)?;
        self.annotate_report(&mut report);
        Ok(report)
    }

    /// [`Self::run`] for an operation on the run path whose report is not a
    /// [`RunReport`], such as `recover_environment` and `state_delete`
    /// (a `StateOperationReport`): the same sources, renderer, drivers,
    /// workspace, prompt, and observer, without the environment's warnings.
    pub fn run_with<T>(
        &self,
        wiring: &Wiring<'_>,
        store: &dyn StateStore,
        identity: Option<&CommitIdentity>,
        request: &RunRequest,
        operation: impl FnOnce(&RunContext<'_>, &RunRequest) -> T,
    ) -> Result<T, Stop> {
        let sources = self.sources()?;
        let renderer = self.renderer();
        let workspace = tempfile::TempDir::new().map_err(NylError::from)?;
        let base = self.context(store, wiring, identity.map(ToString::to_string));
        let secrets = crate::orchestration::secrets::ProjectSecrets::new(&self.inventory.project_root);
        let context = RunContext {
            base: &base,
            sources: &sources,
            renderer: &renderer,
            drivers: wiring.drivers,
            tools: wiring.tools,
            workspace: workspace.path(),
            process_env: &wiring.process_env,
            secrets: Some(&secrets),
            commit_identity: identity,
            observer: wiring.observer,
            progress: wiring.progress,
            prompt: wiring.prompt,
        };
        Ok(operation(&context, request))
    }

    /// Add source repository context and environment-resolution warnings to the report.
    pub fn annotate_report(&self, report: &mut RunReport) {
        for environment in &mut report.environments {
            if environment.source_commit.is_some() {
                environment.source_repository = Some(self.resolved.source_repository.clone());
            }
            let mut warnings = self.resolved.warnings.clone();
            warnings.append(&mut environment.warnings);
            environment.warnings = warnings;
        }
    }

    /// The `--local` options of a run, after checking that the state has
    /// a local view (`NYL-LOCAL-RUN-UNSUPPORTED` otherwise): remote state
    /// whose refs are branches. `approve` are the `--approve` units.
    pub fn local_run(
        &self,
        local_lease: bool,
        approve: &[String],
    ) -> Result<nyl_orchestration::shell::local::LocalRun, Finding> {
        use nyl_orchestration::shell::local::{LeaseScope, LocalRun};
        nyl_orchestration::rules::local::view(&self.resolved.location)?;
        Ok(LocalRun {
            lease: if local_lease {
                LeaseScope::Local
            } else {
                LeaseScope::Remote
            },
            approve: unit_names("--approve", approve)?,
        })
    }

    /// The request of one `plan` or `reconcile`.
    pub fn request(&self, options: &RunOptions<'_>) -> Result<RunRequest, Finding> {
        let units = if options.units.is_empty() {
            None
        } else {
            Some(unit_names("--unit", options.units)?)
        };
        let mut environments = BTreeMap::new();
        let mut environment_errors = BTreeMap::new();
        for environment in declared(&self.inventory).0 {
            match resolve_environment(&self.inventory, &environment.metadata.name) {
                Ok(resolved) => {
                    environments.insert(resolved.name, resolved.location);
                }
                Err(finding) => {
                    if let Ok(name) = environment.name() {
                        environment_errors.insert(name, finding);
                    }
                }
            }
        }
        Ok(RunRequest {
            planned: BTreeMap::new(),
            approvals: nyl_orchestration::rules::approval::Approvals::default(),
            environments,
            environment_errors,
            unavailable: BTreeSet::new(),
            environment: self.resolved.name.clone(),
            entry: self.entry()?,
            location: self.resolved.location.clone(),
            entry_commit: self.entry_commit()?,
            entry_project_dir: self.entry_project_dir(),
            units,
            concurrency: options.concurrency.max(1),
            source_override: options.source.map(str::to_owned),
            worktree_source: None,
            wait: options.wait,
            local: options.local.clone(),
            cancellation: options.cancellation.clone(),
            mode: options.mode.clone(),
            lifecycle: options.lifecycle,
            reapply: options.reapply,
            fail_fast: options.fail_fast,
            reason: options.reason.map(str::to_owned),
        })
    }
}

/// Unit names as an option names them; `flag` names the option in the finding.
pub fn unit_names(flag: &str, units: &[String]) -> Result<BTreeSet<UnitName>, Finding> {
    units
        .iter()
        .map(|unit| {
            UnitName::new(unit).map_err(|error| {
                Finding::new(
                    Code::UnitUnknown,
                    format!("{flag} {unit:?} is not a unit name: {error}"),
                    "name units as `nyl get units -e <env>` lists them",
                )
            })
        })
        .collect()
}

/// The worktree-rooted directory of the inventory's `nyl.toml`, such as
/// `/nyl`, or `/` at the worktree root.
pub fn project_dir(inventory: &GitOpsInventory) -> String {
    let root = inventory
        .project_root
        .canonicalize()
        .unwrap_or_else(|_| inventory.project_root.clone());
    let relative = root
        .strip_prefix(&inventory.worktree_root)
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

/// The options of a `plan`, `reconcile`, or `verify` request.
pub struct RunOptions<'a> {
    pub units: &'a [String],
    pub concurrency: usize,
    pub source: Option<&'a str>,
    pub wait: Option<Duration>,
    /// `--local`, `--local-lease`, and `--approve` of `reconcile`; `None` elsewhere.
    pub local: Option<nyl_orchestration::shell::local::LocalRun>,
    /// The wiring's cancellation for `reconcile` and `verify`; never set for `plan`.
    pub cancellation: nyl_orchestration::shell::run::Cancellation,
    /// The operation on the run path: reconcile, a recover import, or a teardown.
    pub mode: nyl_orchestration::shell::run::RunMode,
    /// `--allow-teardown`, `--allow-dependents`, `--allow-incomplete`,
    /// `--leaving`, `--confirm-removed`, and `--hold`.
    pub lifecycle: nyl_orchestration::shell::run::LifecycleOptions,
    /// `reconcile --reapply` (with `--unit`).
    pub reapply: bool,
    /// `--fail-fast` of `reconcile` and `teardown`.
    pub fail_fast: bool,
    /// `--reason` of an operator action.
    pub reason: Option<&'a str>,
}

/// `environment` as a name for a report of a command that stopped before
/// resolving it: a name the project cannot hold is reported under a
/// placeholder, and the finding names the given one.
pub fn placeholder_environment(environment: &str) -> EnvironmentName {
    EnvironmentName::new(environment)
        .or_else(|_| EnvironmentName::new("unknown"))
        .expect("a valid placeholder name")
}

/// The pretty-printed JSON a command prints for `--output json`.
pub fn pretty_json(value: &serde_json::Value) -> String {
    format!("{}\n", serde_json::to_string_pretty(value).expect("JSON prints"))
}

/// A report of `kind` for `environment` that holds only `finding`, for a
/// command that stopped before the shell ran.
fn finding_report(kind: ReportKind, environment: &str, finding: Finding) -> RunReport {
    let mut report = RunReport::new(kind);
    let mut environment = EnvironmentRunReport::new(placeholder_environment(environment));
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
        RunOutput::Json => pretty_json(&json),
        RunOutput::Table => table(report),
    };
    let mut stderr = diagnostics(report);
    if matches!(output, RunOutput::Table) && !stdout.is_empty() && !stderr.is_empty() {
        stderr.insert(0, '\n');
    }
    CommandOutcome {
        exit,
        report: json,
        stdout,
        stderr,
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

/// The groups of the leaving section, one line per consequence as the
/// contract's Planning section shows them: each unit with its reason, and a
/// deselected unit with the selector labels it no longer matches.
fn leaving_section(leaving: &[LeavingUnit]) -> String {
    let headings = LeavingConsequence::ALL.map(|consequence| format!("{}:", consequence.heading()));
    let width = headings.iter().map(String::len).max().unwrap_or(0);
    let mut text = String::new();
    for (consequence, heading) in LeavingConsequence::ALL.into_iter().zip(&headings) {
        let units = leaving
            .iter()
            .filter(|unit| unit.consequence == consequence)
            .map(|unit| {
                let mut detail = unit.reason.clone();
                if !unit.labels.is_empty() {
                    let labels = unit
                        .labels
                        .iter()
                        .map(|(key, value)| format!("{key}={value}"))
                        .collect::<Vec<_>>()
                        .join(",");
                    detail = format!("{detail}, no longer matches {labels}");
                }
                format!("{} ({detail})", unit.unit)
            })
            .collect::<Vec<_>>();
        let units = if units.is_empty() {
            "(none)".to_owned()
        } else {
            units.join(", ")
        };
        let _ = writeln!(text, "  {heading:<width$} {units}");
    }
    text
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
            text.push_str(&leaving_section(&environment.leaving));
            text.push('\n');
        }
        let rows = environment
            .ordered_units()
            .map(|(name, unit)| unit_row(report.kind, name, unit))
            .collect::<Vec<_>>();
        let headers: &[&str] = match report.kind {
            ReportKind::PlanReport => &["UNIT", "KIND", "STATE", "RECEIPT", "CHANGES", "DETAIL"],
            ReportKind::ReconcileReport | ReportKind::TeardownReport => &["UNIT", "KIND", "RESULT", "DETAIL"],
            ReportKind::StatusReport => &["UNIT", "KIND", "STATE", "RECEIPT", "LAST RUN", "DETAIL"],
            ReportKind::VerifyReport => &["UNIT", "KIND", "VERIFICATION", "DETAIL"],
        };
        if rows.is_empty() {
            // A refused operation reaches no units; its error says why.
            if environment.errors.is_empty() {
                let _ = writeln!(text, "{}: no units", environment.name);
            }
        } else {
            let _ = writeln!(text, "{}", render_table(headers, &rows));
        }
        if environment.run.is_some() || environment.source_commit.is_some() || !environment.commits.is_empty() {
            text.push('\n');
        }
        if let Some(run) = &environment.run {
            let _ = writeln!(text, "  run:      {run}");
        }
        if let Some(source) = &environment.source_commit {
            let _ = writeln!(text, "  source:   {source}");
            if let Some(repository) = &environment.source_repository {
                let _ = writeln!(text, "            {repository}");
            }
        }
        for (side, commit) in &environment.commits {
            let _ = writeln!(text, "  {label:<9} {commit}", label = format!("{side}:"));
        }
    }
    text
}

/// A table cell for a value that serializes as a string, such as a receipt
/// state; `-` when there is none.
pub fn cell_label(value: Option<impl serde::Serialize>) -> String {
    value
        .and_then(|value| serde_json::to_value(value).ok())
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "-".to_owned())
}

/// The state or result cell of a unit: its name, with the condition of a
/// held or deleting unit next to it, such as `held (uncertain)`.
pub fn state_cell(unit: &UnitReport) -> String {
    let outcome = unit.outcome_name().unwrap_or("-");
    match unit.condition {
        Some(condition) => format!("{outcome} ({})", cell_label(Some(condition))),
        None => outcome.to_owned(),
    }
}

/// The kind cell of a unit: `-` for a held marker, which names no kind.
pub fn kind_cell(unit: &UnitReport) -> String {
    if unit.kind.is_empty() {
        "-".to_owned()
    } else {
        unit.kind.clone()
    }
}

fn unit_row(kind: ReportKind, name: &UnitName, unit: &UnitReport) -> Vec<Cell> {
    let dash = || "-".to_owned();
    let outcome = state_cell(unit);
    let receipt = cell_label(unit.receipt);
    let detail = detail(unit);
    let cells = match kind {
        ReportKind::PlanReport => {
            let changes = cell_label(unit.changes);
            vec![name.to_string(), kind_cell(unit), outcome, receipt, changes, detail]
        }
        ReportKind::ReconcileReport | ReportKind::TeardownReport | ReportKind::VerifyReport => {
            vec![name.to_string(), kind_cell(unit), outcome, detail]
        }
        ReportKind::StatusReport => {
            let last_run = unit.last_run.as_ref().map_or_else(dash, ToString::to_string);
            vec![name.to_string(), kind_cell(unit), outcome, receipt, last_run, detail]
        }
    };
    cells.into_iter().map(Cell::plain).collect()
}

/// The most useful single line about a unit: its plan error, blockers,
/// failure message, or driver summary, followed by a held unit's pending
/// changes (`pending changes from <commit>`) and the drift observation.
pub fn detail(unit: &UnitReport) -> String {
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
    let mut parts = Vec::new();
    if let Some(pending) = &unit.pending {
        parts.push(format!("pending changes from {pending}"));
    }
    if let Some(text) = unit.message.as_ref().or(unit.summary.as_ref()) {
        parts.push(text.lines().next().unwrap_or_default().to_owned());
    }
    if let Some(drift) = &unit.drift {
        parts.push(format!("drift: {}", drift.result));
    }
    parts.join("; ")
}

/// A publication's teardown readiness: `ready`, or the unmet requirements.
pub fn readiness_cell(unit: &UnitReport) -> String {
    use nyl_orchestration::run_report::TeardownReadiness;
    match &unit.teardown_readiness {
        None => "-".to_owned(),
        Some(TeardownReadiness::Ready) => "ready".to_owned(),
        Some(TeardownReadiness::Incomplete { reasons }) => format!("incomplete: {}", reasons.join("; ")),
    }
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
    if !text.is_empty() {
        text.push('\n');
    }
    let unit = unit.map(|unit| format!(" ({unit})")).unwrap_or_default();
    let heading = format!("{level} {}{unit}", finding.code);
    let heading = match level {
        "warning" => heading.yellow().bold().to_string(),
        "error" => heading.red().bold().to_string(),
        _ => heading,
    };
    let _ = writeln!(text, "{heading}\n  {}\n  fix: {}", finding.message, finding.fix);
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
        environment.source_commit = Some(CommitId::new("a".repeat(40)).unwrap());
        environment.warnings.push(Finding::new(
            Code::StateLocalDiverged,
            "state branches differ",
            "inspect --local",
        ));
        environment
            .warnings
            .push(Finding::new(Code::DriverError, "unit failed", "retry"));
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
        assert!(table.stdout.contains("\n\n  source:   "), "{}", table.stdout);
        assert_eq!(table.stderr, "\nwarning NYL-STATE-LOCAL-DIVERGED\n  state branches differ\n  fix: inspect --local\n\nwarning NYL-DRIVER-ERROR\n  unit failed\n  fix: retry\n");
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

    /// Catches the leaving section losing its consequence groups, or a
    /// deselected unit hiding the selector labels it no longer matches.
    #[test]
    fn test_leaving_section_groups_units_by_consequence() {
        let leaving = [
            LeavingUnit {
                unit: UnitName::new("network").unwrap(),
                kind: "OpenTofu".to_owned(),
                reason: "removed".to_owned(),
                consequence: LeavingConsequence::Teardown,
                labels: std::collections::BTreeMap::new(),
            },
            LeavingUnit {
                unit: UnitName::new("seed").unwrap(),
                kind: "Command".to_owned(),
                reason: "deselected".to_owned(),
                consequence: LeavingConsequence::Dropped,
                labels: std::collections::BTreeMap::from([("dve".to_owned(), "true".to_owned())]),
            },
        ];
        assert_eq!(
            leaving_section(&leaving),
            "  teardown, needs --allow-teardown:   network (removed)\n  \
             dropped from state, resources kept: seed (deselected, no longer matches dve=true)\n  \
             retained as tombstones:             (none)\n"
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
