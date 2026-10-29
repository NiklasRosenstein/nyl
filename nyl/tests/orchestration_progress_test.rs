//! Live events and summaries share the shell's normalized unit outcomes.
mod scenario;

use std::path::PathBuf;
use std::sync::Mutex;

use nyl_core::orchestration::progress::{ProgressObserver, UnitPhase, UnitProgress};
use nyl_core::orchestration::state::Operation;
use nyl_drivers::process::ProcessRunner;
use nyl_orchestration::run_report::EnvironmentRunReport;
use nyl_orchestration::shell::run::{self, Cancellation, RunContext, RunMode, TeardownTarget};
use scenario::shell::{command, project, Harness, ENVIRONMENT, NYL_TOML};

struct Events {
    events: Mutex<Vec<UnitProgress>>,
    fast_finished: PathBuf,
}

impl ProgressObserver for Events {
    fn observe(&self, event: &UnitProgress) {
        self.events.lock().unwrap().push(event.clone());
        if event.unit.unit.as_str() == "z-fast" && matches!(event.phase, UnitPhase::Finished { .. }) {
            // The slow command can finish only after the fast unit's end event.
            std::fs::write(&self.fast_finished, "finished").unwrap();
        }
    }
}

fn run(harness: &Harness, observer: &Events, mode: RunMode) -> EnvironmentRunReport {
    let mut job = harness.job(&[], Cancellation::default());
    job.request.mode = mode;
    let base = harness.context();
    let tools = ProcessRunner::new();
    let context = RunContext {
        base: &base,
        sources: &job.sources,
        renderer: &job.renderer,
        drivers: &harness.drivers,
        tools: &tools,
        workspace: job.workspace.path(),
        process_env: &harness.process_env,
        secrets: None,
        commit_identity: Some(&harness.identity),
        observer: None,
        progress: Some(observer),
        prompt: None,
    };
    run::reconcile(&context, &job.request).environments.remove(0)
}

#[test]
fn test_parallel_completion_keeps_dispatch_order_and_normalized_events() {
    let signal = tempfile::TempDir::new().unwrap();
    let events = Events {
        events: Mutex::new(Vec::new()),
        fast_finished: signal.path().join("fast-finished"),
    };
    let slow = command(
        "a-slow",
        &format!(
            "i=0; while [ ! -f \"{}\" ]; do i=$((i+1)); test \"$i\" -lt 1000 || exit 1; sleep 0.01; done",
            events.fast_finished.display()
        ),
        "  idempotent: true\n  teardown: [sh, -c, 'true']\n",
    );
    let fast = command("z-fast", "true", "  idempotent: true\n  teardown: [sh, -c, 'true']\n");
    let after = command(
        "b-after",
        "true",
        "  idempotent: true\n  dependsOn: [a-slow, z-fast]\n  teardown: [sh, -c, 'true']\n",
    );
    let project = project(&[
        NYL_TOML,
        ("nyl/environment.yaml", ENVIRONMENT),
        ("nyl/slow.yaml", &slow),
        ("nyl/fast.yaml", &fast),
        ("nyl/after.yaml", &after),
    ]);
    let harness = Harness::new(project.path(), false);
    harness.init();
    let report = run(&harness, &events, RunMode::Reconcile);
    assert_eq!(report.exit.code(), 0, "{report:#?}");
    let expected = ["a-slow", "z-fast", "b-after"];
    assert_eq!(
        report.unit_order.iter().map(|name| name.as_str()).collect::<Vec<_>>(),
        expected
    );
    let recorded = events.events.lock().unwrap().clone();
    let ends = recorded
        .iter()
        .filter(|event| matches!(event.phase, UnitPhase::Finished { .. }))
        .collect::<Vec<_>>();
    assert_eq!(
        ends.iter().map(|event| event.unit.unit.as_str()).collect::<Vec<_>>(),
        ["z-fast", "a-slow", "b-after"]
    );
    for event in ends {
        let UnitPhase::Finished {
            started,
            at,
            result,
            message,
        } = &event.phase
        else {
            unreachable!()
        };
        let entry = &report.units[&event.unit.unit];
        assert_eq!(Some(result), entry.result.as_ref());
        assert_eq!(message, &entry.message);
        assert!(at >= started);
    }
    events.events.lock().unwrap().clear();
    let current = run(&harness, &events, RunMode::Reconcile);
    assert_eq!(current.unit_order, report.unit_order);
    assert!(events.events.lock().unwrap().is_empty());
    let teardown = run(
        &harness,
        &events,
        RunMode::Teardown {
            target: TeardownTarget::All,
        },
    );
    assert_eq!(teardown.exit.code(), 0, "{teardown:#?}");
    assert_eq!(
        teardown.unit_order.iter().map(|name| name.as_str()).collect::<Vec<_>>(),
        ["b-after", "a-slow", "z-fast"]
    );
    assert!(events
        .events
        .lock()
        .unwrap()
        .iter()
        .all(|event| event.operation == Operation::Teardown));
}
