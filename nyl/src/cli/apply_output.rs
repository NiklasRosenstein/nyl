//! Typed apply events and report, and their one terminal rendering.
//!
//! `apply` and `release rollback` emit [`ApplyEvent`]s while they run, so progress is
//! visible as each resource completes, and finish with an [`ApplyReport`]. Every line
//! either command prints about resources, pruning, the summary, and phase timings is
//! rendered here by [`ApplyRenderer`].

use std::collections::HashMap;
use std::time::Duration;

use colored::Colorize;

use nyl_render::kubernetes::{
    lifecycle::{ApplyEvent, ApplyExecutionResult, ReleaseRun},
    prune::PruneDecision,
    ApplyOutcome, DiscoveryMode, ReleaseState, ReleaseStatus, ResourceKey,
};

/// A phase of an apply whose duration is reported.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Phase {
    Discovery(DiscoveryMode),
    Validation,
    Apply,
    Release,
}

impl std::fmt::Display for Phase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Discovery(mode) => write!(f, "discovery ({mode})"),
            Self::Validation => f.write_str("validation"),
            Self::Apply => f.write_str("apply"),
            Self::Release => f.write_str("release"),
        }
    }
}

/// Wall-clock duration of one apply phase.
pub(crate) struct PhaseTiming {
    pub(crate) phase: Phase,
    pub(crate) elapsed: Duration,
}

impl PhaseTiming {
    pub(crate) fn new(phase: Phase, elapsed: Duration) -> Self {
        Self { phase, elapsed }
    }
}

/// The final result of an apply, rendered as the summary.
pub(crate) struct ApplyReport<'a> {
    /// Successful outcomes in manifest order.
    pub(crate) outcomes: &'a [ApplyOutcome],
    pub(crate) failed_count: usize,
    /// What a handoff released, when the apply handed off field ownership.
    pub(crate) ownership: Option<OwnershipReport<'a>>,
    /// Resources that failed to prune; they stay recorded for the next apply to retry.
    pub(crate) prune_failed_count: usize,
    /// The recorded revision, unless the apply ran without release tracking.
    pub(crate) release: Option<&'a ReleaseState>,
    /// The apply changed nothing, so it recorded no revision and `release` is the latest one.
    pub(crate) release_unchanged: bool,
    /// Phase durations, in the order the phases ran.
    pub(crate) timings: &'a [PhaseTiming],
    /// What the hooks did.
    pub(crate) hooks: HookSummary,
}

/// How many hook runs completed and failed, and how many workloads never became healthy.
#[derive(Clone, Copy, Default)]
pub(crate) struct HookSummary {
    pub(crate) run: usize,
    pub(crate) failed: usize,
    pub(crate) unhealthy: usize,
}

impl HookSummary {
    pub(crate) fn of(apply: &ApplyExecutionResult) -> Self {
        Self {
            run: apply.hooks.len(),
            failed: apply.hook_failed_count,
            unhealthy: apply.unhealthy.len(),
        }
    }
}

/// The field ownership a handoff released.
pub(crate) struct OwnershipReport<'a> {
    /// The manager whose fields were released.
    pub(crate) released_manager: &'a str,
    /// Resources whose fields the manager owned and released.
    pub(crate) released_count: usize,
    /// Resources whose fields could not be released.
    pub(crate) release_failed_count: usize,
}

/// Renders apply events and the final report to the terminal.
pub(crate) struct ApplyRenderer<'a> {
    duplicates: &'a HashMap<ResourceKey, usize>,
}

impl<'a> ApplyRenderer<'a> {
    /// `duplicates` maps each resource rendered more than once to its occurrence count.
    pub(crate) fn new(duplicates: &'a HashMap<ResourceKey, usize>) -> Self {
        Self { duplicates }
    }

    /// Print the line for one event.
    pub(crate) fn event(&self, event: ApplyEvent<'_>) {
        match event {
            ApplyEvent::Applied {
                result: Ok(outcome), ..
            } => println!("{}", self.outcome_line(outcome)),
            ApplyEvent::Applied { key, result: Err(e) } => {
                let error_msg = format!("(failed to apply resource: {})", e);
                println!("{} {} {}", "✗".red().bold(), key, error_msg.red());
            }
            ApplyEvent::OwnershipReleased {
                key,
                manager,
                result: Ok(()),
            } => println!(
                "  {} released the fields {} applied to {}",
                "↳".bright_black(),
                manager,
                key
            ),
            ApplyEvent::OwnershipReleased {
                key,
                manager,
                result: Err(e),
            } => {
                let error_msg = format!("(applied, but failed to release the fields {} applied: {})", manager, e);
                println!("{} {} {}", "✗".red().bold(), key, error_msg.red());
            }
            ApplyEvent::PruneStarted { count } => println!("\nPruning {} resources...", count),
            ApplyEvent::Pruned { key, result: Ok(()) } => println!("  ✓ Deleted {}", key),
            ApplyEvent::Pruned { key, result: Err(e) } => println!("  ✗ Failed to delete {}: {}", key, e),
            ApplyEvent::PruneWaiting { count } => {
                println!("  … waiting for {} deleted resource(s) to terminate", count);
            }
            ApplyEvent::PruneWaitTimedOut { keys } => {
                for key in keys {
                    println!("  {} {} is still terminating; continuing", "!".yellow().bold(), key);
                }
            }
            ApplyEvent::PruneFinished => println!(),
            ApplyEvent::DeleteHooksIgnored { finding } => tracing::warn!("{finding}"),
            ApplyEvent::HookStarted { key, phase, wave } => {
                println!("{} {} hook {}{}", "▶".cyan().bold(), phase, key, wave_note(wave));
            }
            ApplyEvent::HookFinished {
                key,
                phase,
                result: Ok(()),
                ..
            } => println!("{} {} hook {} completed", "✓".green().bold(), phase, key),
            ApplyEvent::HookFinished {
                key,
                phase,
                result: Err(why),
                ..
            } => {
                let error_msg = format!("({why})");
                println!("{} {} hook {} failed {}", "✗".red().bold(), phase, key, error_msg.red());
            }
            ApplyEvent::HookDeleted {
                key,
                policy,
                result: Ok(()),
            } => println!("  {} deleted hook {} ({})", "↳".bright_black(), key, policy),
            ApplyEvent::HookDeleted {
                key,
                policy,
                result: Err(e),
            } => println!(
                "  {} Failed to delete hook {} ({}): {}",
                "✗".red().bold(),
                key,
                policy,
                e
            ),
            ApplyEvent::PruneKept {
                key,
                decision: PruneDecision::Retain,
                option,
            } => println!(
                "{} Kept {}: its sync option {option}=false retains it",
                "!".yellow().bold(),
                key
            ),
            ApplyEvent::PruneKept { key, option, .. } => println!(
                "{} Kept {}: its sync option {option}=confirm needs --confirm-{} to delete it",
                "!".yellow().bold(),
                key,
                option.to_lowercase()
            ),
        }
    }

    /// Print the summary counts, the release result, and the phase timings.
    pub(crate) fn summary(&self, report: &ApplyReport<'_>) {
        if !report.outcomes.is_empty() || report.failed_count > 0 {
            println!();
        }

        let mut created = 0;
        let mut updated = 0;
        let mut unchanged = 0;
        for outcome in report.outcomes {
            match effective_outcome(outcome) {
                ApplyOutcome::Created { .. } => created += 1,
                ApplyOutcome::Updated { .. } => updated += 1,
                ApplyOutcome::Unchanged { .. } => unchanged += 1,
                ApplyOutcome::DryRun { .. } => {}
            }
        }

        let total_duplicates_ignored: usize = self.duplicates.values().map(|count| count - 1).sum();

        let mut parts = vec![
            format!("{} created", created.to_string().green()),
            format!("{} updated", updated.to_string().yellow()),
            format!("{} unchanged", unchanged),
        ];
        if total_duplicates_ignored > 0 {
            let plural = if total_duplicates_ignored == 1 {
                "duplicate"
            } else {
                "duplicates"
            };
            parts.push(format!(
                "{} {} ignored",
                total_duplicates_ignored.to_string().bright_black(),
                plural
            ));
        }
        if report.failed_count > 0 {
            parts.push(format!("{} failed", report.failed_count.to_string().red()));
        }
        if report.hooks.run > 0 {
            parts.push(format!("{} hook run(s)", report.hooks.run.to_string().cyan()));
        }
        if report.hooks.failed > 0 {
            parts.push(format!("{} hook(s) failed", report.hooks.failed.to_string().red()));
        }
        if report.hooks.unhealthy > 0 {
            parts.push(format!("{} not healthy", report.hooks.unhealthy.to_string().red()));
        }
        if let Some(ownership) = &report.ownership {
            parts.push(format!(
                "{} released from {}",
                ownership.released_count.to_string().cyan(),
                ownership.released_manager
            ));
            if ownership.release_failed_count > 0 {
                parts.push(format!(
                    "{} failed to release",
                    ownership.release_failed_count.to_string().red()
                ));
            }
        }
        if report.prune_failed_count > 0 {
            parts.push(format!(
                "{} failed to prune",
                report.prune_failed_count.to_string().red()
            ));
        }
        println!("Summary: {}", parts.join(", "));

        if !report.timings.is_empty() {
            let timings: Vec<String> = report
                .timings
                .iter()
                .map(|timing| format!("{} {:.2}s", timing.phase, timing.elapsed.as_secs_f64()))
                .collect();
            println!("{}", format!("Timings: {}", timings.join(", ")).bright_black());
        }

        if let Some(release) = report.release {
            println!();
            if release.status == ReleaseStatus::Deployed && report.release_unchanged {
                println!(
                    "Release: {} revision {} unchanged in namespace {}; no new revision recorded",
                    release.release_name, release.revision, release.release_namespace
                );
            } else if release.status == ReleaseStatus::Deployed && report.prune_failed_count > 0 {
                println!(
                    "Release: {} revision {} deployed to namespace {}; {} resource(s) failed to prune and will be retried by the next apply",
                    release.release_name, release.revision, release.release_namespace, report.prune_failed_count
                );
            } else if release.status == ReleaseStatus::Deployed {
                println!(
                    "Release: {} revision {} deployed successfully to namespace {}",
                    release.release_name, release.revision, release.release_namespace
                );
            } else {
                println!("Release: {} revision {} failed", release.release_name, release.revision);
            }
        }
    }

    fn outcome_line(&self, outcome: &ApplyOutcome) -> String {
        let (marker, resource_key) = match effective_outcome(outcome) {
            ApplyOutcome::Created { resource_key } => ("+".green().bold(), resource_key),
            ApplyOutcome::Updated { resource_key } => ("~".yellow().bold(), resource_key),
            ApplyOutcome::Unchanged { resource_key } => ("=".bright_black().bold(), resource_key),
            ApplyOutcome::DryRun { .. } => unreachable!("effective_outcome unwraps dry runs"),
        };
        format!("{} {}{}", marker, resource_key, self.duplicate_annotation(resource_key))
    }

    fn duplicate_annotation(&self, resource_key: &ResourceKey) -> String {
        // Look up the full ResourceKey (including apiVersion/kind) so resources of the
        // same kind from different API versions are not matched by mistake.
        if let Some(count) = self.duplicates.get(resource_key) {
            let ignored_count = count - 1;
            let plural = if ignored_count == 1 { "duplicate" } else { "duplicates" };
            return format!(" {}", format!("({} {} ignored)", ignored_count, plural).yellow());
        }
        String::new()
    }
}

/// The sync wave of a hook, when it is not the default one.
fn wave_note(wave: i64) -> String {
    if wave == 0 {
        String::new()
    } else {
        format!(" (wave {wave})")
    }
}

/// The outcome a dry run stands for, or the outcome itself.
fn effective_outcome(outcome: &ApplyOutcome) -> &ApplyOutcome {
    match outcome {
        ApplyOutcome::DryRun { would_be } => effective_outcome(would_be),
        other => other,
    }
}

/// What a handoff released, for the summary.
pub(crate) fn ownership_report(apply: &ApplyExecutionResult) -> Option<OwnershipReport<'_>> {
    apply.released_manager.as_deref().map(|manager| OwnershipReport {
        released_manager: manager,
        released_count: apply.released_count,
        release_failed_count: apply.release_failed_count,
    })
}

/// The summary of one applied and recorded release.
pub(crate) fn report_of<'a>(run: &'a ReleaseRun, timings: &'a [PhaseTiming]) -> ApplyReport<'a> {
    ApplyReport {
        outcomes: &run.apply.outcomes,
        failed_count: run.apply.failed_count,
        ownership: ownership_report(&run.apply),
        prune_failed_count: run.prune_failed_count,
        release: Some(&run.release),
        release_unchanged: run.unchanged,
        timings,
        hooks: HookSummary::of(&run.apply),
    }
}
