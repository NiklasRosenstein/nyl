//! Judging what a reconciler reports against a publication: the shared rules
//! behind `accepted` and `healthy` and behind the answer of a teardown wait,
//! whichever reconciler was read.
//!
//! Contract: [Kubernetes publications](../../../../../design/promotion.md#kubernetes-publications)
//! (running revision, matching a publication, coverage, manual syncs, reading
//! absence) and [Attestations](../../../../../design/orchestration-core.md#attestations).
//! Owner of the rules `accepted` and `healthy` follow; the matching to
//! publication commits is [`nyl_render::gitops::observation`]'s, and reading
//! a reconciler is a [`ReconcilerReader`](super::ReconcilerReader)'s. Nothing
//! here names a reconciler: it reads the neutral [`Expected`] and [`Reading`].
//!
//! For one unit's current receipt, whose publication commit is `C`, each
//! deployment the target expects is in one [`State`]:
//!
//! - **accepted**: it runs a revision whose deployment directory tree and
//!   recorded inputs equal those at `C`, so it runs this receipt's
//!   publication, whatever commit it reports (another target's commit and
//!   write-backs on a shared branch included);
//! - **other-publication**: it runs another publication of the target;
//! - **unmatched**: it runs a revision no publication commit explains, such
//!   as a commit another tool made inside its directory, or one that is not
//!   on the publication branch (**not-on-branch**);
//! - **mixed**: a selective or unfinished sync newer than its last full one
//!   leaves it running a mix of revisions;
//! - **never-synced**, **missing**, **deleting**, **other-source**
//!   (it takes a source other than the one Nyl generated), or
//!   **unreadable**.
//!
//! `accepted` passes when every expected deployment is accepted, and
//! `healthy` when, besides, every one reports the health `Healthy` (never
//! `Unknown`). Anything else fails with the deployments that fell short in
//! its reason; the details list every deployment with its running revision,
//! the publication it matched, and its health, so a failure explains itself
//! and promotion can read what runs. Nothing here depends on the time of the
//! reading, so a repeated attestation of an unchanged system is unchanged.
//!
//! A reading proves a deployment is gone only when it is provably visible:
//! [`ensure_provable`] fails the whole evaluation, never one finding, unless
//! every deployment the reading lacks is one the credential could have seen
//! and, where the reader's proof rests on anchors (or the caller concludes
//! from absence), every anchor is visible. An attestation is [`unknown`
//! ](nyl_core::orchestration::driver::AttestationCheck::Unknown) then, and a
//! teardown wait [`Failed`](nyl_core::orchestration::driver::RemovalObservation::Failed),
//! so unknown is never a pass and removal is never concluded from what could
//! not be seen.

use nyl_core::orchestration::driver::AttestationOutcome;
use nyl_core::orchestration::state::AttestationResult;
use nyl_render::gitops::observation::{
    equivalent, running_publication, DeploymentRole, DeploymentTree, Expected, ExpectedDeployment, PublicationHistory,
    RunningMatch, MATCH_WALK_LIMIT,
};
use serde_json::{json, Value};

use super::{Health, Observed, Proof, ReadError, ReadErrorKind, Reading, Running, Visibility};

/// The attestation names the publication unit produces when it observes.
pub const ACCEPTED: &str = "accepted";
pub const HEALTHY: &str = "healthy";

/// What one deployment's state is, for a receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Accepted,
    OtherPublication,
    Unmatched,
    NotOnBranch,
    Mixed,
    NeverSynced,
    Missing,
    Deleting,
    OtherSource,
    Unreadable,
}

impl State {
    /// The state's name in attestation details.
    pub fn name(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::OtherPublication => "other-publication",
            Self::Unmatched => "unmatched",
            Self::NotOnBranch => "not-on-branch",
            Self::Mixed => "mixed",
            Self::NeverSynced => "never-synced",
            Self::Missing => "missing",
            Self::Deleting => "deleting",
            Self::OtherSource => "other-source",
            Self::Unreadable => "unreadable",
        }
    }
}

/// One deployment's finding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub deployment: ExpectedDeployment,
    pub state: State,
    /// The health reported, `None` for a deployment the reading lacks.
    pub health: Option<Health>,
    /// The reconciler's own word for its health, when it is not the name of
    /// `health` (such as `Suspended` for `Unknown`). Shown, never judged.
    pub reported: Option<String>,
    /// The revision the reconciler reports, even when it matched no publication.
    pub revision: Option<String>,
    /// The matched publication, when there is one.
    pub publication: Option<String>,
    pub truncated: bool,
    /// Why the deployment is not accepted, with what to do about it.
    pub reason: Option<String>,
}

impl Finding {
    fn is_healthy(&self) -> bool {
        self.health == Some(Health::Healthy)
    }
}

/// The evaluation of every deployment of a target for one receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Evaluation {
    /// The receipt's publication commit.
    pub commit: String,
    pub findings: Vec<Finding>,
    branch: String,
    path_prefix: String,
}

/// Why an evaluation could not be made.
#[derive(Debug)]
pub enum EvaluateError {
    /// The reading does not prove what its absences would mean.
    Unprovable(ReadError),
    /// The publication branch could not be read.
    History(nyl_render::NylError),
}

impl std::fmt::Display for EvaluateError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unprovable(error) => error.fmt(formatter),
            Self::History(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for EvaluateError {}

/// What a caller concludes from a reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Conclusion {
    /// Findings about what runs: a deployment the reading lacks is a finding.
    Presence,
    /// That a deployment is gone: stricter, since it concludes from absence.
    Absence,
}

/// Fail unless `reading` proves what its absences mean for `deployments`:
///
/// - the reader reports nothing it can prove about the target
///   ([`Reading::unprovable`]);
///
/// The first two apply only to a target with a workload deployment, which
/// is the one that concludes anything from an absence.
///
/// - an anchor is not visible (or not reported), when the reader's proof is
///   [`Proof::Anchors`] or the caller [concludes from absence](Conclusion::Absence):
///   a credential or context for another reconciler, whose list is an empty
///   success, is never mistaken for a target whose deployments are gone;
/// - a deployment the reading lacks is one the credential may not see, or
///   the reader said nothing about.
///
/// Each failure carries the reader's own reason, with its fix.
pub fn ensure_provable<'a>(
    expected: &Expected,
    deployments: impl Iterator<Item = &'a ExpectedDeployment>,
    reading: &Reading,
    conclusion: Conclusion,
) -> Result<(), ReadError> {
    // A target with no workload deployment has nothing to conclude from an
    // absence, so it needs no anchor and an unprovable reader is no obstacle:
    // a reader never has to mirror this rule.
    let anchored = expected.workload().next().is_some();
    if anchored {
        if let Some(reason) = &reading.unprovable {
            return Err(ReadError::new(ReadErrorKind::Declaration, reason.clone()));
        }
    }
    if anchored && (conclusion == Conclusion::Absence || reading.proof == Proof::Anchors) {
        for anchor in &expected.anchors {
            match reading.anchors.get(&anchor.id) {
                Some(Visibility::Visible) => {}
                Some(Visibility::Hidden(reason)) => {
                    return Err(ReadError::new(ReadErrorKind::Forbidden, reason.clone()))
                }
                None => {
                    return Err(ReadError::new(
                        ReadErrorKind::Malformed,
                        format!(
                        "the reading says nothing about anchor {}, so a deployment's absence from it proves nothing",
                        anchor.id
                    ),
                    ))
                }
            }
        }
    }
    for deployment in deployments {
        if reading.deployments.contains_key(&deployment.id) {
            continue;
        }
        match reading.absent.get(&deployment.id) {
            Some(Visibility::Visible) => {}
            Some(Visibility::Hidden(reason)) => return Err(ReadError::new(ReadErrorKind::Forbidden, reason.clone())),
            None => {
                return Err(ReadError::new(
                    ReadErrorKind::Malformed,
                    format!(
                    "the reading says nothing about whether deployment {} could be seen, so its absence proves nothing",
                    deployment.id
                ),
                ))
            }
        }
    }
    Ok(())
}

/// Judge `reading` against the receipt's publication `commit`, for every
/// deployment `expected` names; one the reading lacks is missing. `history`
/// is the fetched publication branch.
pub fn evaluate(
    expected: &Expected,
    commit: &str,
    reading: &Reading,
    history: &dyn PublicationHistory,
) -> Result<Evaluation, EvaluateError> {
    ensure_provable(expected, expected.deployments.iter(), reading, Conclusion::Presence)
        .map_err(EvaluateError::Unprovable)?;
    let committed = history.contains(commit).map_err(EvaluateError::History)?;
    let mut findings = Vec::new();
    for deployment in &expected.deployments {
        let finding = if committed {
            judge(deployment, commit, reading.deployments.get(&deployment.id), history)
                .map_err(EvaluateError::History)?
        } else {
            Finding {
                deployment: deployment.clone(),
                state: State::NotOnBranch,
                health: None,
                reported: None,
                revision: None,
                publication: None,
                truncated: false,
                reason: Some(format!(
                    "the receipt's publication {commit} is not on branch {} of the publication repository, so nothing can run it; \
                     reconcile to publish the tree again",
                    expected.publication.branch
                )),
            }
        };
        findings.push(finding);
    }
    Ok(Evaluation {
        commit: commit.to_owned(),
        findings,
        branch: expected.publication.branch.clone(),
        path_prefix: expected.publication.path_prefix.clone(),
    })
}

/// The ids of the workload deployments `reading` shows, for a teardown wait
/// that ends when none remains. It concludes from absence, so it fails
/// unless every workload deployment the reading lacks was provably visible
/// and every anchor is. It needs no publication history.
pub fn removal(expected: &Expected, reading: &Reading) -> Result<Vec<String>, ReadError> {
    ensure_provable(expected, expected.workload(), reading, Conclusion::Absence)?;
    Ok(expected
        .workload()
        .filter(|deployment| reading.deployments.contains_key(&deployment.id))
        .map(|deployment| deployment.id.clone())
        .collect())
}

fn judge(
    deployment: &ExpectedDeployment,
    commit: &str,
    observed: Option<&Observed>,
    history: &dyn PublicationHistory,
) -> nyl_render::Result<Finding> {
    let mut finding = Finding {
        deployment: deployment.clone(),
        state: State::Missing,
        health: None,
        reported: None,
        revision: None,
        publication: None,
        truncated: false,
        reason: None,
    };
    let Some(revision) = running_revision(&mut finding, observed) else {
        return Ok(finding);
    };
    finding.revision = Some(revision.clone());
    let tree = DeploymentTree {
        path: &deployment.path,
        release: deployment.release.as_deref(),
    };
    match running_publication(history, tree, &revision, MATCH_WALK_LIMIT)? {
        RunningMatch::NotOnBranch => {
            finding.state = State::NotOnBranch;
            finding.reason = Some(format!(
                "it runs revision {revision}, which is not on the publication branch Nyl read: another branch or repository, or a rewritten branch"
            ));
        }
        RunningMatch::NoPublication => {
            finding.state = State::Unmatched;
            finding.reason = Some(format!(
                "it runs revision {revision}, whose directory matches no publication of the target: \
                 another tool changed it, or it was synced from a commit Nyl did not publish"
            ));
        }
        RunningMatch::Matched(matched) => {
            finding.publication = Some(matched.recorded);
            finding.truncated = matched.truncated;
            if equivalent(history, tree, &revision, commit)? {
                finding.state = State::Accepted;
            } else {
                finding.state = State::OtherPublication;
                finding.reason = Some(format!(
                    "it runs another publication ({}), not this receipt's {commit}: have the reconciler apply the current publication",
                    finding.publication.as_deref().unwrap_or_default()
                ));
            }
        }
    }
    Ok(finding)
}

/// How `actual`, a deployment's source, differs from the source Nyl
/// generated, `None` when they agree.
fn source_difference(
    deployment: &ExpectedDeployment,
    actual: &nyl_render::gitops::observation::Source,
) -> Option<String> {
    let differing = deployment.source.differences(actual);
    (!differing.is_empty()).then(|| {
        format!(
            "{} differ",
            differing
                .iter()
                .map(|name| format!("`{name}`"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    })
}

/// The single revision the deployment runs, when it runs one. When it does
/// not, `finding` takes the state and reason that say why: missing,
/// unreadable, deleting, another source, never synced, or a mix.
fn running_revision(finding: &mut Finding, observed: Option<&Observed>) -> Option<String> {
    let deployment = finding.deployment.clone();
    let fall_short = |finding: &mut Finding, state: State, reason: String| {
        finding.state = state;
        finding.reason = Some(reason);
        None
    };
    let Some(observed) = observed else {
        return fall_short(
            finding,
            State::Missing,
            format!(
                "the reconciler reports no deployment {}: it was never created (apply the catalog), was deleted, or the \
                 credential cannot see it",
                deployment.id
            ),
        );
    };
    finding.health = Some(observed.health);
    finding.reported = observed
        .reported
        .clone()
        .filter(|reported| reported != observed.health.name());
    if observed.deleting {
        return fall_short(finding, State::Deleting, "the reconciler is deleting it".to_owned());
    }
    if let Some(difference) = observed
        .source
        .as_ref()
        .and_then(|source| source_difference(&deployment, source))
    {
        return fall_short(
            finding,
            State::OtherSource,
            format!("its source differs from the one Nyl generated ({difference}): restore the generated source by applying the catalog again"),
        );
    }
    match &observed.running {
        Running::None => fall_short(
            finding,
            State::NeverSynced,
            "nothing has been fully applied yet, so it runs nothing: have the reconciler apply it".to_owned(),
        ),
        Running::Mixed { last, reason } => {
            finding.revision.clone_from(last);
            fall_short(finding, State::Mixed, reason.clone())
        }
        Running::Unreadable(reason) => fall_short(finding, State::Unreadable, reason.clone()),
        Running::Exact(revision) => {
            match observed
                .synced_source
                .as_ref()
                .and_then(|source| source_difference(&deployment, source))
            {
                Some(difference) => fall_short(
                    finding,
                    State::OtherSource,
                    format!(
                        "its last sync took a source that differs from the one Nyl generated ({difference}): restore the \
                         generated source by applying the catalog, then have the reconciler apply the deployment again"
                    ),
                ),
                None => Some(revision.clone()),
            }
        }
    }
}

impl Evaluation {
    /// Whether every deployment runs the receipt's publication.
    pub fn accepted(&self) -> bool {
        self.findings.iter().all(|finding| finding.state == State::Accepted)
    }

    /// Whether, besides, every deployment is Healthy.
    pub fn healthy(&self) -> bool {
        self.accepted() && self.findings.iter().all(Finding::is_healthy)
    }

    /// The `accepted` and `healthy` attestations, each with the details;
    /// `reconciler` is the summary of the reconciler that was read.
    pub fn outcomes(&self, reconciler: &Value) -> Vec<AttestationOutcome> {
        let details = self.details(reconciler);
        let not_accepted = self
            .findings
            .iter()
            .filter(|finding| finding.state != State::Accepted)
            .map(describe)
            .collect::<Vec<_>>();
        let unhealthy = self
            .findings
            .iter()
            .filter(|finding| finding.state == State::Accepted && !finding.is_healthy())
            .map(|finding| {
                format!(
                    "{} is {}{}",
                    finding.deployment.id,
                    finding.health.map_or("not reporting health", Health::name),
                    finding
                        .reported
                        .as_ref()
                        .map(|reported| format!(" ({reported})"))
                        .unwrap_or_default()
                )
            })
            .collect::<Vec<_>>();
        let total = self.findings.len();
        let accepted = AttestationOutcome {
            name: ACCEPTED.to_owned(),
            result: result(self.accepted()),
            reason: (!not_accepted.is_empty()).then(|| {
                format!(
                    "{} of {total} deployments do not run publication {}: {}",
                    not_accepted.len(),
                    self.commit,
                    not_accepted.join("; ")
                )
            }),
            details: details.clone(),
        };
        let healthy = AttestationOutcome {
            name: HEALTHY.to_owned(),
            result: result(self.healthy()),
            reason: match (not_accepted.is_empty(), unhealthy.is_empty()) {
                (true, true) => None,
                (false, _) => Some(format!(
                    "not every deployment runs publication {}, so none of it is attested healthy: {}",
                    self.commit,
                    not_accepted.join("; ")
                )),
                (true, false) => Some(format!(
                    "{} of {total} deployments are not Healthy: {}",
                    unhealthy.len(),
                    unhealthy.join("; ")
                )),
            },
            details,
        };
        vec![accepted, healthy]
    }

    /// Per deployment: the running revision, the publication it matched, and
    /// its health, with the state and, when it fell short, why.
    pub fn details(&self, reconciler: &Value) -> Value {
        let deployments = self
            .findings
            .iter()
            .map(|finding| {
                let mut entry = json!({
                    "id": finding.deployment.id,
                    "role": match finding.deployment.role {
                        DeploymentRole::Catalog => "catalog",
                        DeploymentRole::Workload => "workload",
                    },
                    "path": finding.deployment.path,
                    "state": finding.state.name(),
                    "health": finding.health.map(Health::name),
                });
                let object = entry.as_object_mut().expect("an object");
                if let Some(reported) = &finding.reported {
                    object.insert("reported".to_owned(), json!(reported));
                }
                if let Some(release) = &finding.deployment.release {
                    object.insert("release".to_owned(), json!(release));
                }
                if let Some(revision) = &finding.revision {
                    object.insert("running".to_owned(), json!(revision));
                }
                if let Some(publication) = &finding.publication {
                    object.insert("publication".to_owned(), json!(publication));
                    if finding.truncated {
                        object.insert("publicationTruncated".to_owned(), json!(true));
                    }
                }
                if let Some(reason) = &finding.reason {
                    object.insert("reason".to_owned(), json!(reason));
                }
                entry
            })
            .collect::<Vec<_>>();
        json!({
            "publication": {
                "commit": self.commit,
                "branch": self.branch,
                "pathPrefix": self.path_prefix,
            },
            "reconciler": reconciler,
            "deployments": deployments,
        })
    }
}

fn describe(finding: &Finding) -> String {
    format!(
        "{} {} ({})",
        finding.deployment.id,
        finding.state.name(),
        finding.reason.as_deref().unwrap_or("")
    )
}

fn result(pass: bool) -> AttestationResult {
    if pass {
        AttestationResult::Pass
    } else {
        AttestationResult::Fail
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use nyl_render::gitops::observation::{Anchor, ObservedPublication, Revision, Source};

    use super::super::{ReadRequest, ReconcilerReader};
    use super::*;

    /// A branch of publication commits `p1`..: for each commit, the tree of
    /// every deployment directory, and which ones changed the index.
    #[derive(Default)]
    struct Branch {
        chain: Vec<&'static str>,
        trees: BTreeMap<(&'static str, &'static str), &'static str>,
        publishes: BTreeSet<&'static str>,
    }

    impl PublicationHistory for Branch {
        fn contains(&self, revision: &str) -> nyl_render::Result<bool> {
            Ok(self.chain.contains(&revision))
        }
        fn directory(&self, revision: &str, path: &str) -> nyl_render::Result<Option<String>> {
            Ok(self
                .trees
                .iter()
                .find(|((commit, directory), _)| *commit == revision && *directory == path)
                .map(|(_, tree)| (*tree).to_owned()))
        }
        fn recorded_inputs(&self, _: &str, _: &str) -> nyl_render::Result<Option<BTreeMap<String, String>>> {
            Ok(Some(BTreeMap::new()))
        }
        fn publication_commits(&self, revision: &str, limit: usize) -> nyl_render::Result<(Vec<Revision>, bool)> {
            let Some(position) = self.chain.iter().position(|commit| *commit == revision) else {
                return Ok((Vec::new(), false));
            };
            let all = self.chain[..=position]
                .iter()
                .rev()
                .filter(|commit| self.publishes.contains(*commit))
                .map(|commit| (*commit).to_owned())
                .collect::<Vec<_>>();
            Ok((all.into_iter().take(limit).collect(), false))
        }
    }

    /// p1 publishes web=a, api=a; p2 changes web (b); `foreign` is another
    /// target's commit after p2; `edit` changes api's directory without publishing.
    fn branch() -> Branch {
        let mut branch = Branch::default();
        let commits = [
            ("p1", "a", "a", "a", true),
            ("p2", "b", "a", "b", true),
            ("foreign", "b", "a", "b", false),
            ("edit", "b", "edited", "b", false),
        ];
        for (commit, web, api, catalog, publishes) in commits {
            branch.chain.push(commit);
            branch.trees.insert((commit, "dev/web/web"), web);
            branch.trees.insert((commit, "dev/api/api"), api);
            branch.trees.insert((commit, "dev/_nyl/catalog"), catalog);
            if publishes {
                branch.publishes.insert(commit);
            }
        }
        branch
    }

    /// What Nyl generated as a deployment's source.
    fn generated_source(path: &str) -> Source {
        Source {
            repository: "https://example.invalid/deploy.git".to_owned(),
            path: path.to_owned(),
            options: json!({"directory": {"recurse": true}}),
        }
    }

    /// The ids are no reconciler's: any reader that names its deployments
    /// and fills a [`Reading`] is judged by the same rules.
    fn deployment(role: DeploymentRole, id: &str, path: &str) -> ExpectedDeployment {
        ExpectedDeployment {
            id: id.to_owned(),
            display: String::new(),
            role,
            path: path.to_owned(),
            release: (role == DeploymentRole::Workload).then(|| format!("{id}/{id}")),
            source: generated_source(path),
            locator: Value::Null,
        }
    }

    fn expected() -> Expected {
        Expected {
            publication: ObservedPublication {
                url: "https://example.invalid/deploy.git".to_owned(),
                branch: "deploy".to_owned(),
                path_prefix: "dev".to_owned(),
            },
            deployments: vec![
                deployment(DeploymentRole::Catalog, "dev-catalog", "dev/_nyl/catalog"),
                deployment(DeploymentRole::Workload, "api", "dev/api/api"),
                deployment(DeploymentRole::Workload, "web", "dev/web/web"),
            ],
            anchors: vec![Anchor {
                id: "dev-catalog".to_owned(),
                locator: Value::Null,
            }],
        }
    }

    fn observed(revision: &str, health: Health, path: &str) -> Observed {
        Observed {
            running: Running::Exact(revision.to_owned()),
            health,
            reported: None,
            deleting: false,
            source: Some(generated_source(path)),
            synced_source: Some(generated_source(path)),
        }
    }

    /// A reader of a made-up reconciler: it has what it was given, can see
    /// everything but `hidden`, and proves absence the way it is told.
    struct TestReader {
        has: BTreeMap<String, Observed>,
        hidden: BTreeSet<String>,
        anchors_visible: bool,
        proof: Proof,
        unprovable: Option<String>,
    }

    impl TestReader {
        fn new(has: BTreeMap<String, Observed>) -> Self {
            Self {
                has,
                hidden: BTreeSet::new(),
                anchors_visible: true,
                proof: Proof::PerDeployment,
                unprovable: None,
            }
        }
    }

    impl ReconcilerReader for TestReader {
        fn read(&self, request: &ReadRequest<'_>) -> Result<Reading, ReadError> {
            let visibility = |visible: bool, what: &str| {
                if visible {
                    Visibility::Visible
                } else {
                    Visibility::Hidden(format!("{what} cannot be seen"))
                }
            };
            Ok(Reading {
                deployments: request
                    .deployments
                    .iter()
                    .filter_map(|deployment| Some((deployment.id.clone(), self.has.get(&deployment.id)?.clone())))
                    .collect(),
                anchors: if request.concluding_absence || self.proof == Proof::Anchors {
                    request
                        .anchors
                        .iter()
                        .map(|anchor| (anchor.id.clone(), visibility(self.anchors_visible, &anchor.id)))
                        .collect()
                } else {
                    BTreeMap::new()
                },
                absent: request
                    .deployments
                    .iter()
                    .filter(|deployment| !self.has.contains_key(&deployment.id))
                    .map(|deployment| {
                        (
                            deployment.id.clone(),
                            visibility(!self.hidden.contains(&deployment.id), &deployment.id),
                        )
                    })
                    .collect(),
                proof: self.proof,
                unprovable: self.unprovable.clone(),
            })
        }
    }

    fn running(catalog: &str, api: &str, web: &str) -> BTreeMap<String, Observed> {
        BTreeMap::from([
            (
                "dev-catalog".to_owned(),
                observed(catalog, Health::Healthy, "dev/_nyl/catalog"),
            ),
            ("api".to_owned(), observed(api, Health::Healthy, "dev/api/api")),
            ("web".to_owned(), observed(web, Health::Healthy, "dev/web/web")),
        ])
    }

    fn read(reader: &TestReader, expected: &Expected, concluding_absence: bool) -> Reading {
        reader
            .read(&ReadRequest {
                deployments: &expected.deployments,
                anchors: &expected.anchors,
                concluding_absence,
            })
            .unwrap()
    }

    fn try_judge(commit: &str, reader: &TestReader) -> Result<Evaluation, EvaluateError> {
        let expected = expected();
        evaluate(&expected, commit, &read(reader, &expected, false), &branch())
    }

    fn judged(commit: &str, has: &BTreeMap<String, Observed>) -> Evaluation {
        try_judge(commit, &TestReader::new(has.clone())).unwrap()
    }

    fn states(evaluation: &Evaluation) -> Vec<(&str, &'static str)> {
        evaluation
            .findings
            .iter()
            .map(|finding| (finding.deployment.id.as_str(), finding.state.name()))
            .collect()
    }

    fn summary() -> Value {
        json!({"kind": "test", "instance": "made-up"})
    }

    /// Catches the common success path, and the reconciler's own "out of
    /// date" notion being read: deployments that run the receipt's
    /// publication, through a commit of another target on the shared branch,
    /// are accepted and healthy, over a reader that is no Argo CD's.
    #[test]
    fn test_deployments_running_the_receipts_publication_are_accepted_and_healthy() {
        let evaluation = judged("p2", &running("foreign", "p1", "p2"));
        assert_eq!(
            states(&evaluation),
            [("dev-catalog", "accepted"), ("api", "accepted"), ("web", "accepted")]
        );
        assert!(evaluation.accepted() && evaluation.healthy());
        let outcomes = evaluation.outcomes(&summary());
        assert_eq!(
            outcomes
                .iter()
                .map(|outcome| (outcome.name.as_str(), outcome.result))
                .collect::<Vec<_>>(),
            [(ACCEPTED, AttestationResult::Pass), (HEALTHY, AttestationResult::Pass)]
        );
        assert!(outcomes.iter().all(|outcome| outcome.reason.is_none()));
        // api still runs p1: the commit that introduced what runs, for a promotion to name.
        let api = &evaluation.findings[1];
        assert_eq!(
            (api.revision.as_deref(), api.publication.as_deref()),
            (Some("p1"), Some("p1"))
        );
        let details = &outcomes[0].details;
        assert_eq!(details["publication"]["commit"], "p2");
        assert_eq!(details["publication"]["branch"], "deploy");
        assert_eq!(details["deployments"][0]["id"], "dev-catalog");
        assert_eq!(details["deployments"][0]["running"], "foreign");
        assert_eq!(details["deployments"][0]["publication"], "p2");
        assert_eq!(details["deployments"][0]["health"], "Healthy");
        assert_eq!(details["reconciler"], summary());
    }

    /// Catches a deployment on an older publication passing, and the
    /// reason not naming what runs and what to do.
    #[test]
    fn test_a_deployment_on_another_publication_fails_accepted_and_healthy() {
        let evaluation = judged("p2", &running("p2", "p1", "p1"));
        assert_eq!(states(&evaluation)[2], ("web", "other-publication"));
        let outcomes = evaluation.outcomes(&summary());
        assert_eq!(outcomes[0].result, AttestationResult::Fail);
        assert_eq!(outcomes[1].result, AttestationResult::Fail);
        let reason = outcomes[0].reason.as_deref().unwrap();
        assert!(
            reason.starts_with("1 of 3 deployments do not run publication p2"),
            "{reason}"
        );
        assert!(
            reason.contains("web other-publication") && reason.contains("another publication (p1)"),
            "{reason}"
        );
        assert!(outcomes[1]
            .reason
            .as_deref()
            .unwrap()
            .contains("none of it is attested healthy"));
    }

    /// Catches health being attested for deployments that do not run the
    /// publication, and anything but `Healthy` (an unknown health included)
    /// passing `healthy` while `accepted` passes.
    #[test]
    fn test_healthy_needs_every_deployment_healthy_besides_accepted() {
        let mut state = running("p2", "p1", "p2");
        state.get_mut("web").unwrap().health = Health::Degraded;
        let evaluation = judged("p2", &state);
        let outcomes = evaluation.outcomes(&summary());
        assert_eq!(outcomes[0].result, AttestationResult::Pass);
        assert_eq!(outcomes[1].result, AttestationResult::Fail);
        assert_eq!(
            outcomes[1].reason.as_deref(),
            Some("1 of 3 deployments are not Healthy: web is Degraded")
        );
        for health in [Health::Progressing, Health::Unknown] {
            state.get_mut("web").unwrap().health = health;
            assert!(!judged("p2", &state).healthy(), "{health:?}");
        }
    }

    /// Catches a hand-edited deployment directory attesting as published,
    /// an unknown revision matching by luck, and a deployment nothing
    /// reports being skipped instead of failing the aggregate.
    #[test]
    fn test_unmatched_missing_and_never_synced_deployments_fail() {
        let mut state = running("p2", "edit", "p2");
        assert_eq!(states(&judged("p2", &state))[1], ("api", "unmatched"));
        state.insert(
            "api".to_owned(),
            observed(&"f".repeat(40), Health::Healthy, "dev/api/api"),
        );
        assert_eq!(states(&judged("p2", &state))[1], ("api", "not-on-branch"));
        state.remove("api");
        let evaluation = judged("p2", &state);
        assert_eq!(states(&evaluation)[1], ("api", "missing"));
        assert!(!evaluation.accepted());
        assert_eq!(evaluation.findings[1].health, None);
        state.insert(
            "api".to_owned(),
            Observed {
                running: Running::None,
                ..observed("", Health::Unknown, "dev/api/api")
            },
        );
        assert_eq!(states(&judged("p2", &state))[1], ("api", "never-synced"));
        state.insert(
            "api".to_owned(),
            Observed {
                running: Running::Unreadable("it names two revisions".to_owned()),
                ..observed("", Health::Unknown, "dev/api/api")
            },
        );
        assert_eq!(states(&judged("p2", &state))[1], ("api", "unreadable"));
    }

    /// Catches a mix of revisions, a deleting deployment, and one that takes
    /// another source vouching for the receipt's publication.
    #[test]
    fn test_mixed_deleting_and_other_source_deployments_fail() {
        let mut state = running("p2", "p1", "p2");
        let web = || observed("p2", Health::Healthy, "dev/web/web");
        state.insert(
            "web".to_owned(),
            Observed {
                running: Running::Mixed {
                    last: Some("p2".to_owned()),
                    reason: "a selective sync newer than its last full one".to_owned(),
                },
                ..web()
            },
        );
        let evaluation = judged("p2", &state);
        assert_eq!(states(&evaluation)[2], ("web", "mixed"));
        assert_eq!(evaluation.findings[2].revision.as_deref(), Some("p2"));
        assert_eq!(
            evaluation.findings[2].reason.as_deref(),
            Some("a selective sync newer than its last full one")
        );
        state.insert(
            "web".to_owned(),
            Observed {
                deleting: true,
                ..web()
            },
        );
        assert_eq!(states(&judged("p2", &state))[2], ("web", "deleting"));
        state.insert(
            "web".to_owned(),
            Observed {
                source: Some(generated_source("elsewhere")),
                ..web()
            },
        );
        assert_eq!(states(&judged("p2", &state))[2], ("web", "other-source"));
        // The source says dev/web/web, but the last sync took another directory.
        state.insert(
            "web".to_owned(),
            Observed {
                synced_source: Some(generated_source("dev/other")),
                ..web()
            },
        );
        assert_eq!(states(&judged("p2", &state))[2], ("web", "other-source"));
    }

    /// Catches a deployment edited to render its directory differently (an
    /// option of its source, or the repository) passing because it still
    /// sources the same path, in its configuration or in what its last sync
    /// took, and the reason not naming what differs.
    #[test]
    fn test_a_deployment_whose_source_was_edited_is_not_accepted() {
        type Edit = (&'static str, fn(&mut Source));
        let edits: [Edit; 3] = [
            ("recurse", |source| {
                source.options["directory"]["recurse"] = json!(false);
            }),
            ("exclude", |source| {
                source.options["directory"]["exclude"] = json!("*.yaml");
            }),
            ("repository", |source| {
                source.repository = "https://example.invalid/other.git".to_owned();
            }),
        ];
        for (edit, apply) in edits {
            let mut state = running("p2", "p1", "p2");
            let mut configured = observed("p2", Health::Healthy, "dev/web/web");
            apply(configured.source.as_mut().unwrap());
            state.insert("web".to_owned(), configured);
            let evaluation = judged("p2", &state);
            assert_eq!(states(&evaluation)[2], ("web", "other-source"), "{edit}");
            let reason = evaluation.findings[2].reason.as_deref().unwrap();
            assert!(reason.contains("differs from the one Nyl generated"), "{reason}");
            let mut synced = observed("p2", Health::Healthy, "dev/web/web");
            apply(synced.synced_source.as_mut().unwrap());
            state.insert("web".to_owned(), synced);
            assert_eq!(states(&judged("p2", &state))[2], ("web", "other-source"), "{edit}");
        }
    }

    /// Catches a receipt whose publication is no longer on the branch
    /// attesting anything: every deployment fails, never "unknown".
    #[test]
    fn test_a_receipt_publication_missing_from_the_branch_fails_everything() {
        let evaluation = judged(&"e".repeat(40), &running("p2", "p1", "p2"));
        assert!(evaluation
            .findings
            .iter()
            .all(|finding| finding.state == State::NotOnBranch));
        let outcomes = evaluation.outcomes(&summary());
        assert!(outcomes.iter().all(|outcome| outcome.result == AttestationResult::Fail));
        assert!(outcomes[0]
            .reason
            .as_deref()
            .unwrap()
            .contains("reconcile to publish the tree again"));
    }

    /// Catches details that change between two readings of an unchanged
    /// system, which would write a commit per `nyl attest`.
    #[test]
    fn test_details_are_stable() {
        let state = running("foreign", "p1", "p2");
        let first = judged("p2", &state).outcomes(&summary());
        let second = judged("p2", &state).outcomes(&summary());
        assert_eq!(first, second);
    }

    /// A target that expects no deployment has nothing to accept.
    #[test]
    fn test_a_target_without_deployments_attests_vacuously() {
        let mut empty = expected();
        empty.deployments.clear();
        empty.anchors.clear();
        let reading = read(&TestReader::new(BTreeMap::new()), &empty, false);
        let evaluation = evaluate(&empty, "p2", &reading, &branch()).unwrap();
        assert!(evaluation.accepted() && evaluation.healthy());
        assert_eq!(evaluation.details(&summary())["deployments"], json!([]));
    }

    /// Catches a deployment's absence being judged when nothing proves it
    /// would have been seen: unknown is never a pass, so the whole
    /// evaluation fails with the reader's reason instead of reporting the
    /// deployment missing, whichever way the reader proves absence.
    #[test]
    fn test_an_absence_the_reader_cannot_prove_fails_the_evaluation() {
        let mut state = running("p2", "p1", "p2");
        state.remove("api");
        let mut reader = TestReader::new(state.clone());
        assert_eq!(states(&try_judge("p2", &reader).unwrap())[1], ("api", "missing"));
        reader.hidden.insert("api".to_owned());
        let error = try_judge("p2", &reader).unwrap_err();
        assert!(
            matches!(&error, EvaluateError::Unprovable(error) if error.kind == ReadErrorKind::Forbidden && error.message == "api cannot be seen"),
            "{error}"
        );
        // A reader that does not answer for the deployment proves nothing either.
        let expected = expected();
        let mut reading = read(&TestReader::new(state), &expected, false);
        reading.absent.clear();
        let error = evaluate(&expected, "p2", &reading, &branch()).unwrap_err();
        assert!(matches!(error, EvaluateError::Unprovable(ref error) if error.kind == ReadErrorKind::Malformed));
    }

    /// Catches a reading from another reconciler (an empty list is a
    /// success) passing as the target's: for a reader whose proof rests on
    /// anchors, an anchor that is not visible fails the evaluation even when
    /// every deployment is in the reading, as does a reader that can prove
    /// nothing about the target; a reader that answers per deployment is not
    /// held to anchors for an attestation.
    #[test]
    fn test_anchors_gate_a_reader_whose_proof_rests_on_them() {
        let state = running("p2", "p1", "p2");
        let mut reader = TestReader::new(state);
        reader.proof = Proof::Anchors;
        reader.anchors_visible = false;
        let error = try_judge("p2", &reader).unwrap_err();
        assert!(
            matches!(&error, EvaluateError::Unprovable(error) if error.message == "dev-catalog cannot be seen"),
            "{error}"
        );
        reader.proof = Proof::PerDeployment;
        assert!(try_judge("p2", &reader).unwrap().accepted());
        reader.proof = Proof::Anchors;
        reader.anchors_visible = true;
        reader.unprovable = Some("nothing anchors this target".to_owned());
        let error = try_judge("p2", &reader).unwrap_err();
        assert!(
            matches!(&error, EvaluateError::Unprovable(error) if error.kind == ReadErrorKind::Declaration && error.message == "nothing anchors this target"),
            "{error}"
        );
    }

    /// Catches a teardown wait concluding removal from what it could not
    /// see: it reports the workload deployments the reading shows (the
    /// catalog does not count), and fails instead when a missing one may be
    /// hidden or an anchor is not visible, even for a reader that answers per
    /// deployment, since removal concludes from absence.
    #[test]
    fn test_removal_reports_the_workload_that_remains_and_only_from_what_is_visible() {
        let expected = expected();
        let workload = |has: BTreeMap<String, Observed>, configure: fn(&mut TestReader)| {
            let mut reader = TestReader::new(has);
            configure(&mut reader);
            let reading = read(&reader, &expected, true);
            removal(&expected, &reading)
        };
        let all = running("p2", "p1", "p2");
        assert_eq!(workload(all.clone(), |_| {}).unwrap(), ["api", "web"]);
        let mut only_catalog = all.clone();
        only_catalog.remove("api");
        only_catalog.remove("web");
        assert!(workload(only_catalog.clone(), |_| {}).unwrap().is_empty());
        let mut web = all;
        web.remove("api");
        assert_eq!(workload(web, |_| {}).unwrap(), ["web"]);
        let hidden = workload(only_catalog.clone(), |reader| {
            reader.hidden.insert("api".to_owned());
        })
        .unwrap_err();
        assert_eq!(
            (hidden.kind, hidden.message.as_str()),
            (ReadErrorKind::Forbidden, "api cannot be seen")
        );
        let unanchored = workload(only_catalog, |reader| reader.anchors_visible = false).unwrap_err();
        assert_eq!(unanchored.message, "dev-catalog cannot be seen");
    }

    /// Catches the reconciler's own health word being lost or judged: a
    /// health that is `Unknown` because the reconciler said `Suspended` shows
    /// that word in the reason and the details, a word that only repeats the
    /// health is not shown twice, and neither changes a pass or a failure.
    #[test]
    fn test_the_reported_health_word_is_shown_and_never_judged() {
        let mut state = running("p2", "p1", "p2");
        state.get_mut("web").unwrap().health = Health::Unknown;
        state.get_mut("web").unwrap().reported = Some("Suspended".to_owned());
        state.get_mut("api").unwrap().reported = Some("Healthy".to_owned());
        let evaluation = judged("p2", &state);
        let outcomes = evaluation.outcomes(&summary());
        assert_eq!(
            outcomes[1].reason.as_deref(),
            Some("1 of 3 deployments are not Healthy: web is Unknown (Suspended)")
        );
        let details = &outcomes[0].details["deployments"];
        assert_eq!(details[2]["reported"], "Suspended");
        assert!(details[1].get("reported").is_none(), "{}", details[1]);
        // A reconciler word that reads like health changes nothing.
        let mut lying = running("p2", "p1", "p2");
        lying.get_mut("web").unwrap().health = Health::Unknown;
        lying.get_mut("web").unwrap().reported = Some("Healthy".to_owned());
        assert!(!judged("p2", &lying).healthy());
    }

    /// Catches a target without a workload deployment needing anchors it
    /// cannot have, or a reader having to mirror that rule: with only a
    /// catalog, a hidden anchor and a reader that proves nothing are no
    /// obstacle to an attestation or to a removal that finds nothing left.
    #[test]
    fn test_a_target_without_workload_needs_no_anchor() {
        let mut expected = expected();
        expected
            .deployments
            .retain(|deployment| deployment.role == DeploymentRole::Catalog);
        let mut reader = TestReader::new(running("p2", "p1", "p2"));
        reader.proof = Proof::Anchors;
        reader.anchors_visible = false;
        reader.unprovable = Some("nothing anchors this target".to_owned());
        let reading = read(&reader, &expected, true);
        assert!(evaluate(&expected, "p2", &reading, &branch()).unwrap().accepted());
        assert!(removal(&expected, &reading).unwrap().is_empty());
    }
}
