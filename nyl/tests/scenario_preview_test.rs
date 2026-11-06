//! The preview reference scenario (scenario 3 of
//! `design/reference-scenarios.md`, preview expiry, after scenario 2's two
//! pull requests), run with the tier-1 fakes and with the real
//! `KubernetesPublication` driver.
//!
//! The harness reaches M5 with the last pull request of the milestone, so
//! `scenario_harness_test` skips scenarios of M5 for now. This test runs this
//! one already, as the walkthrough tests of the observed-value and
//! attested-evidence rules run theirs.

mod scenario;

fn load(name: &str) -> scenario::Loaded {
    let (loaded, problems) = scenario::load_all();
    assert!(problems.is_empty(), "invalid scenarios:\n{}", problems.join("\n"));
    loaded
        .into_iter()
        .find(|loaded| loaded.name == name)
        .unwrap_or_else(|| panic!("the scenario {name} exists"))
}

fn preview() -> scenario::Loaded {
    load("reference/preview")
}

/// The scenario's steps are the contract of expiry, `maxInstances`, the
/// shared catalog, and an instance reading dev's network, in the scenario
/// format. With the fake publication, which publishes the same bytes.
#[test]
fn test_the_preview_scenario_passes_with_the_fake_publication() {
    if let Err(failure) = scenario::runner::run(&preview()) {
        panic!("{failure}");
    }
}

/// The same steps against the built-in driver: it records the target's
/// teardown settings (reading the shared catalog's), publishes the shared
/// manifest, and removes each preview by its own two phases.
#[test]
fn test_the_preview_scenario_passes_with_the_real_publication_driver() {
    if let Err(failure) = scenario::runner::run_real_publication(&preview()) {
        panic!("{failure}");
    }
}

/// The variant whose Namespaces keep the default `deletePolicy: Confirm`: the
/// built-in driver warns once the unit has resolved (an explicit reconcile or
/// plan, not `state init`), an expired preview's removal does not start, and
/// `--allow-incomplete` removes it. With the real driver only, which records
/// the settings from the render.
#[test]
fn test_the_not_teardown_ready_variant_passes_with_the_real_publication_driver() {
    if let Err(failure) = scenario::runner::run_real_publication(&load("reference/preview-not-ready")) {
        panic!("{failure}");
    }
}

/// The variant that leaves `kubernetes` on the manual wait: the warning comes
/// from the declaration at `state init`, and the removal of an expired preview
/// stops after phase 1 until an operator confirms. With both publication
/// drivers, because it needs no recorded settings.
#[test]
fn test_the_manual_wait_variant_passes_with_both_publication_drivers() {
    let variant = load("reference/preview-manual-wait");
    if let Err(failure) = scenario::runner::run(&variant) {
        panic!("{failure}");
    }
    if let Err(failure) = scenario::runner::run_real_publication(&variant) {
        panic!("{failure}");
    }
}
