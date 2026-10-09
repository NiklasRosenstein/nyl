//! Typed apply events and report, and their one terminal rendering.
//!
//! `apply` and `release rollback` emit [`ApplyEvent`]s while they run, so progress is
//! visible as each resource completes, and finish with an [`ApplyReport`]. Every line
//! either command prints about resources, pruning, the summary, and phase timings is
//! rendered here by [`ApplyRenderer`].

use std::collections::HashMap;
use std::time::Duration;

use colored::Colorize;

use crate::{
    kubernetes::{ApplyOutcome, DiscoveryMode, ReleaseState, ReleaseStatus, ResourceKey},
    Result,
};

/// Something that happened during an apply, reported as it happens.
pub(crate) enum ApplyEvent<'a> {
    /// One resource finished applying.
    Applied {
        key: &'a ResourceKey,
        result: &'a Result<ApplyOutcome>,
    },
    /// Pruning of `count` resources that are no longer desired begins.
    PruneStarted { count: usize },
    /// One pruned resource finished deleting.
    Pruned {
        key: &'a ResourceKey,
        result: &'a Result<()>,
    },
    /// Pruning finished.
    PruneFinished,
}

/// Receiver of [`ApplyEvent`]s.
pub(crate) type ApplyEventSink<'s> = dyn FnMut(ApplyEvent<'_>) + Send + 's;

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
    /// The recorded revision, unless the apply ran without release tracking.
    pub(crate) release: Option<&'a ReleaseState>,
    /// Phase durations, in the order the phases ran.
    pub(crate) timings: &'a [PhaseTiming],
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
            ApplyEvent::PruneStarted { count } => println!("\nPruning {} resources...", count),
            ApplyEvent::Pruned { key, result: Ok(()) } => println!("  ✓ Deleted {}", key),
            ApplyEvent::Pruned { key, result: Err(e) } => println!("  ✗ Failed to delete {}: {}", key, e),
            ApplyEvent::PruneFinished => println!(),
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
            if release.status == ReleaseStatus::Deployed {
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

/// The outcome a dry run stands for, or the outcome itself.
fn effective_outcome(outcome: &ApplyOutcome) -> &ApplyOutcome {
    match outcome {
        ApplyOutcome::DryRun { would_be } => effective_outcome(would_be),
        other => other,
    }
}
