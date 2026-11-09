//! `nyl reconcile -e <env>`: execute an environment's ready units in waves
//! and record the results in one transition commit per state ref.
//!
//! Contract: [Execution](../../../../design/orchestration-core.md#execution).
//! Exit categories: 0 all selected units current; 2 blocked, held, awaiting
//! approval, or the lease is held; 3 an execution failed; 4 uncertain, the
//! lease was lost, or the run was cancelled by SIGINT or SIGTERM; 1 error; the most severe wins (4 > 3 > 1 > 2 > 0).

use std::path::PathBuf;

use clap::Args;
use nyl_core::orchestration::time::Duration;

use nyl_core::orchestration::codes::{Code, Finding};
use nyl_core::orchestration::ports::StateStore;
use nyl_orchestration::rules::expiry::{self, Disposition, Intent};
use nyl_orchestration::run_report::{InstanceAction, ReportKind, RunReport};
use nyl_orchestration::shell::state::{self as shell_state, Requested};
use nyl_state::git::CommitAuthor;

use crate::cli::commands::state::{create_or_update_instance, InstanceWrite};
use crate::orchestration::instances::{remove_expired, Removal};
use crate::orchestration::project::{parse_params, parse_source_ref, InstanceRequest};
use crate::orchestration::run::{CommandOutcome, Wiring};
use crate::orchestration::session::{outcome, stopped, RunOptions, Session, Stop};
use nyl_render::Result;

/// Output format of orchestration commands.
#[derive(Debug, Clone, Copy, Default, clap::ValueEnum)]
pub enum RunOutput {
    /// A table.
    #[default]
    Table,
    /// The typed report.
    Json,
}

/// Execute an environment's ready units.
#[derive(Args, Debug, Clone)]
#[allow(clippy::struct_excessive_bools)] // Independent command-line switches.
pub struct ReconcileArgs {
    /// The environment to reconcile.
    #[arg(short = 'e', long = "environment", required_unless_present_any = ["labels", "group", "template"], conflicts_with_all = ["labels", "group"])]
    pub environment: Vec<String>,

    /// Create or update this environment as an instance of an EnvironmentTemplate
    /// before reconciling it, from this checkout. Without -e, reconcile every
    /// instance of the template at its own recorded source commit, as the
    /// scheduled fleet job does: it never extends an expiry and removes
    /// expired instances.
    #[arg(long, conflicts_with_all = ["labels", "group"])]
    pub template: Option<String>,

    /// A parameter of the instance, `name=value` with the value parsed as YAML
    /// (repeatable); needs --template and -e.
    #[arg(long = "param")]
    pub params: Vec<String>,

    /// With --template and -e, record this full ref name of `origin`
    /// (`refs/pull/123/head`) next to the instance's source commit, for a host
    /// that will not serve a commit by its id: a later run that lacks the
    /// commit fetches the ref by name and looks for the recorded commit in it.
    /// Only a hint, checked by syntax; a run without it keeps the recorded one.
    #[arg(long, requires = "template")]
    pub source_ref: Option<String>,

    /// Keep an expired instance: extend its expiry and reconcile it instead
    /// of removing it.
    #[arg(long)]
    pub renew: bool,

    /// Reconcile an instance as maintenance, without extending its expiry; an
    /// expired instance is removed.
    #[arg(long)]
    pub no_extend: bool,

    /// With --template and -e, when the template is at maxInstances, remove
    /// expired instances first (oldest expiry first, only as many as needed),
    /// then create the instance.
    #[arg(long, requires = "template")]
    pub make_room: bool,

    /// Select environments whose literal labels match key=value pairs.
    #[arg(short = 'l', long, conflicts_with_all = ["environment", "group"])]
    pub labels: Option<String>,

    /// Select members of this EnvironmentGroup.
    #[arg(short = 'g', long, conflicts_with_all = ["environment", "labels"])]
    pub group: Option<String>,

    /// Execute only these units (repeatable, or comma-separated with --units);
    /// resolution still covers the whole environment.
    #[arg(long = "unit", alias = "units", value_delimiter = ',')]
    pub units: Vec<String>,

    #[arg(skip)]
    pub unit_filter: bool,

    /// Units a wave runs at once.
    #[arg(long, default_value_t = nyl_orchestration::shell::run::DEFAULT_CONCURRENCY)]
    pub concurrency: usize,

    /// Render units at this revision or commit instead of the Environment's source, for this run only.
    #[arg(long)]
    pub source: Option<String>,

    /// Reconcile an immutable snapshot of Git-visible working files, including uncommitted changes.
    #[arg(long, conflicts_with_all = ["source", "labels", "group"])]
    pub worktree: bool,

    /// Wait up to this long for another run's lease, such as 10m.
    #[arg(long)]
    pub wait_lease: Option<Duration>,

    /// Operate on the configured state repository without network access.
    #[arg(long)]
    pub local: bool,

    /// Manual execution approvals and their audit identity.
    #[command(flatten)]
    pub approval: crate::orchestration::approval::ApprovalArgs,

    /// Tear down units that left the ownership set (omission) or whose kind
    /// changed, instead of leaving them pending teardown; also authorizes the
    /// teardown of expired instances without the template's allowTeardown.
    #[arg(long)]
    pub allow_teardown: bool,

    /// Tear down units that retained tombstones still cite.
    #[arg(long)]
    pub allow_dependents: bool,

    /// Proceed with teardowns whose readiness requirements are unmet, or
    /// whose removal is not confirmed, recording what may remain.
    #[arg(long)]
    pub allow_incomplete: bool,

    /// Act only on units outside the ownership set: newly leaving, deleting,
    /// pending teardown, or tearing down.
    #[arg(long, conflicts_with = "units")]
    pub leaving: bool,

    /// Execute the --unit units again although their receipts are current,
    /// such as to repair drift; needs --unit and --reason.
    #[arg(long, requires_all = ["units", "reason"])]
    pub reapply: bool,

    /// Why, recorded as the transition commit's Nyl-Reason for --reapply.
    #[arg(long)]
    pub reason: Option<String>,

    /// Start no further unit after the first failure, uncertain outcome, or
    /// approval wait; what finished is still recorded.
    #[arg(long)]
    pub fail_fast: bool,

    /// Author of state commits, as "Name <email>"; defaults to Git's user.name and user.email.
    #[arg(long)]
    pub author: Option<String>,

    /// Output format.
    #[arg(short = 'o', long = "output", value_enum, default_value_t)]
    pub output: RunOutput,

    /// Project directory or a path beneath it.
    #[arg(default_value = ".")]
    pub path: PathBuf,
}

/// What `--template` and `--param` say about the instance, or nothing.
fn instance_request(args: &ReconcileArgs) -> std::result::Result<InstanceRequest, Stop> {
    Ok(InstanceRequest {
        template: args.template.clone(),
        params: parse_params(&args.params)?,
        source_ref: parse_source_ref(args.source_ref.as_deref())?,
        offline: args.local,
    })
}

/// `--template` with `-e` creates or updates exactly one instance, and
/// without `-e` is the fleet reconcile; the options that need one instance
/// are rejected for the fleet.
fn template_selection_error(args: &ReconcileArgs) -> Option<CommandOutcome> {
    let refuse = |finding: Finding| {
        Some(stopped(
            ReportKind::ReconcileReport,
            args.environment.first().map_or("local", String::as_str),
            args.output,
            finding.into(),
        ))
    };
    let option_invalid = |message: String, fix: &str| refuse(Finding::new(Code::InstanceOptionInvalid, message, fix));
    // The expiry options say what a reconcile does with one named instance.
    if args.renew && args.no_extend {
        return option_invalid(
            "--renew extends the expiry and --no-extend leaves it, so they cannot be given together".to_owned(),
            "drop one of them",
        );
    }
    if args.no_extend && args.template.is_some() {
        return option_invalid(
            "--no-extend and --template contradict: the pull request job's `reconcile --template` is activity, which extends the expiry".to_owned(),
            "drop --no-extend, or drop --template to reconcile the instance as maintenance",
        );
    }
    if (args.renew || args.no_extend) && (args.labels.is_some() || args.group.is_some()) {
        return option_invalid(
            "--renew and --no-extend apply to instances named with -e, and a label or group selects declared environments".to_owned(),
            "name the instance with -e <name>",
        );
    }
    let template = args.template.as_ref()?;
    if args.environment.len() > 1 {
        return refuse(Finding::new(
            Code::InstanceTemplateRequired,
            "--template creates or updates exactly one instance",
            "name the instance with one -e <name>",
        ));
    }
    if args.environment.is_empty() {
        let invalid = |option: &str| {
            Finding::new(
                Code::InstanceOptionInvalid,
                format!("{option} applies to one instance, and `reconcile --template {template}` without -e reconciles every instance"),
                "name the instance with -e <name>",
            )
        };
        if !args.params.is_empty() {
            return refuse(invalid("--param"));
        }
        if args.source_ref.is_some() {
            return refuse(invalid("--source-ref"));
        }
        if args.make_room {
            return refuse(invalid("--make-room"));
        }
        if args.renew {
            return refuse(invalid("--renew"));
        }
        if args.worktree || args.source.is_some() {
            return refuse(Finding::new(
                Code::InstanceOptionInvalid,
                format!("--worktree and --source name one source, and every instance of {template} runs at its own recorded source commit"),
                "name the instance with -e <name>",
            ));
        }
    }
    None
}

/// Run the command in-process with `wiring`.
pub fn run(args: &ReconcileArgs, wiring: &Wiring<'_>) -> Result<CommandOutcome> {
    use crate::orchestration::multi_environment::{dispatch, select, Expiry};
    if let Some(outcome) = template_selection_error(args) {
        return Ok(outcome);
    }
    if args.template.is_some() && args.environment.is_empty() {
        return fleet(args, wiring);
    }
    let snapshot = if args.worktree {
        let capture = || -> std::result::Result<_, Stop> {
            if args.environment.len() != 1 {
                return Err(nyl_core::orchestration::codes::Finding::new(
                    nyl_core::orchestration::codes::Code::SourceUnavailable,
                    "--worktree requires exactly one explicitly named environment",
                    "use -e <environment> without a label or group selector",
                )
                .into());
            }
            Session::open_with(&args.path, &args.environment[0], &instance_request(args)?)?.snapshot_worktree(
                wiring,
                args.author.as_deref(),
                args.local,
            )
        };
        match capture() {
            Ok(snapshot) => Some(snapshot),
            Err(stop) => {
                return Ok(stopped(
                    ReportKind::ReconcileReport,
                    args.environment.first().map_or("local", String::as_str),
                    args.output,
                    stop,
                ))
            }
        }
    } else {
        None
    };
    let instance = match instance_request(args) {
        Ok(instance) => instance,
        Err(stop) => {
            return Ok(stopped(
                ReportKind::ReconcileReport,
                args.environment.first().map_or("local", String::as_str),
                args.output,
                stop,
            ))
        }
    };
    let preflight_source = snapshot
        .as_ref()
        .map(|source| source.commit.as_str())
        .or(args.source.as_deref());
    Ok(dispatch(
        ReportKind::ReconcileReport,
        args.output,
        select(
            &args.path,
            &args.environment,
            args.labels.as_deref(),
            args.group.as_deref(),
            true,
            preflight_source,
            args.local,
            wiring.drivers,
            &instance,
            Some(Expiry {
                intent: intent(args, false),
                now: wiring.clock.now(),
            }),
        ),
        |environment, selected, _planned, unavailable| {
            (
                reconcile_selected(
                    args,
                    environment,
                    selected,
                    unavailable,
                    snapshot.as_ref(),
                    wiring,
                    false,
                ),
                std::collections::BTreeSet::default(),
            )
        },
    ))
}

/// Reconcile `environment` of the selection `selected`: its units and
/// approvals qualified for it, and its report or the finding that stopped it.
fn reconcile_selected(
    args: &ReconcileArgs,
    environment: &str,
    selected: &[String],
    unavailable: &std::collections::BTreeSet<nyl_core::orchestration::ids::EnvironmentName>,
    snapshot: Option<&nyl_core::orchestration::ports::WorktreeSnapshot>,
    wiring: &Wiring<'_>,
    fleet: bool,
) -> CommandOutcome {
    let mut args = args.clone();
    args.environment = vec![environment.to_owned()];
    args.unit_filter = !args.units.is_empty();
    let mut prepare = || -> std::result::Result<_, Stop> {
        args.units = nyl_orchestration::rules::multi_environment::units(
            "--unit",
            &args.units,
            environment,
            selected,
            selected.len() > 1,
        )?;
        args.approval.approve = nyl_orchestration::rules::multi_environment::units(
            "--approve",
            &args.approval.approve,
            environment,
            selected,
            selected.len() > 1,
        )?;
        crate::orchestration::multi_environment::single_source(args.source.as_deref(), selected)?;
        reconcile(&args, wiring, unavailable, snapshot, fleet)
    };
    prepare().unwrap_or_else(|stop| stopped(ReportKind::ReconcileReport, environment, args.output, stop))
}

/// `nyl reconcile --template <name>` without `-e`: the scheduled fleet job.
/// It reconciles every live instance of the template at its own recorded
/// source commit and removes the expired ones, and never extends an expiry.
/// An instance whose lease another run holds is skipped and reported as
/// `busy`, not as a failure, so the exit category reflects only the
/// instances the run processed.
///
/// Contract: [Template changes](../../../../design/orchestration-core.md#template-changes)
/// and [Expiry](../../../../design/orchestration-core.md#expiry).
fn fleet(args: &ReconcileArgs, wiring: &Wiring<'_>) -> Result<CommandOutcome> {
    use crate::orchestration::multi_environment::{dispatch, select, Expiry};
    let template = args.template.clone().expect("the fleet names a template");
    let mut args = args.clone();
    let names = match fleet_instances(&args, &template) {
        Ok(names) => names,
        Err(stop) => return Ok(stopped(ReportKind::ReconcileReport, &template, args.output, stop)),
    };
    if names.is_empty() {
        let report = RunReport::new(ReportKind::ReconcileReport);
        return Ok(outcome(&report, args.output, 0));
    }
    // The instances are found by their records, never created or updated here.
    args.template = None;
    let instance = InstanceRequest {
        offline: args.local,
        ..InstanceRequest::default()
    };
    Ok(dispatch(
        ReportKind::ReconcileReport,
        args.output,
        select(
            &args.path,
            &names,
            None,
            None,
            true,
            args.source.as_deref(),
            args.local,
            wiring.drivers,
            &instance,
            Some(Expiry {
                intent: Intent::Maintenance,
                now: wiring.clock.now(),
            }),
        ),
        |environment, selected, _planned, unavailable| {
            (
                reconcile_selected(&args, environment, selected, unavailable, None, wiring, true),
                std::collections::BTreeSet::default(),
            )
        },
    ))
}

/// The live instances of `template`, found by listing its state location.
fn fleet_instances(args: &ReconcileArgs, template: &str) -> std::result::Result<Vec<String>, Stop> {
    use crate::orchestration::project::{list_instances, listing_location};
    let inventory = nyl_render::gitops::discover_gitops_inventory(&args.path, None, crate::resource_catalogue())?;
    let declaration = nyl_render::gitops::instances::find_template(&inventory, template).ok_or_else(|| {
        Finding::new(
            Code::InstanceTemplateUnknown,
            format!("the project declares no EnvironmentTemplate {template:?}"),
            "list the templates with `nyl get environment-templates`",
        )
    })?;
    if listing_location(&inventory, declaration)?.is_none() {
        return Err(Finding::new(
            Code::InstanceNotListable,
            format!(
                "the instances of EnvironmentTemplate {template} cannot be listed: its state location depends on the instance name"
            ),
            "end spec.state.path with `{{ environment.name }}` and keep the repository and refs independent of the name",
        )
        .into());
    }
    let listing = list_instances(&inventory, args.local);
    if listing.unavailable.iter().any(|unavailable| unavailable == template) {
        let reason = listing
            .warnings
            .first()
            .map_or_else(String::new, |finding| format!(": {}", finding.message));
        return Err(Finding::new(
            Code::StateUnavailable,
            format!("the instances of EnvironmentTemplate {template} could not be listed{reason}"),
            "check access to the template's state repository, or run without network access with --local",
        )
        .into());
    }
    Ok(listing
        .instances
        .into_iter()
        .filter(|listed| listed.descriptor.template == template)
        .map(|listed| listed.name.to_string())
        .collect())
}

/// What the invocation says about an instance's expiry: activity extends it
/// and removes an expired instance, maintenance (`--no-extend`, the fleet)
/// only removes, and `--renew` or `--template` keep an expired instance.
fn intent(args: &ReconcileArgs, fleet: bool) -> Intent {
    if args.template.is_some() || args.renew {
        Intent::Renew
    } else if args.no_extend || fleet {
        Intent::Maintenance
    } else {
        Intent::Activity
    }
}

/// How the removal of an expired instance is authorized by this invocation.
fn removal(args: &ReconcileArgs) -> Removal<'_> {
    Removal {
        path: &args.path,
        offline: args.local,
        allow_teardown: args.allow_teardown,
        allow_dependents: args.allow_dependents,
        allow_incomplete: args.allow_incomplete,
        fail_fast: args.fail_fast,
        approval: Some(&args.approval),
        reason: args.reason.as_deref(),
        wait: args.wait_lease,
        author: args.author.as_deref(),
        source: args.source.as_deref(),
    }
}

/// `--renew` and `--no-extend` apply to instances only.
fn expiry_option_error(args: &ReconcileArgs) -> Finding {
    let option = if args.renew { "--renew" } else { "--no-extend" };
    Finding::new(
        Code::InstanceOptionInvalid,
        format!(
            "{option} applies to instances of an EnvironmentTemplate, and {} is a declared Environment, which has no expiry",
            args.environment[0]
        ),
        format!("drop {option}"),
    )
}

#[allow(clippy::too_many_lines)] // One environment: expiry, the instance's record, the run, then its report.
fn reconcile(
    args: &ReconcileArgs,
    wiring: &Wiring<'_>,
    unavailable: &std::collections::BTreeSet<nyl_core::orchestration::ids::EnvironmentName>,
    snapshot: Option<&nyl_core::orchestration::ports::WorktreeSnapshot>,
    fleet: bool,
) -> std::result::Result<CommandOutcome, Stop> {
    let session = Session::open_with(&args.path, &args.environment[0], &instance_request(args)?)?;
    // Reconcile writes state, so it alone of the run commands needs an identity.
    let identity = wiring.commit_identity(&session.resolved.git_dir, args.author.as_deref())?;
    if session.resolved.instance.is_none() && (args.renew || args.no_extend) {
        return Err(expiry_option_error(args).into());
    }
    let removal = removal(args);
    let now = wiring.clock.now();
    let mut evicted = Vec::new();
    // The expiry activity recorded, when it extended it: read from the write's report.
    let mut extended: Option<Option<nyl_core::orchestration::time::Timestamp>> = None;
    if args.template.is_some() {
        // The pull request job creates or updates its instance, which is
        // activity: it extends the expiry and never removes the instance.
        let (written, gone) = create_or_update_instance(
            &session,
            wiring,
            &identity,
            &InstanceWrite {
                reason: args.reason.as_deref(),
                wait: args.wait_lease,
                local: args.local,
                // An instance the fleet removed is gone, and its pull request's next
                // job creates it again: a record that is still deleted under the lease
                // counts as missing here.
                restart: nyl_orchestration::shell::state::Restart::IfDeleted,
                make_room: args.make_room,
                removal: &removal,
            },
        )?;
        evicted = gone;
        extended = Some(written.expires_at);
    } else if let Some(instance) = &session.resolved.instance {
        let expires_at = instance.record.as_ref().and_then(|record| record.expires_at);
        match expiry::decide(intent(args, fleet), expires_at, instance.terms.ttl, now) {
            Disposition::Remove => {
                let mut report = RunReport::new(ReportKind::ReconcileReport);
                let mut environment = remove_expired(&session, wiring, &removal)?;
                if fleet {
                    environment.skip_when_busy();
                }
                report.environments.push(environment);
                let exit = report.exit().code();
                return Ok(outcome(&report, args.output, exit));
            }
            Disposition::Reconcile { extend: true } => {
                extended = Some(extend_expiry(&session, wiring, &identity, args, instance.terms.ttl)?);
            }
            Disposition::Reconcile { extend: false } => {}
        }
    }
    let mut request = session.request(&RunOptions {
        units: &args.units,
        concurrency: args.concurrency,
        source: args.source.as_deref(),
        wait: args.wait_lease,
        local: None,
        cancellation: wiring.cancellation.clone(),
        mode: nyl_orchestration::shell::run::RunMode::Reconcile,
        lifecycle: nyl_orchestration::shell::run::LifecycleOptions {
            allow_teardown: args.allow_teardown,
            allow_dependents: args.allow_dependents,
            allow_incomplete: args.allow_incomplete,
            leaving: args.leaving,
            ..nyl_orchestration::shell::run::LifecycleOptions::default()
        },
        reapply: args.reapply,
        fail_fast: args.fail_fast,
        reason: args.reason.as_deref(),
    })?;
    if args.unit_filter && args.units.is_empty() {
        request.units = Some(std::collections::BTreeSet::default());
    }
    request.unavailable.clone_from(unavailable);
    request.worktree_source = snapshot.cloned();
    request.approvals = args
        .approval
        .resolve(&session, wiring, &identity, request.units.as_ref())?;
    let author = Some(CommitAuthor {
        identity: identity.clone(),
        clock: wiring.clock.clone(),
    });
    let store = if args.local {
        session.offline_store(author)?
    } else {
        session.store(author)?
    };
    let state = &store as &dyn StateStore;
    let mut report = session.run(
        wiring,
        state,
        Some(&identity),
        &request,
        nyl_orchestration::shell::run::reconcile,
    )?;
    if let Some(environment) = report.environments.first_mut() {
        if let Some(instance) = &mut environment.instance {
            if let Some(recorded) = extended {
                instance.expires_at = recorded;
                instance.action = recorded.map(|_| InstanceAction::Extended);
                // Activity renewed it, even if it had expired.
                instance.expired = false;
            }
        }
        for name in &evicted {
            environment.warnings.push(Finding::new(
                Code::InstanceExpired,
                format!(
                    "removed the expired instance {name} to make room for {}",
                    session.resolved.name
                ),
                "the template was at maxInstances",
            ));
        }
        // A fleet run skips an instance another run holds: busy, not failed.
        if fleet {
            environment.skip_when_busy();
        }
    }
    let exit = report.exit().code();
    Ok(outcome(&report, args.output, exit))
}

/// Activity on an instance: move its expiry to now plus `ttl`, before the
/// run, whether or not the run succeeds.
fn extend_expiry(
    session: &Session,
    wiring: &Wiring<'_>,
    identity: &nyl_core::orchestration::ports::CommitIdentity,
    args: &ReconcileArgs,
    ttl: Option<Duration>,
) -> std::result::Result<Option<nyl_core::orchestration::time::Timestamp>, Stop> {
    let author = Some(CommitAuthor {
        identity: identity.clone(),
        clock: wiring.clock.clone(),
    });
    let store = if args.local {
        session.offline_store(author)?
    } else {
        session.store(author)?
    };
    let context = session.context(&store, wiring, Some(identity.to_string()));
    let requested = Requested {
        reason: args.reason.clone(),
        wait: args.wait_lease,
        cancellation: wiring.cancellation.clone(),
    };
    let report = shell_state::renew_instance(
        &context,
        &session.resolved.name,
        &session.resolved.location,
        ttl,
        &requested,
    );
    match report.errors.first() {
        Some(finding) => Err(Stop::Finding(finding.clone())),
        None => Ok(report.expires_at),
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;

    use crate::cli::Cli;

    fn parses(args: &[&str]) -> bool {
        Cli::try_parse_from(std::iter::once("nyl").chain(args.iter().copied())).is_ok()
    }

    /// Drift decisions: `--reapply` forces named units only, and records why.
    #[test]
    fn test_reconcile_reapply_needs_unit_and_reason() {
        assert!(!parses(&["reconcile", "-e", "dev", "--reapply"]));
        assert!(!parses(&["reconcile", "-e", "dev", "--reapply", "--unit", "api"]));
        assert!(parses(&[
            "reconcile",
            "-e",
            "dev",
            "--reapply",
            "--unit",
            "api",
            "--reason",
            "repair"
        ]));
    }
}
