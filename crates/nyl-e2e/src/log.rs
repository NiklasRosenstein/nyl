//! Console and transcript output. Everything goes to stderr with the time
//! since the run started, so a CI log reads as a timeline; the transcript file
//! also receives the output of commands that were not echoed, so a failed run
//! can upload one file that holds everything.

use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

static START: OnceLock<Instant> = OnceLock::new();
static DEPTH: AtomicUsize = AtomicUsize::new(0);
static TRANSCRIPT: Mutex<Option<File>> = Mutex::new(None);

/// Start the clock that stamps every line.
pub fn init() {
    START.get_or_init(Instant::now);
}

/// Also write everything to `path` (appending).
pub fn open_transcript(path: &Path) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match std::fs::OpenOptions::new().create(true).append(true).open(path) {
        Ok(file) => {
            if let Ok(mut slot) = TRANSCRIPT.lock() {
                *slot = Some(file);
            }
        }
        Err(error) => eprintln!("warning: cannot open the transcript {}: {error}", path.display()),
    }
}

/// `mm:ss` since [`init`].
pub fn elapsed_label() -> String {
    let seconds = START.get_or_init(Instant::now).elapsed().as_secs();
    format!("{:02}:{:02}", seconds / 60, seconds % 60)
}

/// One timestamped line.
pub fn line(message: impl AsRef<str>) {
    let text = format!("[{}] {}\n", elapsed_label(), message.as_ref());
    raw(&text);
}

/// Text exactly as given, to stderr and the transcript.
pub fn raw(text: &str) {
    let _ = std::io::stderr().write_all(text.as_bytes());
    transcript_only(text);
}

/// Text for the transcript file only, such as the output of a command that
/// succeeded and was too noisy for the console.
pub fn transcript_only(text: &str) {
    if let Ok(mut slot) = TRANSCRIPT.lock() {
        if let Some(file) = slot.as_mut() {
            let _ = file.write_all(text.as_bytes());
        }
    }
}

/// Whether the suite runs in GitHub Actions, where `::group::` folds output.
pub fn in_github_actions() -> bool {
    std::env::var_os("GITHUB_ACTIONS").is_some()
}

/// A folded section of the CI log, closed when dropped. GitHub Actions cannot
/// nest groups, so only the outermost one folds and an inner one is a heading.
pub struct Group {
    folded: bool,
}

impl Group {
    pub fn open(title: &str) -> Self {
        let outermost = DEPTH.fetch_add(1, Ordering::SeqCst) == 0;
        let folded = outermost && in_github_actions();
        if folded {
            raw(&format!("::group::{title}\n"));
        } else {
            raw(&format!("\n==> {title}\n"));
        }
        Self { folded }
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        DEPTH.fetch_sub(1, Ordering::SeqCst);
        if self.folded {
            raw("::endgroup::\n");
        }
    }
}

/// Ask GitHub Actions to hide `secret` from the logs, and do nothing elsewhere.
pub fn mask_in_ci(secret: &str) {
    if in_github_actions() && !secret.is_empty() {
        raw(&format!("::add-mask::{secret}\n"));
    }
}

/// A failure annotation that GitHub Actions shows on the run's summary page.
pub fn error_annotation(message: &str) {
    if in_github_actions() {
        // Annotations are one line.
        raw(&format!("::error::{}\n", message.replace('\n', " | ")));
    }
}
