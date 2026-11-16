//! Rule tests over synthetic histories. dev records states of two units,
//! `web-image` (an artifact and an output) and `kubernetes` (which declares
//! `accepted` and `healthy`, and whose resolved spec holds the Release
//! input), and declares the environment-wide attestation `qa`.

use std::collections::{BTreeMap, BTreeSet};

use nyl_core::orchestration::driver::{DeploymentRuns, DeploymentVerdict};
use nyl_core::orchestration::ids::{CommitId, EnvironmentName, PromotionPathName, UnitName};
use nyl_core::orchestration::ports::EnvironmentSnapshot;
use nyl_core::orchestration::promotion::{EvidenceLevel, PromotionFrom, PromotionRecord, PromotionRequest};
use nyl_core::orchestration::state::{
    ApprovalIdentity, Attestation, AttestationKind, AttestationResult, EnvironmentRecord, EnvironmentRecordKind,
    SourceChoice, StateV1,
};
use nyl_core::orchestration::time::Timestamp;
use nyl_core::resources::promotion::PromotionPath;
use serde_json::{json, Value};

use super::*;
use crate::rules::fixtures::{artifact, desired, key, listed, name, observed, receipt, run};
use crate::run_report::ExitCategory;

const NOW: &str = "2026-09-25T12:00:00Z";

fn at(text: &str) -> Timestamp {
    text.parse().unwrap()
}

fn commit(character: char) -> CommitId {
    CommitId::new(character.to_string().repeat(40)).unwrap()
}

fn dev() -> EnvironmentName {
    EnvironmentName::new("dev").unwrap()
}

fn run_id(number: u8) -> nyl_core::orchestration::ids::RunId {
    run(&format!("0192f0c4-8d0a-7c3e-9a51-3c1f0e7d2b{number:02x}"))
}

/// What one unit looks like in a state.
#[derive(Clone, Copy)]
enum Receipt {
    /// A current receipt for this image.
    Current,
    /// The desired document names this image's key, but nothing finished.
    Missing,
    /// The desired document names this image's key, and the receipt is still
    /// the one for this other image: the run did not finish this unit.
    Stale(&'static str),
}

/// A recorded state of dev: run `number`, desired commit `number` as a
/// character, ran source commit `source`, with image `image`.
struct StateBuilder {
    number: u8,
    source: char,
    image: String,
    web_image: Receipt,
    kubernetes: Receipt,
    chosen_by: SourceChoice,
    attestations: StateAttestations,
    running: BTreeMap<UnitName, UnitRunning>,
}

fn state(number: u8, source: char, image: &str) -> StateBuilder {
    StateBuilder {
        number,
        source,
        image: image.to_owned(),
        web_image: Receipt::Current,
        kubernetes: Receipt::Current,
        chosen_by: SourceChoice::Entry,
        attestations: StateAttestations::default(),
        running: BTreeMap::new(),
    }
}

fn desired_commit_of(number: u8) -> CommitId {
    CommitId::new(format!("{:040x}", 0xd0 + u32::from(number))).unwrap()
}

fn web_image_seed(image: &str) -> String {
    format!("web-image-{image}")
}

fn kubernetes_seed(image: &str) -> String {
    format!("kubernetes-{image}")
}

impl StateBuilder {
    fn without_receipt(mut self, unit: &str) -> Self {
        match unit {
            "web-image" => self.web_image = Receipt::Missing,
            _ => self.kubernetes = Receipt::Missing,
        }
        self
    }

    fn worktree(mut self) -> Self {
        self.chosen_by = SourceChoice::Override {
            revision: "refs/nyl-worktrees/main/abc".to_owned(),
        };
        self
    }

    /// The environment-wide attestation `name` for this state.
    fn env_attestation(mut self, attested: &str, result: AttestationResult, recorded: &str) -> Self {
        let commit = desired_commit_of(self.number).to_string();
        self.attestations.environment.insert(
            attested.to_owned(),
            attestation(attested, &commit, result, recorded, Value::Null),
        );
        self
    }

    /// `kubernetes`'s `accepted` and `healthy` are running-scoped, as its
    /// driver declares them: its receipt published `publication`, and each
    /// attestation recorded for it says `verdicts` about the deployments.
    fn running(mut self, publication: &str, verdicts: &[DeploymentVerdict]) -> Self {
        let recorded: Vec<String> = self
            .attestations
            .units
            .get(&name("kubernetes"))
            .map(|recorded| recorded.keys().cloned().collect())
            .unwrap_or_default();
        self.running.insert(
            name("kubernetes"),
            UnitRunning {
                names: BTreeSet::from(["accepted".to_owned(), "healthy".to_owned()]),
                publication: Some(publication.to_owned()),
                verdicts: recorded
                    .into_iter()
                    .map(|attested| (attested, verdicts.to_vec()))
                    .collect(),
            },
        );
        self
    }

    /// `kubernetes`'s `accepted` and `healthy` for this state's receipt,
    /// with `result`, running-scoped and saying `verdicts`.
    fn observed(self, result: AttestationResult, publication: &str, verdicts: &[DeploymentVerdict]) -> Self {
        self.kubernetes_attestation("accepted", result, FRESH, Value::Null)
            .kubernetes_attestation("healthy", result, FRESH, Value::Null)
            .running(publication, verdicts)
    }

    /// `kubernetes`'s attestation for this state's receipt.
    fn kubernetes_attestation(
        mut self,
        attested: &str,
        result: AttestationResult,
        recorded: &str,
        details: Value,
    ) -> Self {
        let seed = key(&kubernetes_seed(&self.image)).to_string();
        self.attestations.units.entry(name("kubernetes")).or_default().insert(
            attested.to_owned(),
            attestation(attested, &seed, result, recorded, details),
        );
        self
    }

    fn build(self) -> RecordedState {
        let image = self.image.clone();
        let mut snapshot = EnvironmentSnapshot::default();
        let image_artifact = artifact(
            "web-image",
            "image",
            "OciImage",
            &web_image_seed(&image),
            json!({"reference": image}),
        );
        let mut web_image = desired(
            "web-image",
            "Command",
            json!({"outputs": {"digest": {"type": "string"}}}),
            &web_image_seed(&image),
        );
        web_image.resolved_spec = Some(json!({"source": {"path": "/app", "commit": commit(self.source)}}));
        let mut kubernetes = desired(
            "kubernetes",
            "Command",
            json!({"declares": ["accepted", "healthy"]}),
            &kubernetes_seed(&image),
        );
        kubernetes.attestations = vec!["accepted".to_owned(), "healthy".to_owned()];
        kubernetes.resolved_spec =
            Some(json!({"releases": {"web/web": {"image": image}, "web/api": {"image": image}}}));
        snapshot.desired.insert(name("web-image"), web_image);
        snapshot.desired.insert(name("kubernetes"), kubernetes);
        let finished = |seed: &str, outputs: Value, listed_artifacts| receipt(seed, outputs, listed_artifacts);
        let web_receipt = match self.web_image {
            Receipt::Current => Some(finished(
                &web_image_seed(&image),
                json!({"digest": image}),
                vec![listed(&image_artifact)],
            )),
            Receipt::Missing | Receipt::Stale(_) => None,
        };
        if matches!(self.web_image, Receipt::Current) {
            snapshot.artifacts.insert(
                name("web-image"),
                BTreeMap::from([("image".to_owned(), image_artifact)]),
            );
        }
        snapshot
            .observed
            .insert(name("web-image"), observed("web-image", "Command", web_receipt));
        let kubernetes_receipt = match self.kubernetes {
            Receipt::Current => Some(finished(&kubernetes_seed(&image), json!({}), Vec::new())),
            Receipt::Stale(old) => Some(finished(&kubernetes_seed(old), json!({}), Vec::new())),
            Receipt::Missing => None,
        };
        snapshot.observed.insert(
            name("kubernetes"),
            observed("kubernetes", "Command", kubernetes_receipt),
        );
        snapshot.environment = Some(EnvironmentRecord {
            api_version: StateV1,
            kind: EnvironmentRecordKind,
            environment: dev(),
            spec: json!({"attestations": ["qa"]}),
            source_commit: commit(self.source),
            source_chosen_by: self.chosen_by,
            applied_promotion_sequence: None,
            ownership: BTreeMap::new(),
        });
        RecordedState {
            run: run_id(self.number),
            desired_commit: desired_commit_of(self.number),
            observed_commit: Some(commit(char::from(b'0' + self.number))),
            snapshot,
            attestations: self.attestations,
            running: self.running,
        }
    }
}

fn attestation(name: &str, key: &str, result: AttestationResult, recorded: &str, details: Value) -> Attestation {
    Attestation {
        api_version: StateV1,
        kind: AttestationKind,
        name: name.to_owned(),
        unit: None,
        execution_key: key.to_owned(),
        result,
        at: at(recorded),
        source: json!({"driver": "attest"}),
        reason: None,
        details,
    }
}

const FRESH: &str = "2026-09-25T11:30:00Z";
const OLD: &str = "2026-09-25T08:00:00Z";

/// Argo CD's reading when every Application runs publication `p`.
fn running(publication: &str) -> Value {
    json!({"deployments": [
        {"id": "web/web", "state": "accepted", "health": "Healthy", "publication": publication},
        {"id": "web/api", "state": "accepted", "health": "Healthy", "publication": publication},
    ]})
}

#[allow(clippy::needless_pass_by_value)] // Builders take the literal under test.
fn path(spec: Value) -> PromotionPath {
    let mut base = json!({
        "from": {"environment": "dev"}, "to": {"environment": "prod"},
        "values": {"webImage": {"select": {"unit": "web-image", "artifact": "image", "pointer": "/reference"}}}
    });
    for (field, value) in spec.as_object().unwrap() {
        base[field] = value.clone();
    }
    let path: PromotionPath = serde_json::from_value(json!({
        "apiVersion": "gitops.nyl/v1", "kind": "PromotionPath",
        "metadata": {"name": "dev-to-prod"}, "spec": base
    }))
    .unwrap();
    path.validate().unwrap();
    path
}

fn requested(reason: Option<&str>) -> PromotionRequest {
    PromotionRequest {
        by: "alice".to_owned(),
        identity: ApprovalIdentity::Verified,
        source: None,
        at: at(NOW),
        reason: reason.map(str::to_owned),
    }
}

fn request(select: Selection) -> Request {
    Request {
        select,
        evidence_published: false,
        ignore: Vec::new(),
        requested: requested(Some("Release")),
    }
}

/// An invocation: the path, dev's history (newest first), and the request.
struct Case {
    path: PromotionPath,
    history: Vec<RecordedState>,
    request: Request,
    promotes_source: bool,
    records: BTreeMap<PromotionPathName, PromotionRecord>,
    sequence: u64,
    consumers: BTreeMap<String, String>,
}

fn case(path: PromotionPath, history: Vec<StateBuilder>) -> Case {
    Case {
        path,
        history: history.into_iter().map(StateBuilder::build).collect(),
        request: request(Selection::Newest),
        promotes_source: true,
        records: BTreeMap::new(),
        sequence: 1,
        consumers: BTreeMap::new(),
    }
}

impl Case {
    fn input<'a>(&'a self, targets: &'a BTreeSet<UnitName>, source: &'a EnvironmentName) -> PromotionInput<'a> {
        PromotionInput {
            path: &self.path,
            source,
            history: &self.history,
            target_units: targets,
            promotes_source: self.promotes_source,
            consumers: &self.consumers,
            target_records: &self.records,
            sequence: self.sequence,
            now: at(NOW),
            request: &self.request,
        }
    }

    fn run(&self) -> Result<Decision, Refusal> {
        let targets = BTreeSet::from([name("web-image"), name("kubernetes")]);
        let source = dev();
        decide(&self.input(&targets, &source))
    }

    fn promoted(&self) -> Promotion {
        match self.run() {
            Ok(Decision::Promote(promotion)) => *promotion,
            other => panic!("expected a promotion: {other:?}"),
        }
    }

    fn refused(&self) -> Refusal {
        match self.run() {
            Err(refusal) => refusal,
            Ok(decision) => panic!("expected a refusal: {decision:?}"),
        }
    }

    fn evaluations(&self) -> Vec<StateEvaluation> {
        let targets = BTreeSet::from([name("web-image"), name("kubernetes")]);
        let source = dev();
        evaluate(&self.input(&targets, &source))
    }
}

fn from_run(record: &PromotionRecord) -> String {
    match &record.from {
        PromotionFrom::Environment(from) => from.run.to_string(),
        PromotionFrom::Target(_) => panic!("an environment source"),
    }
}

fn published() -> PromotionPath {
    path(json!({"evidence": "published"}))
}

fn attested() -> PromotionPath {
    path(json!({"evidence": "attested"}))
}

use AttestationResult::{Fail, Pass};

/// Missing evidence: with no recorded state nothing is promoted, exit 2 with
/// the stable code and a fix naming what to run.
#[test]
fn test_no_recorded_state_is_missing_evidence() {
    let refusal = case(published(), Vec::new()).refused();
    assert_eq!(refusal.finding.code, Code::PromoteNoEvidence);
    assert_eq!(refusal.exit, ExitCategory::Incomplete);
    assert!(
        refusal.finding.fix.contains("nyl reconcile -e dev"),
        "{}",
        refusal.finding.fix
    );
}

/// The stale evidence walkthrough: the newest state has no receipt for the
/// unit the path selects nor the one the target also selects, so the whole-source
/// rule passes over it and takes the older state's source commit and image.
#[test]
fn test_stale_newest_state_falls_back_to_the_newest_complete_one() {
    let history = vec![
        state(2, 'b', "sha256:two")
            .without_receipt("web-image")
            .without_receipt("kubernetes"),
        state(1, 'a', "sha256:one"),
    ];
    let case = case(published(), history);
    let promotion = case.promoted();
    let record = &promotion.record;
    assert_eq!(from_run(record), run_id(1).to_string());
    assert_eq!(record.source_commit, Some(commit('a')));
    assert_eq!(record.values["webImage"].value, json!("sha256:one"));
    assert_eq!(record.evidence.level, EvidenceLevel::Published);
    assert!(record.evidence.override_.is_none());
    // `get promotion-candidates` shows the newest state at no level, naming both units.
    let evaluations = case.evaluations();
    assert_eq!(evaluations[0].level, None);
    let missing: Vec<_> = evaluations[0].gaps.iter().map(|gap| gap.scope.as_str()).collect();
    assert_eq!(missing, ["kubernetes", "web-image"]);
    assert_eq!(evaluations[1].level, Some(EvidenceLevel::Published));
}

/// A state is judged whole: a unit the target selects without a current
/// receipt keeps a state below `published` even when the selected value's
/// unit is fine, and units the target does not select are not required.
#[test]
fn test_whole_source_covers_units_the_target_selects() {
    let history = vec![state(1, 'a', "sha256:one").without_receipt("kubernetes")];
    let case = case(published(), history);
    let refusal = case.refused();
    assert!(
        refusal.finding.message.contains("kubernetes has no current receipt"),
        "{}",
        refusal.finding.message
    );
    let targets = BTreeSet::from([name("web-image")]);
    let source = dev();
    assert!(matches!(
        decide(&case.input(&targets, &source)),
        Ok(Decision::Promote(_))
    ));
}

/// At `attested`, an environment attestation nobody recorded is missing
/// evidence naming it; once recorded for the state, the state qualifies.
#[test]
fn test_attested_needs_every_declared_attestation() {
    let pending = case(attested(), vec![state(1, 'a', "sha256:one")]);
    let refusal = pending.refused();
    assert!(
        refusal.finding.message.contains("environment/qa"),
        "{}",
        refusal.finding.message
    );
    assert_eq!(
        refusal.gaps.iter().filter(|gap| gap.kind == GapKind::Pending).count(),
        3
    );
    let recorded = case(
        attested(),
        vec![state(1, 'a', "sha256:one")
            .env_attestation("qa", Pass, FRESH)
            .kubernetes_attestation("accepted", Pass, FRESH, running("p1"))
            .kubernetes_attestation("healthy", Pass, FRESH, running("p1"))],
    );
    let promotion = recorded.promoted();
    let used: Vec<_> = promotion
        .record
        .evidence
        .attestations
        .iter()
        .map(|item| format!("{}/{}", item.scope, item.name))
        .collect();
    assert_eq!(used, ["environment/qa", "kubernetes/accepted", "kubernetes/healthy"]);
    assert_eq!(promotion.record.evidence.level, EvidenceLevel::Attested);
}

/// An environment attestation names the state's desired commit: one recorded
/// for another state never counts for this one.
#[test]
fn test_an_attestation_counts_only_for_the_state_it_names() {
    let mut builder = state(1, 'a', "sha256:one");
    builder.attestations.environment.insert(
        "qa".to_owned(),
        attestation("qa", desired_commit_of(9).as_str(), Pass, FRESH, Value::Null),
    );
    let path = path(json!({"evidence": "published", "attestations": {"environment": ["qa"]}}));
    let refusal = case(path, vec![builder]).refused();
    assert!(refusal
        .gaps
        .iter()
        .any(|gap| gap.kind == GapKind::Pending && gap.scope == "environment"));
}

/// The newest passing state wins for environment-wide attestations: the
/// newest state has no `qa`, an older one does.
#[test]
fn test_newest_passing_state_wins_for_environment_attestations() {
    let path = path(json!({"evidence": "attested", "attestations": {"units": {"kubernetes": [], "web-image": []}}}));
    let history = vec![
        state(2, 'b', "sha256:two"),
        state(1, 'a', "sha256:one").env_attestation("qa", Pass, FRESH),
    ];
    let promotion = case(path, history).promoted();
    assert_eq!(from_run(&promotion.record), run_id(1).to_string());
    assert_eq!(promotion.record.source_commit, Some(commit('a')));
}

/// A path's `attestations` replaces the unit's default: a unit named with an
/// empty list needs only its receipt, and at `published` a named attestation
/// is an added requirement.
#[test]
fn test_path_attestations_replace_at_attested_and_add_at_published() {
    let receipt_only =
        path(json!({"evidence": "attested", "attestations": {"units": {"kubernetes": []}, "environment": []}}));
    case(receipt_only, vec![state(1, 'a', "sha256:one")]).promoted();
    let added = path(json!({"evidence": "published", "attestations": {"units": {"kubernetes": ["accepted"]}}}));
    let refusal = case(added.clone(), vec![state(1, 'a', "sha256:one")]).refused();
    assert!(
        refusal.finding.message.contains("kubernetes/accepted"),
        "{}",
        refusal.finding.message
    );
    case(
        added,
        vec![state(1, 'a', "sha256:one").kubernetes_attestation("accepted", Pass, FRESH, running("p1"))],
    )
    .promoted();
}

/// `maxAttestationAge` ages attestations: an aged required one is missing
/// evidence that names the setting, while an equal attestation within the age
/// counts.
#[test]
fn test_aged_attestations_do_not_count() {
    let path = path(
        json!({"evidence": "attested", "maxAttestationAge": "1h", "attestations": {"units": {"kubernetes": [], "web-image": []}}}),
    );
    let aged = case(
        path.clone(),
        vec![state(1, 'a', "sha256:one").env_attestation("qa", Pass, OLD)],
    );
    let refusal = aged.refused();
    assert!(
        refusal.finding.message.contains("maxAttestationAge"),
        "{}",
        refusal.finding.message
    );
    assert_eq!(refusal.gaps[0].kind, GapKind::Aged);
    let fresh = case(
        path,
        vec![state(1, 'a', "sha256:one").env_attestation("qa", Pass, FRESH)],
    );
    fresh.promoted();
}

/// Age retires a pass, never a failure: an old failing attestation keeps
/// blocking until a later pass of the same name or an ignore entry clears it.
#[test]
fn test_an_aged_failing_attestation_keeps_blocking() {
    let path = path(json!({"evidence": "published", "maxAttestationAge": "1h"}));
    let history = vec![state(1, 'a', "sha256:one").env_attestation("qa", Fail, OLD)];
    let mut blocked = case(path, history);
    assert_eq!(blocked.refused().gaps[0].kind, GapKind::Failing);
    blocked.request.ignore = vec![IgnoreRequest::parse("environment/qa=known").unwrap()];
    assert_eq!(blocked.promoted().record.ignored_attestations.len(), 1);
}

/// A failing attestation blocks its state on every path, required or not,
/// and the older state without it is taken.
#[test]
fn test_a_failing_attestation_blocks_its_state_even_when_not_required() {
    let history = vec![
        state(2, 'b', "sha256:two").env_attestation("qa", Fail, FRESH),
        state(1, 'a', "sha256:one"),
    ];
    let promotion = case(published(), history).promoted();
    assert_eq!(from_run(&promotion.record), run_id(1).to_string());
    // With nothing older, the refusal names the failing attestation.
    let only = case(
        published(),
        vec![state(2, 'b', "sha256:two").env_attestation("qa", Fail, FRESH)],
    );
    let refusal = only.refused();
    assert_eq!(refusal.gaps[0].kind, GapKind::Failing);
    assert!(refusal.finding.fix.contains("--ignore-attestation"));
}

/// `--ignore-attestation` sets a failing attestation aside and the record
/// keeps who and why; `--evidence published` does not clear a failing
/// attestation; an entry that set nothing aside is reported, not recorded.
#[test]
fn test_ignored_attestations_are_recorded_and_unused_ones_reported() {
    let history = || vec![state(1, 'a', "sha256:one").env_attestation("qa", Fail, FRESH)];
    let mut ignoring = case(attested(), history());
    ignoring.request.evidence_published = true;
    assert_eq!(ignoring.refused().gaps[0].kind, GapKind::Failing);
    let path = path(json!({"evidence": "attested", "attestations": {"units": {"kubernetes": [], "web-image": []}}}));
    let mut ignoring = case(path, history());
    ignoring.request.ignore = vec![
        IgnoreRequest::parse("environment/qa=Checkout is behind a flag in prod").unwrap(),
        IgnoreRequest::parse("kubernetes/healthy=never failed").unwrap(),
    ];
    let promotion = ignoring.promoted();
    let record = &promotion.record;
    assert_eq!(record.ignored_attestations.len(), 1);
    assert_eq!(record.ignored_attestations[0].scope, "environment");
    assert_eq!(record.ignored_attestations[0].name, "qa");
    assert_eq!(
        record.ignored_attestations[0].reason,
        "Checkout is behind a flag in prod"
    );
    assert_eq!(record.ignored_attestations[0].by, "alice");
    assert_eq!(record.evidence.level, EvidenceLevel::Attested);
    assert_eq!(promotion.unused_ignores[0].scope, "kubernetes");
}

#[test]
fn test_ignore_request_parses_scope_name_and_reason() {
    let ignore = IgnoreRequest::parse("environment/qa=a=b/c").unwrap();
    assert_eq!(
        (ignore.scope.as_str(), ignore.name.as_str(), ignore.reason.as_str()),
        ("environment", "qa", "a=b/c")
    );
    for bad in ["qa=reason", "environment/qa", "environment/qa=", "/qa=x", "a/b/c=x"] {
        assert!(IgnoreRequest::parse(bad).is_err(), "{bad}");
    }
}

/// A publication's attestations are judged like any unit's: a failing one
/// blocks its state, and an older state whose attestations passed is taken.
/// Without a driver that declares running-scoped attestations (no reader),
/// a publication's attestations are judged like any unit's: the newest
/// failing state is passed over for the older one that passed.
#[test]
fn test_publication_attestations_without_a_reader_are_judged_generically() {
    let history = vec![
        state(2, 'b', "sha256:two")
            .env_attestation("qa", Pass, FRESH)
            .kubernetes_attestation("accepted", Fail, FRESH, Value::Null)
            .kubernetes_attestation("healthy", Fail, FRESH, Value::Null),
        state(1, 'a', "sha256:one")
            .env_attestation("qa", Pass, FRESH)
            .kubernetes_attestation("accepted", Pass, FRESH, running("p1"))
            .kubernetes_attestation("healthy", Pass, FRESH, running("p1")),
    ];
    let promotion = case(attested(), history).promoted();
    assert_eq!(from_run(&promotion.record), run_id(1).to_string());
}

fn verdict(
    deployment: &str,
    group: &str,
    runs: DeploymentRuns,
    meets: bool,
    reason: Option<&str>,
) -> DeploymentVerdict {
    DeploymentVerdict {
        name: deployment.to_owned(),
        group: Some(group.to_owned()),
        runs,
        meets,
        reason: reason.map(str::to_owned),
    }
}

fn runs_receipt(deployment: &str) -> DeploymentVerdict {
    verdict(deployment, "web", DeploymentRuns::Receipt, true, None)
}

fn runs_other(deployment: &str, publication: &str) -> DeploymentVerdict {
    verdict(
        deployment,
        "web",
        DeploymentRuns::Publication {
            publication: publication.to_owned(),
            running: None,
            truncated: false,
        },
        true,
        Some(&format!("{deployment} runs another publication ({publication})")),
    )
}

/// D8: a publication's attestations say what runs now, so only the current
/// receipt's count. State 1 passed; state 2's newest attestation fails, as
/// its Applications still run state 1's publication. The promotion is
/// refused with the driver's reason and does not fall back to state 1,
/// whose attestations no longer describe what runs, and the fix names
/// `--state-revision`. Explicitly chosen, state 1 is judged on the
/// attestations recorded for it and promotes. Catches a fallback to an
/// older state that once passed.
#[test]
fn test_only_the_current_receipts_publication_attestations_count() {
    let history = vec![
        state(2, 'b', "sha256:two").env_attestation("qa", Pass, FRESH).observed(
            Fail,
            "p2",
            &[runs_other("web", "p1"), runs_other("api", "p1")],
        ),
        state(1, 'a', "sha256:one").env_attestation("qa", Pass, FRESH).observed(
            Pass,
            "p1",
            &[runs_receipt("web"), runs_receipt("api")],
        ),
    ];
    let mut d8 = case(attested(), history);
    let refusal = d8.refused();
    assert_eq!(refusal.finding.code, Code::PromoteNoEvidence);
    assert!(
        refusal
            .finding
            .message
            .contains("the newest recorded state falls short"),
        "{}",
        refusal.finding.message
    );
    assert!(refusal.gaps.iter().all(|gap| gap.kind == GapKind::Failing));
    assert!(
        refusal
            .finding
            .fix
            .contains(&format!("--state-revision {}", desired_commit_of(1))),
        "{}",
        refusal.finding.fix
    );
    // `get promotion-candidates` shows why state 1 no longer qualifies.
    let evaluations = d8.evaluations();
    assert!(!evaluations[1].eligible);
    assert!(evaluations[1]
        .gaps
        .iter()
        .all(|gap| gap.kind == GapKind::NotRunning && gap.scope == "kubernetes"));

    d8.request.select = Selection::StateRevision(desired_commit_of(1));
    let promotion = d8.promoted();
    assert_eq!(from_run(&promotion.record), run_id(1).to_string());
    assert!(promotion.record.evidence.override_.is_none());
    d8.request.select = Selection::SourceRevision(commit('a'));
    assert_eq!(from_run(&d8.promoted().record), run_id(1).to_string());
}

/// D8 narrows only running-scoped attestations: an environment-wide
/// attestation names a state, so the newest state that passed it still wins
/// while the publication's receipt is the same in both states.
#[test]
fn test_environment_attestations_still_take_the_newest_passing_state() {
    let both = [runs_receipt("web"), runs_receipt("api")];
    let history = vec![
        state(2, 'b', "sha256:one")
            .env_attestation("qa", Fail, FRESH)
            .observed(Pass, "p1", &both),
        state(1, 'a', "sha256:one")
            .env_attestation("qa", Pass, FRESH)
            .observed(Pass, "p1", &both),
    ];
    let promotion = case(attested(), history).promoted();
    assert_eq!(from_run(&promotion.record), run_id(1).to_string());
}

/// Two values of one publication, each consumed by its Release's
/// Application (D7).
fn per_release() -> (PromotionPath, BTreeMap<String, String>) {
    let path = path(json!({
        "evidence": "attested",
        "attestations": {"environment": []},
        "values": {
            "webImage": {"select": {"unit": "kubernetes", "input": "/releases/web~1web/image"}},
            "apiImage": {"select": {"unit": "kubernetes", "input": "/releases/web~1api/image"}},
        }
    }));
    let consumers = BTreeMap::from([
        ("webImage".to_owned(), "web".to_owned()),
        ("apiImage".to_owned(), "api".to_owned()),
    ]);
    (path, consumers)
}

/// D6: a value-only path judges each value for the Application that consumes
/// it, from the current receipt's attestations. web runs state 2's
/// publication and api still runs state 1's, so the aggregate fails, yet
/// each value comes from the state its consumer runs, and only a consumer's
/// own finding blocks: api running a revision no publication explains
/// refuses apiImage with the driver's reason. Catches a value-only path that
/// blocks on the aggregate or takes a value its consumer does not run.
#[test]
fn test_value_only_values_follow_what_their_consumer_runs() {
    let (path, consumers) = per_release();
    let mixed = vec![
        state(2, 'b', "sha256:two").observed(Fail, "p2", &[runs_receipt("web"), runs_other("api", "p1")]),
        state(1, 'a', "sha256:one").observed(Pass, "p1", &[runs_receipt("web"), runs_receipt("api")]),
    ];
    let mut values = case(path, mixed);
    values.promotes_source = false;
    values.consumers = consumers;
    let promotion = values.promoted();
    let record = &promotion.record;
    assert_eq!(from_run(record), run_id(2).to_string());
    assert_eq!(record.values["webImage"].value, json!("sha256:two"));
    assert!(record.values["webImage"].state.is_none());
    assert_eq!(record.values["apiImage"].value, json!("sha256:one"));
    assert_eq!(record.values["apiImage"].state.as_ref().unwrap().run, run_id(1));
    let used: Vec<String> = record
        .evidence
        .attestations
        .iter()
        .map(|item| format!("{}/{}", item.scope, item.name))
        .collect();
    assert_eq!(used, ["kubernetes/accepted", "kubernetes/healthy"]);
    // `get promotion-candidates` agrees per value.
    let evaluations = values.evaluations();
    assert!(evaluations[0].by_value["webImage"].eligible);
    assert!(!evaluations[0].by_value["apiImage"].eligible);
    assert!(evaluations[1].by_value["apiImage"].eligible);

    let unmatched = verdict(
        "api",
        "web",
        DeploymentRuns::Unproven,
        false,
        Some("api is unmatched: it runs revision c0ffee"),
    );
    values.history[0] = state(2, 'b', "sha256:two")
        .observed(Fail, "p2", &[runs_receipt("web"), unmatched])
        .build();
    let refusal = values.refused();
    assert!(
        refusal.finding.message.contains("\"apiImage\""),
        "{}",
        refusal.finding.message
    );
    assert!(
        refusal.finding.message.contains("api is unmatched"),
        "{}",
        refusal.finding.message
    );
}

/// A consumer that is not healthy blocks its value when `healthy` is
/// required, and a path's `coverage.excludeApplications` accepts it at what
/// it runs; the record lists the exclusion.
#[test]
fn test_an_unhealthy_consumer_blocks_unless_excluded() {
    let (path, consumers) = per_release();
    let degraded = verdict("web", "web", DeploymentRuns::Receipt, false, Some("web is Degraded"));
    let history = vec![state(1, 'a', "sha256:one").observed(Fail, "p1", &[degraded, runs_receipt("api")])];
    let mut values = case(path, history);
    values.promotes_source = false;
    values.consumers = consumers;
    let refusal = values.refused();
    assert!(
        refusal.finding.message.contains("web is Degraded"),
        "{}",
        refusal.finding.message
    );
    values.path.spec.coverage.exclude_applications = vec!["web".to_owned()];
    let promotion = values.promoted();
    assert_eq!(promotion.record.excluded_applications, ["web"]);
    assert_eq!(promotion.record.values["webImage"].value, json!("sha256:one"));
}

/// Coverage for a path that promotes the source: `applicationGroups` and
/// `excludeApplications` narrow the Applications whose findings count
/// toward the publication's attestations, and `requireApplications` adds
/// Applications that must meet them at whatever publication they run.
#[test]
fn test_coverage_narrows_and_extends_the_source_judgement() {
    let jobs = verdict(
        "worker",
        "jobs",
        DeploymentRuns::Publication {
            publication: "p0".to_owned(),
            running: None,
            truncated: false,
        },
        true,
        Some("worker runs another publication (p0)"),
    );
    let history = vec![state(1, 'a', "sha256:one").env_attestation("qa", Pass, FRESH).observed(
        Fail,
        "p1",
        &[runs_receipt("web"), runs_receipt("api"), jobs.clone()],
    )];
    let mut source = case(attested(), history);
    // Without coverage, the attestation counts as a whole.
    let refusal = source.refused();
    assert!(refusal
        .gaps
        .iter()
        .all(|gap| gap.kind == GapKind::Failing && gap.scope == "kubernetes"));
    // A group the path does not cover is left out; the one it covers still counts.
    source.path.spec.coverage.application_groups = vec!["jobs".to_owned()];
    let refusal = source.refused();
    assert!(
        refusal.finding.message.contains("worker runs another publication"),
        "{}",
        refusal.finding.message
    );

    source.path.spec.coverage.application_groups = vec!["web".to_owned()];
    assert_eq!(from_run(&source.promoted().record), run_id(1).to_string());

    source.path.spec.coverage.application_groups = Vec::new();
    source.path.spec.coverage.exclude_applications = vec!["worker".to_owned()];
    assert_eq!(from_run(&source.promoted().record), run_id(1).to_string());

    // worker runs another publication but meets `accepted` and `healthy`
    // there, which is all `requireApplications` asks.
    source.path.spec.coverage.require_applications = vec!["worker".to_owned()];
    source.promoted();
    let broken = verdict(
        "worker",
        "jobs",
        DeploymentRuns::Unproven,
        false,
        Some("worker is missing"),
    );
    source.history[0] = state(1, 'a', "sha256:one")
        .env_attestation("qa", Pass, FRESH)
        .observed(Fail, "p1", &[runs_receipt("web"), runs_receipt("api"), broken])
        .build();
    let refusal = source.refused();
    assert!(
        refusal.finding.message.contains("requireApplications"),
        "{}",
        refusal.finding.message
    );
    source.path.spec.coverage.require_applications = vec!["nowhere".to_owned()];
    let refusal = source.refused();
    assert!(
        refusal.finding.message.contains("nowhere"),
        "{}",
        refusal.finding.message
    );
}

/// D13: an explicitly chosen older state needs `--evidence published` only
/// when its own recorded attestations do not reach the level, and the record
/// then carries the override and the warning code.
#[test]
fn test_explicit_state_needs_the_override_only_without_its_own_attestations() {
    let path = path(
        json!({"evidence": "attested", "maxAttestationAge": "1h", "attestations": {"units": {"kubernetes": [], "web-image": []}}}),
    );
    let history = || {
        vec![
            state(2, 'b', "sha256:two").env_attestation("qa", Pass, FRESH),
            state(1, 'a', "sha256:one").env_attestation("qa", Pass, OLD),
        ]
    };
    let mut older = case(path.clone(), history());
    older.request.select = Selection::SourceRevision(commit('a'));
    let refusal = older.refused();
    assert_eq!(refusal.finding.code, Code::PromoteNoEvidence);
    assert!(refusal.finding.message.contains("maxAttestationAge"));
    assert!(refusal.finding.message.contains(&"a".repeat(40)));
    older.request.evidence_published = true;
    older.request.requested = requested(Some("Roll back release 2"));
    let promotion = older.promoted();
    let record = &promotion.record;
    assert_eq!(from_run(record), run_id(1).to_string());
    assert_eq!(record.evidence.level, EvidenceLevel::Published);
    assert_eq!(
        record.evidence.override_.as_ref().unwrap().reason,
        "Roll back release 2"
    );
    assert_eq!(promotion.warnings.len(), 1);
    assert_eq!(promotion.warnings[0].code, Code::PromoteEvidenceOverride);
    // The same state with fresh attestations needs no override.
    let fresh = vec![state(1, 'a', "sha256:one").env_attestation("qa", Pass, FRESH)];
    let mut chosen = case(path, fresh);
    chosen.request.select = Selection::StateRevision(desired_commit_of(1));
    let promotion = chosen.promoted();
    assert_eq!(promotion.record.evidence.level, EvidenceLevel::Attested);
    assert!(promotion.warnings.is_empty());
}

/// `--revision` takes the newest state that ran that source commit and
/// qualifies; a commit no state ran, or a state that does not exist, is
/// missing evidence.
#[test]
fn test_revision_selection() {
    let history = vec![
        state(3, 'b', "sha256:three"),
        state(2, 'a', "sha256:two").without_receipt("web-image"),
        state(1, 'a', "sha256:one"),
    ];
    let mut by_source = case(published(), history);
    by_source.request.select = Selection::SourceRevision(commit('a'));
    let promotion = by_source.promoted();
    assert_eq!(
        from_run(&promotion.record),
        run_id(1).to_string(),
        "state 2 does not qualify"
    );
    by_source.request.select = Selection::SourceRevision(commit('c'));
    let refusal = by_source.refused();
    assert!(refusal
        .finding
        .message
        .contains("no recorded state of dev ran source commit"));
    by_source.request.select = Selection::StateRevision(commit('9'));
    assert_eq!(by_source.refused().finding.code, Code::PromoteNoEvidence);
    by_source.request.select = Selection::StateRevision(desired_commit_of(3));
    assert_eq!(from_run(&by_source.promoted().record), run_id(3).to_string());
}

/// A state that ran an archived worktree snapshot cannot establish clean
/// promotion evidence, however complete it is.
#[test]
fn test_a_worktree_state_is_never_a_candidate() {
    let history = vec![state(2, 'b', "sha256:two").worktree(), state(1, 'a', "sha256:one")];
    let case = case(published(), history);
    assert_eq!(from_run(&case.promoted().record), run_id(1).to_string());
    assert_eq!(case.evaluations().len(), 1);
}

/// D2: promoting what the path's record already names, when that record is
/// the newest on the target, writes nothing; another path's newer record
/// makes it a promotion again.
#[test]
fn test_repromoting_the_recorded_state_is_unchanged() {
    let history = vec![state(1, 'a', "sha256:one")];
    let mut case = case(published(), history);
    let record = case.promoted().record;
    case.records.insert(record.path.clone(), record.clone());
    case.sequence = 2;
    match case.run() {
        Ok(Decision::Unchanged(existing)) => assert_eq!(*existing, record),
        other => panic!("expected unchanged: {other:?}"),
    }
    let mut other_path = record.clone();
    other_path.path = PromotionPathName::new("hotfix-to-prod").unwrap();
    other_path.sequence = 2;
    case.records.insert(other_path.path.clone(), other_path);
    case.sequence = 3;
    assert!(matches!(case.run(), Ok(Decision::Promote(_))));
}

/// Values read through the shared resolver: an artifact field, a declared
/// output, and a unit's resolved input, each with the source unit and its
/// receipt or input digest.
#[test]
fn test_values_are_read_through_the_resolver() {
    let path = path(json!({
        "evidence": "published",
        "values": {
            "webImage": {"select": {"unit": "web-image", "artifact": "image", "kind": "OciImage", "pointer": "/reference"}},
            "digest": {"select": {"unit": "web-image", "output": "digest"}},
            "release": {"select": {"unit": "kubernetes", "input": "/releases/web~1web/image"}},
            "source": {"select": {"unit": "web-image", "input": "/source"}},
        }
    }));
    let promotion = case(path, vec![state(1, 'a', "sha256:one")]).promoted();
    let values = &promotion.record.values;
    assert_eq!(values["webImage"].value, json!("sha256:one"));
    assert_eq!(
        values["webImage"].execution_key,
        Some(key(&web_image_seed("sha256:one")))
    );
    assert!(values["webImage"].input_digest.is_none());
    assert_eq!(values["webImage"].unit.as_ref().unwrap().name, name("web-image"));
    assert_eq!(values["digest"].value, json!("sha256:one"));
    assert_eq!(values["release"].value, json!("sha256:one"));
    assert!(values["release"].execution_key.is_none());
    let selected = nyl_core::digest::canonical_json_bytes(&json!("sha256:one")).unwrap();
    assert_eq!(
        values["release"].input_digest,
        Some(nyl_core::digest::sha256_prefixed(&selected)),
        "the digest of the selected value, not of the whole resolved spec"
    );
    assert_eq!(
        values["source"].value,
        json!({"path": "/app", "commit": commit('a')}),
        "an input selector carries the exact commit the unit executed"
    );
    assert_eq!(
        promotion.artifacts,
        [ArtifactCheck {
            value: "webImage".to_owned(),
            unit: name("web-image"),
            artifact: "image".to_owned(),
            kind: Some("OciImage".to_owned()),
        }]
    );
}

/// A selector that resolves to nothing is a configuration error (exit 1),
/// not missing evidence, and the resolver's fix reaches the refusal.
#[test]
fn test_unreadable_value_is_a_selector_error() {
    for select in [
        json!({"unit": "web-image", "artifact": "image", "pointer": "/nowhere"}),
        json!({"unit": "web-image", "output": "undeclared"}),
        json!({"unit": "kubernetes", "input": "/releases/missing"}),
    ] {
        let path = path(json!({"values": {"v": {"select": select}}}));
        let refusal = case(path, vec![state(1, 'a', "sha256:one")]).refused();
        assert_eq!(refusal.finding.code, Code::PromoteSelectorInvalid);
        assert_eq!(refusal.exit, ExitCategory::Error);
        assert!(!refusal.finding.fix.is_empty());
    }
}

fn value_only() -> PromotionPath {
    path(json!({
        "evidence": "attested",
        "attestations": {"units": {"kubernetes": [], "web-image": ["healthy"]}, "environment": []},
        "values": {
            "webImage": {"select": {"unit": "web-image", "artifact": "image", "pointer": "/reference"}},
            "release": {"select": {"unit": "kubernetes", "input": "/releases/web~1web/image"}},
        }
    }))
}

/// A value-only path takes each value from the newest state in which its
/// consumer reaches the level: no source commit, `from` is the newest state
/// any value came from, and the value from an older state names it (D11).
#[test]
fn test_value_only_path_selects_a_state_per_value() {
    let mut value_case = case(
        value_only(),
        vec![state(2, 'b', "sha256:two"), state(1, 'a', "sha256:one")],
    );
    value_case.promotes_source = false;
    // web-image declares `healthy` in both states and the path requires it:
    // state 1 has it recorded, state 2 does not, while kubernetes needs only
    // its receipt, which state 2 has.
    for (state, recorded) in value_case.history.iter_mut().zip([false, true]) {
        state.snapshot.desired.get_mut(&name("web-image")).unwrap().attestations = vec!["healthy".to_owned()];
        if recorded {
            let seed = key(&web_image_seed("sha256:one")).to_string();
            state.attestations.units.entry(name("web-image")).or_default().insert(
                "healthy".to_owned(),
                attestation("healthy", &seed, Pass, FRESH, Value::Null),
            );
        }
    }
    let promotion = value_case.promoted();
    let record = &promotion.record;
    assert_eq!(record.source_commit, None, "values only");
    assert_eq!(
        from_run(record),
        run_id(2).to_string(),
        "the newest state any value came from"
    );
    assert_eq!(record.values["release"].value, json!("sha256:two"));
    assert!(record.values["release"].state.is_none());
    assert_eq!(record.values["webImage"].value, json!("sha256:one"));
    let from_other = record.values["webImage"].state.as_ref().unwrap();
    assert_eq!(from_other.run, run_id(1));
    assert_eq!(from_other.desired_commit, desired_commit_of(1));
    assert_eq!(record.evidence.attestations.len(), 1);
    assert_eq!(record.evidence.attestations[0].scope, "web-image");
}

/// A value whose consumer reaches the level in no state refuses the whole
/// promotion, naming the value; an explicit state takes every value from it.
#[test]
fn test_value_only_refusal_names_the_value_and_explicit_state_takes_all() {
    let mut value_case = case(
        value_only(),
        vec![state(2, 'b', "sha256:two"), state(1, 'a', "sha256:one")],
    );
    value_case.promotes_source = false;
    for state in &mut value_case.history {
        state.snapshot.desired.get_mut(&name("web-image")).unwrap().attestations = vec!["healthy".to_owned()];
    }
    let refusal = value_case.refused();
    assert!(
        refusal.finding.message.contains("\"webImage\""),
        "{}",
        refusal.finding.message
    );
    let mut path_published = value_case.path.clone();
    path_published.spec.evidence = nyl_core::resources::promotion::PathEvidence::Published;
    path_published.spec.attestations = nyl_core::resources::promotion::PathAttestations::default();
    value_case.path = path_published;
    value_case.request.select = Selection::StateRevision(desired_commit_of(1));
    let promotion = value_case.promoted();
    assert_eq!(from_run(&promotion.record), run_id(1).to_string());
    assert!(!promotion.record.values.is_empty());
    assert!(promotion.record.values.values().all(|value| value.state.is_none()));
}

/// The record states what the path and request say: the path, sequence,
/// excluded Applications, who asked, and that artifacts are not yet verified.
#[test]
fn test_record_carries_the_request_and_the_path() {
    let path = path(json!({"coverage": {"excludeApplications": ["batch"]}}));
    let mut case = case(path, vec![state(1, 'a', "sha256:one")]);
    case.sequence = 7;
    let promotion = case.promoted();
    let record = &promotion.record;
    assert_eq!(record.path.as_str(), "dev-to-prod");
    assert_eq!(record.sequence, 7);
    assert_eq!(record.excluded_applications, ["batch"]);
    assert_eq!(record.requested.by, "alice");
    assert_eq!(record.requested.reason.as_deref(), Some("Release"));
    assert!(record.superseded.is_empty());
    assert!(!record.artifacts_verified);
    match &record.from {
        PromotionFrom::Environment(from) => {
            assert_eq!(from.desired_commit, desired_commit_of(1));
            assert_eq!(from.observed_commit, Some(commit('1')));
        }
        PromotionFrom::Target(_) => panic!("an environment source"),
    }
}

/// Candidates report the highest level each state reaches, independent of
/// the invocation's flags: what `get promotion-candidates` shows.
#[test]
fn test_evaluate_reports_the_level_each_state_reaches() {
    let history = vec![
        state(3, 'c', "sha256:three").without_receipt("kubernetes"),
        state(2, 'b', "sha256:two"),
        state(1, 'a', "sha256:one")
            .env_attestation("qa", Pass, FRESH)
            .kubernetes_attestation("accepted", Pass, FRESH, running("p1"))
            .kubernetes_attestation("healthy", Pass, FRESH, running("p1")),
    ];
    let evaluations = case(attested(), history).evaluations();
    let levels: Vec<_> = evaluations.iter().map(|evaluation| evaluation.level).collect();
    assert_eq!(
        levels,
        [None, Some(EvidenceLevel::Published), Some(EvidenceLevel::Attested)]
    );
    assert!(evaluations[2].gaps.is_empty());
    assert!(!evaluations[1].gaps.is_empty());
}

/// A `published` path reports a state at `published` even when it would
/// also pass `attested`: the level is the path's, not the strongest
/// imaginable, as `get promotion-candidates` shows it. Catches a candidate
/// listed as `attested` on a path that never asks for attestations.
#[test]
fn test_evaluate_caps_the_level_at_the_paths() {
    let history = vec![
        state(2, 'b', "sha256:two").without_receipt("kubernetes"),
        state(1, 'a', "sha256:one")
            .env_attestation("qa", Pass, FRESH)
            .kubernetes_attestation("accepted", Pass, FRESH, running("p1"))
            .kubernetes_attestation("healthy", Pass, FRESH, running("p1")),
    ];
    let evaluations = case(published(), history).evaluations();
    let levels: Vec<_> = evaluations.iter().map(|evaluation| evaluation.level).collect();
    assert_eq!(levels, [None, Some(EvidenceLevel::Published)]);
    assert_eq!(evaluations[0].gaps[0].scope, "kubernetes");
}

/// A value-only path judges each state as `nyl promote` does: for the units
/// its values read, so a receipt missing elsewhere does not lower it.
/// Catches candidates judged by the whole-source rule on a path whose
/// promotion would take them.
#[test]
fn test_evaluate_judges_a_value_only_path_for_its_values() {
    let image_only = path(json!({
        "evidence": "attested",
        "attestations": {"units": {"web-image": []}, "environment": []},
        "values": {
            "webImage": {"select": {"unit": "web-image", "artifact": "image", "pointer": "/reference"}},
        }
    }));
    let mut value_case = case(
        image_only,
        vec![state(1, 'a', "sha256:one").without_receipt("kubernetes")],
    );
    value_case.promotes_source = false;
    let evaluations = value_case.evaluations();
    assert_eq!(
        evaluations[0].level,
        Some(EvidenceLevel::Attested),
        "{:?}",
        evaluations[0].gaps
    );
    // The same state, judged for the whole source, lacks kubernetes.
    value_case.promotes_source = true;
    assert_eq!(value_case.evaluations()[0].level, None);
}

/// `--evidence published` without an explicit state takes the newest state
/// that reaches `published`, recording an override only when that state is
/// below the path's level: the newer unattested state wins over the older
/// attested one, while a newest state that is attested needs no override.
#[test]
fn test_evidence_published_without_a_selection_takes_the_newest_published_state() {
    let path = path(json!({"evidence": "attested", "attestations": {"units": {"kubernetes": [], "web-image": []}}}));
    let history = || {
        vec![
            state(2, 'b', "sha256:two"),
            state(1, 'a', "sha256:one").env_attestation("qa", Pass, FRESH),
        ]
    };
    let mut without = case(path.clone(), history());
    assert_eq!(from_run(&without.promoted().record), run_id(1).to_string());
    without.request.evidence_published = true;
    without.request.requested = requested(None);
    let promotion = without.promoted();
    assert_eq!(from_run(&promotion.record), run_id(2).to_string());
    assert!(promotion.record.evidence.override_.is_some());
    assert_eq!(promotion.record.evidence.level, EvidenceLevel::Published);
    assert!(promotion.record.evidence.override_.unwrap().reason.contains("--reason"));
    let mut attested_newest = case(
        path,
        vec![state(2, 'b', "sha256:two").env_attestation("qa", Pass, FRESH)],
    );
    attested_newest.request.evidence_published = true;
    let promotion = attested_newest.promoted();
    assert!(promotion.record.evidence.override_.is_none());
    assert_eq!(promotion.record.evidence.level, EvidenceLevel::Attested);
    assert!(promotion.warnings.is_empty());
}

/// Until the per-Application judgement exists, a value-only path is blocked
/// by a failing attestation of any unit of the state, not only its consumers.
#[test]
fn test_value_only_path_is_blocked_by_a_failing_attestation_of_another_unit() {
    let path = path(json!({
        "evidence": "published",
        "values": {"webImage": {"select": {"unit": "web-image", "artifact": "image", "pointer": "/reference"}}}
    }));
    let history = vec![
        state(2, 'b', "sha256:two").kubernetes_attestation("accepted", Fail, FRESH, Value::Null),
        state(1, 'a', "sha256:one"),
    ];
    let mut value_case = case(path, history);
    value_case.promotes_source = false;
    let promotion = value_case.promoted();
    assert_eq!(
        promotion.record.values["webImage"].value,
        json!("sha256:one"),
        "state 2's failing kubernetes attestation blocks it though kubernetes consumes no value"
    );
}

/// An ignore entry sets only a failing attestation aside: one against a
/// pending attestation leaves it pending.
#[test]
fn test_ignoring_a_pending_attestation_does_not_satisfy_it() {
    let mut pending = case(attested(), vec![state(1, 'a', "sha256:one")]);
    pending.request.ignore = vec![IgnoreRequest::parse("environment/qa=skip").unwrap()];
    let refusal = pending.refused();
    assert!(refusal
        .gaps
        .iter()
        .any(|gap| gap.kind == GapKind::Pending && gap.scope == "environment"));
}

/// A state chosen by `--state-revision` that ran a worktree snapshot says so.
#[test]
fn test_a_worktree_state_revision_is_named() {
    let mut chosen = case(published(), vec![state(1, 'a', "sha256:one").worktree()]);
    chosen.request.select = Selection::StateRevision(desired_commit_of(1));
    let refusal = chosen.refused();
    assert!(
        refusal.finding.message.contains("worktree snapshot"),
        "{}",
        refusal.finding.message
    );
}

/// Two consumers that pass in different states: `webImage`'s consumer only
/// in the newer state, `release`'s only in the older one. `nyl promote` takes
/// each value from its own state, so `get promotion-candidates` must call
/// both states eligible, judged per value. Catches candidates judged for
/// every consumer at once, which would call a state promote selects
/// ineligible.
#[test]
fn test_evaluate_judges_each_value_of_a_value_only_path_on_its_own() {
    let mut value_case = case(
        value_only(),
        vec![
            state(2, 'b', "sha256:two").without_receipt("kubernetes"),
            state(1, 'a', "sha256:one"),
        ],
    );
    value_case.promotes_source = false;
    // web-image requires `healthy`, recorded only for the newer state's receipt.
    for (state, image) in value_case.history.iter_mut().zip(["sha256:two", ""]) {
        state.snapshot.desired.get_mut(&name("web-image")).unwrap().attestations = vec!["healthy".to_owned()];
        if !image.is_empty() {
            let seed = key(&web_image_seed(image)).to_string();
            state.attestations.units.entry(name("web-image")).or_default().insert(
                "healthy".to_owned(),
                attestation("healthy", &seed, Pass, FRESH, Value::Null),
            );
        }
    }
    let promotion = value_case.promoted();
    assert_eq!(from_run(&promotion.record), run_id(2).to_string());
    assert_eq!(
        promotion.record.values["release"]
            .state
            .as_ref()
            .map(|state| state.run.clone()),
        Some(run_id(1)),
        "release comes from the older state"
    );

    let evaluations = value_case.evaluations();
    assert!(
        evaluations.iter().all(|evaluation| evaluation.eligible),
        "{evaluations:?}"
    );
    let newer = &evaluations[0].by_value;
    assert!(newer["webImage"].eligible);
    assert!(!newer["release"].eligible);
    assert_eq!(newer["release"].level, None);
    let older = &evaluations[1].by_value;
    assert!(!older["webImage"].eligible);
    assert!(older["release"].eligible);
    // What falls short is the value that is not eligible there.
    assert!(evaluations[0].gaps.iter().any(|gap| gap.scope == "kubernetes"));
    assert!(evaluations[1].gaps.iter().any(|gap| gap.scope == "web-image"));
}

/// A `record: source` block matches its path and the state it names exactly
/// when it is the record `nyl promote` wrote for that state: another value,
/// source commit, or state, a value naming a state of its own, a state of
/// another environment than the path's source, a value the path does not
/// declare, or another selector is `NYL-PROMOTION-RECORD-INVALID`. Catches
/// a block edited by hand, or copied from another promotion, being run on
/// trust.
#[test]
fn test_a_source_block_must_match_the_state_it_names() {
    let case = case(
        published(),
        vec![state(2, 'b', "sha256:two"), state(1, 'a', "sha256:one")],
    );
    let record = case.promoted().record;
    let named = &case.history[0];
    check_block(&record, &case.path, named).unwrap();

    let tamper = |change: &dyn Fn(&mut PromotionRecord)| {
        let mut tampered = record.clone();
        change(&mut tampered);
        tampered
    };
    let value = tamper(&|r| r.values.get_mut("webImage").unwrap().value = json!("sha256:other"));
    let source = tamper(&|r| r.source_commit = Some(commit('a')));
    let own_state = tamper(&|r| {
        r.values.get_mut("webImage").unwrap().state = Some(nyl_core::orchestration::promotion::PromotedState {
            run: run_id(1),
            desired_commit: desired_commit_of(1),
            observed_commit: None,
        });
    });
    let other_environment = tamper(&|r| {
        if let PromotionFrom::Environment(from) = &mut r.from {
            from.environment = EnvironmentName::new("qa").unwrap();
        }
    });
    let undeclared = tamper(&|r| {
        let extra = r.values["webImage"].clone();
        r.values.insert("extra".to_owned(), extra);
    });
    let selector = tamper(&|r| r.values.get_mut("webImage").unwrap().select.pointer = Some("/digest".to_owned()));
    for (tampered, state) in [
        (&value, named),
        (&source, named),
        (&record, &case.history[1]),
        (&own_state, named),
        (&other_environment, named),
        (&undeclared, named),
        (&selector, named),
    ] {
        let finding = check_block(tampered, &case.path, state).unwrap_err();
        assert_eq!(finding.code, Code::PromotionRecordInvalid, "{}", finding.message);
    }
}

fn runs_running(deployment: &str, publication: &str, running: &str) -> DeploymentVerdict {
    verdict(
        deployment,
        "web",
        DeploymentRuns::Publication {
            publication: publication.to_owned(),
            running: Some(running.to_owned()),
            truncated: false,
        },
        true,
        Some(&format!("{deployment} runs another publication ({publication})")),
    )
}

fn without_unit(mut state: RecordedState, unit: &str) -> RecordedState {
    state.snapshot.desired.remove(&name(unit));
    state.snapshot.observed.remove(&name(unit));
    state.attestations.units.remove(&name(unit));
    state.running.remove(&name(unit));
    state
}

/// B1: `coverage.applicationGroups` never fails open. A group no
/// Application of the attestation belongs to (a typo) is a coverage gap
/// naming the field, and so is a coverage that leaves no Application to
/// judge while the attestation fails. Catches a misspelt group turning a
/// failing `accepted` into a pass.
#[test]
fn test_coverage_application_groups_never_fail_open() {
    let broken = verdict("api", "web", DeploymentRuns::Unproven, false, Some("api is unmatched"));
    let history = vec![state(1, 'a', "sha256:one").env_attestation("qa", Pass, FRESH).observed(
        Fail,
        "p1",
        &[runs_receipt("web"), broken],
    )];
    let mut source = case(attested(), history);
    source.path.spec.coverage.application_groups = vec!["wbe".to_owned()];
    let refusal = source.refused();
    assert!(refusal
        .gaps
        .iter()
        .any(|gap| gap.kind == GapKind::Coverage && gap.message.contains("coverage.applicationGroups names wbe")));
    assert!(refusal.finding.fix.contains("coverage"), "{}", refusal.finding.fix);
    // A real group whose every Application is excluded leaves nothing to judge.
    source.path.spec.coverage.application_groups = vec!["web".to_owned()];
    source.path.spec.coverage.exclude_applications = vec!["web".to_owned(), "api".to_owned()];
    let refusal = source.refused();
    assert!(refusal
        .gaps
        .iter()
        .any(|gap| gap.kind == GapKind::Coverage && gap.message.contains("leaves no Application")));
    // Excluding only the failing Application passes.
    source.path.spec.coverage.exclude_applications = vec!["api".to_owned()];
    source.promoted();
}

/// S1: a `requireApplications` name no running-scoped attestation lists is
/// a coverage gap whatever the path requires: with no reader at all, and
/// with a path that requires only the environment's `qa`. A listed name
/// passes there, because nothing of it is required.
#[test]
fn test_require_applications_needs_a_listed_application() {
    let qa_only = path(json!({"evidence": "attested", "attestations": {"units": {"kubernetes": [], "web-image": []}}}));
    let mut unobserved = case(
        qa_only.clone(),
        vec![state(1, 'a', "sha256:one").env_attestation("qa", Pass, FRESH)],
    );
    unobserved.path.spec.coverage.require_applications = vec!["web".to_owned()];
    let refusal = unobserved.refused();
    assert!(refusal
        .gaps
        .iter()
        .any(|gap| gap.kind == GapKind::Coverage && gap.message.contains("coverage.requireApplications names web")));
    let mut observed = case(
        qa_only,
        vec![state(1, 'a', "sha256:one").env_attestation("qa", Pass, FRESH).observed(
            Pass,
            "p1",
            &[runs_receipt("web")],
        )],
    );
    observed.path.spec.coverage.require_applications = vec!["web".to_owned()];
    observed.promoted();
    observed.path.spec.coverage.require_applications = vec!["gateway".to_owned()];
    assert!(observed.refused().gaps.iter().any(|gap| gap.kind == GapKind::Coverage));
}

/// S5: an explicitly chosen state on a value-only path honours
/// `excludeApplications`: the excluded, Degraded Application leaves the
/// judgement of the publication's attestations.
#[test]
fn test_an_explicit_value_only_state_honours_excluded_applications() {
    let (path, consumers) = per_release();
    let degraded = verdict("web", "web", DeploymentRuns::Receipt, false, Some("web is Degraded"));
    let history = vec![state(1, 'a', "sha256:one").observed(Fail, "p1", &[degraded, runs_receipt("api")])];
    let mut values = case(path, history);
    values.promotes_source = false;
    values.consumers = consumers;
    values.request.select = Selection::StateRevision(desired_commit_of(1));
    assert!(values
        .refused()
        .gaps
        .iter()
        .any(|gap| gap.kind == GapKind::Failing && gap.scope == "kubernetes"));
    values.path.spec.coverage.exclude_applications = vec!["web".to_owned()];
    assert_eq!(from_run(&values.promoted().record), run_id(1).to_string());
}

/// S2: the current receipt is the newest state's observed receipt. The
/// newest run changed the publication but did not finish it, so the
/// publication still has state 1's receipt, and its attestations (recorded
/// for that receipt) say what runs: both values come from state 1, on a
/// value-only path and judged per Application.
#[test]
fn test_a_failed_newest_run_keeps_the_observed_receipt_current() {
    let (path, consumers) = per_release();
    let older = state(1, 'a', "sha256:one").observed(Pass, "p1", &[runs_receipt("web"), runs_receipt("api")]);
    let older = older.build();
    let mut newest = state(2, 'b', "sha256:two");
    newest.kubernetes = Receipt::Stale("sha256:one");
    let mut newest = newest.build();
    newest.attestations.stale_receipts.insert(
        name("kubernetes"),
        older.attestations.units[&name("kubernetes")].clone(),
    );
    newest
        .running
        .insert(name("kubernetes"), older.running[&name("kubernetes")].clone());
    let mut values = case(path, Vec::new());
    values.history = vec![newest, older];
    values.promotes_source = false;
    values.consumers = consumers;
    let promotion = values.promoted();
    assert_eq!(from_run(&promotion.record), run_id(1).to_string());
    assert_eq!(promotion.record.values["apiImage"].value, json!("sha256:one"));
    // The whole source: state 1's receipt is still the current one, so its
    // publication attestations count for it.
    let mut source = case(path_without_values(), Vec::new());
    source.history = values.history.clone();
    let targets = BTreeSet::from([name("kubernetes")]);
    let dev = dev();
    let mut input = source.input(&targets, &dev);
    input.path = &source.path;
    let evaluations = evaluate(&input);
    assert!(
        evaluations[1].gaps.iter().all(|gap| gap.kind != GapKind::NotRunning),
        "{:?}",
        evaluations[1].gaps
    );
}

fn path_without_values() -> PromotionPath {
    let mut path = path(json!({"evidence": "attested", "attestations": {"environment": []}}));
    path.spec.values.clear();
    path
}

/// S3/U3: values follow what their consumer runs at every evidence level:
/// at `published` with an observing publication, and under `--evidence
/// published`, apiImage still comes from the state whose publication its
/// Application runs. The gap on the newer state says so.
#[test]
fn test_values_follow_their_consumer_at_published_too() {
    let (mut path, consumers) = per_release();
    path.spec.evidence = nyl_core::resources::promotion::PathEvidence::Published;
    let history = || {
        vec![
            state(2, 'b', "sha256:two").observed(Fail, "p2", &[runs_receipt("web"), runs_other("api", "p1")]),
            state(1, 'a', "sha256:one").observed(Pass, "p1", &[runs_receipt("web"), runs_receipt("api")]),
        ]
    };
    let mut published = case(path, history());
    published.promotes_source = false;
    published.consumers = consumers.clone();
    let record = published.promoted().record;
    assert_eq!(record.values["apiImage"].value, json!("sha256:one"));
    assert_eq!(record.values["webImage"].value, json!("sha256:two"));
    let gaps = &published.evaluations()[0].gaps;
    assert!(gaps.iter().any(|gap| gap.kind == GapKind::NotRunning
        && gap
            .message
            .contains("takes each value from the state whose receipt published what its consumer runs")));

    let (attested, _) = per_release();
    let mut relaxed = case(attested, history());
    relaxed.promotes_source = false;
    relaxed.consumers = consumers;
    relaxed.request.evidence_published = true;
    assert_eq!(relaxed.promoted().record.values["apiImage"].value, json!("sha256:one"));
}

/// S4: a deployment's publication matches a state by the matched commit or
/// by the revision it runs; one no recorded state published refuses with a
/// message that says so and names `--state-revision`.
#[test]
fn test_a_publication_no_state_published_is_named() {
    let (path, consumers) = per_release();
    let history = |api: DeploymentVerdict| {
        vec![
            state(2, 'b', "sha256:two").observed(Fail, "p2", &[runs_receipt("web"), api]),
            state(1, 'a', "sha256:one").observed(Pass, "p1", &[runs_receipt("web"), runs_receipt("api")]),
        ]
    };
    let mut values = case(path.clone(), history(runs_running("api", "p0", "p1")));
    values.promotes_source = false;
    values.consumers = consumers.clone();
    assert_eq!(values.promoted().record.values["apiImage"].value, json!("sha256:one"));

    let mut unknown = case(path, history(runs_other("api", "p9")));
    unknown.promotes_source = false;
    unknown.consumers = consumers;
    let refusal = unknown.refused();
    assert!(
        refusal
            .finding
            .message
            .contains("no recorded state published what api runs (p9)"),
        "{}",
        refusal.finding.message
    );
    assert!(refusal.finding.message.contains("--state-revision"));
}

/// The refusal names an older state and `--state-revision` only when what
/// runs now is all that holds it back: an older state that also fails
/// another attestation is not offered.
#[test]
fn test_the_state_revision_hint_needs_only_not_running_gaps() {
    let history = vec![
        state(2, 'b', "sha256:two").env_attestation("qa", Pass, FRESH).observed(
            Fail,
            "p2",
            &[runs_other("web", "p1"), runs_other("api", "p1")],
        ),
        state(1, 'a', "sha256:one").env_attestation("qa", Fail, FRESH).observed(
            Pass,
            "p1",
            &[runs_receipt("web"), runs_receipt("api")],
        ),
    ];
    let refusal = case(attested(), history).refused();
    assert!(
        !refusal.finding.fix.contains("--state-revision"),
        "{}",
        refusal.finding.fix
    );
}

/// U1: when the newest state has no receipt of the publication (it was
/// removed or torn down), an older state does not qualify through that
/// unit's publication attestations; explicitly chosen, it still does.
#[test]
fn test_a_removed_publication_leaves_no_current_receipt() {
    let both = [runs_receipt("web"), runs_receipt("api")];
    let mut removed = case(
        path(json!({"evidence": "attested", "attestations": {"units": {"web-image": []}}})),
        Vec::new(),
    );
    removed.history = vec![
        // The newest state falls short on its own (qa fails), so only the
        // older one could be taken.
        without_unit(
            state(2, 'b', "sha256:two").env_attestation("qa", Fail, FRESH).build(),
            "kubernetes",
        ),
        state(1, 'a', "sha256:one")
            .env_attestation("qa", Pass, FRESH)
            .observed(Pass, "p1", &both)
            .build(),
    ];
    let targets = BTreeSet::from([name("web-image"), name("kubernetes")]);
    let dev = dev();
    let evaluations = evaluate(&removed.input(&targets, &dev));
    assert!(
        evaluations[1]
            .gaps
            .iter()
            .any(|gap| gap.kind == GapKind::NotRunning && gap.message.contains("no receipt of unit kubernetes")),
        "{:?}",
        evaluations[1].gaps
    );
    assert_eq!(removed.refused().finding.code, Code::PromoteNoEvidence);
    removed.request.select = Selection::StateRevision(desired_commit_of(1));
    assert_eq!(from_run(&removed.promoted().record), run_id(1).to_string());
}
