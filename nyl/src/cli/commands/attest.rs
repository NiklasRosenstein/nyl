//! `nyl attest -e <env>`: the driver form asks the drivers (see [`super::check`]);
//! `nyl attest -e <env> [--unit <u>] --name <n> --result pass|fail` is the
//! external form, which records a result the caller already knows.
//!
//! Contract: [Attestations](../../../../design/orchestration-core.md#attestations)
//! and the [promotion contract](../../../../design/promotion.md#attestations).
//! The external form attests the unit's current receipt with `--unit`, and
//! the environment's current state without it. It refuses a name the scope
//! does not declare with `from: external` (exit 1), runs under the lease
//! (`--wait-lease`; held is exit 2), records the attester through the
//! approval identity sources, and exits 0 once the result is recorded,
//! whether it passes or fails. `--state-revision` and `--revision` do not
//! exist (M11). The driver form keeps exit 3 for a failing attestation.

use std::collections::BTreeMap;

use clap::{Args, ValueEnum};
use nyl_core::orchestration::ids::UnitName;
use nyl_core::orchestration::state::{AttestationResult, Operation};
use nyl_orchestration::rules::checks::Check;
use nyl_orchestration::rules::external_attest::External;
use nyl_orchestration::shell::external_attest;

use super::check::{self, CheckArgs};
use super::reconcile::RunOutput;
use super::state::{requested, Common, Output};
use crate::orchestration::approval::attester;
use crate::orchestration::run::{CommandOutcome, Wiring};
use nyl_core::orchestration::codes::Finding;
use nyl_orchestration::report::StateOperationReport;
use nyl_render::{NylError, Result};

/// A result an external attestation records.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ResultArg {
    /// The check passed.
    Pass,
    /// The check failed.
    Fail,
}

/// Ask the drivers to attest an environment's current receipts, or record an
/// external attestation with `--name`.
#[derive(Args, Debug, Clone)]
pub struct AttestArgs {
    #[command(flatten)]
    pub check: CheckArgs,

    /// Record an external attestation of this name for the unit's current receipt (with `--unit`) or the environment's current state.
    #[arg(long, requires = "result", conflicts_with_all = ["labels", "group"])]
    pub name: Option<String>,

    /// The result to record.
    #[arg(long, value_enum, requires = "name")]
    pub result: Option<ResultArg>,

    /// Why, recorded with the attestation and as the commit's Nyl-Reason.
    #[arg(long, requires = "name")]
    pub reason: Option<String>,

    /// Where the result can be read, such as a CI job or QA ticket.
    #[arg(long, requires = "name")]
    pub url: Option<String>,

    /// Assert who attests (repeatable, or comma-separated); recorded as `asserted`.
    #[arg(long, value_delimiter = ',', requires = "name")]
    pub attested_by: Vec<String>,

    /// Assert where the attestation was given, such as a CI run URL.
    #[arg(long, requires = "name")]
    pub attestation_source: Option<String>,
}

/// Run `nyl attest` in-process with `wiring`.
pub fn run(args: &AttestArgs, wiring: &Wiring<'_>) -> Result<CommandOutcome> {
    let (Some(name), Some(result)) = (&args.name, args.result) else {
        return check::run(&args.check, Check::Attest, wiring);
    };
    let [environment] = args.check.environment.as_slice() else {
        return Err(NylError::config(
            "`nyl attest --name` records for one environment: pass a single -e",
        ));
    };
    let unit = match args.check.units.as_slice() {
        [] => None,
        [unit] => Some(
            UnitName::new(unit)
                .map_err(|error| NylError::config(format!("--unit {unit:?} is not a unit name: {error}")))?,
        ),
        _ => {
            return Err(NylError::config(
                "`nyl attest --name` records for one unit: pass a single --unit",
            ))
        }
    };
    let common = Common {
        environment: environment.clone(),
        reason: args.reason.clone(),
        wait_lease: args.check.wait_lease,
        author: args.check.author.clone(),
        output: match args.check.output {
            RunOutput::Table => Output::Text,
            RunOutput::Json => Output::Json,
        },
        path: args.check.path.clone(),
    };
    let requested = requested(&common, wiring);
    let result = match result {
        ResultArg::Pass => AttestationResult::Pass,
        ResultArg::Fail => AttestationResult::Fail,
    };
    super::state::run(&common, wiring, Operation::Attest.as_str(), None, false, |ctx| {
        let refuse = |finding: Finding| {
            StateOperationReport::new(ctx.environment.clone(), Operation::Attest.as_str()).refused(finding)
        };
        let attester = match attester(
            ctx.session,
            wiring,
            &ctx.context.requested_by,
            (&args.attested_by, args.attestation_source.as_deref()),
            "--attested-by",
            &BTreeMap::new(),
        ) {
            Ok(attester) => attester,
            Err(finding) => return refuse(finding),
        };
        let request = External {
            name: name.clone(),
            result,
            reason: args.reason.clone(),
            url: args.url.clone(),
            attester,
        };
        external_attest::record(
            ctx.context,
            &ctx.environment,
            ctx.location,
            unit.as_ref(),
            &request,
            &requested,
        )
    })
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;

    use crate::cli::Cli;

    fn parses(args: &[&str]) -> bool {
        Cli::try_parse_from(std::iter::once("nyl").chain(args.iter().copied())).is_ok()
    }

    /// Catches the external form accepted without its pair of options, with
    /// an environment selector it cannot write to, or with the flags D9
    /// leaves to M11 (an earlier state), and the driver form losing its own.
    #[test]
    fn test_external_form_takes_name_with_result_and_no_revision() {
        assert!(parses(&["attest", "-e", "dev"]));
        assert!(parses(&[
            "attest",
            "-e",
            "dev",
            "--unit",
            "api",
            "--name",
            "qa",
            "--result",
            "fail",
            "--url",
            "https://x"
        ]));
        assert!(!parses(&["attest", "-e", "dev", "--name", "qa"]));
        assert!(!parses(&["attest", "-e", "dev", "--result", "pass"]));
        assert!(!parses(&["attest", "-e", "dev", "--reason", "x"]));
        assert!(!parses(&["attest", "-l", "a=b", "--name", "qa", "--result", "pass"]));
        assert!(!parses(&[
            "attest",
            "-e",
            "dev",
            "--name",
            "qa",
            "--result",
            "pass",
            "--state-revision",
            "abc"
        ]));
        assert!(!parses(&[
            "attest",
            "-e",
            "dev",
            "--name",
            "qa",
            "--result",
            "pass",
            "--revision",
            "abc"
        ]));
    }
}
