//! `nyl promote <path>`: record what one environment proved as a
//! PromotionRecord in another environment's desired state, or with `record:
//! source` in that Environment's source.
//!
//! Contract: [Promotion paths](../../../../design/promotion.md#promotion-paths)
//! and [Exit categories](../../../../design/promotion.md#exit-categories). It
//! runs under the target environment's lease (`--wait-lease` waits for a
//! running reconcile; a held lease is exit 2), reads the source's recorded
//! state and writes nothing to it, and writes one desired commit holding the
//! record and a summary; with `record: source` it writes no state and
//! commits the record to the branch the entry worktree has checked out
//! instead ([`crate::orchestration::record_source`]). `--dry-run` takes no
//! lease, writes nothing, and reports the record it would write. The decisions are
//! `nyl_orchestration::rules::promotion`'s and the effects the shell's; this
//! module resolves the invocation, the project's declaration, and who asks.

use std::collections::BTreeMap;
use std::path::PathBuf;

use clap::{Args, ValueEnum};
use nyl_core::orchestration::codes::{Code, Finding};
use nyl_core::orchestration::ids::CommitId;
use nyl_core::orchestration::ports::{SourceRepository, SourceResolver as _};
use nyl_core::orchestration::promotion::PromotionRequest;
use nyl_core::orchestration::time::Duration;
use nyl_core::resources::promotion::PromotionPath;
use nyl_orchestration::report::PromoteReport;
use nyl_orchestration::rules::promotion::supersede::{RepositoryNames, SupersedeRequest};
use nyl_orchestration::rules::promotion::{IgnoreRequest, Selection};
use nyl_orchestration::shell::promote::{self, PromoteOptions};
use nyl_orchestration::shell::run::{LifecycleOptions, RunMode};
use nyl_render::gitops::discover_gitops_inventory;
use nyl_render::Result;
use nyl_state::git::CommitAuthor;

use super::state::Output;
use crate::orchestration::approval::attester;
use crate::orchestration::promotion::{target_view, TargetView};
use crate::orchestration::run::{CommandOutcome, Wiring};
use crate::orchestration::session::{placeholder_environment, pretty_json, write_finding, RunOptions, Session, Stop};

/// The evidence level `--evidence` can lower a promotion to.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum EvidenceArg {
    /// Accept a state that only has receipts; the record names the override.
    Published,
}

/// Promote the state of the path's source environment into its target.
#[derive(Args, Debug, Clone)]
pub struct PromoteArgs {
    /// The PromotionPath to promote along.
    pub name: String,

    /// Promote only this value of the path (repeatable); the others stay at their recorded lineage.
    /// Only for a path that promotes values; a path that promotes the target's source moves everything together.
    #[arg(long = "value")]
    pub values: Vec<String>,

    /// Show the chosen state and the record it would write; take no lease and write nothing.
    #[arg(long)]
    pub dry_run: bool,

    /// Why, recorded in the record's `requested.reason` and as the commit's Nyl-Reason; names the cause of a rollback or an override.
    #[arg(long)]
    pub reason: Option<String>,

    /// Promote the newest qualifying state of the source that ran this source commit: a full commit ID (used as given, not fetched), or a branch or tag name of this repository; a short SHA is rejected.
    #[arg(long, conflicts_with = "state_revision")]
    pub revision: Option<String>,

    /// Promote exactly this recorded state of the source, by its desired state commit.
    #[arg(long)]
    pub state_revision: Option<String>,

    /// Accept a state below the path's evidence level, such as to roll back; recorded as an override.
    #[arg(long, value_enum)]
    pub evidence: Option<EvidenceArg>,

    /// Set a failing attestation aside, with the reason the record keeps: `<unit>/<name>=<reason>` or `environment/<name>=<reason>` (repeatable).
    #[arg(long = "ignore-attestation")]
    pub ignore_attestation: Vec<String>,

    /// Accept that the promotion replaces missing commits of another path's record (repeatable): `[<repository>:]<commit or range>[=<reason>]`, such as `--supersede a1b2c3^..c3d4e5=<reason>`.
    /// An entry without its own reason takes `--reason`; one with neither is refused. Commits it names that are not missing are ignored.
    #[arg(long = "supersede")]
    pub supersede: Vec<String>,

    /// Record the promotion without checking that the promoted artifacts still exist, such as from a runner without registry access; the record then says `artifactsVerified: false`.
    #[arg(long)]
    pub no_verify_artifacts: bool,

    /// Wait up to this long for another run's lease on the target, such as 10m.
    #[arg(long)]
    pub wait_lease: Option<Duration>,

    /// Author of the state commit, or of the source commit with `record: source`, as "Name <email>"; defaults to Git's user.name and user.email.
    #[arg(long)]
    pub author: Option<String>,

    /// Output format.
    #[arg(short = 'o', long = "output", value_enum, default_value_t)]
    pub output: Output,

    /// Project directory or a path beneath it.
    #[arg(default_value = ".")]
    pub path: PathBuf,
}

/// Run `nyl promote` in-process with `wiring`.
pub fn run(args: &PromoteArgs, wiring: &Wiring<'_>) -> Result<CommandOutcome> {
    let placeholder = || placeholder_environment("unknown");
    let refused = |finding: Finding| {
        render(
            &PromoteReport::new(&args.name, placeholder(), placeholder(), args.dry_run).refused(finding),
            args.output,
        )
    };
    let inventory = discover_gitops_inventory(&args.path, None, crate::resource_catalogue())?;
    let (path, source, target) = match crate::orchestration::promotion::promotable_path(&inventory, &args.name) {
        Ok(found) => found,
        Err(finding) => return Ok(refused(finding)),
    };
    let session = match Session::open(&args.path, &target) {
        Ok(session) => session,
        Err(Stop::Finding(finding)) => return Ok(refused(finding)),
        Err(Stop::Error(error)) => return Err(error),
    };
    let report_for = |finding: Finding| {
        render(
            &PromoteReport::new(
                &args.name,
                placeholder_environment(&source),
                placeholder_environment(&target),
                args.dry_run,
            )
            .refused(finding),
            args.output,
        )
    };
    // A broken path, binding, or promoted source is refused as `plan` and `reconcile` refuse it.
    let coverage_warnings = match session.check_promotion_paths(wiring) {
        Ok(warnings) => crate::orchestration::session::coverage_warnings(&warnings),
        Err(finding) => return Ok(report_for(finding)),
    };
    match prepare(args, wiring, &session, &path) {
        Ok((options, store_identity)) => {
            // With `record: source` the record is a commit in the project's
            // source; a dry run only checks that it could be written.
            let writer = if options.records_in_source() {
                match crate::orchestration::record_source::writer_for(
                    &session,
                    store_identity.clone(),
                    wiring.clock.clone(),
                ) {
                    Ok(writer) => Some(writer),
                    Err(error) => {
                        return Ok(report_for(promote::writer_finding(
                            &error,
                            &session.resolved.name,
                            &path
                                .name()
                                .map_err(|error| nyl_render::NylError::config(error.to_string()))?,
                        )))
                    }
                }
            } else {
                None
            };
            let author = store_identity.map(|identity| CommitAuthor {
                identity,
                clock: wiring.clock.clone(),
            });
            let store = session.store(author.clone())?;
            let request = match session.request(&RunOptions {
                units: &[],
                concurrency: 1,
                source: None,
                wait: args.wait_lease,
                local: None,
                cancellation: wiring.cancellation.clone(),
                mode: RunMode::Reconcile,
                lifecycle: LifecycleOptions::default(),
                reapply: false,
                detect_drift: false,
                fail_fast: false,
                reason: args.reason.as_deref(),
            }) {
                Ok(request) => request,
                Err(finding) => return Ok(report_for(finding)),
            };
            let identity = author.as_ref().map(|author| &author.identity);
            let mut report = match session.run_with(wiring, &store, identity, &request, |context, request| {
                promote::promote(
                    context,
                    request,
                    &options,
                    writer
                        .as_ref()
                        .map(|writer| writer as &dyn nyl_core::orchestration::ports::PromotionRecordWriter),
                )
            }) {
                Ok(report) => report,
                Err(Stop::Finding(finding)) => return Ok(report_for(finding)),
                Err(Stop::Error(error)) => return Err(error),
            };
            let mut warnings = session.resolved.warnings.clone();
            warnings.extend(coverage_warnings);
            warnings.append(&mut report.warnings);
            report.warnings = warnings;
            Ok(render(&report, args.output))
        }
        Err(finding) => Ok(report_for(finding)),
    }
}

/// The invocation resolved against the declaration: what to promote and who
/// asks, and the identity that writes the commit (none for a dry run).
fn prepare(
    args: &PromoteArgs,
    wiring: &Wiring<'_>,
    session: &Session,
    path: &PromotionPath,
) -> std::result::Result<(PromoteOptions, Option<nyl_core::orchestration::ports::CommitIdentity>), Finding> {
    let invalid = |message: String, fix: &str| Finding::new(Code::PromoteSelectorInvalid, message, fix);
    let select = match (&args.revision, &args.state_revision) {
        (Some(revision), _) => Selection::SourceRevision(source_commit(session, revision)?),
        (_, Some(commit)) => {
            Selection::StateRevision(CommitId::new(commit.trim().to_ascii_lowercase()).map_err(|error| {
                invalid(
                    format!("--state-revision {commit:?} is not a full commit ID: {error}"),
                    "pass the desired state commit as `nyl get states` prints it",
                )
            })?)
        }
        _ => Selection::Newest,
    };
    let ignore = args
        .ignore_attestation
        .iter()
        .map(|text| {
            IgnoreRequest::parse(text)
                .map_err(|message| invalid(message, "write <unit>/<name>=<reason>, or environment/<name>=<reason>"))
        })
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let supersede = args
        .supersede
        .iter()
        .map(|text| {
            SupersedeRequest::parse(text).map_err(|message| {
                invalid(
                    message,
                    "write [<repository>:]<commit or range>[=<reason>], as the refusal prints the range",
                )
            })
        })
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let identity = if args.dry_run {
        None
    } else {
        Some(wiring.commit_identity(&session.resolved.git_dir, args.author.as_deref())?)
    };
    let requested_by = identity.as_ref().map_or_else(
        || crate::orchestration::project::requested_by(&session.resolved.git_dir, &wiring.runner),
        ToString::to_string,
    );
    let attester = attester(
        session,
        wiring,
        &requested_by,
        (&[], None),
        "--attested-by",
        &BTreeMap::new(),
        None,
    )?;
    let requested = PromotionRequest {
        by: if attester.by.is_empty() {
            attester.source.clone().unwrap_or_else(|| requested_by.clone())
        } else {
            attester.by.join(", ")
        },
        identity: attester.identity,
        source: attester.source,
        at: wiring.clock.now(),
        reason: args.reason.clone(),
    };
    let TargetView {
        target_units,
        promotes_source,
        promoted,
        rebuilt,
    } = target_view(session, wiring.drivers, path)?;
    Ok((
        PromoteOptions {
            path: path.clone(),
            select,
            evidence_published: args.evidence.is_some(),
            ignore,
            values: args.values.iter().cloned().collect(),
            verify_artifacts: path.spec.verify_artifacts && !args.no_verify_artifacts,
            dry_run: args.dry_run,
            target_units,
            rebuilt,
            promotes_source,
            promoted,
            requested,
            supersede,
            repository_names: repository_names(session),
        },
        identity,
    ))
}

/// The project's names for repositories ([`RepositoryNames`]): its
/// GitRepositories and the entry clone's `origin`.
fn repository_names(session: &Session) -> RepositoryNames {
    let declared: Vec<(String, String)> = session
        .inventory
        .resources
        .values()
        .filter_map(|resource| match &resource.resource {
            Some(nyl_render::resources::GitOpsResource::GitRepository(repository)) => {
                Some((repository.metadata.name.clone(), repository.spec.repo_url.clone()))
            }
            _ => None,
        })
        .collect();
    let origin = session
        .sources()
        .ok()
        .and_then(|sources| sources.repository_url(&SourceRepository::This));
    RepositoryNames::new(&declared, origin.as_deref())
}

/// `--revision` as a source commit: a full commit ID as is, anything else
/// resolved in this repository like a revision of a recorded source.
fn source_commit(session: &Session, revision: &str) -> std::result::Result<CommitId, Finding> {
    let revision = revision.trim();
    if let Ok(commit) = CommitId::new(revision.to_ascii_lowercase()) {
        return Ok(commit);
    }
    let unresolved = |message: String| {
        Finding::new(
            Code::SourceUnavailable,
            format!("--revision {revision:?} does not name a commit: {message}"),
            "pass a full commit ID, or a revision this clone can resolve",
        )
    };
    let sources = session.sources().map_err(|error| unresolved(error.to_string()))?;
    sources
        .resolve_revision(&nyl_core::orchestration::ports::SourceRepository::This, revision)
        .map_err(|error| unresolved(error.to_string()))
}

/// Render a promote report as the command's outcome.
pub(crate) fn render(report: &PromoteReport, output: Output) -> CommandOutcome {
    let json = serde_json::to_value(report).expect("reports serialize");
    let stdout = match output {
        Output::Json => pretty_json(&json),
        Output::Text => report.table(),
    };
    let mut stderr = String::new();
    for finding in &report.warnings {
        write_finding(&mut stderr, "warning", None, finding);
    }
    for finding in &report.errors {
        write_finding(&mut stderr, "error", None, finding);
    }
    CommandOutcome {
        exit: report.exit,
        report: json,
        stdout,
        stderr,
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;

    use crate::cli::Cli;

    fn parses(args: &[&str]) -> bool {
        Cli::try_parse_from(std::iter::once("nyl").chain(args.iter().copied())).is_ok()
    }

    /// Catches `promote` accepted without its path, with both ways to name a
    /// state at once, or with an evidence level the contract does not offer.
    #[test]
    fn test_promote_takes_a_path_and_one_way_to_name_a_state() {
        assert!(parses(&["promote", "dev-to-prod"]));
        assert!(parses(&[
            "promote",
            "dev-to-prod",
            "--dry-run",
            "--value",
            "webImage",
            "--reason",
            "r",
            "--evidence",
            "published",
            "--ignore-attestation",
            "environment/qa=flaky",
            "--supersede",
            "a1b2c3^..c3d4e5=merged differently",
            "--supersede",
            "web:a1b2c3",
            "--no-verify-artifacts",
            "--wait-lease",
            "5m",
        ]));
        assert!(!parses(&["promote"]));
        assert!(!parses(&["promote", "p", "--revision", "a", "--state-revision", "b"]));
        assert!(!parses(&["promote", "p", "--evidence", "attested"]));
    }
}
