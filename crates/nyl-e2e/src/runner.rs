//! Running variants: the prerequisites, the infrastructure, the scenario, the
//! diagnostics on failure, and the cleanup, and a summary of what happened.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::cmd::Cmd;
use crate::diag;
use crate::env::Env;
use crate::error::{Context, Error, Result};
use crate::infra::Infra;
use crate::log;
use crate::prereq::{self, SystemProbe, TOOLS};
use crate::reconciler::{reconciler_for, Reconciler};
use crate::scenario;
use crate::variant::Variant;

/// How a variant ended.
#[derive(Debug)]
pub enum Outcome {
    Passed(Duration),
    Failed(String),
    /// Declared but not wired; says what it waits for.
    Pending(String),
}

impl Outcome {
    fn label(&self) -> &'static str {
        match self {
            Self::Passed(_) => "passed",
            Self::Failed(_) => "FAILED",
            Self::Pending(_) => "pending",
        }
    }
}

/// What `run` needs besides the variants.
pub struct RunOptions {
    pub nyl: PathBuf,
    pub work_dir: PathBuf,
    /// Leave the cluster and containers running afterwards, also after a failure.
    pub keep: bool,
}

/// The directory a variant keeps its files in.
pub fn variant_directory(work_dir: &Path, variant: Variant) -> PathBuf {
    work_dir.join(variant.name())
}

/// Check what the cluster scenarios need: the tools and the `nyl` binary.
pub fn check_prerequisites(nyl: Option<&Path>) -> Result<()> {
    prereq::check(&TOOLS, &SystemProbe)?;
    if let Some(nyl) = nyl {
        let output = Cmd::new(nyl)
            .arg("--version")
            .timeout(Duration::from_secs(30))
            .run()
            .map_err(|error| error.context("the nyl binary (--nyl or NYL_E2E_NYL)"))?;
        if !output.success() {
            return Err(Error::new(output.describe_failure(&[0])).context("the nyl binary does not run"));
        }
        log::line(format!("using {} ({})", nyl.display(), output.stdout_trimmed()));
    }
    Ok(())
}

/// Run the scenario of `variant`; the outcome says how it ended.
pub fn run_variant(variant: Variant, options: &RunOptions) -> Outcome {
    let reconciler = reconciler_for(variant);
    if let Err(error) = reconciler.availability() {
        return if error.is_pending() {
            Outcome::Pending(error.to_string())
        } else {
            Outcome::Failed(error.to_string())
        };
    }
    let directory = variant_directory(&options.work_dir, variant);
    log::open_transcript(&directory.join("logs").join("transcript.log"));
    let started = Instant::now();
    log::line(format!("===== variant {} =====", variant.name()));
    let result = run_scenario(variant, &directory, reconciler.as_ref(), options);
    let outcome = match result {
        Ok(()) => Outcome::Passed(started.elapsed()),
        Err(error) if error.is_pending() => Outcome::Pending(error.to_string()),
        Err(error) => {
            log::error_annotation(&format!("{}: {error}", variant.name()));
            log::raw(&format!("\nFAILED {}: {error}\n", variant.name()));
            Outcome::Failed(error.to_string())
        }
    };
    if options.keep {
        log::line(format!(
            "keeping the infrastructure of {}; `nyl-e2e teardown --variant {}` removes it",
            variant.name(),
            variant.name()
        ));
    } else {
        Infra::down(variant, &directory);
    }
    outcome
}

fn run_scenario(variant: Variant, directory: &Path, reconciler: &dyn Reconciler, options: &RunOptions) -> Result<()> {
    let (infra, reused) = Infra::up(variant, directory).inspect_err(|_| diag::overview())?;
    let mut env = None;
    let result = (|| {
        if reused {
            infra.wait_nodes_ready()?;
            infra.reset()?;
        }
        let search_path = std::env::var("PATH").context("read PATH")?;
        let created = Env::new(infra.clone(), &options.nyl, reconciler, search_path)?;
        let result = scenario::run(&created, reconciler);
        env = Some(created);
        result
    })();
    if let Err(error) = &result {
        if !error.is_pending() {
            diag::dump(&infra, reconciler, env.as_ref());
        }
    }
    result
}

/// The table printed after all variants ran.
pub fn summary(outcomes: &[(Variant, Outcome)]) -> String {
    let mut text = String::from("\nE2E summary\n");
    for (variant, outcome) in outcomes {
        let detail = match outcome {
            Outcome::Passed(elapsed) => format!("in {}s", elapsed.as_secs()),
            Outcome::Failed(message) => message.lines().next().unwrap_or_default().to_owned(),
            Outcome::Pending(message) => message.lines().next().unwrap_or_default().to_owned(),
        };
        text.push_str(&format!("  {:<18} {:<8} {detail}\n", variant.name(), outcome.label()));
    }
    text
}

/// The process exit code: 1 when any variant failed, 3 when none passed and
/// some are pending, 0 otherwise.
pub fn exit_code(outcomes: &[(Variant, Outcome)]) -> u8 {
    if outcomes
        .iter()
        .any(|(_, outcome)| matches!(outcome, Outcome::Failed(_)))
    {
        1
    } else if outcomes
        .iter()
        .all(|(_, outcome)| matches!(outcome, Outcome::Pending(_)))
    {
        3
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcomes(list: Vec<Outcome>) -> Vec<(Variant, Outcome)> {
        Variant::ALL.into_iter().zip(list).collect()
    }

    #[test]
    fn test_any_failure_fails_the_run() {
        let results = outcomes(vec![
            Outcome::Passed(Duration::from_secs(1)),
            Outcome::Failed("x".to_owned()),
            Outcome::Pending("p".to_owned()),
        ]);
        assert_eq!(exit_code(&results), 1);
    }

    #[test]
    fn test_a_pending_variant_beside_passing_ones_does_not_fail_all() {
        let results = outcomes(vec![
            Outcome::Passed(Duration::from_secs(1)),
            Outcome::Passed(Duration::from_secs(2)),
            Outcome::Pending("p".to_owned()),
        ]);
        assert_eq!(exit_code(&results), 0);
    }

    #[test]
    fn test_asking_only_for_a_pending_variant_exits_with_its_own_code() {
        let results = vec![(Variant::Direct, Outcome::Pending("waits for m5/10".to_owned()))];
        assert_eq!(exit_code(&results), 3);
    }

    #[test]
    fn test_the_summary_names_each_variant_and_how_it_ended() {
        let results = outcomes(vec![
            Outcome::Passed(Duration::from_secs(95)),
            Outcome::Failed("step \"attest\": timed out\nmore".to_owned()),
            Outcome::Pending("the direct variant is pending".to_owned()),
        ]);
        let text = summary(&results);
        assert!(text.contains("argocd-kubernetes  passed   in 95s"), "{text}");
        assert!(
            text.contains("argocd-api         FAILED   step \"attest\": timed out"),
            "{text}"
        );
        assert!(
            !text.contains("more"),
            "only the first line of a message is shown: {text}"
        );
        assert!(
            text.contains("direct             pending  the direct variant is pending"),
            "{text}"
        );
    }
}
