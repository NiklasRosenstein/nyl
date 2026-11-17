//! The promotion forms of `nyl get`: `states -e <env>`, `promotion-candidates
//! <path>`, and `promotions -e <env>`.
//!
//! Contract: [Promoting an environment's source](../../../../design/promotion.md#promoting-an-environments-source),
//! "Inspection". Each prints one typed report of
//! `nyl_orchestration::shell::promotion_inspect` as text, JSON, or YAML; they
//! only read state, take no lease, and exit 0 whenever they can read it.
//! `promotion-candidates` resolves the path and the target's units the one way
//! `nyl promote` does, and refuses what it refuses.

use std::path::PathBuf;

use clap::Args;
use nyl_core::orchestration::codes::Finding;
use nyl_core::orchestration::ids::EnvironmentName;
use nyl_orchestration::shell::promote::source_location;
use nyl_orchestration::shell::promotion_inspect::{self, CandidatesRequest};
use nyl_render::gitops::discover_gitops_inventory;
use nyl_render::Result;
use serde::Serialize;

use super::get::GetOutput;
use crate::orchestration::promotion::{promotable_path, target_view};
use crate::orchestration::run::{CommandOutcome, Wiring};
use crate::orchestration::session::{pretty_json, write_finding, Session, Stop};

/// Options of `get states` and `get promotions`.
#[derive(Args, Debug)]
pub struct GetHistoryArgs {
    /// The environment whose state is read.
    #[arg(short = 'e', long = "environment")]
    environment: String,

    /// Read this clone's local state branches, including --local runs.
    #[arg(long)]
    local: bool,

    /// Output format.
    #[arg(short = 'o', long = "output", value_enum, default_value_t)]
    output: GetOutput,

    /// Project directory or a path beneath it.
    #[arg(default_value = ".")]
    path: PathBuf,
}

/// Options of `get promotion-candidates`.
#[derive(Args, Debug)]
pub struct GetCandidatesArgs {
    /// The PromotionPath whose source states are evaluated.
    name: String,

    /// Output format.
    #[arg(short = 'o', long = "output", value_enum, default_value_t)]
    output: GetOutput,

    /// Project directory or a path beneath it.
    #[arg(default_value = ".")]
    path: PathBuf,
}

/// Which history form.
#[derive(Debug, Clone, Copy)]
pub enum HistoryForm {
    States,
    Promotions,
}

/// `nyl get states -e <env>` and `nyl get promotions -e <env>`.
pub fn history(form: HistoryForm, args: &GetHistoryArgs) -> Result<CommandOutcome> {
    let session = match Session::open_offline(&args.path, &args.environment, args.local) {
        Ok(session) => session,
        Err(Stop::Finding(finding)) => return Ok(failed(&finding)),
        Err(Stop::Error(error)) => return Err(error),
    };
    let store = if args.local {
        session.offline_store(None)?
    } else {
        session.store(None)?
    };
    let environment = &session.resolved.name;
    let location = &session.resolved.location;
    let mut outcome = match form {
        HistoryForm::States => {
            let report = promotion_inspect::states(&store, location, environment);
            let names = report.states.iter().map(|state| state.desired_commit.to_string());
            rendered(&report, report.exit, &report.errors, report.table(), names, args.output)?
        }
        HistoryForm::Promotions => {
            // With `record: source` the entry worktree's block is the path's record.
            let promoted = session
                .entry()
                .ok()
                .and_then(|entry| entry.spec.source.and_then(|source| source.from_promotion));
            let report = promotion_inspect::promotions(&store, location, environment, promoted.as_ref());
            let names = report.promotions.iter().map(|record| record.path.to_string());
            rendered(&report, report.exit, &report.errors, report.table(), names, args.output)?
        }
    };
    prepend_warnings(&mut outcome, &session.resolved.warnings);
    Ok(outcome)
}

/// `nyl get promotion-candidates <path>`.
pub fn candidates(args: &GetCandidatesArgs, wiring: &Wiring<'_>) -> Result<CommandOutcome> {
    let inventory = discover_gitops_inventory(&args.path, None, crate::resource_catalogue())?;
    let (path, source, target) = match promotable_path(&inventory, &args.name) {
        Ok(found) => found,
        Err(finding) => return Ok(failed(&finding)),
    };
    let session = match Session::open_offline(&args.path, &target, false) {
        Ok(session) => session,
        Err(Stop::Finding(finding)) => return Ok(failed(&finding)),
        Err(Stop::Error(error)) => return Err(error),
    };
    // What `nyl promote` refuses, inspection refuses too: a broken path
    // would be judged by rules it does not satisfy.
    let mut coverage_warnings = Vec::new();
    let prepared = session
        .check_promotion_paths(wiring)
        .and_then(|warnings| {
            coverage_warnings = crate::orchestration::session::coverage_warnings(&warnings);
            target_view(&session, wiring.drivers, &path)
        })
        .and_then(|view| {
            let source = EnvironmentName::new(&source).map_err(|error| {
                Finding::new(
                    nyl_core::orchestration::codes::Code::PromoteEnvironmentUnknown,
                    format!(
                        "path {} names an invalid source environment: {error}",
                        path.metadata.name
                    ),
                    "correct the path's `spec.from.environment`",
                )
            })?;
            Ok((view, source))
        });
    let (view, source) = match prepared {
        Ok(prepared) => prepared,
        Err(finding) => return Ok(failed(&finding)),
    };
    let (environments, _, errors) = session.environment_locations();
    let source_location = match source_location(&environments, &errors, &source, &path.metadata.name) {
        Ok(location) => location,
        Err(finding) => return Ok(failed(&finding)),
    };
    let store = session.store(None)?;
    let report = promotion_inspect::candidates(
        &store,
        &CandidatesRequest {
            path: &path,
            target: (&session.resolved.name, &session.resolved.location),
            source: (&source, source_location),
            target_units: &view.target_units,
            promotes_source: view.promotes_source,
            promoted: view.promoted.as_ref(),
            now: wiring.clock.now(),
            drivers: wiring.drivers,
        },
    );
    let names = report
        .candidates
        .iter()
        .map(|candidate| candidate.desired_commit.to_string());
    let mut outcome = rendered(&report, report.exit, &report.errors, report.table(), names, args.output)?;
    let mut warnings = session.resolved.warnings.clone();
    warnings.extend(coverage_warnings);
    prepend_warnings(&mut outcome, &warnings);
    Ok(outcome)
}

/// One report as the command's outcome, in the asked format.
fn rendered(
    report: &impl Serialize,
    exit: u8,
    errors: &[Finding],
    table: String,
    names: impl Iterator<Item = String>,
    output: GetOutput,
) -> Result<CommandOutcome> {
    let json = serde_json::to_value(report)?;
    let stdout = match output {
        GetOutput::Json => pretty_json(&json),
        GetOutput::Yaml => nyl_render::yaml::serialize_yaml_value(&json).map_err(nyl_render::NylError::YamlEmit)?,
        GetOutput::Name => names.fold(String::new(), |mut text, name| {
            text.push_str(&name);
            text.push('\n');
            text
        }),
        GetOutput::Table | GetOutput::Wide => table,
    };
    let mut stderr = String::new();
    for finding in errors {
        write_finding(&mut stderr, "error", None, finding);
    }
    Ok(CommandOutcome {
        exit,
        report: json,
        stdout,
        stderr,
    })
}

/// An outcome that failed before a report could be made: exit 1.
fn failed(finding: &Finding) -> CommandOutcome {
    let mut stderr = String::new();
    write_finding(&mut stderr, "error", None, finding);
    CommandOutcome {
        exit: 1,
        report: serde_json::json!({ "errors": [finding] }),
        stdout: String::new(),
        stderr,
    }
}

fn prepend_warnings(outcome: &mut CommandOutcome, warnings: &[Finding]) {
    let mut text = String::new();
    for finding in warnings {
        write_finding(&mut text, "warning", None, finding);
    }
    outcome.stderr = text + &outcome.stderr;
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;

    use crate::cli::Cli;

    fn parses(args: &[&str]) -> bool {
        Cli::try_parse_from(std::iter::once("nyl").chain(args.iter().copied())).is_ok()
    }

    /// `states` and `promotions` read one environment's state, so they need
    /// `-e`; `promotion-candidates` names a path and reads both of its
    /// environments, so it takes no `-e`. Catches a form that reads the
    /// wrong environment silently.
    #[test]
    fn test_promotion_forms_name_what_they_read() {
        assert!(parses(&["get", "states", "-e", "dev", "-o", "json"]));
        assert!(parses(&["get", "promotions", "-e", "prod", "--local"]));
        assert!(parses(&["get", "promotion-candidates", "dev-to-prod", "-o", "yaml"]));
        assert!(!parses(&["get", "states"]));
        assert!(!parses(&["get", "promotions"]));
        assert!(!parses(&["get", "promotion-candidates"]));
        assert!(!parses(&["get", "promotion-candidates", "dev-to-prod", "-e", "prod"]));
    }
}
