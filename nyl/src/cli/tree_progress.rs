use std::io::IsTerminal;

use clap::{Args, ValueEnum};
use tracing::Span;
use tracing_indicatif::span_ext::IndicatifSpanExt;
use tracing_indicatif::style::ProgressStyle;

use crate::gitops::{ReleaseProgress, TreeRenderObserver};

/// Progress controls shared by the rendered-tree commands.
#[derive(Args, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TreeProgressArgs {
    /// How to report Release rendering progress.
    #[arg(long, value_enum, default_value = "auto")]
    pub progress: TreeProgressMode,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum TreeProgressMode {
    /// Use a progress bar on an attended stderr and lines otherwise.
    #[default]
    Auto,
    /// Use a terminal progress bar.
    Bar,
    /// Print one line when each Release starts.
    Plain,
    /// Do not report Release progress.
    Off,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResolvedProgressMode {
    Bar,
    Plain,
    Off,
}

impl TreeProgressMode {
    fn resolve(self, stderr_is_terminal: bool) -> ResolvedProgressMode {
        match (self, stderr_is_terminal) {
            (Self::Auto, true) | (Self::Bar, _) => ResolvedProgressMode::Bar,
            (Self::Auto, false) | (Self::Plain, _) => ResolvedProgressMode::Plain,
            (Self::Off, _) => ResolvedProgressMode::Off,
        }
    }
}

pub struct TreeProgressReporter {
    phase: Option<String>,
    state: ReporterState,
}

enum ReporterState {
    Bar { span: Option<Span> },
    Plain,
    Off,
}

impl TreeProgressReporter {
    pub fn new(args: TreeProgressArgs, phase: Option<String>) -> Self {
        Self::with_terminal(args, phase, std::io::stderr().is_terminal())
    }

    fn with_terminal(args: TreeProgressArgs, phase: Option<String>, stderr_is_terminal: bool) -> Self {
        let state = match args.progress.resolve(stderr_is_terminal) {
            ResolvedProgressMode::Bar => {
                let span = tracing::info_span!("tree_render_progress", "indicatif.pb_show" = tracing::field::Empty);
                ReporterState::Bar { span: Some(span) }
            }
            ResolvedProgressMode::Plain => ReporterState::Plain,
            ResolvedProgressMode::Off => ReporterState::Off,
        };
        Self { phase, state }
    }

    fn phase_prefix(&self) -> String {
        self.phase
            .as_deref()
            .map_or_else(String::new, |phase| format!("{phase}: "))
    }

    fn display_release(release: &ReleaseProgress) -> String {
        let identity = release.name.as_deref().map_or_else(
            || release.application_group.clone(),
            |name| format!("{}/{}", release.application_group, name),
        );
        format!("{identity} ({})", release.source_path.display())
    }
}

impl TreeRenderObserver for TreeProgressReporter {
    fn started(&mut self, total: usize) {
        if total == 0 {
            return;
        }
        if let ReporterState::Bar { span: Some(span) } = &self.state {
            let template = format!(
                "{{spinner}} {}[{{bar:30}}] {{pos}}/{{len}} {{msg}}",
                self.phase_prefix()
            );
            let style = ProgressStyle::with_template(&template)
                .expect("the tree progress template is valid")
                .progress_chars("=>-");
            span.pb_set_style(&style);
            span.pb_set_length(total as u64);
            span.pb_start();
        }
    }

    fn release_started(&mut self, current: usize, total: usize, release: &ReleaseProgress) {
        let display = Self::display_release(release);
        match &self.state {
            ReporterState::Bar { span: Some(span) } => span.pb_set_message(&display),
            ReporterState::Plain => eprintln!("{}[{current}/{total}] Release {display}", self.phase_prefix()),
            ReporterState::Bar { span: None } | ReporterState::Off => {}
        }
    }

    fn release_finished(&mut self, completed: usize) {
        if let ReporterState::Bar { span: Some(span) } = &self.state {
            span.pb_set_position(completed as u64);
        }
    }

    fn finished(&mut self) {
        if let ReporterState::Bar { span } = &mut self.state {
            span.take();
        }
    }
}

impl Drop for TreeProgressReporter {
    fn drop(&mut self) {
        self.finished();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn automatic_progress_follows_stderr_terminal_state() {
        assert_eq!(TreeProgressMode::Auto.resolve(true), ResolvedProgressMode::Bar);
        assert_eq!(TreeProgressMode::Auto.resolve(false), ResolvedProgressMode::Plain);
    }

    #[test]
    fn explicit_progress_modes_ignore_terminal_state() {
        assert_eq!(TreeProgressMode::Bar.resolve(false), ResolvedProgressMode::Bar);
        assert_eq!(TreeProgressMode::Plain.resolve(true), ResolvedProgressMode::Plain);
        assert_eq!(TreeProgressMode::Off.resolve(true), ResolvedProgressMode::Off);
    }
}
