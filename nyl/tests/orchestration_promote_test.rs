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
use scenario::world::git_output;
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
  attestations: [qa]
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
metadata: {name: attested}
spec:
  from: {environment: dev}
  to: {environment: prod}
  evidence: attested
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

/// prod's own unit, which binds the promoted image.
const BINDER: &str = "---
apiVersion: units.gitops.nyl/v1
kind: Command
metadata:
  name: binder
  labels: {prod: 'true'}
spec:
  command: [sh, -c, 'true']
  values:
    image: {fromPromotion: {value: webImage}}
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
        Self::of(ENVIRONMENTS, &format!("{UNITS}{BINDER}"), reconciled)
    }

    fn of(environments: &str, units: &str, reconciled: bool) -> Self {
        let project = project(&[
            shell::NYL_TOML,
            ("nyl/environments.yaml", environments),
            ("nyl/units.yaml", units),
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
    assert_eq!(
        dry.report["outcome"], "done",
        "a dry run that would write is not `unchanged`"
    );
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
    // The carried value names the state the old record took it from.
    assert_eq!(
        values["webImage"]["state"]["desiredCommit"],
        first.report["record"]["from"]["desiredCommit"]
    );
    assert_eq!(
        values["webImage"]["state"]["run"],
        first.report["record"]["from"]["run"]
    );
    assert!(
        values["repository"]["state"].is_null(),
        "the promoted value is from `from`"
    );
    let unknown = harness.promote(&["values", "--value", "nope"]);
    assert_eq!(unknown.exit, 1);
    assert!(
        unknown.stderr.contains("NYL-PROMOTE-SELECTOR-INVALID"),
        "{}",
        unknown.stderr
    );
    let source = harness.promote(&["dev-to-prod", "--value", "webImage"]);
    assert_eq!(source.exit, 1);
    assert!(
        source.stderr.contains("NYL-PROMOTE-SELECTOR-INVALID"),
        "{}",
        source.stderr
    );
}

/// At `attested` the environment's `qa` must pass for the state: without it
/// the promotion is exit 2 and says what is missing; recorded for a state,
/// it still counts after a later run replaced the current file, because the
/// walk of the observed history finds it, and the newer state without it
/// is passed over. Catches evidence read from the current files only, which
/// would refuse the proven state, or accept one nobody attested.
#[test]
fn test_attested_promotion_reads_attestations_recorded_for_older_states() {
    let harness = Promote::new();
    let refused = harness.promote(&["attested"]);
    assert_eq!(refused.exit, 2, "{}", refused.stderr);
    assert!(
        refused.stderr.contains("NYL-PROMOTE-NO-EVIDENCE") && refused.stderr.contains("qa"),
        "{}",
        refused.stderr
    );
    let attested = harness
        .shell
        .invoke(&["attest", "-e", "dev", "--name", "qa", "--result", "pass"]);
    assert_eq!(attested.exit, 0, "{}", attested.stderr);
    let proven = harness.shell.tip(DEV_DESIRED);
    harness.release("2");
    assert_ne!(harness.shell.tip(DEV_DESIRED), proven);
    let promoted = harness.promote(&["attested"]);
    assert_eq!(promoted.exit, 0, "{}", promoted.stderr);
    let record = &promoted.report["record"];
    assert_eq!(record["from"]["desiredCommit"], proven, "the state qa was recorded for");
    assert_eq!(record["evidence"]["level"], "attested");
    assert_eq!(record["evidence"]["attestations"][0]["scope"], "environment");
    assert_eq!(record["evidence"]["attestations"][0]["name"], "qa");
}

impl Promote {
    fn get(&self, args: &[&str]) -> nyl::orchestration::run::CommandOutcome {
        let mut full = vec!["get"];
        full.extend_from_slice(args);
        full.extend(["-o", "json"]);
        self.shell.invoke(&full)
    }
}

/// `get states` lists the source's recorded states newest first, each with
/// its commits and the attestations recorded for it, including one recorded
/// for an older state after a newer run; it writes nothing. Catches states
/// listed oldest first, attestations read from the current files only, and
/// an inspection that touches state.
#[test]
fn test_get_states_lists_recorded_states_with_their_attestations() {
    let harness = Promote::new();
    let attested = harness.shell.invoke(&[
        "attest", "-e", "dev", "--name", "qa", "--result", "pass", "--reason", "QA ok",
    ]);
    assert_eq!(attested.exit, 0, "{}", attested.stderr);
    let first = harness.shell.tip(DEV_DESIRED);
    harness.release("2");
    let second = harness.shell.tip(DEV_DESIRED);
    let tips = [DEV_DESIRED, DEV_OBSERVED].map(|reference| harness.shell.tip(reference));

    let states = harness.get(&["states", "-e", "dev"]);
    assert_eq!(states.exit, 0, "{}", states.stderr);
    assert_eq!(states.report["kind"], "StatesReport");
    let listed = states.report["states"].as_array().unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0]["desiredCommit"], second);
    assert_eq!(listed[1]["desiredCommit"], first);
    assert_eq!(
        listed[0]["attestations"],
        serde_json::json!([]),
        "nobody attested the new state"
    );
    assert_eq!(listed[1]["attestations"][0]["scope"], "environment");
    assert_eq!(listed[1]["attestations"][0]["name"], "qa");
    assert_eq!(listed[1]["attestations"][0]["result"], "pass");
    assert_eq!(listed[1]["attestations"][0]["reason"], "QA ok");
    assert!(listed[0]["sourceCommit"].is_string());
    assert!(listed[0]["units"]
        .as_array()
        .unwrap()
        .iter()
        .all(|unit| unit["receipt"] == true));
    assert_eq!(
        tips,
        [DEV_DESIRED, DEV_OBSERVED].map(|reference| harness.shell.tip(reference))
    );

    let text = harness.shell.invoke(&["get", "states", "-e", "dev"]);
    assert_eq!(text.exit, 0, "{}", text.stderr);
    assert!(text.stdout.starts_with(&format!("{second}\n")), "{}", text.stdout);
    assert!(text.stdout.contains("environment/qa pass at"), "{}", text.stdout);
}

/// `get promotion-candidates` judges every state as `nyl promote` does: the
/// state it marks `selected` is the one the dry run would record, at the
/// path's level; before any state reaches it, it shows `nyl promote`'s own
/// refusal and still exits 0; after the promotion the recorded state is
/// `promoted`. Catches a second implementation of the evidence rule that
/// disagrees with `promote`.
#[test]
fn test_promotion_candidates_agree_with_promote() {
    let harness = Promote::new();
    let refused = harness.get(&["promotion-candidates", "attested"]);
    assert_eq!(refused.exit, 0, "{}", refused.stderr);
    assert_eq!(refused.report["evidence"], "attested");
    assert_eq!(refused.report["refusal"]["code"], "NYL-PROMOTE-NO-EVIDENCE");
    let candidate = &refused.report["candidates"][0];
    assert_eq!(candidate["level"], "published");
    assert_eq!(candidate["eligible"], false);
    assert_eq!(candidate["selected"], false);
    assert_eq!(candidate["missing"][0]["attestation"], "qa");
    assert_eq!(candidate["missing"][0]["reason"], "pending");
    assert!(candidate["missing"][0].get("unit").is_none(), "environment-wide");
    assert!(candidate["values"]["webImage"]
        .as_str()
        .unwrap()
        .starts_with("registry.invalid:5000/web@sha256:"));

    let dry = harness.promote(&["dev-to-prod", "--dry-run"]);
    let candidates = harness.get(&["promotion-candidates", "dev-to-prod"]);
    assert_eq!(candidates.exit, 0, "{}", candidates.stderr);
    assert!(candidates.report.get("refusal").is_none());
    let selected = &candidates.report["candidates"][0];
    assert_eq!(selected["selected"], true);
    assert_eq!(selected["promoted"], false);
    assert_eq!(selected["desiredCommit"], dry.report["record"]["from"]["desiredCommit"]);
    assert_eq!(selected["sourceCommit"], dry.report["record"]["sourceCommit"]);
    assert_eq!(
        selected["values"]["webImage"],
        dry.report["record"]["values"]["webImage"]["value"]
    );

    assert_eq!(harness.promote(&["dev-to-prod"]).exit, 0);
    harness.release("2");
    let after = harness.get(&["promotion-candidates", "dev-to-prod"]);
    let listed = after.report["candidates"].as_array().unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(
        (&listed[0]["selected"], &listed[0]["promoted"]),
        (&true.into(), &false.into())
    );
    assert_eq!(
        (&listed[1]["selected"], &listed[1]["promoted"]),
        (&false.into(), &true.into())
    );

    let unknown = harness.shell.invoke(&["get", "promotion-candidates", "nowhere"]);
    assert_eq!(unknown.exit, 1);
    assert!(
        unknown.stderr.contains("NYL-PROMOTE-BINDING-UNKNOWN"),
        "{}",
        unknown.stderr
    );
}

/// `get promotions` shows the current record of each path, newest first,
/// the one prod's last run applied, and every promotion in prod's history,
/// including a record a later promotion replaced; `status` shows the lineage
/// of what prod runs, and a newer record waiting for its run. Catches
/// lineage read from a record no run applied, and history lost with the
/// replaced record.
#[test]
fn test_get_promotions_and_status_show_lineage() {
    let harness = Promote::new();
    let none = harness.get(&["promotions", "-e", "prod"]);
    assert_eq!(none.exit, 0, "{}", none.stderr);
    assert_eq!(none.report["promotions"], serde_json::json!([]));

    let first = harness.promote(&["dev-to-prod", "--reason", "Release 1"]);
    assert_eq!(first.exit, 0, "{}", first.stderr);
    let first_from = first.report["record"]["from"]["desiredCommit"].clone();
    let applied = harness.shell.invoke(&["reconcile", "-e", "prod"]);
    assert_eq!(applied.exit, 0, "{}", applied.stderr);
    let status = harness.shell.invoke(&["status", "-e", "prod", "-o", "json"]);
    let lineage = &status.report["environments"][0]["promotion"];
    assert_eq!(lineage["path"], "dev-to-prod");
    assert_eq!(lineage["sequence"], 1);
    assert_eq!(lineage["sourceCommit"], first.report["record"]["sourceCommit"]);
    assert_eq!(lineage["from"]["environment"], "dev");
    assert_eq!(lineage["from"]["desiredCommit"], first_from);
    assert!(lineage.get("pendingSequence").is_none());
    let text = harness.shell.invoke(&["status", "-e", "prod"]);
    assert!(
        text.stdout.contains("promoted: dev-to-prod #1 from dev run"),
        "{}",
        text.stdout
    );
    let running = |report: &serde_json::Value| -> Vec<serde_json::Value> {
        report["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|candidate| candidate["running"] == true)
            .map(|candidate| candidate["desiredCommit"].clone())
            .collect()
    };
    let candidates = harness.get(&["promotion-candidates", "dev-to-prod"]);
    assert_eq!(
        running(&candidates.report),
        std::slice::from_ref(&first_from),
        "prod runs #1's state"
    );

    harness.release("2");
    let second = harness.promote(&["dev-to-prod", "--reason", "Release 2"]);
    assert_eq!(second.exit, 0, "{}", second.stderr);
    let status = harness.shell.invoke(&["status", "-e", "prod", "-o", "json"]);
    let lineage = &status.report["environments"][0]["promotion"];
    assert_eq!(lineage["sequence"], 1, "prod still runs #1");
    assert!(lineage.get("from").is_none(), "#1's record was replaced");
    assert_eq!(lineage["pendingSequence"], 2);
    let candidates = harness.get(&["promotion-candidates", "dev-to-prod"]);
    assert!(
        running(&candidates.report).is_empty(),
        "#1's record was replaced: nothing names what prod runs"
    );

    let promotions = harness.get(&["promotions", "-e", "prod"]);
    assert_eq!(promotions.exit, 0, "{}", promotions.stderr);
    assert_eq!(
        promotions.report["applied"],
        serde_json::json!({"path": "dev-to-prod", "sequence": 1})
    );
    assert_eq!(promotions.report["promotions"][0]["sequence"], 2);
    assert_eq!(promotions.report["promotions"][0]["path"], "dev-to-prod");
    let history = promotions.report["history"].as_array().unwrap();
    assert_eq!(
        history
            .iter()
            .map(|entry| entry["sequence"].clone())
            .collect::<Vec<_>>(),
        [2, 1]
    );
    assert_eq!(history[1]["from"]["desiredCommit"], first_from);
    assert_eq!(history[1]["reason"], "Release 1");
    assert!(history[1]["commit"].is_string());
}

/// A target that keeps its record in its source (`record: source`): the
/// promotion is one commit on the checked-out branch, by the invocation's
/// author, carrying the block, and writes no state; repeating it is
/// `unchanged`; a run reads the block from source and applies its sequence;
/// `status`, `get promotions`, and `promotion-candidates` show it as the
/// path's record. Catches a record written into state, a second block, a
/// run that ignores the block, and inspection blind to it.
#[test]
fn test_record_source_promotion_is_a_source_commit_that_runs_read() {
    let environments = record_source_environments();
    let harness = Promote::of(&environments, &format!("{UNITS}{BINDER}"), true);
    let (prod_desired, main) = (harness.shell.tip(PROD_DESIRED), harness.shell.tip("refs/heads/main"));

    let dry = harness.promote(&["dev-to-prod", "--dry-run"]);
    assert_eq!(dry.exit, 0, "{}", dry.stderr);
    assert_eq!(harness.shell.tip("refs/heads/main"), main, "a dry run writes nothing");

    let promoted = harness.promote(&["dev-to-prod", "--reason", "Release 1"]);
    assert_eq!(promoted.exit, 0, "{}", promoted.stderr);
    assert_eq!(promoted.report["record"]["sequence"], 1);
    let commit = promoted.report["commits"]["source"].as_str().unwrap().to_owned();
    assert_eq!(harness.shell.tip("refs/heads/main"), commit);
    assert_eq!(harness.shell.tip("refs/heads/main^"), main, "exactly one commit");
    // In prod's state only the marker: the same message, no record file.
    let marker = harness.shell.tip(PROD_DESIRED);
    assert_eq!(promoted.report["commits"]["desired"], marker.as_str());
    assert_eq!(harness.shell.tip(&format!("{marker}^")), prod_desired);
    assert_eq!(
        harness.shell.message(PROD_DESIRED),
        harness.shell.message("refs/heads/main")
    );
    assert_eq!(
        git_output(
            &harness.shell.world.bare(),
            &["diff", "--name-only", &prod_desired, &marker]
        ),
        ""
    );
    assert!(harness.prod_lease_released());
    let message = harness.shell.message("refs/heads/main");
    assert!(message.starts_with("nyl: promote prod (dev-to-prod #1)"), "{message}");
    assert!(message.contains("Nyl-Reason: Release 1"), "{message}");
    assert_eq!(
        git_output(
            &harness.shell.world.bare(),
            &["log", "-1", "--format=%an <%ae>", "main"]
        ),
        harness.shell.identity.to_string()
    );
    let declared = harness.shell.show("refs/heads/main", "nyl/environments.yaml");
    assert!(
        declared.contains("      promoted: # written by nyl promote"),
        "{declared}"
    );
    assert!(declared.contains("        sequence: 1\n"), "{declared}");
    assert_eq!(
        git_output(&harness.shell.world.bare(), &["diff", "--name-only", "main^", "main"]),
        "nyl/environments.yaml"
    );

    // The block in source names what was promoted: the same again is unchanged.
    let again = harness.promote(&["dev-to-prod"]);
    assert_eq!(again.exit, 0, "{}", again.stderr);
    assert_eq!(again.report["outcome"], "unchanged");
    assert_eq!(harness.shell.tip("refs/heads/main"), commit);
    assert_eq!(harness.shell.tip(PROD_DESIRED), marker, "the marker is there already");

    let applied = harness.shell.invoke(&["reconcile", "-e", "prod", "-o", "json"]);
    assert_eq!(applied.exit, 0, "{}", applied.stderr);
    let record = harness.shell.show(PROD_DESIRED, "desired/environment.yaml");
    assert!(record.contains("appliedPromotionSequence: 1"), "{record}");
    assert!(
        record.contains(promoted.report["record"]["sourceCommit"].as_str().unwrap()),
        "{record}"
    );

    let status = harness.shell.invoke(&["status", "-e", "prod", "-o", "json"]);
    let lineage = &status.report["environments"][0]["promotion"];
    assert_eq!(lineage["sequence"], 1);
    assert_eq!(lineage["from"], promoted.report["record"]["from"]);
    let promotions = harness.get(&["promotions", "-e", "prod"]);
    assert_eq!(promotions.exit, 0, "{}", promotions.stderr);
    assert_eq!(promotions.report["promotions"][0]["sequence"], 1);
    let candidates = harness.get(&["promotion-candidates", "dev-to-prod"]);
    assert_eq!(candidates.exit, 0, "{}", candidates.stderr);
    let marked = |key: &str| -> Vec<serde_json::Value> {
        candidates.report["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|candidate| candidate[key] == true)
            .map(|candidate| candidate["desiredCommit"].clone())
            .collect()
    };
    let from = promoted.report["record"]["from"]["desiredCommit"].clone();
    assert_eq!(marked("promoted"), std::slice::from_ref(&from));
    assert_eq!(marked("running"), std::slice::from_ref(&from));

    // A newer promotion in source waits for its run: status names it.
    harness.release("2");
    let second = harness.promote(&["dev-to-prod", "--reason", "Release 2"]);
    assert_eq!(second.exit, 0, "{}", second.stderr);
    assert_eq!(second.report["record"]["sequence"], 2);
    let status = harness.shell.invoke(&["status", "-e", "prod", "-o", "json"]);
    let lineage = &status.report["environments"][0]["promotion"];
    assert_eq!(lineage["sequence"], 1);
    assert_eq!(lineage["pendingSequence"], 2);
}

/// A block that does not match the state it names, such as one edited by
/// hand to another image, is refused by the run, which writes nothing.
/// Catches a block in source taken on trust.
#[test]
fn test_a_record_source_block_that_does_not_match_its_state_is_refused() {
    let environments = record_source_environments();
    let harness = Promote::of(&environments, &format!("{UNITS}{BINDER}"), true);
    let promoted = harness.promote(&["dev-to-prod"]);
    assert_eq!(promoted.exit, 0, "{}", promoted.stderr);
    let declared = harness.shell.show("refs/heads/main", "nyl/environments.yaml");
    let image = promoted.report["record"]["values"]["webImage"]["value"]
        .as_str()
        .unwrap();
    harness.shell.world.commit_files(
        "main",
        "Edit the block by hand",
        &[(
            "nyl/environments.yaml",
            &declared.replace(image, "registry.invalid:5000/web@sha256:0000"),
        )],
    );
    let before = harness.shell.tip(PROD_DESIRED);
    let run = harness.shell.invoke(&["reconcile", "-e", "prod"]);
    assert_eq!(run.exit, 1, "{}", run.stderr);
    assert!(run.stderr.contains("NYL-PROMOTION-RECORD-INVALID"), "{}", run.stderr);
    assert_eq!(harness.shell.tip(PROD_DESIRED), before);
}

/// Two environments that share a desired ref (`spec.state.path`) share its
/// history, but not their promotions: a promotion into one neither raises
/// the other's sequence nor appears in its `get promotions` history.
/// Catches sequences and histories read from every `promote` commit on the
/// ref.
#[test]
fn test_environments_sharing_a_ref_keep_their_own_promotions() {
    let shared = |name: &str| {
        format!("  state: {{desiredRef: nyl/shared/desired, observedRef: nyl/shared/observed, path: {name}}}\n")
    };
    let environments = ENVIRONMENTS
        .replace(
            "  source: {fromPromotion: {path: dev-to-prod}}\n",
            &format!("  source: {{fromPromotion: {{path: dev-to-prod}}}}\n{}", shared("prod")),
        )
        .replace("metadata: {name: values}\nspec:\n  from: {environment: dev}\n  to: {environment: prod}", "metadata: {name: values}\nspec:\n  from: {environment: dev}\n  to: {environment: staging}")
        + &format!(
            "---\napiVersion: gitops.nyl/v1\nkind: Environment\nmetadata: {{name: staging}}\nspec:\n  unitSelector: {{matchLabels: {{staging: 'true'}}}}\n  allowUnprotectedSource: true\n{}",
            shared("staging")
        );
    let harness = Promote::of(&environments, UNITS, true);
    assert_eq!(harness.shell.invoke(&["state", "init", "-e", "staging"]).exit, 0);
    for _ in 0..2 {
        let staged = harness.promote(&["values"]);
        assert_eq!(staged.exit, 0, "{}", staged.stderr);
        harness.release(&format!("{}", staged.report["record"]["sequence"]));
    }
    let promoted = harness.promote(&["dev-to-prod"]);
    assert_eq!(promoted.exit, 0, "{}", promoted.stderr);
    assert_eq!(
        promoted.report["record"]["sequence"], 1,
        "staging's promotions are not prod's"
    );
    let promotions = harness.get(&["promotions", "-e", "prod"]);
    let history = promotions.report["history"].as_array().unwrap();
    assert_eq!(history.len(), 1, "{history:?}");
    assert_eq!(history[0]["path"], "dev-to-prod");
    let staging = harness.get(&["promotions", "-e", "staging"]);
    assert_eq!(staging.report["history"].as_array().unwrap().len(), 2);
}

/// prod with `record: source`, its `fromPromotion` in block style.
fn record_source_environments() -> String {
    ENVIRONMENTS.replace(
        "  source: {fromPromotion: {path: dev-to-prod}}\n",
        "  source:\n    fromPromotion:\n      path: dev-to-prod\n      record: source\n",
    )
}

/// A writer that answers what it is told, and counts what it was asked.
struct FakeWriter {
    error: Option<nyl_core::orchestration::ports::RecordWriteError>,
    calls: std::sync::Mutex<Vec<&'static str>>,
    /// Runs once a commit succeeded, such as to take the target's lease away.
    after_commit: Option<Box<dyn Fn() + Send + Sync>>,
}

impl FakeWriter {
    fn answering(error: Option<nyl_core::orchestration::ports::RecordWriteError>) -> Self {
        Self {
            error,
            calls: std::sync::Mutex::default(),
            after_commit: None,
        }
    }

    fn calls(&self) -> Vec<&'static str> {
        self.calls.lock().unwrap().clone()
    }
}

impl nyl_core::orchestration::ports::PromotionRecordWriter for FakeWriter {
    fn check(
        &self,
        _record: &nyl_core::orchestration::promotion::PromotionRecord,
    ) -> Result<(), nyl_core::orchestration::ports::RecordWriteError> {
        self.calls.lock().unwrap().push("check");
        self.error.clone().map_or(Ok(()), Err)
    }

    fn published(&self) -> Result<(), nyl_core::orchestration::ports::RecordWriteError> {
        self.calls.lock().unwrap().push("published");
        Ok(())
    }

    fn commit_record(
        &self,
        _record: &nyl_core::orchestration::promotion::PromotionRecord,
        _message: &str,
    ) -> Result<nyl_core::orchestration::ids::CommitId, nyl_core::orchestration::ports::RecordWriteError> {
        self.calls.lock().unwrap().push("commit");
        match &self.error {
            Some(error) => Err(error.clone()),
            None => {
                if let Some(after) = &self.after_commit {
                    after();
                }
                Ok(nyl_core::orchestration::ids::CommitId::new("c".repeat(40)).unwrap())
            }
        }
    }
}

/// `nyl promote` of a `record: source` target through the shell, with `writer`.
fn promote_with(
    harness: &Promote,
    writer: Option<&FakeWriter>,
    dry_run: bool,
) -> nyl_orchestration::report::PromoteReport {
    use nyl_orchestration::shell::promote::{promote, PromoteOptions};
    let prod = EnvironmentName::new("prod").unwrap();
    let mut job = harness.shell.job_for(
        &prod,
        location(&harness.shell, "prod"),
        &[],
        nyl_orchestration::shell::run::Cancellation::default(),
    );
    for environment in ["dev", "prod"] {
        job.request.environments.insert(
            EnvironmentName::new(environment).unwrap(),
            location(&harness.shell, environment),
        );
    }
    let path: nyl_core::resources::promotion::PromotionPath = ENVIRONMENTS
        .split("---\n")
        .find(|document| document.contains("name: dev-to-prod"))
        .map(|document| serde_saphyr::from_str(document).unwrap())
        .unwrap();
    let options = PromoteOptions {
        path,
        select: nyl_orchestration::rules::promotion::Selection::Newest,
        evidence_published: false,
        ignore: Vec::new(),
        values: std::collections::BTreeSet::new(),
        verify_artifacts: false,
        dry_run,
        entry_units: ["svc", "binder"]
            .into_iter()
            .map(|unit| nyl_core::orchestration::ids::UnitName::new(unit).unwrap())
            .collect(),
        promotes_source: true,
        promoted: job
            .request
            .entry
            .spec
            .source
            .as_ref()
            .and_then(|source| source.from_promotion.clone()),
        requested: nyl_core::orchestration::promotion::PromotionRequest {
            by: "test".to_owned(),
            identity: nyl_core::orchestration::state::ApprovalIdentity::Asserted,
            source: None,
            at: harness.shell.clock.now(),
            reason: None,
        },
        supersede: Vec::new(),
        repository_names: nyl_orchestration::rules::promotion::supersede::RepositoryNames::default(),
    };
    harness.shell.with_context(&job, |context, request| {
        promote(
            context,
            request,
            &options,
            writer.map(|writer| writer as &dyn nyl_core::orchestration::ports::PromotionRecordWriter),
        )
    })
}

/// The shell maps each writer error to its code and exit: a moved branch is
/// exit 2 to retry, every other refusal exit 1 with its own fix; a refusal
/// carries no record, releases the target's lease, and writes no state; a
/// dry run only checks; a successful write is the commit the writer made.
/// Catches a refused push reported as written, a retryable refusal that
/// fails the pipeline, a lease left held, and a dry run that commits.
#[test]
fn test_writer_outcomes_map_to_codes_and_exits() {
    use nyl_core::orchestration::codes::Code;
    use nyl_core::orchestration::ports::RecordWriteError;
    let harness = Promote::of(&record_source_environments(), &format!("{UNITS}{BINDER}"), true);
    let prod_desired = harness.shell.tip(PROD_DESIRED);
    let cases = [
        (
            RecordWriteError::Detached,
            1,
            Code::PromoteSourceUnwritable,
            "check out the branch",
        ),
        (
            RecordWriteError::Rejected("non-fast-forward".to_owned()),
            2,
            Code::PromoteSourceRejected,
            "git pull --ff-only",
        ),
        (
            RecordWriteError::RemoteRejected("protected branch".to_owned()),
            1,
            Code::PromoteSourceUnwritable,
            "changeGate: pullRequest",
        ),
        (
            RecordWriteError::Ahead {
                branch: "main".to_owned(),
                upstream: "refs/remotes/origin/main".to_owned(),
                commits: 2,
            },
            1,
            Code::PromoteSourceUnwritable,
            "push those commits",
        ),
        (
            RecordWriteError::Dirty {
                file: "nyl/environments.yaml".to_owned(),
            },
            1,
            Code::PromoteSourceUnwritable,
            "commit or discard",
        ),
    ];
    for (error, exit, code, fix) in cases {
        let writer = FakeWriter::answering(Some(error.clone()));
        let report = promote_with(&harness, Some(&writer), false);
        assert_eq!(report.exit, exit, "{error:?}: {report:?}");
        assert_eq!(report.errors[0].code, code, "{error:?}");
        assert!(
            report.errors[0].fix.contains(fix),
            "{error:?}: {}",
            report.errors[0].fix
        );
        assert!(report.record.is_none(), "a refusal carries no record");
        assert!(harness.prod_lease_released(), "{error:?}");
        assert_eq!(writer.calls(), ["commit"]);
        assert_eq!(harness.shell.tip(PROD_DESIRED), prod_desired);
    }

    // A dry run checks, and commits nothing.
    let writer = FakeWriter::answering(Some(RecordWriteError::Detached));
    let dry = promote_with(&harness, Some(&writer), true);
    assert_eq!(dry.exit, 1);
    assert_eq!(dry.errors[0].code, Code::PromoteSourceUnwritable);
    assert_eq!(writer.calls(), ["check"]);
    let writer = FakeWriter::answering(None);
    let dry = promote_with(&harness, Some(&writer), true);
    assert_eq!(dry.exit, 0, "{dry:?}");
    assert_eq!(writer.calls(), ["check"]);

    // No writer for a record kept in source: nothing can be written.
    let none = promote_with(&harness, None, false);
    assert_eq!(none.exit, 1);
    assert!(harness.prod_lease_released());

    let writer = FakeWriter::answering(None);
    let written = promote_with(&harness, Some(&writer), false);
    assert_eq!(written.exit, 0, "{written:?}");
    assert_eq!(written.commits["source"].as_str(), "c".repeat(40));
    // Only the marker: one desired commit that changes no document.
    let marker = harness.shell.tip(PROD_DESIRED);
    assert_eq!(written.commits["desired"].as_str(), marker);
    assert_eq!(
        git_output(&harness.shell.world.bare(), &["rev-parse", &format!("{marker}^")]),
        prod_desired
    );
    assert_eq!(
        git_output(
            &harness.shell.world.bare(),
            &["diff", "--name-only", &prod_desired, &marker]
        ),
        "",
        "the marker carries no record file"
    );
}

/// The marker of a `record: source` promotion in the target's state: a
/// checkout on another branch, whose Environment lacks the block the first
/// promotion pushed, takes the next sequence rather than the same one,
/// because the marker's summary is in prod's desired history; and `nyl get
/// promotions -e prod` lists both promotions in its history, while prod's
/// snapshot and `status` lineage stay what they were. Catches two branches
/// promoting under one sequence, and a history blind to source records.
#[test]
fn test_record_source_markers_reserve_the_sequence_and_show_in_history() {
    let harness = Promote::of(&record_source_environments(), &format!("{UNITS}{BINDER}"), true);
    let records_before = harness.shell.invoke(&["get", "promotions", "-e", "prod", "-o", "json"]);
    assert_eq!(records_before.exit, 0, "{}", records_before.stderr);
    let writer = FakeWriter::answering(None);
    let first = promote_with(&harness, Some(&writer), false);
    assert_eq!(first.exit, 0, "{first:?}");
    assert_eq!(first.record.as_ref().unwrap().sequence, 1);
    // The other branch's checkout still declares no block.
    let other = promote_with(&harness, Some(&writer), false);
    assert_eq!(other.exit, 0, "{other:?}");
    assert_eq!(other.record.as_ref().unwrap().sequence, 2, "the sequence is reserved");
    assert_eq!(writer.calls(), ["commit", "commit"]);

    let listed = harness.shell.invoke(&["get", "promotions", "-e", "prod", "-o", "json"]);
    assert_eq!(listed.exit, 0, "{}", listed.stderr);
    let history = listed.report["history"].as_array().unwrap();
    let entries: Vec<(u64, &str)> = history
        .iter()
        .map(|entry| (entry["sequence"].as_u64().unwrap(), entry["commit"].as_str().unwrap()))
        .collect();
    assert_eq!(
        entries,
        [
            (2, other.commits["desired"].as_str()),
            (1, first.commits["desired"].as_str())
        ]
    );
    assert_eq!(history[0]["path"], "dev-to-prod");
    // No record in state: the snapshot is what it was.
    assert_eq!(listed.report["promotions"], records_before.report["promotions"]);
    assert_eq!(listed.report["applied"], records_before.report["applied"]);
}

/// The source commit is pushed before the marker, so a marker that cannot
/// be written leaves a promotion that stands: the report says `done` with
/// the source commit, and exits 1 with `NYL-PROMOTE-MARKER-UNWRITTEN`, whose
/// fix is to promote again. Catches a failed marker reported as a failed
/// promotion (a retry would then write a second block) or not at all.
#[test]
fn test_a_marker_that_cannot_be_written_leaves_the_promotion_standing() {
    let harness = Promote::of(&record_source_environments(), &format!("{UNITS}{BINDER}"), true);
    let prod_desired = harness.shell.tip(PROD_DESIRED);
    let bare = harness.shell.world.bare();
    let lease = format!("{}prod/lease", location(&harness.shell, "prod").coordination_ref_prefix);
    let mut writer = FakeWriter::answering(None);
    writer.after_commit = Some(Box::new(move || {
        git_output(&bare, &["update-ref", "-d", &lease]);
    }));
    let report = promote_with(&harness, Some(&writer), false);
    assert_eq!(report.exit, 1, "{report:?}");
    assert_eq!(report.outcome, nyl_orchestration::report::Outcome::Done);
    assert_eq!(report.commits["source"].as_str(), "c".repeat(40));
    assert!(report.record.is_some());
    let error = &report.errors[0];
    assert_eq!(error.code, nyl_core::orchestration::codes::Code::PromoteMarkerUnwritten);
    assert!(error.fix.contains("nyl promote dev-to-prod"), "{}", error.fix);
    assert_eq!(harness.shell.tip(PROD_DESIRED), prod_desired, "no marker");
}

/// `environments` with `record`'s block placed into prod's Environment.
fn with_block(environments: &str, record: &nyl_core::orchestration::promotion::PromotionRecord) -> String {
    let documents: Vec<String> = environments
        .split("---\n")
        .map(|document| {
            if document.contains("name: prod}") && document.contains("record: source") {
                nyl::orchestration::record_source::place_block(document, record).unwrap()
            } else {
                document.to_owned()
            }
        })
        .collect();
    let placed = documents.join("---\n");
    assert_ne!(placed, environments);
    placed
}

/// The record `nyl promote dev-to-prod` would write now.
fn dry_record(harness: &Promote) -> nyl_core::orchestration::promotion::PromotionRecord {
    let dry = harness.promote(&["dev-to-prod", "--dry-run"]);
    assert_eq!(dry.exit, 0, "{}", dry.stderr);
    serde_json::from_value(dry.report["record"].clone()).unwrap()
}

/// `verify` end to end. A block whose last change is unsigned is refused
/// by the next run of prod (`NYL-PROMOTION-RECORD-UNSIGNED`, naming that
/// commit), through the verifier every command that resolves prod is
/// given; and `nyl promote` from a clone whose commits a run would not
/// trust is refused before anything is pushed
/// (`NYL-PROMOTE-SOURCE-UNWRITABLE`). Catches a run that trusts an
/// unsigned block, and an untrusted promotion pushed.
#[test]
fn test_verify_refuses_unsigned_blocks_before_pushing_and_when_running() {
    let environments = record_source_environments().replace(
        "      record: source\n",
        "      record: source\n      verify: {allowedSigners: allowed_signers}\n",
    );
    // The record is taken before `verify` is on: a dry run with `verify`
    // set refuses a clone that does not sign, as CI's runners do not.
    let harness = Promote::of(&record_source_environments(), &format!("{UNITS}{BINDER}"), true);
    let record = dry_record(&harness);
    harness.shell.world.commit_files(
        "main",
        "Promote dev-to-prod #1 without a signature",
        &[
            ("nyl/environments.yaml", &with_block(&environments, &record)),
            ("nyl/allowed_signers", ""),
        ],
    );
    let unsigned_commit = harness.shell.tip("refs/heads/main");
    let refused = harness.shell.invoke(&["reconcile", "-e", "prod"]);
    assert_eq!(refused.exit, 1, "{}", refused.stderr);
    assert!(
        refused.stderr.contains("NYL-PROMOTION-RECORD-UNSIGNED"),
        "{}",
        refused.stderr
    );
    assert!(refused.stderr.contains(&unsigned_commit), "{}", refused.stderr);
    assert!(refused.stderr.contains("not signed"), "{}", refused.stderr);

    // The allowed signers file lists nobody: whatever this clone signs
    // with, the promotion commit would not be trusted, so it is not pushed.
    harness.release("2");
    let main = harness.shell.tip("refs/heads/main");
    let prod_desired = harness.shell.tip(PROD_DESIRED);
    let untrusted = harness.promote(&["dev-to-prod", "--reason", "Release 2"]);
    assert_eq!(untrusted.exit, 1, "{}", untrusted.stderr);
    assert!(
        untrusted.stderr.contains("NYL-PROMOTE-SOURCE-UNWRITABLE"),
        "{}",
        untrusted.stderr
    );
    assert_eq!(harness.shell.tip("refs/heads/main"), main, "nothing pushed");
    assert_eq!(harness.shell.tip(PROD_DESIRED), prod_desired, "no marker");
}

/// A block pushed without its marker, such as after a marker that failed:
/// running `nyl promote` again finds the block `unchanged` and writes only
/// the marker, under the re-runner's `Nyl-Reason`, and `nyl get
/// promotions` then lists it. Catches a retry that writes a second block,
/// and a marker never repaired.
#[test]
fn test_a_rerun_writes_the_missing_marker() {
    let harness = Promote::of(&record_source_environments(), &format!("{UNITS}{BINDER}"), true);
    let record = dry_record(&harness);
    // The block lands in source by hand, as a pushed promotion whose marker failed would leave it.
    harness.shell.world.commit_files(
        "main",
        "Promote dev-to-prod #1",
        &[(
            "nyl/environments.yaml",
            &with_block(&record_source_environments(), &record),
        )],
    );
    let main = harness.shell.tip("refs/heads/main");
    let prod_desired = harness.shell.tip(PROD_DESIRED);

    let rerun = harness.promote(&["dev-to-prod", "--reason", "Write the missing marker"]);
    assert_eq!(rerun.exit, 0, "{}", rerun.stderr);
    assert_eq!(rerun.report["outcome"], "unchanged");
    assert_eq!(harness.shell.tip("refs/heads/main"), main, "no second block");
    let marker = harness.shell.tip(PROD_DESIRED);
    assert_ne!(marker, prod_desired);
    assert_eq!(rerun.report["commits"]["desired"], marker.as_str());
    let message = harness.shell.message(PROD_DESIRED);
    assert!(message.contains("Nyl-Reason: Write the missing marker"), "{message}");
    let listed = harness.get(&["promotions", "-e", "prod"]);
    assert_eq!(listed.report["history"][0]["sequence"], 1);
    assert_eq!(listed.report["history"][0]["commit"], marker.as_str());
    // Once there, a re-run writes nothing.
    let again = harness.promote(&["dev-to-prod"]);
    assert_eq!(again.exit, 0, "{}", again.stderr);
    assert_eq!(harness.shell.tip(PROD_DESIRED), marker);
}

/// After switching prod from `record: state` to `record: source`, the
/// state's record is no record of the path: the first promotion writes the
/// block instead of finding the state's record `unchanged`. Catches a
/// switched environment that can never be promoted into its source.
#[test]
fn test_switching_to_record_source_writes_the_first_block() {
    let harness = Promote::new();
    let first = harness.promote(&["dev-to-prod"]);
    assert_eq!(first.exit, 0, "{}", first.stderr);
    let applied = harness.shell.invoke(&["reconcile", "-e", "prod"]);
    assert_eq!(applied.exit, 0, "{}", applied.stderr);
    harness.shell.world.commit_files(
        "main",
        "Keep prod's record in source",
        &[("nyl/environments.yaml", &record_source_environments())],
    );
    let switched = harness.promote(&["dev-to-prod"]);
    assert_eq!(switched.exit, 0, "{}", switched.stderr);
    assert_eq!(switched.report["outcome"], "done");
    assert_eq!(switched.report["record"]["sequence"], 2);
    assert!(switched.report["commits"]["source"].is_string());
}

/// prod's source from two paths out of dev, so one path can promote an
/// older state than the other recorded.
fn two_source_paths() -> String {
    ENVIRONMENTS
        .replace(
            "source: {fromPromotion: {path: dev-to-prod}}",
            "source: {fromPromotion: {paths: [dev-to-prod, hotfix-to-prod]}}",
        )
        .replace(
            "metadata: {name: attested}\nspec:\n  from: {environment: dev}\n  to: {environment: prod}\n  evidence: attested",
            "metadata: {name: hotfix-to-prod}\nspec:\n  from: {environment: dev}\n  to: {environment: prod}",
        )
}

/// The supersede check end to end: after another path promoted a newer
/// source commit, promoting an older state is refused with
/// `NYL-PROMOTE-SUPERSEDES` (exit 2) naming the missing commit and writing
/// nothing; a `--supersede` entry without a reason is refused with
/// `NYL-PROMOTE-SUPERSEDE-REASON` (exit 1) before anything is read; with
/// `--reason` the entry passes and the record lists the commit, its reason,
/// and who gave it. Catches a promotion that silently drops a hotfix, and a
/// reason-less supersede recorded.
#[test]
fn test_supersede_refuses_missing_commits_and_records_the_override() {
    let harness = Promote::of(&two_source_paths(), &format!("{UNITS}{BINDER}"), true);
    let older = harness.shell.tip(DEV_DESIRED);
    harness.release("2");
    let hotfix = harness.promote(&["hotfix-to-prod", "--reason", "Hotfix"]);
    assert_eq!(hotfix.exit, 0, "{}", hotfix.stderr);
    let fix = hotfix.report["record"]["sourceCommit"].as_str().unwrap().to_owned();

    let before = harness.shell.tip(PROD_DESIRED);
    let refused = harness.promote(&["dev-to-prod", "--state-revision", &older]);
    assert_eq!(refused.exit, 2, "{}", refused.stderr);
    assert!(refused.stderr.contains("NYL-PROMOTE-SUPERSEDES"), "{}", refused.stderr);
    assert!(refused.stderr.contains("release 2"), "{}", refused.stderr);
    assert!(refused.stderr.contains("hotfix-to-prod #1"), "{}", refused.stderr);
    let missing = &refused.report["missing"][0];
    assert_eq!(missing["path"], "hotfix-to-prod");
    assert_eq!(missing["commits"][0]["commit"], fix.as_str());
    assert_eq!(missing["commits"][0]["subject"], "release 2");
    assert_eq!(harness.shell.tip(PROD_DESIRED), before, "nothing was written");

    let reasonless = harness.promote(&["dev-to-prod", "--state-revision", &older, "--supersede", &fix]);
    assert_eq!(reasonless.exit, 1, "{}", reasonless.stderr);
    assert!(
        reasonless.stderr.contains("NYL-PROMOTE-SUPERSEDE-REASON"),
        "{}",
        reasonless.stderr
    );
    assert_eq!(harness.shell.tip(PROD_DESIRED), before);

    let accepted = harness.promote(&[
        "dev-to-prod",
        "--state-revision",
        &older,
        "--supersede",
        &fix,
        "--reason",
        "Roll back the hotfix",
    ]);
    assert_eq!(accepted.exit, 0, "{}", accepted.stderr);
    let superseded = &accepted.report["record"]["superseded"];
    assert_eq!(superseded.as_array().unwrap().len(), 1, "{superseded}");
    assert_eq!(superseded[0]["commit"], fix.as_str());
    assert_eq!(superseded[0]["reason"], "Roll back the hotfix");
    assert!(superseded[0]["by"].is_string());

    // dev-to-prod has now promoted after the hotfix: its next promotion is
    // no longer checked against it.
    let newer = harness.promote(&["dev-to-prod"]);
    assert_eq!(newer.exit, 0, "{}", newer.stderr);
    assert_eq!(newer.report["record"]["superseded"], serde_json::json!([]));
}

/// `nyl get promotions` names the record each binding reads: a path-less
/// binding in an environment with several source paths follows the record
/// that supplied the source. Catches inspection that shows another record
/// than the one a run resolves.
#[test]
fn test_get_promotions_shows_each_bindings_supplier() {
    let harness = Promote::of(&two_source_paths(), &format!("{UNITS}{BINDER}"), true);
    assert_eq!(harness.promote(&["dev-to-prod"]).exit, 0);
    let run = harness.shell.invoke(&["reconcile", "-e", "prod"]);
    assert_eq!(run.exit, 0, "{}", run.stderr);
    harness.release("2");
    assert_eq!(harness.promote(&["hotfix-to-prod", "--reason", "Hotfix"]).exit, 0);
    let shown = harness.shell.invoke(&["get", "promotions", "-e", "prod", "-o", "json"]);
    assert_eq!(shown.exit, 0, "{}", shown.stderr);
    assert_eq!(
        shown.report["source"],
        serde_json::json!({"path": "hotfix-to-prod", "sequence": 2})
    );
    let bindings = shown.report["bindings"].as_array().unwrap();
    let binder = bindings.iter().find(|binding| binding["unit"] == "binder").unwrap();
    assert_eq!(binder["value"], "webImage");
    assert_eq!(
        binder["supplier"],
        serde_json::json!({"path": "hotfix-to-prod", "sequence": 2})
    );
    let status = harness.shell.invoke(&["status", "-e", "prod", "-o", "json"]);
    assert!(status.stdout.contains("\"pendingSequence\": 2"), "{}", status.stdout);
}

/// Decision D8, the 12:00/12:35 story: at 12:00 dev's state S5 passed
/// `accepted` and `healthy`; then R6 was published, and the newest
/// attestation, the one of R6's receipt, fails while both Applications are
/// still on R5's publication and Degraded. The publication's attestations
/// say what runs now, so only the current receipt's count: `promote`
/// refuses with the driver's reason and does not fall back to S5, whose
/// attestations no longer describe what runs. `--state-revision S5` is
/// still allowed on the attestations recorded for S5. Catches a promotion
/// that takes an older state through a publication attestation that once
/// passed.
#[test]
fn test_a_failing_newest_publication_attestation_blocks_without_falling_back() {
    let text = r#"contract: design/promotion.md#promoting-an-environments-source
milestone: M6
tiers: [1]
publication: fake
steps:
  - nyl: [state, init, -e, dev]
    exit: 0
  - nyl: [state, init, -e, prod]
    exit: 0
  - nyl: [reconcile, -e, dev]
    id: s5
    exit: 0
  - argocd: {target: dev, all: {runs: s5}}
  - nyl: [attest, -e, dev]
    exit: 0
  - commit: {branch: main, message: "Release 6", apply: patches/release-2}
  - nyl: [reconcile, -e, dev]
    id: r6
    exit: 0
  - argocd: {target: dev, all: {runs: s5, health: Degraded}}
  - nyl: [attest, -e, dev]
    exit: 3
    expect:
      codes: [NYL-ATTESTATION-FAILED]
  - nyl: [promote, dev-to-prod]
    exit: 2
    expect:
      codes: [NYL-PROMOTE-NO-EVIDENCE]
      messages: [other-publication, --state-revision]
      commits: {desired: 0, observed: 0}
  - nyl: [promote, dev-to-prod, --state-revision, '%STATE(s5)%', --reason, "Roll back to S5"]
    exit: 0
    expect:
      commits: {desired: 1, observed: 0}
      promotion:
        path: dev-to-prod
        sequence: 1
        sourceCommit: main~1
        from: {environment: dev, run: s5}
        evidence: {level: attested, override: false, attestations: [kubernetes/accepted, kubernetes/healthy]}
"#;
    let loaded = scenario::Loaded {
        name: "inline/d8-what-runs-now".to_owned(),
        directory: scenario::scenarios_root().join("walkthroughs/value-only-promotion"),
        scenario: serde_saphyr::from_str(text).unwrap(),
    };
    scenario::format::validate(&loaded.scenario, &loaded.directory, &scenario::repository_root()).unwrap();
    scenario::runner::run(&loaded).unwrap();
}
