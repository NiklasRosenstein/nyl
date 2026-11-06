//! Observed values, end to end through the orchestration shell with Git
//! state: a driver reads a value of the system a unit manages, the shell
//! records it for the unit's current receipt, and consumers resolve it.
//!
//! Contract: [Observed values](../../design/orchestration-core.md#observed-values).

mod scenario;

/// The `observed-value` walkthrough, which `scenario_harness_test` runs once
/// the harness reaches M5, runs here already with the publication fake: its
/// steps are the contract of the rules below, in the scenario format.
#[test]
fn test_the_observed_value_walkthrough_passes_with_the_fake_publication() {
    let (loaded, problems) = scenario::load_all();
    assert!(problems.is_empty(), "invalid scenarios:\n{}", problems.join("\n"));
    let walkthrough = loaded
        .iter()
        .find(|loaded| loaded.name == "walkthroughs/observed-value")
        .expect("the walkthrough exists");
    if let Err(failure) = scenario::runner::run(walkthrough) {
        panic!("{failure}");
    }
}

/// The same walkthrough against the built-in `KubernetesPublication` driver:
/// its declaration is validated, its resolution renders the target and
/// checks the observed Service is one it renders, and only the live system
/// it reads is the script.
#[test]
fn test_the_observed_value_walkthrough_passes_with_the_real_publication_driver() {
    let (loaded, problems) = scenario::load_all();
    assert!(problems.is_empty(), "invalid scenarios:\n{}", problems.join("\n"));
    let walkthrough = loaded
        .iter()
        .find(|loaded| loaded.name == "walkthroughs/observed-value")
        .expect("the walkthrough exists");
    if let Err(failure) = scenario::runner::run_real_publication(walkthrough) {
        panic!("{failure}");
    }
}
