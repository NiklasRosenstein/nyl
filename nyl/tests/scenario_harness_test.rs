//! Runs the scenario harness over every scenario file: the format, the test
//! world, and the accounting of scenarios the harness has reached.

mod scenario;

use scenario::format::{PublicationDrivers, StepKind};
use scenario::world::World;
use scenario::{PENDING, REACHED};

/// Run `run` over every scenario on a bounded number of threads and return the
/// failures in scenario order.
///
/// Each scenario builds a test world with its own Git repositories and child
/// processes, and the tier tests run concurrently with each other, so one
/// thread per scenario exhausts a CI runner's memory. `NYL_SCENARIO_JOBS`
/// overrides the bound.
fn run_bounded<'a, T: Sync>(scenarios: &'a [T], run: impl Fn(&'a T) -> Result<(), String> + Sync) -> Vec<String> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    let jobs = std::env::var("NYL_SCENARIO_JOBS")
        .ok()
        .and_then(|jobs| jobs.parse::<usize>().ok())
        .filter(|jobs| *jobs > 0)
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|cores| (cores.get() / 2).max(1))
                .unwrap_or(2)
        })
        .min(scenarios.len().max(1));
    let next = AtomicUsize::new(0);
    let results = Mutex::new(vec![None; scenarios.len()]);
    std::thread::scope(|scope| {
        for _ in 0..jobs {
            scope.spawn(|| loop {
                let index = next.fetch_add(1, Ordering::Relaxed);
                let Some(scenario) = scenarios.get(index) else { break };
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(scenario)))
                    .unwrap_or_else(|_| Err("the scenario panicked".to_owned()));
                results.lock().unwrap_or_else(|poisoned| poisoned.into_inner())[index] = outcome.err();
            });
        }
    });
    results
        .into_inner()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .into_iter()
        .flatten()
        .collect()
}

/// Every reached scenario that is not pending runs in tier 1 and passes its
/// checks.
#[test]
fn test_reached_scenarios_pass_in_tier_one() {
    let (loaded, problems) = scenario::load_all();
    assert!(problems.is_empty(), "invalid scenarios:\n{}", problems.join("\n"));
    let runnable = scenario::selected(&loaded)
        .into_iter()
        .filter(|loaded| loaded.scenario.tiers.contains(&1))
        .filter(|loaded| loaded.scenario.publication != PublicationDrivers::Real)
        .collect::<Vec<_>>();
    assert!(!runnable.is_empty(), "no scenario runs in tier 1");
    let failures = run_bounded(&runnable, |loaded| scenario::runner::run(loaded));
    assert!(failures.is_empty(), "failed scenarios:\n{}", failures.join("\n\n"));
}

/// Enabling tier 2 runs every reached two-tier scenario through the real
/// executable. A failed process or unmet assertion fails instead of skipping.
#[cfg(feature = "tier2")]
#[test]
fn test_reached_scenarios_pass_in_tier_two() {
    let (loaded, problems) = scenario::load_all();
    assert!(problems.is_empty(), "invalid scenarios:\n{}", problems.join("\n"));
    let runnable = scenario::selected(&loaded)
        .into_iter()
        .filter(|loaded| loaded.scenario.tiers.contains(&2))
        .collect::<Vec<_>>();
    assert!(!runnable.is_empty(), "no scenario runs in tier 2");
    let failures = run_bounded(&runnable, |loaded| scenario::runner::run_binary(loaded));
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
    // Every reached scenario runs in tier 1 (test_reached_scenarios_pass_in_tier_one)
    // or is pending; a pending one must exist and be reached.
    for loaded in loaded.iter().filter(|loaded| loaded.scenario.milestone <= REACHED) {
        assert!(
            PENDING.iter().any(|(name, _)| *name == loaded.name) || loaded.scenario.tiers.contains(&1),
            "{} is reached at {REACHED} but neither runs in tier 1 nor is listed as pending",
            loaded.name
        );
    }
    scenario::validate_pending(&loaded, REACHED, &PENDING).unwrap();
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
    for loaded in scenario::selected(&loaded) {
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
                StepKind::Merge => world.merge(step.merge.as_ref().unwrap()),
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

/// A `ref` step either deletes a ref or sets a pull request ref to a branch's
/// tip, never both and never half of the second. Catches a scenario whose
/// host ref silently does nothing.
#[test]
fn test_validate_ref_steps() {
    let step = |reference: &str| format!("  - ref: {reference}\n");
    assert!(
        validation_error(&step("{delete: refs/nyl/dev/lease, set: refs/pull/1/head, to: pr-1}"))
            .contains("exactly one of delete and set with to")
    );
    assert!(validation_error(&step("{set: refs/pull/1/head}")).contains("set and to together"));
    assert!(validation_error(&step("{to: pr-1}")).contains("set and to together"));
    assert!(validation_error(&step("{}")).contains("exactly one of delete and set with to"));
}

/// Observe steps script observed values: they run from M5, are succeeded,
/// pending, or failed, and a failed read names no failure category because
/// it does not fail the unit.
#[test]
fn test_validate_observe_driver_steps() {
    let at_m5 = |driver: &str| {
        let text = format!(
            "contract: design/orchestration-core.md#observed-values\nmilestone: M5\ntiers: [1]\nproject: ../_project\nsteps:\n  - driver: {driver}\n"
        );
        let loaded = inline_scenario("inline/observe", &text);
        scenario::format::validate(&loaded.scenario, &loaded.directory, &scenario::repository_root())
    };
    assert_eq!(
        at_m5("{unit: kubernetes, operation: observe, outcome: pending}"),
        Ok(())
    );
    assert_eq!(at_m5("{unit: kubernetes, operation: observe, outcome: failed}"), Ok(()));
    let categorized =
        at_m5("{unit: kubernetes, operation: observe, outcome: failed, category: credentials, retryable: true}");
    assert!(categorized.unwrap_err().contains("other than observe's"));
    let drifted = at_m5("{unit: kubernetes, operation: observe, outcome: drifted}");
    assert!(drifted.unwrap_err().contains("succeeded, pending, or failed"));
    let pending = validation_error("  - driver: {unit: network, operation: detect-drift, outcome: pending}\n");
    assert!(pending.contains("only an observe outcome"), "{pending}");
    let early = validation_error("  - driver: {unit: kubernetes, operation: observe, outcome: pending}\n");
    assert!(early.contains("runs from M5"), "{early}");
}

/// M6 step kinds, the verify-artifact operation, and argument placeholders
/// run from M6, and each names only what an earlier step defines. Catches an
/// M6 scenario whose placeholder or Argo CD reading could never resolve,
/// found only once the harness reaches M6.
#[test]
fn test_validate_m6_steps_and_placeholders() {
    use scenario::format::{placeholders, Placeholder};
    let at_m6 = |steps: &str| {
        let text = format!(
            "contract: design/promotion.md#promotion-paths\nmilestone: M6\ntiers: [1]\npublication: fake\nproject: ../_project\nsteps:\n{steps}"
        );
        let loaded = inline_scenario("inline/m6", &text);
        scenario::format::validate(&loaded.scenario, &loaded.directory, &scenario::repository_root())
    };
    let reconcile = "  - nyl: [reconcile, -e, dev]\n    id: r1\n    exit: 0\n";
    assert_eq!(
        at_m6(&format!(
            "{reconcile}  - nyl: [promote, p, --state-revision, '%STATE(r1)%', --revision, '%COMMIT(main~1)%']\n    exit: 0\n  - argocd: {{target: dev, all: {{runs: r1}}, releases: {{web/web: {{runs: unmatched, health: Degraded}}}}}}\n  - merge: {{branch: main, into: main}}\n"
        )),
        Ok(())
    );
    let early = at_m6("  - nyl: [promote, p, --state-revision, '%STATE(r1)%']\n    exit: 0\n").unwrap_err();
    assert!(early.contains("no earlier nyl step has that id"), "{early}");
    let unknown = at_m6("  - nyl: [get, x, '%NOW(1)%']\n    exit: 0\n").unwrap_err();
    assert!(unknown.contains("unknown placeholder"), "{unknown}");
    let reading = at_m6("  - argocd: {target: dev, all: {runs: r9}}\n").unwrap_err();
    assert!(reading.contains("no earlier nyl step has that id"), "{reading}");
    let release = at_m6(&format!(
        "{reconcile}  - argocd: {{target: dev, all: {{runs: r1}}, releases: {{web: {{runs: r1}}}}}}\n"
    ))
    .unwrap_err();
    assert!(release.contains("<group>/<release>"), "{release}");
    let verify = at_m6("  - driver: {unit: web-image, operation: verify-artifact, outcome: uncertain}\n").unwrap_err();
    assert!(verify.contains("succeeded or failed"), "{verify}");
    assert_eq!(
        at_m6("  - driver: {unit: web-image, operation: verify-artifact, outcome: failed}\n"),
        Ok(())
    );
    // The promotion expectation's commits, requester, and ignored
    // attestations; placeholders in them and in a get value are validated.
    let promote = |expect: &str| {
        at_m6(&format!(
            "{reconcile}  - nyl: [promote, p]\n    exit: 0\n    expect:\n      promotion: {{path: p, from: {{run: r1, desiredCommit: '%STATE(r1)%', observedCommit: '%OBSERVED(r1)%'}}, {expect}}}\n"
        ))
    };
    assert_eq!(
        promote("requested: {reason: Release 1}, ignoredAttestations: [{attestation: environment/qa, reason: flaky}]"),
        Ok(())
    );
    let ignored = promote("ignoredAttestations: [{attestation: qa, reason: flaky}]").unwrap_err();
    assert!(ignored.contains("environment/<name>"), "{ignored}");
    let observed = at_m6("  - nyl: [promote, p]\n    exit: 0\n    expect:\n      promotion: {path: p, from: {run: r0, observedCommit: '%OBSERVED(r9)%'}}\n    id: r0\n").unwrap_err();
    assert!(observed.contains("Observed(\"r9\")"), "{observed}");
    let value =
        at_m6("  - nyl: [get, x]\n    exit: 0\n    expect:\n      get: {args: [get, x], value: '%STATE(r9)%'}\n")
            .unwrap_err();
    assert!(value.contains("no earlier nyl step has that id"), "{value}");
    // Only the test publication reads the scripted Argo CD.
    let both = scenario::format::validate(
        &inline_scenario(
            "inline/argocd-both",
            &format!("contract: design/promotion.md#promotion-paths\nmilestone: M6\ntiers: [1]\nproject: ../_project\nsteps:\n{reconcile}  - argocd: {{target: dev, all: {{runs: r1}}}}\n"),
        )
        .scenario,
        &scenario::scenarios_root().join("walkthroughs/dependency-wave"),
        &scenario::repository_root(),
    )
    .unwrap_err();
    assert!(both.contains("publication: fake"), "{both}");
    let state = validation_error(&format!("{reconcile}  - nyl: [get, x, '%STATE(r1)%']\n    exit: 0\n"));
    assert!(state.contains("placeholders run from M6"), "{state}");
    let merge = validation_error("  - merge: {branch: a, into: main}\n");
    assert!(merge.contains("Merge runs from M6"), "{merge}");
    let argocd = validation_error("  - argocd: {target: dev, all: {runs: unmatched}}\n");
    assert!(argocd.contains("Argocd runs from M6"), "{argocd}");
    assert_eq!(
        placeholders("%COMMIT(s1)%..%COMMIT(release/prod)%=reason"),
        Ok(vec![
            Placeholder::Commit("s1".to_owned()),
            Placeholder::Commit("release/prod".to_owned())
        ])
    );
    assert_eq!(
        placeholders("database=%PLAN-DIGEST(prod/database)%"),
        Ok(vec![Placeholder::PlanDigest {
            environment: "prod".to_owned(),
            unit: "database".to_owned()
        }])
    );
    assert_eq!(placeholders("100% sure, %SCENARIO_TMP%"), Ok(vec![]));
    assert!(placeholders("%STATE(r1").unwrap_err().contains("not closed"));
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

/// `argocd` steps feed the fake Argo CD, and the fake publication attests
/// what they report through the real driver's matching: a publication
/// running everywhere passes; an Application left on the previous
/// publication fails `accepted` as `other-publication`; one running a
/// revision edited outside Nyl is `unmatched`; and one that is `Degraded`
/// fails only `healthy`. Catches a fake that judges readings by rules of its
/// own, or a harness that resolves `runs` to the wrong publication commit.
#[test]
fn test_argocd_steps_attest_through_the_shared_matching() {
    run_inline(
        "walkthroughs/value-only-promotion",
        "inline/fake-argocd",
        r#"contract: design/promotion.md#kubernetes-publications
milestone: M6
tiers: [1]
publication: fake
steps:
  - nyl: [state, init, -e, dev]
    exit: 0
  - nyl: [reconcile, -e, dev]
    id: r1
    exit: 0
  # Nothing scripted: the attestation is unknown, never a failure.
  - nyl: [attest, -e, dev]
    exit: 2
    expect:
      codes: [NYL-ATTEST-UNKNOWN]
      commits: {desired: 0, observed: 0}
  - argocd: {target: dev, all: {runs: r1}}
  - nyl: [attest, -e, dev]
    exit: 0
    expect:
      commits: {desired: 0, observed: 1}
  - commit: {branch: main, message: "Release 2", apply: patches/release-2}
  - nyl: [reconcile, -e, dev]
    id: r2
    exit: 0
  - argocd: {target: dev, all: {runs: r2}, releases: {web/api: {runs: r1}}}
  - nyl: [attest, -e, dev]
    exit: 3
    expect:
      codes: [NYL-ATTESTATION-FAILED]
      messages: [other-publication]
      commits: {desired: 0, observed: 1}
  - argocd: {target: dev, all: {runs: r2}, releases: {web/api: {runs: unmatched}}}
  - nyl: [attest, -e, dev]
    exit: 3
    expect:
      codes: [NYL-ATTESTATION-FAILED]
      messages: [unmatched]
  - argocd: {target: dev, all: {runs: r2}, releases: {web/web: {runs: r2, health: Degraded}}}
  - nyl: [attest, -e, dev]
    exit: 3
    expect:
      codes: [NYL-ATTESTATION-FAILED]
      messages: [Degraded]
  - argocd: {target: dev, all: {runs: r2}}
  - nyl: [attest, -e, dev]
    exit: 0
    expect:
      commits: {desired: 0, observed: 1}
"#,
    )
    .unwrap();
}

/// An `argocd` step naming a Release the target has no Application for
/// fails the scenario, instead of scripting a reading nothing reads.
#[test]
fn test_argocd_step_naming_no_release_of_the_target_fails() {
    let error = run_inline(
        "walkthroughs/value-only-promotion",
        "inline/fake-argocd-unknown-release",
        r#"contract: design/promotion.md#kubernetes-publications
milestone: M6
tiers: [1]
publication: fake
steps:
  - nyl: [state, init, -e, dev]
    exit: 0
  - nyl: [reconcile, -e, dev]
    id: r1
    exit: 0
  - argocd: {target: dev, all: {runs: r1}, releases: {web/nope: {runs: unmatched}}}
"#,
    )
    .unwrap_err();
    assert!(error.contains("no Application for Release web/nope"), "{error}");
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

/// A drift result scripts only `detect-drift`; a file step stays inside
/// `%SCENARIO_TMP%`; and `nyl` arguments must parse with the real command
/// line, so the boolean `--reapply` given a unit is caught up front.
#[test]
fn test_validate_part_five_steps_and_options() {
    let drifted = validation_error("  - driver: {unit: network, operation: reconcile, outcome: drifted}\n");
    assert!(
        drifted.contains("only a detect-drift outcome can be drifted"),
        "{drifted}"
    );
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
            "  - driver: {unit: network, operation: detect-drift, outcome: drifted}\n",
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

/// `drift-checks` names exactly the units whose report entry and transition
/// summary carry a `driftCheck`: a run whose commit leaves one out, or one
/// that checked a unit the step did not expect, fails the step.
#[test]
fn test_drift_checks_compare_report_and_summary() {
    use scenario::runner::check_results;
    let report = json(
        r#"{"environments": [{"units": {
            "api": {"result": "executed", "driftCheck": {"check": "drifted", "summary": "1 change"}},
            "job": {"result": "current", "driftCheck": {"check": "clean"}},
            "web": {"result": "current"}}}]}"#,
    );
    let summaries = [json(
        r#"{"units": {"api": {"result": "executed", "driftCheck": {"check": "drifted"}},
            "job": {"result": "current", "driftCheck": {"check": "clean"}}, "web": {"result": "current"}}}"#,
    )];
    let check = |text: &str, summaries: Option<&[serde_json::Value]>| check_results(&expect(text), &report, summaries);
    assert_eq!(
        check("drift-checks: {api: drifted, job: clean}\n", Some(&summaries)),
        Ok(())
    );
    let error = check("drift-checks: {api: clean, job: clean}\n", Some(&summaries)).unwrap_err();
    assert!(error.contains("drift-checks: the report has"), "{error}");
    let error = check("drift-checks: {api: drifted}\n", Some(&summaries)).unwrap_err();
    assert!(error.contains("drift-checks: the report has"), "{error}");
    // The report is right, the commit left a check out.
    let short = [json(
        r#"{"units": {"api": {"result": "executed", "driftCheck": {"check": "drifted"}}}}"#,
    )];
    let error = check("drift-checks: {api: drifted, job: clean}\n", Some(&short)).unwrap_err();
    assert!(
        error.contains("drift-checks: the transition commit summary has"),
        "{error}"
    );
    // A run that wrote no commit satisfies only an empty expectation; a `--local` run's commit is not read.
    let none: [serde_json::Value; 0] = [];
    assert!(check("drift-checks: {api: drifted, job: clean}\n", Some(&none)).is_err());
    assert_eq!(check("drift-checks: {api: drifted, job: clean}\n", None), Ok(()));
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

/// Tier 1 keeps the test kinds, which scenarios script, but every scenario
/// also holds the real `KubernetesPublication` driver to its contract: every
/// walkthrough and reference scenario passes the same checks with images and
/// infrastructure faked and publication real. The real driver's live reads
/// (Argo CD and the workload Cluster) are scripted like the fakes are, so a
/// scenario that observes values runs against it too.
#[test]
fn test_reached_scenarios_pass_in_tier_one_with_the_real_publication_driver() {
    let (loaded, problems) = scenario::load_all();
    assert!(problems.is_empty(), "invalid scenarios:\n{}", problems.join("\n"));
    let runnable = scenario::selected(&loaded)
        .into_iter()
        .filter(|loaded| loaded.scenario.tiers.contains(&1))
        .filter(|loaded| loaded.scenario.publication != PublicationDrivers::Fake)
        .collect::<Vec<_>>();
    assert!(
        runnable.iter().any(|loaded| loaded.name == "reference/platform"),
        "the reference platform runs against the real driver"
    );
    let failures = run_bounded(&runnable, |loaded| scenario::runner::run_real_publication(loaded));
    assert!(failures.is_empty(), "failed scenarios:\n{}", failures.join("\n\n"));
}

/// `deploy.mentions` reads the published files: a literal text and the value
/// of a `nyl get` invocation must each appear in a matching file. Catches an
/// exit-criterion check that passes without the manifests carrying the image
/// digest and the infrastructure output.
#[test]
fn test_deploy_mentions_fail_when_no_published_file_carries_the_value() {
    let run = |mention: &str| {
        let text = format!(
            "contract: design/reference-scenarios.md#scenario-1-platform-environment\nmilestone: M5\ntiers: [1]\nsteps:\n  - nyl: [state, init, -e, dev]\n    exit: 0\n  - nyl: [reconcile, -e, dev]\n    exit: 0\n    expect:\n      deploy:\n        branch: deploy\n        mentions:\n          'dev/web/**': [{mention}]\n"
        );
        run_inline("reference/platform", "inline/mentions", &text)
    };
    run("[get, output, database/connection, -e, dev, --pointer, /host]").unwrap();
    run("[get, artifact, web-image/image, -e, dev, --pointer, /reference]").unwrap();
    let missing = run("'a text no manifest has'").unwrap_err();
    assert!(missing.contains("mentions"), "{missing}");
    let other = run("[get, output, network/vpcId, -e, dev]").unwrap_err();
    assert!(other.contains("mentions"), "{other}");
}

/// `keep-refs` counts the refs of `refs/nyl-keep/` and checks that the named
/// revisions have one, and `source` compares the commit a run rendered. Catches
/// a close-job scenario that passes with a keep ref missing or a stale source.
#[test]
fn test_keep_refs_and_source_expectations_compare_with_the_repository() {
    let run = |expect: &str| {
        let text = format!(
            "contract: design/reference-scenarios.md#scenario-2-preview-closed-with-its-pull-request\nmilestone: M5\ntiers: [1]\nproject: ../preview/project\nsteps:\n  - nyl: [state, init, -e, dev]\n    exit: 0\n  - nyl: [reconcile, -e, dev]\n    exit: 0\n  - branch: {{name: pr-1, from: main}}\n  - commit: {{branch: pr-1, message: change, apply: ../preview/patches/web}}\n  - nyl: [reconcile, -e, pr-1, --template, preview, --param, pr=1]\n    at: pr-1\n    exit: 0\n    expect:\n{expect}"
        );
        run_inline("reference/preview-closed", "inline/keep-refs", &text)
    };
    run("      keep-refs: {count: 1, kept: [pr-1]}\n      source: pr-1\n").unwrap();
    let count = run("      keep-refs: {count: 2}\n").unwrap_err();
    assert!(count.contains("1 keep refs instead of 2"), "{count}");
    let kept = run("      keep-refs: {count: 1, kept: [main]}\n").unwrap_err();
    assert!(kept.contains("no keep ref for main"), "{kept}");
    let source = run("      source: main\n").unwrap_err();
    assert!(source.contains("the run's source is"), "{source}");
}

/// A scenario that depends on what the built-in publication driver derives
/// from its render says so, and tier 1 then skips the test kind for it.
#[test]
fn test_publication_drivers_default_to_both_and_real_narrows_them() {
    let (loaded, problems) = scenario::load_all();
    assert!(problems.is_empty(), "invalid scenarios:\n{}", problems.join("\n"));
    let real_only = loaded
        .iter()
        .filter(|loaded| loaded.scenario.publication == PublicationDrivers::Real)
        .map(|loaded| loaded.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(real_only, ["reference/preview-not-ready"]);
    let fake_only = loaded
        .iter()
        .filter(|loaded| loaded.scenario.publication == PublicationDrivers::Fake)
        .map(|loaded| loaded.name.as_str())
        .collect::<Vec<_>>();
    // health-wait scripts the publication's outcome; value-only-promotion
    // scripts the Argo CD that only the fake publication reads in tier 1.
    assert_eq!(
        fake_only,
        ["walkthroughs/health-wait", "walkthroughs/value-only-promotion"]
    );
    let bad = "contract: design/reference-scenarios.md#tiers\nmilestone: M3\npublication: neither\nsteps: []\n";
    assert!(serde_saphyr::from_str::<scenario::format::Scenario>(bad).is_err());
}

/// One selection function decides what every runner runs: a pending scenario
/// is in none of them, whatever its tiers and publication drivers, and every
/// other reached scenario is in all that its tiers and drivers admit. Catches a
/// runner that filters on the milestone alone and so runs a scenario that
/// cannot pass yet.
#[test]
fn test_a_pending_scenario_runs_in_no_runner() {
    let (loaded, problems) = scenario::load_all();
    assert!(problems.is_empty(), "invalid scenarios:\n{}", problems.join("\n"));
    let selected = scenario::selected(&loaded)
        .into_iter()
        .map(|loaded| loaded.name.as_str())
        .collect::<Vec<_>>();
    for (name, _) in PENDING {
        assert!(!selected.contains(&name), "{name} is pending yet selected to run");
    }
    // Every reached scenario runs unless it is listed.
    for loaded in &loaded {
        let pending = PENDING.iter().any(|(name, _)| *name == loaded.name);
        assert_eq!(
            selected.contains(&loaded.name.as_str()),
            loaded.scenario.milestone <= REACHED && !pending,
            "{}",
            loaded.name
        );
    }
    assert!(selected.contains(&"reference/platform"));

    // The same holds for a synthetic list over the real scenarios, where
    // a reached scenario is made pending.
    let pending = [("reference/platform", "synthetic")];
    let runs = scenario::runnable(&loaded, REACHED, &pending);
    assert!(runs.iter().all(|loaded| loaded.name != "reference/platform"));
    assert!(runs.iter().any(|loaded| loaded.name == "walkthroughs/drift"));
    // A lower REACHED excludes the M6 scenarios without any list.
    let lower = scenario::runnable(&loaded, scenario::format::Milestone(5), &[]);
    assert!(lower.iter().all(|loaded| loaded.scenario.milestone.0 <= 5));
    assert!(lower.len() < scenario::runnable(&loaded, REACHED, &[]).len());
}

/// The pending list cannot rot: a name that is no scenario, or a scenario
/// above the reached milestone, is an error that names the entry.
#[test]
fn test_pending_entries_must_name_reached_scenarios() {
    let (loaded, _) = scenario::load_all();
    scenario::validate_pending(&loaded, REACHED, &[("reference/platform", "ok")]).unwrap();
    let missing = scenario::validate_pending(&loaded, REACHED, &[("reference/renamed-away", "gone")]).unwrap_err();
    assert!(
        missing.contains("reference/renamed-away") && missing.contains("no such scenario"),
        "{missing}"
    );
    let above = scenario::validate_pending(
        &loaded,
        scenario::format::Milestone(5),
        &[("reference/promotion", "too early"), ("reference/platform", "fine")],
    )
    .unwrap_err();
    assert!(
        above.contains("reference/promotion") && above.contains("above M5"),
        "{above}"
    );
    assert!(!above.contains("reference/platform"), "{above}");
}

// ---------------------------------------------------------------------------
// Argument placeholders
// ---------------------------------------------------------------------------

fn resolve_with(text: &str) -> Result<String, String> {
    use scenario::runner::{expand_placeholders, Resolver};
    let state = |id: &str| match id {
        "r1" => Ok("d1".to_owned()),
        "obs-only" => Ok("o2".to_owned()),
        other => Err(format!("no nyl step with id {other} has run")),
    };
    let observed = |id: &str| match id {
        "r1" => Ok("o1".to_owned()),
        other => Err(format!("the step {other} wrote no observed-state commit")),
    };
    let commit = |revision: &str| match revision {
        "main" => Ok("c0ffee".to_owned()),
        "main~1" => Ok("beef".to_owned()),
        other => Err(format!("{other:?} names no commit of project.git")),
    };
    let plan_digest = |environment: &str, unit: &str| Ok(format!("sha256:{environment}-{unit}"));
    expand_placeholders(
        text,
        &Resolver {
            state: &state,
            observed: &observed,
            commit: &commit,
            plan_digest: &plan_digest,
        },
    )
}

/// Each placeholder is replaced by its resolver's value wherever it appears in
/// an argument, several to one argument, and a failed lookup names the
/// placeholder that failed. Catches a placeholder passed to `nyl` verbatim.
#[test]
fn test_placeholders_are_replaced_by_their_resolvers() {
    assert_eq!(
        resolve_with("--state-revision=%STATE(r1)%").unwrap(),
        "--state-revision=d1"
    );
    assert_eq!(resolve_with("%OBSERVED(r1)%").unwrap(), "o1");
    assert_eq!(resolve_with("%COMMIT(main~1)%").unwrap(), "beef");
    assert_eq!(
        resolve_with("database=%PLAN-DIGEST(prod/database)%").unwrap(),
        "database=sha256:prod-database"
    );
    assert_eq!(
        resolve_with("%COMMIT(main)%..%COMMIT(main)% %STATE(r1)%/%OBSERVED(r1)%").unwrap(),
        "c0ffee..c0ffee d1/o1"
    );
    assert_eq!(
        resolve_with("no placeholder, 100% sure, %PROJECT_URL%").unwrap(),
        "no placeholder, 100% sure, %PROJECT_URL%"
    );
    let unknown = resolve_with("%STATE(nope)%").unwrap_err();
    assert!(
        unknown.contains("%STATE(nope)%") && unknown.contains("no nyl step with id nope"),
        "{unknown}"
    );
    let unobserved = resolve_with("x %OBSERVED(obs-only)%").unwrap_err();
    assert!(unobserved.contains("wrote no observed-state commit"), "{unobserved}");
    let revision = resolve_with("%COMMIT(nowhere)%").unwrap_err();
    assert!(revision.contains("names no commit"), "{revision}");
    assert!(resolve_with("%BOGUS(x)%").unwrap_err().contains("unknown placeholder"));
}

/// The runner resolves placeholders against the real run: `%STATE%` and
/// `%OBSERVED%` are the commits an earlier step wrote (`get --state-revision`
/// accepts only a real state commit), `%COMMIT%` a revision of `project.git`
/// resolved when the step runs, and `%PLAN-DIGEST%` the digest `plan` reports.
#[test]
fn test_the_runner_resolves_placeholders_against_the_run() {
    let run = |steps: &str| {
        let text = format!(
            "contract: design/reference-scenarios.md#scenario-1-platform-environment\nmilestone: M6\ntiers: [1]\nsteps:\n  - branch: {{name: deploy, from: main}}\n  - nyl: [state, init, -e, dev]\n    exit: 0\n{steps}"
        );
        run_inline("reference/platform", "inline/placeholders", &text)
    };
    run(
        "  - nyl: [plan, -e, dev, --unit, network]\n    exit: 0\n    expect:\n      get:\n        args: [plan, -e, dev, --unit, network, -o, json]\n        json:\n          /environments/0/units/network/changeDigest: '%PLAN-DIGEST(dev/network)%'\n  - nyl: [reconcile, -e, dev]\n    id: r1\n    exit: 0\n  - nyl: [get, units, -e, dev, --state-revision, '%STATE(r1)%']\n    exit: 0\n  - nyl: [get, units, -e, dev, --state-revision, '%OBSERVED(r1)%']\n    exit: 0\n",
    )
    .unwrap();
    // A literal placeholder is no state commit, so it is the substitution that makes these pass.
    let wrong = run(
        "  - nyl: [reconcile, -e, dev]\n    id: r1\n    exit: 0\n  - nyl: [get, units, -e, dev, --state-revision, '%COMMIT(main)%']\n    exit: 0\n",
    )
    .unwrap_err();
    assert!(
        wrong.contains("exit 1 instead of 0") && wrong.contains("is not a commit of the environment"),
        "{wrong}"
    );
    let missing =
        run("  - nyl: [get, units, -e, dev, --state-revision, '%COMMIT(no-such-branch)%']\n    exit: 1\n").unwrap_err();
    assert!(
        missing.contains("%COMMIT(no-such-branch)%") && missing.contains("names no commit"),
        "{missing}"
    );
    // A step that failed wrote no state commit for a later step to name.
    let unwritten = run(
        "  - nyl: [reconcile, -e, nope]\n    id: r1\n    exit: 1\n  - nyl: [get, units, -e, dev, --state-revision, '%STATE(r1)%']\n    exit: 1\n",
    )
    .unwrap_err();
    assert!(unwritten.contains("wrote no state commit"), "{unwritten}");
}

// ---------------------------------------------------------------------------
// The `promotion` expectation
// ---------------------------------------------------------------------------

const PROMOTION_EXPECTATION: &str = r#"
path: dev-to-prod
sequence: 3
sourceCommit: main
from: {environment: dev, run: r1, desiredCommit: '%STATE(r1)%', observedCommit: '%OBSERVED(r1)%'}
requested: {reason: Release 1}
values:
  webImage:
    equals: [get, artifact, web-image/image, -e, dev, --pointer, /reference]
  tag:
    equals: v1
    state: r2
evidence: {level: attested, override: false, attestations: [environment/qa, web/accepted]}
superseded:
  - {commit: main~1, reason: Fix merged}
artifactsVerified: true
ignoredAttestations:
  - {attestation: environment/qa, reason: Behind a flag}
excludedApplications: [web-batch]
"#;

fn promotion_record() -> serde_json::Value {
    serde_json::json!({
        "path": "dev-to-prod",
        "sequence": 3,
        "sourceCommit": "c0ffee",
        "from": {"environment": "dev", "run": "run-1", "desiredCommit": "d1", "observedCommit": "o1"},
        "values": {
            "webImage": {"value": "registry/web@sha256:aaa", "select": {"unit": "web-image"}},
            "tag": {"value": "v1", "state": {"run": "run-2", "desiredCommit": "d2"}},
        },
        "evidence": {
            "level": "attested",
            "attestations": [{"scope": "environment", "name": "qa"}, {"scope": "web", "name": "accepted"}],
            "override": null,
        },
        "ignoredAttestations": [{"scope": "environment", "name": "qa", "reason": "Behind a flag"}],
        "excludedApplications": ["web-batch"],
        "superseded": [{"commit": "beef", "reason": "Fix merged"}],
        "artifactsVerified": true,
        "requested": {"by": "someone", "reason": "Release 1"},
    })
}

fn check_promotion(expectation: &str, record: &serde_json::Value, path_from: Option<&str>) -> Result<(), String> {
    use scenario::runner::{check_promotion_record, expand_placeholders, PromotionLookups, Resolver};
    let expected: scenario::format::PromotionExpectation = serde_saphyr::from_str(expectation).unwrap();
    let run_of = |id: &str| match id {
        "r1" => Ok("run-1".to_owned()),
        "r2" => Ok("run-2".to_owned()),
        other => Err(format!("no step {other}")),
    };
    let get = |args: &[String]| {
        assert_eq!(args[0], "get");
        // The value prints, as `nyl get` does for a string.
        Ok((serde_json::Value::Null, "registry/web@sha256:aaa".to_owned()))
    };
    let state = |id: &str| Ok(format!("d{}", &id[1..]));
    let observed = |id: &str| Ok(format!("o{}", &id[1..]));
    let commit = |revision: &str| match revision {
        "main" => Ok("c0ffee".to_owned()),
        "main~1" => Ok("beef".to_owned()),
        other => Err(format!("{other:?} names no commit")),
    };
    let plan = |_: &str, _: &str| Err("no plan".to_owned());
    let resolver = Resolver {
        state: &state,
        observed: &observed,
        commit: &commit,
        plan_digest: &plan,
    };
    let expand = |text: &str| expand_placeholders(text, &resolver);
    check_promotion_record(
        &expected,
        record,
        &PromotionLookups {
            path_from,
            run_of: &run_of,
            get: &get,
            expand: &expand,
            commit: &commit,
        },
    )
}

fn changed(pointer: &str, value: serde_json::Value) -> serde_json::Value {
    let mut record = promotion_record();
    let (parent, key) = pointer.rsplit_once('/').unwrap();
    let parent = record.pointer_mut(parent).unwrap();
    match parent {
        serde_json::Value::Array(items) => items[key.parse::<usize>().unwrap()] = value,
        other => {
            other.as_object_mut().unwrap().insert(key.to_owned(), value);
        }
    }
    record
}

/// Every field of the `promotion` expectation is compared with the record,
/// including `from.observedCommit`, `requested.reason`, a value's own state,
/// the evidence's override and attestations, and the ignored ones. One
/// differing field fails and names that field.
#[test]
fn test_promotion_expectation_checks_every_field_of_the_record() {
    use serde_json::json;
    check_promotion(PROMOTION_EXPECTATION, &promotion_record(), None).unwrap();
    // The state file carries its markers beside the flat fields; a record nested
    // under `spec` is not the shape Nyl writes, so the scenarios pin the flat one.
    let mut document = promotion_record();
    document["apiVersion"] = json!("gitops.nyl/v1");
    document["kind"] = json!("PromotionRecord");
    check_promotion(PROMOTION_EXPECTATION, &document, None).unwrap();
    let nested = json!({"apiVersion": "gitops.nyl/v1", "kind": "PromotionRecord", "spec": promotion_record()});
    let error = check_promotion(PROMOTION_EXPECTATION, &nested, None).unwrap_err();
    assert!(error.contains("path is"), "{error}");

    let cases = [
        ("/path", json!("other"), "path is"),
        ("/sequence", json!(4), "sequence is 4 instead of 3"),
        ("/sourceCommit", json!("deadbeef"), "sourceCommit"),
        ("/from/environment", json!("stage"), "from.environment"),
        ("/from/run", json!("run-9"), "from.run"),
        ("/from/desiredCommit", json!("d9"), "from.desiredCommit"),
        ("/from/observedCommit", json!("o9"), "from.observedCommit"),
        (
            "/values/webImage/value",
            json!("registry/web@sha256:bbb"),
            "values.webImage",
        ),
        ("/values/tag/value", json!("v2"), "values.tag"),
        ("/values/tag/state/run", json!("run-9"), "values.tag.state.run"),
        (
            "/values/webImage/state",
            json!({"run": "run-2"}),
            "values.webImage names its own state",
        ),
        ("/values/extra", json!({"value": 1}), "values are"),
        ("/evidence/level", json!("published"), "evidence.level"),
        ("/evidence/override", json!({"reason": "late"}), "evidence.override"),
        (
            "/evidence/attestations",
            json!([{"scope": "environment", "name": "qa"}]),
            "evidence.attestations",
        ),
        ("/superseded", json!([]), "superseded"),
        ("/superseded/0/reason", json!("Other"), "superseded"),
        ("/superseded/0/commit", json!("c0ffee"), "superseded"),
        ("/artifactsVerified", json!(false), "artifactsVerified"),
        ("/requested/reason", json!("Release 2"), "requested.reason"),
        ("/ignoredAttestations", json!([]), "ignoredAttestations"),
        ("/ignoredAttestations/0/reason", json!("Because"), "ignoredAttestations"),
        ("/ignoredAttestations/0/name", json!("smoke"), "ignoredAttestations"),
        ("/excludedApplications", json!([]), "excludedApplications"),
        ("/excludedApplications/0", json!("web-api"), "excludedApplications"),
    ];
    for (pointer, value, expected) in cases {
        let error = check_promotion(PROMOTION_EXPECTATION, &changed(pointer, value), None)
            .expect_err(&format!("{pointer} differs but passed"));
        assert!(
            error.contains(expected) && error.contains("dev-to-prod"),
            "{pointer}: {error}"
        );
    }
}

/// A key the expectation leaves out is not checked, `sourceCommit: null` means
/// the record has none, a value taken from `from` names no state, and
/// `from.environment` defaults to the path's source.
#[test]
fn test_promotion_expectation_checks_only_the_keys_it_gives() {
    use serde_json::json;
    let minimal = "path: dev-to-prod\nfrom: {run: r1}\n";
    let mut junk = promotion_record();
    junk["sequence"] = json!(99);
    junk["values"] = json!({"anything": {"value": 1}});
    junk["evidence"]["level"] = json!("published");
    check_promotion(minimal, &junk, None).unwrap();
    // Without `from.environment`, the path's own source is expected.
    check_promotion(minimal, &promotion_record(), Some("dev")).unwrap();
    let error = check_promotion(minimal, &promotion_record(), Some("stage")).unwrap_err();
    assert!(
        error.contains("from.environment is \"dev\" instead of \"stage\""),
        "{error}"
    );
    // The step the expectation names must have run.
    let unknown = check_promotion("path: dev-to-prod\nfrom: {run: nope}\n", &promotion_record(), None).unwrap_err();
    assert!(
        unknown.contains("from.run") && unknown.contains("no step nope"),
        "{unknown}"
    );

    let value_only = "path: values\nfrom: {run: r1}\nsourceCommit: null\n";
    let mut record = promotion_record();
    record["path"] = json!("values");
    record.as_object_mut().unwrap().remove("sourceCommit");
    check_promotion(value_only, &record, None).unwrap();
    record["sourceCommit"] = json!(null);
    check_promotion(value_only, &record, None).unwrap();
    record["sourceCommit"] = json!("c0ffee");
    let error = check_promotion(value_only, &record, None).unwrap_err();
    assert!(error.contains("sourceCommit is \"c0ffee\" instead of null"), "{error}");
    // Empty lists are exact too: nothing superseded, nothing ignored.
    let empty = "path: dev-to-prod\nfrom: {run: r1}\nsuperseded: []\nignoredAttestations: []\nevidence: {level: attested, attestations: []}\n";
    assert!(check_promotion(empty, &promotion_record(), None).is_err());
    let mut bare = promotion_record();
    bare["superseded"] = json!([]);
    bare["ignoredAttestations"] = json!([]);
    bare["evidence"]["attestations"] = json!([]);
    check_promotion(empty, &bare, None).unwrap();
}

// ---------------------------------------------------------------------------
// Counting a `promote` step's commits
// ---------------------------------------------------------------------------

fn platform_documents(scenario: &str) -> Vec<serde_json::Value> {
    let text =
        std::fs::read_to_string(scenario::scenarios_root().join(format!("{scenario}/project/nyl/platform.yaml")))
            .unwrap();
    serde_saphyr::from_multiple(&text).unwrap()
}

fn words(text: &str) -> Vec<String> {
    text.split(' ').map(str::to_owned).collect()
}

/// A `promote` step counts the refs of the environment its path promotes into,
/// read from the PromotionPath; `environment_of` alone would count dev's, which
/// the step never writes, and `commits: {desired: 0, observed: 0}` would prove
/// nothing. Other steps keep counting their `-e`, or dev.
#[test]
fn test_a_promote_step_counts_the_target_environments_refs() {
    use scenario::runner::{counted_environment, promotion_target};
    let documents = platform_documents("reference/promotion");
    let promote = words("promote dev-to-prod --reason Release");
    let path = promotion_target(&documents, &promote);
    let target = path.as_ref().unwrap();
    assert_eq!((target.from.as_deref(), target.to.as_str()), (Some("dev"), "prod"));
    assert_eq!(counted_environment(&promote, Some(&path)).as_deref(), Some("prod"));
    // The path may follow its options.
    let dry = promotion_target(&documents, &words("promote --dry-run dev-to-prod")).unwrap();
    assert_eq!(dry.to, "prod");
    // The hotfix scenario's second path promotes from another environment into prod too.
    let hotfix = promotion_target(
        &platform_documents("reference/hotfix"),
        &words("promote hotfix-to-prod"),
    )
    .unwrap();
    assert_eq!(
        (hotfix.from.as_deref(), hotfix.to.as_str()),
        (Some("prod-hotfix"), "prod")
    );
    // Not a promote step: its `-e`, or dev.
    assert_eq!(
        counted_environment(&words("reconcile -e prod"), None).as_deref(),
        Some("prod")
    );
    assert_eq!(counted_environment(&words("reconcile"), None).as_deref(), Some("dev"));
    // A path that cannot be read counts nothing, and is no silent count of dev's refs.
    let missing = promotion_target(&documents, &words("promote no-such-path"));
    assert!(
        missing.as_ref().unwrap_err().contains("no PromotionPath no-such-path"),
        "{missing:?}"
    );
    assert_eq!(
        counted_environment(&words("promote no-such-path"), Some(&missing)),
        None
    );
    // The first positional argument names the path, not any argument that equals a path's name.
    let reason_is_a_name = promotion_target(&documents, &words("promote --reason dev-to-prod hotfix-nope"));
    assert!(reason_is_a_name.unwrap_err().contains("hotfix-nope"));
    assert_eq!(
        promotion_target(&documents, &words("promote dev-to-prod --reason hotfix-nope"))
            .unwrap()
            .to,
        "prod"
    );
    // record: source is read from the target Environment.
    assert!(!target.record_source);
    let source = "kind: Environment\nmetadata: {name: prod}\nspec: {source: {fromPromotion: {path: dev-to-prod, record: source}}}\n";
    let mut with_source = documents.clone();
    with_source.push(serde_saphyr::from_str(source).unwrap());
    assert!(promotion_target(&with_source, &promote).unwrap().record_source);
}

/// Every state ref but the target's own must be unchanged after a `promote`
/// step: the source environment's branches, other environments', and their
/// leases. A moved, added, or removed ref is reported by name.
#[test]
fn test_a_promote_step_leaves_every_other_state_ref_unchanged() {
    use scenario::runner::{excused_refs, unchanged_violations};
    let refs = |entries: &[(&str, &str)]| {
        entries
            .iter()
            .map(|(name, commit)| ((*name).to_owned(), (*commit).to_owned()))
            .collect::<std::collections::BTreeMap<_, _>>()
    };
    let before = refs(&[
        ("refs/heads/nyl/dev/desired", "a1"),
        ("refs/heads/nyl/dev/observed", "b1"),
        ("refs/heads/nyl/prod/desired", "c1"),
        ("refs/nyl/prod/lease", "l1"),
        ("refs/nyl/dev/lease", "l2"),
        ("refs/heads/nyl/staging/desired", "s1"),
    ]);
    // The target's desired ref moves and its lease is taken and released.
    let promoted = refs(&[
        ("refs/heads/nyl/dev/desired", "a1"),
        ("refs/heads/nyl/dev/observed", "b1"),
        ("refs/heads/nyl/prod/desired", "c2"),
        ("refs/heads/nyl/prod/observed", "c3"),
        ("refs/nyl/dev/lease", "l2"),
        ("refs/heads/nyl/staging/desired", "s1"),
    ]);
    assert_eq!(
        unchanged_violations(&before, &promoted, &excused_refs("prod", None)),
        Vec::<String>::new()
    );
    // The source moved: a promotion that wrote to dev.
    let mut moved = promoted.clone();
    moved.insert("refs/heads/nyl/dev/desired".to_owned(), "a2".to_owned());
    assert_eq!(
        unchanged_violations(&before, &moved, &excused_refs("prod", None)),
        ["refs/heads/nyl/dev/desired"]
    );
    // A ref that appeared, and one that disappeared, outside the target.
    let mut added = promoted.clone();
    added.insert("refs/heads/nyl/dev/extra".to_owned(), "x".to_owned());
    added.remove("refs/nyl/dev/lease");
    assert_eq!(
        unchanged_violations(&before, &added, &excused_refs("prod", None)),
        ["refs/heads/nyl/dev/extra", "refs/nyl/dev/lease"]
    );
    // Counting dev as the target would have excused exactly the source's change.
    assert!(!unchanged_violations(&before, &moved, &excused_refs("dev", None))
        .contains(&"refs/heads/nyl/dev/desired".to_owned()));
    // A target whose name prefixes another environment's does not excuse it.
    let mut prefixed = promoted;
    prefixed.insert("refs/heads/nyl/prod-hotfix/desired".to_owned(), "h".to_owned());
    assert_eq!(
        unchanged_violations(&before, &prefixed, &excused_refs("prod", None)),
        ["refs/heads/nyl/prod-hotfix/desired"]
    );
}

/// A dry run excuses no ref, the target's included; `record: source` also
/// excuses the branch it commits to, and only that branch; project branches
/// and keep refs are part of what a promotion may not move.
#[test]
fn test_promote_excuses_only_the_refs_it_writes() {
    use scenario::runner::{excused_refs, unchanged_violations};
    let refs = |entries: &[(&str, &str)]| {
        entries
            .iter()
            .map(|(name, commit)| ((*name).to_owned(), (*commit).to_owned()))
            .collect::<std::collections::BTreeMap<_, _>>()
    };
    let before = refs(&[
        ("refs/heads/main", "m1"),
        ("refs/heads/main-2", "n1"),
        ("refs/heads/nyl/prod/desired", "c1"),
        ("refs/nyl-keep/pr-1/abc", "k1"),
    ]);
    let wrote_target = refs(&[
        ("refs/heads/main", "m1"),
        ("refs/heads/main-2", "n1"),
        ("refs/heads/nyl/prod/desired", "c2"),
        ("refs/nyl-keep/pr-1/abc", "k1"),
    ]);
    // With no excuse at all, as for a dry run, the target's write is a violation.
    assert_eq!(
        unchanged_violations(&before, &wrote_target, &[]),
        ["refs/heads/nyl/prod/desired"]
    );
    assert!(unchanged_violations(&before, &wrote_target, &excused_refs("prod", None)).is_empty());
    // `record: state` that also commits to main is caught; `record: source` excuses main alone.
    let mut committed = wrote_target.clone();
    committed.insert("refs/heads/main".to_owned(), "m2".to_owned());
    assert_eq!(
        unchanged_violations(&before, &committed, &excused_refs("prod", None)),
        ["refs/heads/main"]
    );
    assert!(unchanged_violations(&before, &committed, &excused_refs("prod", Some("main"))).is_empty());
    committed.insert("refs/heads/main-2".to_owned(), "n2".to_owned());
    assert_eq!(
        unchanged_violations(&before, &committed, &excused_refs("prod", Some("main"))),
        ["refs/heads/main-2"]
    );
    let mut kept = wrote_target;
    kept.remove("refs/nyl-keep/pr-1/abc");
    assert_eq!(
        unchanged_violations(&before, &kept, &excused_refs("prod", None)),
        ["refs/nyl-keep/pr-1/abc"]
    );
}

/// The guard sits where scenarios execute: a runner refuses a pending scenario
/// and one above the reached milestone, whatever selected it. Catches a runner
/// test reverted to filter on the milestone alone.
#[test]
fn test_every_runner_refuses_pending_and_unreached_scenarios() {
    let (loaded, _) = scenario::load_all();
    for (name, _) in PENDING {
        let pending = loaded.iter().find(|loaded| loaded.name == name).unwrap();
        for result in [
            scenario::runner::run(pending),
            scenario::runner::run_real_publication(pending),
            #[cfg(feature = "tier2")]
            scenario::runner::run_binary(pending),
        ] {
            let error = result.expect_err(name);
            assert!(error.contains("is pending and runs in no runner"), "{name}: {error}");
        }
    }
    let above = inline_scenario(
        "inline/above",
        "contract: design/reference-scenarios.md#tiers\nmilestone: M99\nsteps: []\n",
    );
    let error = scenario::runner::run(&above).unwrap_err();
    assert!(error.contains("above the reached milestone"), "{error}");
}

/// `%STATE(<id>)%` of a step that wrote only an observed commit is that
/// commit, and `%OBSERVED%` agrees; a desired commit wins when there is one.
#[test]
fn test_state_placeholder_falls_back_to_the_observed_commit() {
    use scenario::runner::StepWrite;
    let observed_only = StepWrite {
        desired: None,
        observed: Some("o1".to_owned()),
        ..StepWrite::default()
    };
    assert_eq!(observed_only.state().map(String::as_str), Some("o1"));
    let both = StepWrite {
        desired: Some("d1".to_owned()),
        observed: Some("o1".to_owned()),
        ..StepWrite::default()
    };
    assert_eq!(both.state().map(String::as_str), Some("d1"));
    assert_eq!(StepWrite::default().state(), None);
    // In a real run: `attest` writes only an observed commit, and its id still names a state.
    let text = "contract: design/orchestration-core.md#attestations\nmilestone: M6\ntiers: [1]\nsteps:\n  - nyl: [state, init, -e, dev]\n    exit: 0\n  - nyl: [reconcile, -e, dev]\n    exit: 2\n  - file: {path: health.json, content: '{\"healthy\": \"fail\"}'}\n  - nyl: [attest, -e, dev]\n    id: a1\n    exit: 3\n    expect:\n      commits: {desired: 0, observed: 1}\n  - nyl: [get, units, -e, dev, --state-revision, '%STATE(a1)%']\n    exit: 0\n  - nyl: [get, units, -e, dev, --state-revision, '%OBSERVED(a1)%']\n    exit: 0\n";
    run_inline("walkthroughs/attested-evidence", "inline/observed-only", text).unwrap();
}

/// A `%PLAN-DIGEST%` would consume a `plan` script meant for the step, so the
/// format rejects the combination.
#[test]
fn test_plan_digest_after_a_plan_script_is_rejected() {
    let steps = |between: &str| {
        format!(
            "contract: design/reference-scenarios.md#scenario-1-platform-environment\nmilestone: M6\ntiers: [1]\nsteps:\n  - nyl: [state, init, -e, dev]\n    exit: 0\n  - driver: {{unit: network, operation: plan, outcome: succeeded}}\n{between}  - nyl: [reconcile, -e, dev, --approve, 'network=%PLAN-DIGEST(dev/network)%']\n    exit: 0\n"
        )
    };
    let loaded = |text: &str| scenario::Loaded {
        name: "inline/plan-script".to_owned(),
        directory: scenario::scenarios_root().join("reference/platform"),
        scenario: serde_saphyr::from_str(text).unwrap(),
    };
    let rejected = loaded(&steps(""));
    let error =
        scenario::format::validate(&rejected.scenario, &rejected.directory, &scenario::repository_root()).unwrap_err();
    assert!(error.contains("PLAN-DIGEST") && error.contains("plan"), "{error}");
    // A `nyl` step in between consumes the script first.
    let accepted = loaded(&steps("  - nyl: [plan, -e, dev]\n    exit: 0\n"));
    scenario::format::validate(&accepted.scenario, &accepted.directory, &scenario::repository_root()).unwrap();
}

/// An empty list the expectation names is a key the record must write, and a
/// superseded commit's repository is checked when given.
#[test]
fn test_promotion_expectation_requires_empty_lists_and_checks_repositories() {
    use serde_json::json;
    let expectation = "path: dev-to-prod\nfrom: {run: r1}\nsuperseded: []\nignoredAttestations: []\nevidence: {level: attested, attestations: []}\n";
    let mut record = promotion_record();
    record["superseded"] = json!([]);
    record["ignoredAttestations"] = json!([]);
    record["evidence"]["attestations"] = json!([]);
    check_promotion(expectation, &record, None).unwrap();
    for (pointer, name) in [
        ("superseded", "superseded"),
        ("ignoredAttestations", "ignoredAttestations"),
    ] {
        let mut missing = record.clone();
        missing.as_object_mut().unwrap().remove(pointer);
        let error = check_promotion(expectation, &missing, None).unwrap_err();
        assert!(error.contains(&format!("{name} is absent")), "{error}");
    }
    record["evidence"].as_object_mut().unwrap().remove("attestations");
    assert!(check_promotion(expectation, &record, None)
        .unwrap_err()
        .contains("evidence.attestations is absent"));

    let with_repository = "path: dev-to-prod\nfrom: {run: r1}\nsuperseded:\n  - {repository: platform, commit: main~1, reason: Fix merged}\n";
    let mut record = promotion_record();
    record["superseded"][0]["repository"] = json!("platform");
    check_promotion(with_repository, &record, None).unwrap();
    record["superseded"][0]["repository"] = json!("other");
    assert!(check_promotion(with_repository, &record, None)
        .unwrap_err()
        .contains("superseded[0].repository"));
}
