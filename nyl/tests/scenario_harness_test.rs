//! Runs the scenario harness over every scenario file: the format, the test
//! world, and the accounting of scenarios the harness has reached.

mod scenario;

use std::collections::BTreeMap;

use scenario::format::StepKind;
use scenario::world::World;
use scenario::REACHED;

/// Scenarios the harness has reached that cannot run until the commands they
/// use exist, with what they wait for. A scenario leaves this list in the
/// change that makes it run, so none is skipped silently.
const PENDING: [(&str, &str); 6] = [
    ("walkthroughs/competing-runners", "leases and reconcile (M3)"),
    ("walkthroughs/deletion", "reconcile, plan, and deletion (M3)"),
    ("walkthroughs/dependency-wave", "state init, plan, and reconcile (M3)"),
    ("walkthroughs/effects-without-receipt", "run takeover and recovery (M3)"),
    ("walkthroughs/unavailable-upstream-output", "reconcile and plan (M3)"),
    ("walkthroughs/unit-jobs", "unit-scoped reconcile (M3)"),
];

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
    for name in &reached {
        assert!(
            pending.contains_key(name),
            "{name} is reached at {REACHED} but neither runs nor is listed as pending"
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
