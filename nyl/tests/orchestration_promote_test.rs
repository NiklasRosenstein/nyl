//! `nyl promote` end to end, in-process with Git state: dev builds an image
//! and prod, whose source is promoted, takes its source commit from the
//! record. `values` is a second path that promotes two values into prod.
//!
//! Contract: [Promotion paths](../../design/promotion.md#promotion-paths) and
//! [Exit categories](../../design/promotion.md#exit-categories). Promotion
//! takes the target's lease, writes one desired commit with the record and
//! never touches the source; `--dry-run` writes nothing and takes no lease.

mod scenario;

use nyl_core::orchestration::ids::EnvironmentName;
use nyl_core::orchestration::ports::{Clock as _, IdGenerator as _};
use nyl_core::orchestration::state::{Lease, LeaseKind, Operation, StateLocation, StateV1};
use nyl_units::fake::Scripted;
use scenario::shell::{self, project};
use tempfile::TempDir;

const PROD_DESIRED: &str = "refs/heads/nyl/prod/desired";
const PROD_OBSERVED: &str = "refs/heads/nyl/prod/observed";
const DEV_DESIRED: &str = "refs/heads/nyl/dev/desired";
const DEV_OBSERVED: &str = "refs/heads/nyl/dev/observed";

const ENVIRONMENTS: &str = "apiVersion: gitops.nyl/v1
kind: Environment
metadata: {name: dev}
spec:
  unitSelector: {matchLabels: {dev: 'true'}}
  allowUnprotectedSource: true
---
apiVersion: gitops.nyl/v1
kind: Environment
metadata: {name: prod}
spec:
  source: {fromPromotion: {path: dev-to-prod}}
  unitSelector: {matchLabels: {prod: 'true'}}
  allowUnprotectedSource: true
---
apiVersion: gitops.nyl/v1
kind: PromotionPath
metadata: {name: dev-to-prod}
spec:
  from: {environment: dev}
  to: {environment: prod}
  values:
    webImage:
      select: {unit: web-image, artifact: image, pointer: /reference}
---
apiVersion: gitops.nyl/v1
kind: PromotionPath
metadata: {name: values}
spec:
  from: {environment: dev}
  to: {environment: prod}
  values:
    webImage:
      select: {unit: web-image, artifact: image, pointer: /reference}
    repository:
      select: {unit: web-image, input: /repository}
";

const UNITS: &str = "apiVersion: units.gitops.nyl/v1
kind: OciImage
metadata:
  name: web-image
  labels: {dev: 'true'}
spec:
  context: {path: ../services/web}
  repository: registry.invalid:5000/web
---
apiVersion: units.gitops.nyl/v1
kind: Command
metadata:
  name: svc
  labels: {dev: 'true', prod: 'true'}
spec:
  command: [sh, -c, 'true']
";

struct Promote {
    shell: shell::Harness,
    _project: TempDir,
}

fn location(harness: &shell::Harness, environment: &str) -> StateLocation {
    StateLocation {
        desired_ref: format!("nyl/{environment}/desired"),
        observed_ref: format!("nyl/{environment}/observed"),
        ..harness.location.clone()
    }
}

impl Promote {
    /// dev and prod with state, dev reconciled once.
    fn new() -> Self {
        Self::with(true)
    }

    fn with(reconciled: bool) -> Self {
        Self::of(ENVIRONMENTS, reconciled)
    }

    fn of(environments: &str, reconciled: bool) -> Self {
        let project = project(&[
            shell::NYL_TOML,
            ("nyl/environments.yaml", environments),
            ("nyl/units.yaml", UNITS),
            ("services/web/Dockerfile", "FROM scratch\n"),
        ]);
        let shell = shell::Harness::new(project.path(), true);
        let harness = Self {
            shell,
            _project: project,
        };
        for environment in ["dev", "prod"] {
            assert_eq!(harness.shell.invoke(&["state", "init", "-e", environment]).exit, 0);
        }
        if reconciled {
            harness.reconcile_dev();
        }
        harness
    }

    /// The next artifact verification finds the image gone.
    fn fail_verification(&self) {
        self.shell.script.push(
            nyl_core::orchestration::ids::UnitName::new("web-image").unwrap(),
            nyl_core::orchestration::driver::DriverOperation::VerifyArtifact,
            &Scripted::Failed {
                category: nyl_core::orchestration::state::FailureCategory::ToolError,
                retryable: false,
            },
            1,
        );
    }

    fn reconcile_dev(&self) {
        let run = self.shell.invoke(&["reconcile", "-e", "dev"]);
        assert_eq!(run.exit, 0, "{}", run.stderr);
    }

    /// dev's source changes, so its next reconcile builds another image.
    fn release(&self, version: &str) {
        self.shell.world.commit_files(
            "main",
            &format!("release {version}"),
            &[("services/web/Dockerfile", &format!("FROM scratch\nLABEL v={version}\n"))],
        );
        self.reconcile_dev();
    }

    fn promote(&self, args: &[&str]) -> nyl::orchestration::run::CommandOutcome {
        let mut full = vec!["promote"];
        full.extend_from_slice(args);
        full.extend(["-o", "json"]);
        self.shell.invoke(&full)
    }

    fn prod_lease_released(&self) -> bool {
        let coordination = self
            .shell
            .store
            .coordination(&location(&self.shell, "prod"), &EnvironmentName::new("prod").unwrap())
            .unwrap();
        coordination.lease.is_none() && coordination.runs.is_empty()
    }
}

/// One promotion writes exactly one desired commit of the target with the
/// record and a summary, under the operation `promote`, and leaves both of
/// the source's refs and the target's observed ref alone; the lease is
/// released. Catches a record written with more or fewer commits, into the
/// wrong ref, or by something that touches the source.
#[test]
fn test_promote_writes_one_desired_commit_and_leaves_the_source_alone() {
    let harness = Promote::new();
    let (dev_desired, dev_observed, prod_observed) = (
        harness.shell.tip(DEV_DESIRED),
        harness.shell.tip(DEV_OBSERVED),
        harness.shell.tip(PROD_OBSERVED),
    );
    let before = harness.shell.tip(PROD_DESIRED);
    let promoted = harness.promote(&["dev-to-prod", "--reason", "Release 1"]);
    assert_eq!(promoted.exit, 0, "{}", promoted.stderr);
    assert_eq!(promoted.report["outcome"], "done");
    assert_eq!(promoted.report["record"]["sequence"], 1);
    assert_ne!(harness.shell.tip(PROD_DESIRED), before);
    assert_eq!(
        harness.shell.tip(&format!("{PROD_DESIRED}^")),
        before,
        "exactly one commit"
    );
    assert_eq!(harness.shell.tip(PROD_OBSERVED), prod_observed);
    assert_eq!(harness.shell.tip(DEV_DESIRED), dev_desired);
    assert_eq!(harness.shell.tip(DEV_OBSERVED), dev_observed);
    assert!(harness.prod_lease_released());
    let message = harness.shell.message(PROD_DESIRED);
    assert!(message.starts_with("nyl: promote prod (dev-to-prod #1)"), "{message}");
    assert!(message.contains("Nyl-Reason: Release 1"), "{message}");
    let record = harness.shell.show(PROD_DESIRED, "desired/promotions/dev-to-prod.yaml");
    for expected in [
        "kind: PromotionRecord",
        "sequence: 1",
        "registry.invalid:5000/web@sha256:",
        "reason: Release 1",
        "artifactsVerified: true",
    ] {
        assert!(record.contains(expected), "{expected}: {record}");
    }
}

/// A dry run takes no lease, writes no commit anywhere, and reports the
/// record it would write at the top level, with the same exit the
/// promotion would have. Catches a dry run that writes, or a report without
/// the record the scenarios read.
#[test]
fn test_a_dry_run_writes_nothing_and_reports_the_record() {
    let harness = Promote::new();
    let refs = [PROD_DESIRED, PROD_OBSERVED, DEV_DESIRED, DEV_OBSERVED];
    let tips: Vec<String> = refs.iter().map(|reference| harness.shell.tip(reference)).collect();
    // Hold the target's lease: a dry run neither waits for it nor takes it.
    let holder = Lease {
        api_version: StateV1,
        kind: LeaseKind,
        run: harness.shell.ids.run_id(),
        runner: "https://ci.example.com/runs/1".to_owned(),
        operation: Operation::Reconcile,
        units: Vec::new(),
        deadline: harness.shell.clock.now() + chrono::Duration::minutes(5),
        taken_at: harness.shell.clock.now(),
        broken: None,
    };
    harness
        .shell
        .store
        .take_lease(
            &location(&harness.shell, "prod"),
            &EnvironmentName::new("prod").unwrap(),
            &holder,
            None,
        )
        .unwrap();
    let dry = harness.promote(&["dev-to-prod", "--dry-run"]);
    assert_eq!(dry.exit, 0, "{}", dry.stderr);
    assert_eq!(dry.report["dryRun"], true);
    assert_eq!(dry.report["record"]["path"], "dev-to-prod");
    assert_eq!(dry.report["record"]["sequence"], 1);
    assert!(dry.report.get("commits").is_none());
    let after: Vec<String> = refs.iter().map(|reference| harness.shell.tip(reference)).collect();
    assert_eq!(tips, after);
}

/// Promoting the state the path's record already names writes nothing and
/// exits 0 (`unchanged`, D2); a newer state is a promotion with the next
/// sequence. Catches a re-promotion that piles up commits, and a sequence
/// that does not grow.
#[test]
fn test_promoting_the_recorded_state_again_is_unchanged() {
    let harness = Promote::new();
    assert_eq!(harness.promote(&["dev-to-prod"]).exit, 0);
    let tip = harness.shell.tip(PROD_DESIRED);
    let again = harness.promote(&["dev-to-prod"]);
    assert_eq!(again.exit, 0, "{}", again.stderr);
    assert_eq!(again.report["outcome"], "unchanged");
    assert_eq!(again.report["record"]["sequence"], 1);
    assert_eq!(harness.shell.tip(PROD_DESIRED), tip);
    harness.release("2");
    let next = harness.promote(&["dev-to-prod"]);
    assert_eq!(next.exit, 0, "{}", next.stderr);
    assert_eq!(next.report["record"]["sequence"], 2);
}

/// The target's lease is taken: held by another run is exit 2 with nothing
/// written, and `--wait-lease` waits for it. Catches a promotion that
/// writes under a running reconcile.
#[test]
fn test_promote_takes_the_targets_lease() {
    let harness = Promote::new();
    let holder = Lease {
        api_version: StateV1,
        kind: LeaseKind,
        run: harness.shell.ids.run_id(),
        runner: "https://ci.example.com/runs/1".to_owned(),
        operation: Operation::Reconcile,
        units: Vec::new(),
        deadline: harness.shell.clock.now() + chrono::Duration::minutes(5),
        taken_at: harness.shell.clock.now(),
        broken: None,
    };
    harness
        .shell
        .store
        .take_lease(
            &location(&harness.shell, "prod"),
            &EnvironmentName::new("prod").unwrap(),
            &holder,
            None,
        )
        .unwrap();
    let tip = harness.shell.tip(PROD_DESIRED);
    let held = harness.promote(&["dev-to-prod"]);
    assert_eq!(held.exit, 2, "{}", held.stderr);
    assert!(held.stderr.contains("NYL-LEASE-HELD"), "{}", held.stderr);
    assert_eq!(harness.shell.tip(PROD_DESIRED), tip);
    let waited = harness.promote(&["dev-to-prod", "--wait-lease", "10m"]);
    assert_eq!(waited.exit, 0, "{}", waited.stderr);
    assert_ne!(harness.shell.tip(PROD_DESIRED), tip);
}

/// `state restore` removes newer records but not their commits, so the next
/// sequence still counts them: after restoring prod to before promotion 2,
/// the next promotion is 3. Catches a sequence taken from the current
/// snapshot, which would hand out 2 again.
#[test]
fn test_the_sequence_comes_from_history_not_the_snapshot() {
    let harness = Promote::new();
    assert_eq!(harness.promote(&["dev-to-prod"]).exit, 0);
    let first = harness.shell.tip(PROD_DESIRED);
    harness.release("2");
    let second = harness.promote(&["dev-to-prod"]);
    assert_eq!(second.report["record"]["sequence"], 2, "{}", second.stderr);
    let restored = harness
        .shell
        .invoke(&["state", "restore", "-e", "prod", "--to", &first]);
    assert_eq!(restored.exit, 0, "{}", restored.stderr);
    let record = harness.shell.show(PROD_DESIRED, "desired/promotions/dev-to-prod.yaml");
    assert!(record.contains("sequence: 1"), "{record}");
    let third = harness.promote(&["dev-to-prod"]);
    assert_eq!(third.exit, 0, "{}", third.stderr);
    assert_eq!(third.report["record"]["sequence"], 3);
}

/// An environment whose source is promoted cannot reconcile before the first
/// promotion: exit 2, `NYL-PROMOTION-MISSING`, the fix naming `nyl promote`.
/// After it, the run applies the record's source commit and records the
/// choice and the sequence it applied. Catches a promoted source running at
/// the entry commit, and a missing record failing as a configuration error.
#[test]
fn test_a_target_reconcile_needs_a_promotion_and_records_what_it_applied() {
    let harness = Promote::new();
    let missing = harness.shell.invoke(&["reconcile", "-e", "prod", "-o", "json"]);
    assert_eq!(missing.exit, 2, "{}", missing.stderr);
    assert!(missing.stderr.contains("NYL-PROMOTION-MISSING"), "{}", missing.stderr);
    assert!(missing.stderr.contains("nyl promote dev-to-prod"), "{}", missing.stderr);
    assert_eq!(harness.promote(&["dev-to-prod"]).exit, 0);
    let run = harness.shell.invoke(&["reconcile", "-e", "prod"]);
    assert_eq!(run.exit, 0, "{}", run.stderr);
    let environment = harness.shell.show(PROD_DESIRED, "desired/environment.yaml");
    assert!(environment.contains("by: promotion"), "{environment}");
    assert!(environment.contains("path: dev-to-prod"), "{environment}");
    assert!(environment.contains("appliedPromotionSequence: 1"), "{environment}");
}

/// A refused evidence check writes nothing and exits 2; a missing artifact
/// exits 1 (`NYL-PROMOTE-ARTIFACT-MISSING`) until `--no-verify-artifacts`,
/// which the record says. Catches a promotion of an image the registry no
/// longer has, and a skipped check that is not recorded.
#[test]
fn test_missing_evidence_and_missing_artifacts_refuse() {
    let harness = Promote::new();
    let tip = harness.shell.tip(PROD_DESIRED);
    harness.fail_verification();
    let missing = harness.promote(&["dev-to-prod"]);
    assert_eq!(missing.exit, 1, "{}", missing.stderr);
    assert!(
        missing.stderr.contains("NYL-PROMOTE-ARTIFACT-MISSING"),
        "{}",
        missing.stderr
    );
    assert_eq!(harness.shell.tip(PROD_DESIRED), tip);
    harness.fail_verification();
    let skipped = harness.promote(&["dev-to-prod", "--no-verify-artifacts"]);
    assert_eq!(skipped.exit, 0, "{}", skipped.stderr);
    assert_eq!(skipped.report["record"]["artifactsVerified"], false);
}

/// With nothing reconciled in dev there is no state to promote: exit 2,
/// `NYL-PROMOTE-NO-EVIDENCE`, nothing written. Catches a promotion of nothing.
#[test]
fn test_no_recorded_state_is_missing_evidence() {
    let harness = Promote::with(false);
    let tip = harness.shell.tip(PROD_DESIRED);
    let refused = harness.promote(&["dev-to-prod"]);
    assert_eq!(refused.exit, 2, "{}", refused.stderr);
    assert!(refused.stderr.contains("NYL-PROMOTE-NO-EVIDENCE"), "{}", refused.stderr);
    assert_eq!(harness.shell.tip(PROD_DESIRED), tip);
    assert!(harness.prod_lease_released());
}

/// `--value` promotes a subset and leaves the other values at their recorded
/// lineage; it names only values the path declares, and not on a path that
/// promotes the target's source. Catches a subset that drops or rewrites the
/// values it did not select.
#[test]
fn test_value_promotes_a_subset_and_keeps_the_rest() {
    let harness = Promote::new();
    let first = harness.promote(&["values"]);
    assert_eq!(first.exit, 0, "{}", first.stderr);
    let old_image = first.report["record"]["values"]["webImage"]["value"].clone();
    harness.release("2");
    let subset = harness.promote(&["values", "--value", "repository"]);
    assert_eq!(subset.exit, 0, "{}", subset.stderr);
    let values = &subset.report["record"]["values"];
    assert_eq!(
        values["webImage"]["value"], old_image,
        "the other value keeps its lineage"
    );
    assert!(values["repository"].is_object());
    assert_eq!(subset.report["record"]["sequence"], 2);
    let unknown = harness.promote(&["values", "--value", "nope"]);
    assert_eq!(unknown.exit, 1);
    assert!(
        unknown.stderr.contains("NYL-PROMOTE-SELECTOR-INVALID"),
        "{}",
        unknown.stderr
    );
    let source = harness.promote(&["dev-to-prod", "--value", "webImage"]);
    assert_eq!(source.exit, 1);
    assert!(source.stderr.contains("NYL-PROMOTE-UNSUPPORTED"), "{}", source.stderr);
}

/// A target that also selects the unit a value reads builds its own result:
/// the promotion succeeds and warns (`NYL-PROMOTE-REBUILT`) naming the unit.
/// Catches a promoted value silently replacing what the target builds.
#[test]
fn test_a_target_that_builds_the_promoted_unit_is_warned() {
    let harness = Promote::of(&ENVIRONMENTS.replace("{prod: 'true'}", "{dev: 'true'}"), true);
    let promoted = harness.promote(&["dev-to-prod"]);
    assert_eq!(promoted.exit, 0, "{}", promoted.stderr);
    assert!(promoted.stderr.contains("NYL-PROMOTE-REBUILT"), "{}", promoted.stderr);
    assert!(promoted.stderr.contains("web-image"), "{}", promoted.stderr);
}
