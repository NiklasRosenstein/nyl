//! The scenario harness of `design/reference-scenarios.md`.
//!
//! [`format`] parses and validates scenario files against the documented
//! format; [`world`] is the Git test world they run in. Scenarios run in
//! tier 1, in-process with fakes, once the orchestration commands exist;
//! until then a scenario whose milestone the harness has reached is listed
//! as pending, by name and with the reason, so none is skipped silently.

#![allow(dead_code)] // Each test binary uses part of the harness.

pub mod format;
pub mod runner;
pub mod world;

use std::path::{Path, PathBuf};

use format::{Milestone, Scenario};

/// The milestone this harness has reached.
pub const REACHED: Milestone = Milestone(3);

/// A scenario file and its parsed content.
pub struct Loaded {
    pub name: String,
    pub directory: PathBuf,
    pub scenario: Scenario,
}

pub fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

pub fn scenarios_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/scenarios")
}

/// Every scenario under `tests/scenarios/<group>/<name>/scenario.yaml`,
/// named `<group>/<name>`, with the problems of those that do not load.
pub fn load_all() -> (Vec<Loaded>, Vec<String>) {
    let mut loaded = Vec::new();
    let mut problems = Vec::new();
    let mut files = walkdir::WalkDir::new(scenarios_root())
        .min_depth(3)
        .max_depth(3)
        .into_iter()
        .map(|entry| entry.unwrap().into_path())
        .filter(|path| path.file_name().is_some_and(|name| name == "scenario.yaml"))
        .collect::<Vec<_>>();
    files.sort();
    for file in files {
        let directory = file.parent().unwrap().to_path_buf();
        // Names use `/` on every platform, as scenarios refer to each other.
        let name = directory
            .strip_prefix(scenarios_root())
            .unwrap()
            .components()
            .map(|component| component.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        let text = std::fs::read_to_string(&file).unwrap();
        let result = serde_saphyr::from_str::<Scenario>(&text)
            .map_err(|error| error.to_string())
            .and_then(|scenario| format::validate(&scenario, &directory, &repository_root()).map(|()| scenario));
        match result {
            Ok(scenario) => loaded.push(Loaded {
                name,
                directory,
                scenario,
            }),
            Err(error) => problems.push(format!("{name}: {error}")),
        }
    }
    (loaded, problems)
}
