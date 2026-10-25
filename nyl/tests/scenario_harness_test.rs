//! Runs the scenario harness over every scenario file: the format, the test
//! world, and the accounting of scenarios the harness has reached.

mod scenario;

use std::collections::BTreeMap;

use scenario::format::StepKind;
use scenario::world::World;
use scenario::REACHED;

/// Scenarios the harness has reached that cannot run yet, with what they
/// wait for. A scenario leaves this list in the change that makes it run, so
/// none is skipped silently: every reached scenario runs, or is pending.
const PENDING: [(&str, &str); 3] = [
    (
        "walkthroughs/competing-runners",
        "paused invocations and run takeover (M3)",
    ),
    ("walkthroughs/deletion", "deletion and teardown (M3)"),
    ("walkthroughs/effects-without-receipt", "run takeover and recovery (M3)"),
];

/// Every reached scenario that is not pending runs in tier 1 and passes its
/// checks.
#[test]
fn test_reached_scenarios_pass_in_tier_one() {
    let (loaded, problems) = scenario::load_all();
    assert!(problems.is_empty(), "invalid scenarios:\n{}", problems.join("\n"));
    let pending = PENDING.into_iter().collect::<BTreeMap<_, _>>();
    let runnable = loaded
        .iter()
        .filter(|loaded| loaded.scenario.milestone <= REACHED)
        .filter(|loaded| loaded.scenario.tiers.contains(&1))
        .filter(|loaded| !pending.contains_key(loaded.name.as_str()))
        .collect::<Vec<_>>();
    assert!(!runnable.is_empty(), "no scenario runs in tier 1");
    let failures = std::thread::scope(|scope| {
        let handles = runnable
            .iter()
            .map(|loaded| scope.spawn(|| scenario::runner::run(loaded)))
            .collect::<Vec<_>>();
        handles
            .into_iter()
            .filter_map(|handle| {
                handle
                    .join()
                    .unwrap_or_else(|_| Err("the scenario panicked".to_owned()))
                    .err()
            })
            .collect::<Vec<_>>()
    });
    assert!(failures.is_empty(), "failed scenarios:\n{}", failures.join("\n\n"));
}

#[test]
fn test_deploy_globs_match_by_segment() {
    use scenario::runner::glob_matches;
    assert!(glob_matches("dev/web/**", "dev/web/a/b.yaml"));
    assert!(glob_matches("dev/_nyl/index.json", "dev/_nyl/index.json"));
    assert!(glob_matches("dev/*.yaml", "dev/a.yaml"));
    assert!(!glob_matches("dev/*.yaml", "dev/a/b.yaml"));
    assert!(!glob_matches("dev/web/**", "prod/web/a"));
}

#[test]
fn test_every_scenario_follows_the_format() {
    let (loaded, problems) = scenario::load_all();
    assert!(problems.is_empty(), "invalid scenarios:\n{}", problems.join("\n"));
    assert!(loaded.len() >= 9, "only {} scenarios were found", loaded.len());
}

#[test]
fn test_reached_scenarios_run_or_are_listed_as_pending() {
    let (loaded, _) = scenario::load_all();
    let pending = PENDING.into_iter().collect::<BTreeMap<_, _>>();
    let reached = loaded
        .iter()
        .filter(|loaded| loaded.scenario.milestone <= REACHED)
        .map(|loaded| loaded.name.as_str())
        .collect::<Vec<_>>();
    // Every reached scenario runs in tier 1 (test_reached_scenarios_pass_in_tier_one)
    // or is pending; a pending one must still be reached.
    for loaded in loaded.iter().filter(|loaded| loaded.scenario.milestone <= REACHED) {
        assert!(
            pending.contains_key(loaded.name.as_str()) || loaded.scenario.tiers.contains(&1),
            "{} is reached at {REACHED} but neither runs in tier 1 nor is listed as pending",
            loaded.name
        );
    }
    for name in pending.keys() {
        assert!(
            reached.contains(name),
            "{name} is listed as pending but not reached at {REACHED}"
        );
    }
    // Step kinds of later milestones are validated now and executed from their milestone.
    for loaded in &loaded {
        if loaded.scenario.milestone <= REACHED {
            for step in &loaded.scenario.steps {
                let kind = step.kind().unwrap();
                assert!(
                    kind.executes_from() <= REACHED,
                    "{} uses {kind:?}, which runs from {}",
                    loaded.name,
                    kind.executes_from()
                );
            }
        }
    }
}

/// Every scenario's project commits, and its Git steps apply in order, in a
/// test world of its own.
#[test]
fn test_scenario_git_steps_apply_in_the_test_world() {
    let (loaded, problems) = scenario::load_all();
    assert!(problems.is_empty(), "invalid scenarios:\n{}", problems.join("\n"));
    for loaded in loaded {
        let world = World::new(&loaded.directory.join(&loaded.scenario.project));
        for step in &loaded.scenario.steps {
            match step.kind().unwrap() {
                StepKind::Commit => world.commit(step.commit.as_ref().unwrap(), &loaded.directory),
                StepKind::Branch => world.branch(step.branch.as_ref().unwrap()),
                StepKind::SquashMerge => world.squash_merge(step.squash_merge.as_ref().unwrap()),
                StepKind::DeleteBranch => world.delete_branch(step.delete_branch.as_ref().unwrap()),
                _ => {}
            }
        }
        let files = world.files("main");
        assert!(
            files.iter().any(|file| file.ends_with("nyl.toml")),
            "{}: main has no nyl.toml after its steps",
            loaded.name
        );
        let project =
            std::fs::read_to_string(world.fresh_clone("main").join(nyl_toml_or_resources(&files))).unwrap_or_default();
        assert!(
            !project.contains("%PROJECT_URL%") && !project.contains("%SCENARIO_TMP%"),
            "{}: placeholders were not substituted",
            loaded.name
        );
    }
}

fn nyl_toml_or_resources(files: &[String]) -> String {
    files
        .iter()
        .find(|file| file.ends_with("platform.yaml") || file.ends_with("dev.yaml"))
        .or_else(|| files.iter().find(|file| file.ends_with("nyl.toml")))
        .cloned()
        .unwrap()
}
