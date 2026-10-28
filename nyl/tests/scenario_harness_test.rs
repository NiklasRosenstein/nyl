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
const PENDING: [(&str, &str); 0] = [];

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

/// Enabling tier 2 runs every reached two-tier scenario through the real
/// executable. A failed process or unmet assertion fails instead of skipping.
#[cfg(feature = "tier2")]
#[test]
fn test_reached_scenarios_pass_in_tier_two() {
    let (loaded, problems) = scenario::load_all();
    assert!(problems.is_empty(), "invalid scenarios:\n{}", problems.join("\n"));
    let runnable = loaded
        .iter()
        .filter(|loaded| loaded.scenario.milestone <= REACHED && loaded.scenario.tiers.contains(&2))
        .collect::<Vec<_>>();
    assert!(!runnable.is_empty(), "no scenario runs in tier 2");
    let failures = std::thread::scope(|scope| {
        let handles = runnable
            .iter()
            .map(|loaded| scope.spawn(|| scenario::runner::run_binary(loaded)))
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
    assert!(
        failures.is_empty(),
        "failed binary scenarios:\n{}",
        failures.join("\n\n")
    );
}

/// A tier-1-only command must not create state for the binary path, and a
/// wrong expected binary exit must fail the scenario instead of being skipped.
#[cfg(feature = "tier2")]
#[test]
fn test_tier_two_filters_steps_and_checks_process_exits() {
    let text = "contract: design/reference-scenarios.md#tiers\nmilestone: M3\nproject: ../_project\nsteps:\n  - nyl: [state, init, -e, dev]\n    tier: 1\n    exit: 0\n  - nyl: [reconcile, -e, dev]\n    exit: 1\n";
    let loaded = inline_scenario("inline/binary-filter", text);
    scenario::runner::run_binary(&loaded).unwrap();
    let wrong = inline_scenario("inline/binary-exit", &text.replace("exit: 1", "exit: 0"));
    let error = scenario::runner::run_binary(&wrong).unwrap_err();
    assert!(error.contains("exit 1 instead of 0"), "{error}");
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
    assert!(loaded.len() >= 13, "only {} scenarios were found", loaded.len());
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

/// Every `nyl` step of a scenario that runs parses with the real command
/// line, so a misused option fails here with its step, not mid-scenario.
#[test]
fn test_running_scenarios_parse_with_the_command_line() {
    let (loaded, problems) = scenario::load_all();
    assert!(problems.is_empty(), "invalid scenarios:\n{}", problems.join("\n"));
    let pending = PENDING.into_iter().collect::<BTreeMap<_, _>>();
    for loaded in loaded
        .iter()
        .filter(|loaded| loaded.scenario.milestone <= REACHED && !pending.contains_key(loaded.name.as_str()))
    {
        for (index, step) in loaded.scenario.steps.iter().enumerate() {
            if let Some(args) = &step.nyl {
                if let Err(error) = scenario::format::validate_arguments(args) {
                    panic!("{}: step {}: {error}", loaded.name, index + 1);
                }
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

/// A scenario parsed from `text` whose relative paths resolve in an existing
/// walkthrough directory, so it can use their shared project.
fn inline_scenario(name: &str, text: &str) -> scenario::Loaded {
    scenario::Loaded {
        name: name.to_owned(),
        directory: scenario::scenarios_root().join("walkthroughs/dependency-wave"),
        scenario: serde_saphyr::from_str(text).unwrap(),
    }
}

/// Run a valid inline scenario whose relative paths resolve in the scenario
/// directory `directory`, so it can use that directory's patches.
fn run_inline(directory: &str, name: &str, text: &str) -> Result<(), String> {
    let loaded = scenario::Loaded {
        name: name.to_owned(),
        directory: scenario::scenarios_root().join(directory),
        scenario: serde_saphyr::from_str(text).unwrap(),
    };
    scenario::format::validate(&loaded.scenario, &loaded.directory, &scenario::repository_root())
        .unwrap_or_else(|error| panic!("{name} is invalid: {error}"));
    scenario::runner::run(&loaded)
}

fn validation_error(steps: &str) -> String {
    let text = format!(
        "contract: design/orchestration-core.md#walkthroughs\nmilestone: M3\ntiers: [1]\nproject: ../_project\nsteps:\n{steps}"
    );
    let loaded = inline_scenario("inline/validation", &text);
    scenario::format::validate(&loaded.scenario, &loaded.directory, &scenario::repository_root()).unwrap_err()
}

/// A fault names the unit it concerns exactly where the shell consults the
/// point with one; a fault that could never fire is rejected up front.
#[test]
fn test_validate_fault_unit_rules() {
    let fault = |fault: &str| format!("  - fault: {fault}\n  - nyl: [reconcile, -e, dev]\n    killed: true\n");
    let valid = |fault_step: &str| {
        let text = format!(
            "contract: design/orchestration-core.md#walkthroughs\nmilestone: M3\ntiers: [1]\nproject: ../_project\nsteps:\n{}",
            fault(fault_step)
        );
        let loaded = inline_scenario("inline/valid", &text);
        scenario::format::validate(&loaded.scenario, &loaded.directory, &scenario::repository_root())
    };
    assert_eq!(valid("{point: before-lease-update}"), Ok(()));
    assert_eq!(valid("{point: before-lease-update, unit: database}"), Ok(()));
    assert_eq!(valid("{point: before-push}"), Ok(()));
    assert_eq!(valid("{point: after-effect, unit: network}"), Ok(()));
    assert!(validation_error(&fault("{point: after-effect}")).contains("needs the unit"));
    assert!(validation_error(&fault("{point: before-checkpoint}")).contains("needs the unit"));
    assert!(validation_error(&fault("{point: before-push, unit: network}")).contains("takes no unit"));
    assert!(validation_error(&fault("{point: executing, unit: network}")).contains("only pause"));
    assert!(validation_error(&fault("{point: waiting-lease}")).contains("only pause"));
}

/// A resume names an earlier paused step, once, and every paused step is resumed.
#[test]
fn test_validate_resume_ids() {
    let unknown = validation_error("  - resume: first\n    exit: 0\n");
    assert!(unknown.contains("no earlier nyl step has that id"), "{unknown}");
    let paused = "  - nyl: [reconcile, -e, dev]\n    id: first\n    pause: {point: executing, unit: database}\n";
    let twice = validation_error(&format!(
        "{paused}  - resume: first\n    exit: 0\n  - resume: first\n    exit: 0\n"
    ));
    assert!(twice.contains("already resumed"), "{twice}");
    let never = validation_error(paused);
    assert!(never.contains("never resumed"), "{never}");
}

/// A paused invocation that ends without reaching its pause point fails
/// the scenario with the point it missed, instead of hanging it.
#[test]
fn test_paused_invocation_that_never_reaches_its_point_fails() {
    let loaded = inline_scenario(
        "inline/never-reached",
        "contract: design/orchestration-core.md#walkthroughs
milestone: M3
tiers: [1]
project: ../_project
steps:
  - nyl: [state, init, -e, dev]
    exit: 0
  - nyl: [plan, -e, dev]
    id: plan
    pause: {point: executing, unit: database}
  - resume: plan
    exit: 0
",
    );
    let error = scenario::runner::run(&loaded).unwrap_err();
    assert!(
        error.contains("step 2: plan never reached its pause point executing for database"),
        "{error}"
    );
}

/// A paused invocation left without a resume step is let go and reported,
/// so the scenario neither hangs nor passes.
#[test]
fn test_paused_invocation_never_resumed_fails() {
    let loaded = inline_scenario(
        "inline/never-resumed",
        "contract: design/orchestration-core.md#walkthroughs
milestone: M3
tiers: [1]
project: ../_project
steps:
  - nyl: [state, init, -e, dev]
    exit: 0
  - nyl: [reconcile, -e, dev]
    id: first
    pause: {point: executing, unit: database}
",
    );
    let error = scenario::runner::run(&loaded).unwrap_err();
    assert!(error.contains("first was paused and never resumed"), "{error}");
}

/// Concurrent invocations pause and resume where a scenario says: a run
/// paused mid-wave holds the lease, a run waiting for it pauses inside the
/// wait, the first finishes when resumed, and the waiting run then finds
/// nothing to do. Each resumed step counts only its own commits, not those
/// another step wrote while it was paused. A run whose lease ref was deleted
/// while it was paused loses its lease and pushes nothing.
///
/// These are the steps of `walkthroughs/competing-runners` that need no run
/// takeover, so they run while that scenario is pending.
#[test]
fn test_paused_invocations_resume_concurrently() {
    run_inline(
        "walkthroughs/competing-runners",
        "inline/competing-runners",
        r#"contract: design/orchestration-core.md#walkthroughs
milestone: M3
tiers: [1]
project: ../_project
steps:
  - nyl: [state, init, -e, dev]
    exit: 0
  - nyl: [reconcile, -e, dev]
    id: first
    pause: {point: executing, unit: database}
  - nyl: [reconcile, -e, dev]
    exit: 2
    expect:
      codes: [NYL-LEASE-HELD]
      commits: {desired: 0, observed: 0}
  - nyl: [reconcile, -e, dev, --wait-lease, 30m]
    id: waiting
    pause: {point: waiting-lease}
  - resume: first
    exit: 0
    expect:
      executed: [network, web-image, database, kubernetes]
      commits: {desired: 1, observed: 1}
  - resume: waiting
    exit: 0
    expect:
      executed: []
      commits: {desired: 0, observed: 0}
  - commit: {branch: main, message: "Change the web page", apply: patches/web-page}
  - nyl: [reconcile, -e, dev]
    id: third
    pause: {point: executing, unit: web-image}
  - ref: {delete: refs/nyl/dev/lease}
  - nyl: [reconcile, -e, dev]
    exit: 2
    expect:
      codes: [NYL-LEASE-HELD]
      executed: []
  - resume: third
    exit: 4
    expect:
      codes: [NYL-LEASE-LOST]
      commits: {desired: 0, observed: 0}
"#,
    )
    .unwrap();
}

/// A fault naming a unit kills only where the shell consults the point with
/// that unit: a `before-lease-update` fault for `database` passes the lease
/// updates consulted without a unit and wave 1's, so the run reaches its
/// pause in wave 1, and dies at database's phase once resumed. The dead run
/// leaves the lease held and wrote no state commit.
#[test]
fn test_fault_for_a_unit_kills_at_that_units_point() {
    run_inline(
        "walkthroughs/dependency-wave",
        "inline/unit-fault",
        r#"contract: design/orchestration-core.md#runs-and-leases
milestone: M3
tiers: [1]
project: ../_project
steps:
  - nyl: [state, init, -e, dev]
    exit: 0
  - fault: {point: before-lease-update, unit: database}
  - nyl: [reconcile, -e, dev]
    id: first
    pause: {point: executing, unit: web-image}
  - resume: first
    killed: true
  - nyl: [reconcile, -e, dev]
    exit: 2
    expect:
      codes: [NYL-LEASE-HELD]
      commits: {desired: 0, observed: 0}
"#,
    )
    .unwrap();
}

/// A fault without a unit kills where the shell consults the point without
/// one: `before-push` ends the run before it pushes anything, and the lease
/// stays held.
#[test]
fn test_fault_without_a_unit_kills_before_push() {
    run_inline(
        "walkthroughs/dependency-wave",
        "inline/push-fault",
        r#"contract: design/orchestration-core.md#runs-and-leases
milestone: M3
tiers: [1]
project: ../_project
steps:
  - nyl: [state, init, -e, dev]
    exit: 0
  - fault: {point: before-push}
  - nyl: [reconcile, -e, dev]
    killed: true
  - nyl: [reconcile, -e, dev]
    exit: 2
    expect:
      codes: [NYL-LEASE-HELD]
      commits: {desired: 0, observed: 0}
"#,
    )
    .unwrap();
}

/// A `killed: true` step whose fault never fired fails the scenario, so a
/// crash-injection scenario cannot pass without crashing: the second
/// reconcile has nothing to execute, so `database` has no effect to die after.
#[test]
fn test_killed_step_whose_fault_never_fired_fails() {
    let error = run_inline(
        "walkthroughs/dependency-wave",
        "inline/fault-never-fired",
        r#"contract: design/orchestration-core.md#runs-and-leases
milestone: M3
tiers: [1]
project: ../_project
steps:
  - nyl: [state, init, -e, dev]
    exit: 0
  - nyl: [reconcile, -e, dev]
    exit: 0
  - fault: {point: after-effect, unit: database}
  - nyl: [reconcile, -e, dev]
    killed: true
"#,
    )
    .unwrap_err();
    assert!(error.contains("step 4: the armed fault never fired"), "{error}");
}

fn expect(text: &str) -> scenario::format::Expect {
    serde_saphyr::from_str(text).unwrap()
}

fn json(text: &str) -> serde_json::Value {
    serde_json::from_str(text).unwrap()
}

/// The part 5 keys parse, so scenarios can state them; a misspelt key is
/// rejected instead of silently checking nothing.
#[test]
fn test_expect_accepts_part_five_keys_and_rejects_misspelt_ones() {
    let parsed = expect(
        "held: [a]\ndrifted: [b]\nreleased: [c]\npending-teardown: [d]\ntearing-down: [e]\ndriftRepaired: [b]\n\
         reapplied: [b]\nlease-broken: true\noutcome: done\nunits:\n  a: {uid: none, condition-state: uncertain, drift: {result: drifted}}\n",
    );
    assert_eq!(parsed.held, Some(vec!["a".to_owned()]));
    assert_eq!(parsed.lease_broken, Some(true));
    let unit = &parsed.units.unwrap()["a"];
    assert_eq!(unit.uid, Some(scenario::format::UidExpectation::None));
    assert_eq!(unit.condition_state, Some(serde_json::json!("uncertain")));
    for misspelt in [
        "drift-repaired: [a]\n",
        "lease_broken: true\n",
        "releasd: [a]\n",
        "outcome: delivered\n",
        "units:\n  a: {condition_state: uncertain}\n",
        "units:\n  a: {drift: {result: drifted, waits: true}}\n",
        "units:\n  a: {uid: gone}\n",
    ] {
        assert!(
            serde_saphyr::from_str::<scenario::format::Expect>(misspelt).is_err(),
            "{misspelt:?} parsed"
        );
    }
}

/// A drift verdict scripts only `verify`; a file step stays inside
/// `%SCENARIO_TMP%`; and `nyl` arguments must parse with the real command
/// line, so the boolean `--reapply` given a unit is caught up front.
#[test]
fn test_validate_part_five_steps_and_options() {
    let drifted = validation_error("  - driver: {unit: network, operation: reconcile, outcome: drifted}\n");
    assert!(drifted.contains("only a verify outcome can be drifted"), "{drifted}");
    for (step, error) in [
        ("{path: ../outside, content: x}", "stay inside it"),
        ("{path: /etc/passwd, delete: true}", "stay inside it"),
        ("{path: resources/a}", "exactly one of content and delete"),
        (
            "{path: resources/a, content: x, delete: true}",
            "exactly one of content and delete",
        ),
    ] {
        let message = validation_error(&format!("  - file: {step}\n"));
        assert!(message.contains(error), "{step}: {message}");
    }
    let arguments = |args: &[&str]| {
        scenario::format::validate_arguments(&args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>())
    };
    assert!(arguments(&["reconcile", "-e", "dev", "--reapply", "network", "--reason", "why"]).is_err());
    assert!(arguments(&["reconcile", "-e", "dev", "--reapply", "--reason", "why"]).is_err());
    assert_eq!(
        arguments(&[
            "reconcile",
            "-e",
            "dev",
            "--unit",
            "network",
            "--reapply",
            "--reason",
            "why",
            "--fail-fast"
        ]),
        Ok(())
    );
    assert_eq!(
        arguments(&["teardown", "-e", "dev", "--unit", "network", "--fail-fast"]),
        Ok(())
    );
    let valid = |steps: &str| {
        let text = format!(
            "contract: design/orchestration-core.md#walkthroughs\nmilestone: M3\ntiers: [1]\nproject: ../_project\nsteps:\n{steps}"
        );
        let loaded = inline_scenario("inline/valid", &text);
        scenario::format::validate(&loaded.scenario, &loaded.directory, &scenario::repository_root())
    };
    assert_eq!(
        valid(concat!(
            "  - driver: {unit: network, operation: verify, outcome: drifted}\n",
            "  - file: {path: resources/network, content: changed}\n",
            "  - file: {path: resources/network, delete: true}\n",
        )),
        Ok(())
    );
}

/// The outcome sets compare against both the report and the transition
/// summary: a run that reported the right units but whose commit listed
/// another, or the reverse, fails the step.
#[test]
fn test_outcome_sets_compare_report_and_summary() {
    use scenario::runner::check_results;
    let report = json(
        r#"{"environments": [{"units": {
            "cache": {"result": "pending-teardown", "reason": "awaits --allow-teardown"},
            "worker": {"result": "retained"},
            "api": {"result": "drifted"}}}]}"#,
    );
    let summary = json(
        r#"{"units": {"cache": {"result": "pending-teardown"}, "worker": {"result": "retained"}, "api": {"result": "drifted"}}}"#,
    );
    let summaries = [summary];
    let check = |text: &str, summaries: Option<&[serde_json::Value]>| check_results(&expect(text), &report, summaries);
    assert_eq!(
        check(
            "pending-teardown: [cache]\nretained: [worker]\ndrifted: [api]\n",
            Some(&summaries)
        ),
        Ok(())
    );
    let error = check("pending-teardown: [cache, worker]\n", Some(&summaries)).unwrap_err();
    assert!(error.contains("pending-teardown: the report has"), "{error}");
    let error = check("drifted: []\n", Some(&summaries)).unwrap_err();
    assert!(error.contains("drifted: the report has"), "{error}");

    // The report is right, the commit is not.
    let wrong = [json(
        r#"{"units": {"cache": {"result": "removed"}, "api": {"result": "executed"}}}"#,
    )];
    let error = check("pending-teardown: [cache]\n", Some(&wrong)).unwrap_err();
    assert!(
        error.contains("pending-teardown: the transition commit summary lists {}"),
        "{error}"
    );
    let error = check("drifted: [api]\n", Some(&wrong)).unwrap_err();
    assert!(
        error.contains("drifted: the transition commit summary lists {}"),
        "{error}"
    );

    // A state operation's actions carry no outcome: only its report is read,
    // here the teardown run a `state delete --teardown` report carries.
    let state_delete = json(
        r#"{"kind": "StateOperationReport", "outcome": "done",
            "details": {"teardown": {"environments": [{"units": {"seed": {"result": "released"}}}]}}}"#,
    );
    let actions = [json(r#"{"units": {}}"#)];
    assert_eq!(
        check_results(
            &expect("released: [seed]\noutcome: done\n"),
            &state_delete,
            Some(&actions)
        ),
        Ok(())
    );
    let error = check_results(&expect("outcome: refused\n"), &state_delete, Some(&actions)).unwrap_err();
    assert!(error.contains("outcome: the report has Some(\"done\")"), "{error}");
}

/// `driftRepaired`, `reapplied`, and `lease-broken` are read from the
/// summary (and the lists from the report), missing meaning empty, so a run
/// that repaired drift without recording it, or recorded a break twice, fails.
#[test]
fn test_summary_lists_and_lease_break_are_read_from_the_summary() {
    use scenario::runner::check_results;
    let report = json(r#"{"environments": [{"units": {}, "driftRepaired": ["api"], "reapplied": ["db"]}]}"#);
    let summary = json(
        r#"{"units": {}, "recovered": [], "imported": [], "driftRepaired": ["api"],
            "reapplied": {"by": "Scenario", "at": "2026-09-24T10:00:00Z", "reason": "why", "units": ["db"]},
            "leaseBroken": {"by": "Scenario", "at": "2026-09-24T10:00:00Z", "reason": "runner lost"}}"#,
    );
    let summaries = [summary];
    let check = |text: &str| check_results(&expect(text), &report, Some(&summaries));
    assert_eq!(
        check("driftRepaired: [api]\nreapplied: [db]\nlease-broken: true\nrecovered: []\n"),
        Ok(())
    );
    assert!(check("driftRepaired: []\n")
        .unwrap_err()
        .contains("driftRepaired: the transition commit summary lists"));
    assert!(check("reapplied: [api]\n")
        .unwrap_err()
        .contains("reapplied: the transition commit summary lists"));
    assert!(check("lease-broken: false\n")
        .unwrap_err()
        .contains("records a lease break"));

    // Without a commit, there is no break and no list to read.
    let none: [serde_json::Value; 0] = [];
    assert_eq!(
        check_results(&expect("lease-broken: false\n"), &report, Some(&none)),
        Ok(())
    );
    assert!(check_results(&expect("driftRepaired: [api]\n"), &report, Some(&none))
        .unwrap_err()
        .contains("wrote no transition commit"));
    // A `--local` run's commit stays in its clone: only the report is read.
    assert_eq!(check_results(&expect("driftRepaired: [api]\n"), &report, None), Ok(()));
    // A recover reports its lists in `details`.
    let recover = json(r#"{"kind": "StateOperationReport", "details": {"imported": [], "recovered": ["web"]}}"#);
    let recover_summary = [json(r#"{"units": {}, "recovered": ["web"], "imported": []}"#)];
    assert_eq!(
        check_results(
            &expect("recovered: [web]\nimported: []\n"),
            &recover,
            Some(&recover_summary)
        ),
        Ok(())
    );
}
