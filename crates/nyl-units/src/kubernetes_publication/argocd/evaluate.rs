//! Judging what Argo CD reports against a publication: the shared rules
//! behind `accepted` and `healthy`, whichever way Argo CD was read.
//!
//! Contract: [Kubernetes publications](../../../../../design/promotion.md#kubernetes-publications)
//! (running revision, matching a publication, coverage, manual syncs) and
//! [Attestations](../../../../../design/orchestration-core.md#attestations).
//! Owner of the rules `accepted` and `healthy` follow; the matching to
//! publication commits is [`nyl_render::gitops::observation`]'s, and
//! reading Argo CD is a reader's.
//!
//! For one unit's current receipt, whose publication commit is `C`, each
//! Application the target generates is in one [`State`]:
//!
//! - **accepted**: it runs a revision whose Application directory tree and
//!   recorded inputs equal those at `C`, so it runs this receipt's
//!   publication, whatever commit it reports (another target's commit and
//!   write-backs on a shared branch included);
//! - **other-publication**: it runs another publication of the target;
//! - **unmatched**: it runs a revision no publication commit explains, such
//!   as a commit another tool made inside its directory, or one that is not
//!   on the publication branch (**not-on-branch**);
//! - **mixed**: a selective or unfinished sync newer than its history leaves
//!   it running a mix of revisions;
//! - **never-synced**, **missing**, **deleting**, **other-source**
//!   (it syncs a directory other than the one Nyl generated), or
//!   **unreadable**.
//!
//! `accepted` passes when every generated Application is accepted, and
//! `healthy` when, besides, every one reports the live health `Healthy`.
//! Anything else fails with the Applications that fell short in its reason;
//! the details list every Application with its running revision, the
//! publication it matched, and its health, so a failure explains itself and
//! promotion can read what runs. Nothing here depends on the time of the
//! reading, so a repeated attestation of an unchanged system is unchanged.

use std::collections::BTreeMap;

use nyl_core::orchestration::driver::AttestationOutcome;
use nyl_core::orchestration::state::AttestationResult;
use nyl_render::gitops::observation::{
    equivalent, running_publication, ApplicationRole, ApplicationTree, ArgoCdObservation, GeneratedApplication,
    PublicationHistory, RunningMatch, MATCH_WALK_LIMIT,
};
use serde_json::{json, Value};

use super::live::{LiveApplication, Mixed, Running};

/// The attestation names the publication unit produces in observe mode.
pub const ACCEPTED: &str = "accepted";
pub const HEALTHY: &str = "healthy";

/// What one Application's state is, for a receipt.
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

/// One Application's finding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub application: GeneratedApplication,
    pub state: State,
    pub health: Option<String>,
    /// The revision Argo CD reports, even when it matched no publication.
    pub revision: Option<String>,
    /// The matched publication, when there is one.
    pub publication: Option<String>,
    pub truncated: bool,
    /// Why the Application is not accepted, with what to do about it.
    pub reason: Option<String>,
}

impl Finding {
    fn is_healthy(&self) -> bool {
        self.health.as_deref() == Some("Healthy")
    }
}

/// The evaluation of every Application of a target for one receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Evaluation {
    /// The receipt's publication commit.
    pub commit: String,
    pub findings: Vec<Finding>,
}

/// Judge `live` (by namespace and name; an Application not in it is
/// missing) against the receipt's publication `commit`, for every
/// Application `observation` declares. `history` is the fetched publication
/// branch. An error is an unreadable repository.
pub fn evaluate(
    observation: &ArgoCdObservation,
    commit: &str,
    live: &BTreeMap<(String, String), Value>,
    history: &dyn PublicationHistory,
) -> nyl_render::Result<Evaluation> {
    let committed = history.contains(commit)?;
    let mut findings = Vec::new();
    for application in &observation.applications {
        let finding = if committed {
            judge(
                application,
                commit,
                live.get(&(application.namespace.clone(), application.name.clone())),
                history,
            )?
        } else {
            let base = |state, reason: String| Finding {
                application: application.clone(),
                state,
                health: None,
                revision: None,
                publication: None,
                truncated: false,
                reason: Some(reason),
            };
            base(
                State::NotOnBranch,
                format!(
                    "the receipt's publication {commit} is not on branch {} of the publication repository, so nothing can run it; \
                     reconcile to publish the tree again",
                    observation.publication.branch
                ),
            )
        };
        findings.push(finding);
    }
    Ok(Evaluation {
        commit: commit.to_owned(),
        findings,
    })
}

fn judge(
    application: &GeneratedApplication,
    commit: &str,
    live: Option<&Value>,
    history: &dyn PublicationHistory,
) -> nyl_render::Result<Finding> {
    let mut finding = Finding {
        application: application.clone(),
        state: State::Missing,
        health: None,
        revision: None,
        publication: None,
        truncated: false,
        reason: None,
    };
    let Some(revision) = running_revision(&mut finding, live) else {
        return Ok(finding);
    };
    finding.revision = Some(revision.clone());
    let tree = ApplicationTree {
        path: &application.path,
        release: application.release.as_deref(),
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
                    "it runs another publication ({}), not this receipt's {commit}: sync it",
                    finding.publication.as_deref().unwrap_or_default()
                ));
            }
        }
    }
    Ok(finding)
}

/// How `actual`, an Application's source (as it is now, or as its last sync
/// took it), differs from the source Nyl generated, `None` when they agree.
/// Both are compared without `targetRevision` and without empty values, so
/// what Argo CD defaults or omits is no difference; a source recorded before
/// the generated one was is compared by its directory only.
fn source_difference(application: &GeneratedApplication, actual: &Value) -> Option<String> {
    if application.source.is_null() {
        let path = actual.get("path").and_then(Value::as_str);
        return path
            .filter(|path| *path != application.path)
            .map(|path| format!("path {path:?}, not {:?}", application.path));
    }
    let (generated, actual) = (normalized(&application.source), normalized(actual));
    if generated == actual {
        return None;
    }
    let (Value::Object(generated), Value::Object(actual)) = (&generated, &actual) else {
        return Some("not a mapping".to_owned());
    };
    let differing = generated
        .keys()
        .chain(actual.keys())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .filter(|key| generated.get(*key) != actual.get(*key))
        .map(|key| format!("`{key}`"))
        .collect::<Vec<_>>();
    Some(format!("{} differ", differing.join(", ")))
}

/// A source without `targetRevision`, nulls, and empty containers.
fn normalized(source: &Value) -> Value {
    fn prune(value: &Value) -> Option<Value> {
        match value {
            Value::Null => None,
            Value::Object(map) => {
                let map = map
                    .iter()
                    .filter_map(|(key, value)| prune(value).map(|value| (key.clone(), value)))
                    .collect::<serde_json::Map<_, _>>();
                (!map.is_empty()).then_some(Value::Object(map))
            }
            Value::Array(items) => {
                let items = items.iter().filter_map(prune).collect::<Vec<_>>();
                (!items.is_empty()).then_some(Value::Array(items))
            }
            other => Some(other.clone()),
        }
    }
    let mut source = source.clone();
    if let Some(object) = source.as_object_mut() {
        object.remove("targetRevision");
    }
    prune(&source).unwrap_or(Value::Null)
}

/// The single revision the live Application runs, when it runs one. When it
/// does not, `finding` takes the state and reason that say why: missing,
/// unreadable, deleting, another source directory, never synced, or a mix.
fn running_revision(finding: &mut Finding, live: Option<&Value>) -> Option<String> {
    let application = finding.application.clone();
    let mut fall_short = |state: State, reason: String| {
        finding.state = state;
        finding.reason = Some(reason);
        None
    };
    let Some(live) = live else {
        return fall_short(
            State::Missing,
            format!(
                "Argo CD reports no Application {}/{}: it was never created (sync the catalog), was deleted, or the \
                 credential cannot see it",
                application.namespace, application.name
            ),
        );
    };
    let parsed = match LiveApplication::parse(live) {
        Ok(parsed) => parsed,
        Err(error) => return fall_short(State::Unreadable, error),
    };
    finding.health.clone_from(&parsed.health);
    let mut fall_short = |state: State, reason: String| {
        finding.state = state;
        finding.reason = Some(reason);
        None
    };
    if parsed.deleting {
        return fall_short(State::Deleting, "Argo CD is deleting it".to_owned());
    }
    if let Some(difference) = parsed
        .source
        .as_ref()
        .and_then(|source| source_difference(&application, source))
    {
        return fall_short(
            State::OtherSource,
            format!(
                "its spec.source differs from the one Nyl generated ({difference}): sync the catalog to restore it"
            ),
        );
    }
    match parsed.running {
        Running::NeverSynced => fall_short(
            State::NeverSynced,
            "no full sync has completed, so it runs nothing yet: sync it".to_owned(),
        ),
        Running::Mixed { history: before, why } => {
            finding.revision = before;
            let reason = match why {
                Mixed::SelectiveSync => {
                    "a selective sync newer than its last full sync leaves it running a mix of revisions: sync the whole Application".to_owned()
                }
                Mixed::DryRun => {
                    "a dry run is its latest operation, so an earlier selective sync may be hidden behind it: sync the whole Application".to_owned()
                }
                Mixed::UnfinishedSync(phase) => format!(
                    "a full sync newer than its last completed one is {phase}, so it may run part of another revision: \
                     wait for it, or sync again"
                ),
            };
            finding.state = State::Mixed;
            finding.reason = Some(reason);
            None
        }
        Running::Unreadable(reason) => fall_short(State::Unreadable, reason),
        Running::Revision { revision, source } => {
            match source.and_then(|source| source_difference(&application, &source)) {
                Some(difference) => fall_short(
                    State::OtherSource,
                    format!(
                        "its last sync took a source that differs from the one Nyl generated ({difference}): sync the \
                     catalog and the Application"
                    ),
                ),
                None => Some(revision),
            }
        }
    }
}

impl Evaluation {
    /// Whether every Application runs the receipt's publication.
    pub fn accepted(&self) -> bool {
        self.findings.iter().all(|finding| finding.state == State::Accepted)
    }

    /// Whether, besides, every Application is Healthy.
    pub fn healthy(&self) -> bool {
        self.accepted() && self.findings.iter().all(Finding::is_healthy)
    }

    /// The `accepted` and `healthy` attestations, each with the details.
    pub fn outcomes(&self, observation: &ArgoCdObservation) -> Vec<AttestationOutcome> {
        let details = self.details(observation);
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
                    "{} is {}",
                    finding.application.name,
                    finding.health.as_deref().unwrap_or("not reporting health")
                )
            })
            .collect::<Vec<_>>();
        let total = self.findings.len();
        let accepted = AttestationOutcome {
            name: ACCEPTED.to_owned(),
            result: result(self.accepted()),
            reason: (!not_accepted.is_empty()).then(|| {
                format!(
                    "{} of {total} Applications do not run publication {}: {}",
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
                    "not every Application runs publication {}, so none of it is attested healthy: {}",
                    self.commit,
                    not_accepted.join("; ")
                )),
                (true, false) => Some(format!(
                    "{} of {total} Applications are not Healthy: {}",
                    unhealthy.len(),
                    unhealthy.join("; ")
                )),
            },
            details,
        };
        vec![accepted, healthy]
    }

    /// Per Application: the running revision, the publication it matched, and
    /// its health, with the state and, when it fell short, why.
    pub fn details(&self, observation: &ArgoCdObservation) -> Value {
        let applications = self
            .findings
            .iter()
            .map(|finding| {
                let mut entry = json!({
                    "name": finding.application.name,
                    "namespace": finding.application.namespace,
                    "role": match finding.application.role {
                        ApplicationRole::Catalog => "catalog",
                        ApplicationRole::Workload => "workload",
                    },
                    "path": finding.application.path,
                    "state": finding.state.name(),
                    "health": finding.health,
                });
                let object = entry.as_object_mut().expect("an object");
                if let Some(release) = &finding.application.release {
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
                "branch": observation.publication.branch,
                "pathPrefix": observation.publication.path_prefix,
            },
            "instance": {
                "name": observation.location.instance,
                "namespace": observation.location.namespace,
                "access": match observation.location.access.method {
                    nyl_render::resources::ArgoCDAccessMethod::Kubernetes => "kubernetes",
                    nyl_render::resources::ArgoCDAccessMethod::Api => "api",
                },
            },
            "applications": applications,
        })
    }
}

fn describe(finding: &Finding) -> String {
    format!(
        "{} {} ({})",
        finding.application.name,
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
    use std::collections::BTreeSet;

    use nyl_render::gitops::observation::{ArgoCdLocation, ObservedPublication, Revision};
    use nyl_render::resources::ArgoCDAccess;

    use super::*;

    /// A branch of publication commits `p1`..: for each commit, the tree of
    /// every Application directory (`dir`), and which ones changed the index.
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

    fn application(role: ApplicationRole, name: &str, path: &str) -> GeneratedApplication {
        GeneratedApplication {
            role,
            name: name.to_owned(),
            namespace: "argocd".to_owned(),
            path: path.to_owned(),
            project: "default".to_owned(),
            release: (role == ApplicationRole::Workload).then(|| format!("{name}/{name}")),
            source: generated_source(path),
        }
    }

    /// What Nyl generates as `spec.source`, without `targetRevision`.
    fn generated_source(path: &str) -> Value {
        json!({"repoURL": "https://example.invalid/deploy.git", "path": path, "directory": {"recurse": true}})
    }

    fn observation() -> ArgoCdObservation {
        ArgoCdObservation {
            location: ArgoCdLocation {
                instance: "implicit:dev".to_owned(),
                namespace: "argocd".to_owned(),
                cluster: "dev".to_owned(),
                context: Some("dev".to_owned()),
                access: ArgoCDAccess::kubernetes(),
            },
            publication: ObservedPublication {
                url: "https://example.invalid/deploy.git".to_owned(),
                branch: "deploy".to_owned(),
                path_prefix: "dev".to_owned(),
            },
            applications: vec![
                application(ApplicationRole::Catalog, "dev-catalog", "dev/_nyl/catalog"),
                application(ApplicationRole::Workload, "api", "dev/api/api"),
                application(ApplicationRole::Workload, "web", "dev/web/web"),
            ],
        }
    }

    /// The source as Argo CD serves it: Nyl's, with the target revision.
    type Edit = (&'static str, fn(&mut Value));

    fn live_source(path: &str) -> Value {
        let mut source = generated_source(path);
        source["targetRevision"] = json!("deploy");
        source
    }

    fn live(name: &str, revision: &str, health: &str, path: &str) -> ((String, String), Value) {
        (
            ("argocd".to_owned(), name.to_owned()),
            json!({
                "metadata": {"name": name},
                "spec": {"source": live_source(path)},
                "status": {
                    "sync": {"status": "OutOfSync"},
                    "health": {"status": health},
                    "history": [{"id": 1, "revision": revision, "deployedAt": "2026-01-01T00:00:00Z",
                                 "source": live_source(path)}],
                },
            }),
        )
    }

    fn running(catalog: &str, api: &str, web: &str) -> BTreeMap<(String, String), Value> {
        BTreeMap::from([
            live("dev-catalog", catalog, "Healthy", "dev/_nyl/catalog"),
            live("api", api, "Healthy", "dev/api/api"),
            live("web", web, "Healthy", "dev/web/web"),
        ])
    }

    fn judged(commit: &str, live: &BTreeMap<(String, String), Value>) -> Evaluation {
        evaluate(&observation(), commit, live, &branch()).unwrap()
    }

    fn states(evaluation: &Evaluation) -> Vec<(&str, &'static str)> {
        evaluation
            .findings
            .iter()
            .map(|finding| (finding.application.name.as_str(), finding.state.name()))
            .collect()
    }

    /// Catches the common success path, and OutOfSync (what Argo CD says of
    /// every Application a newer publication has not reached) being read:
    /// Applications that run the receipt's publication, through a commit of
    /// another target on the shared branch, are accepted and healthy.
    #[test]
    fn test_applications_running_the_receipts_publication_are_accepted_and_healthy() {
        let evaluation = judged("p2", &running("foreign", "p1", "p2"));
        assert_eq!(
            states(&evaluation),
            [("dev-catalog", "accepted"), ("api", "accepted"), ("web", "accepted")]
        );
        assert!(evaluation.accepted() && evaluation.healthy());
        let outcomes = evaluation.outcomes(&observation());
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
        assert_eq!(details["applications"][0]["running"], "foreign");
        assert_eq!(details["applications"][0]["publication"], "p2");
        assert_eq!(details["instance"]["access"], "kubernetes");
    }

    /// Catches an Application on an older publication passing, and the
    /// reason not naming what runs and what to do.
    #[test]
    fn test_an_application_on_another_publication_fails_accepted_and_healthy() {
        let evaluation = judged("p2", &running("p2", "p1", "p1"));
        assert_eq!(states(&evaluation)[2], ("web", "other-publication"));
        let outcomes = evaluation.outcomes(&observation());
        assert_eq!(outcomes[0].result, AttestationResult::Fail);
        assert_eq!(outcomes[1].result, AttestationResult::Fail);
        let reason = outcomes[0].reason.as_deref().unwrap();
        assert!(
            reason.starts_with("1 of 3 Applications do not run publication p2"),
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

    /// Catches health being attested for Applications that do not run the
    /// publication, and an unhealthy one passing `healthy` while `accepted` passes.
    #[test]
    fn test_healthy_needs_every_application_healthy_besides_accepted() {
        let mut state = running("p2", "p1", "p2");
        state.insert(
            ("argocd".to_owned(), "web".to_owned()),
            live("web", "p2", "Degraded", "dev/web/web").1,
        );
        let evaluation = judged("p2", &state);
        let outcomes = evaluation.outcomes(&observation());
        assert_eq!(outcomes[0].result, AttestationResult::Pass);
        assert_eq!(outcomes[1].result, AttestationResult::Fail);
        assert_eq!(
            outcomes[1].reason.as_deref(),
            Some("1 of 3 Applications are not Healthy: web is Degraded")
        );
        // Progressing, Suspended, and Missing are no more Healthy than Degraded.
        for health in ["Progressing", "Suspended", "Missing", "Unknown"] {
            state.insert(
                ("argocd".to_owned(), "web".to_owned()),
                live("web", "p2", health, "dev/web/web").1,
            );
            assert!(!judged("p2", &state).healthy(), "{health}");
        }
    }

    /// Catches a hand-edited Application directory attesting as published,
    /// an unknown revision matching by luck, and an Application nothing
    /// reports being skipped instead of failing the aggregate.
    #[test]
    fn test_unmatched_missing_and_never_synced_applications_fail() {
        let mut state = running("p2", "edit", "p2");
        assert_eq!(states(&judged("p2", &state))[1], ("api", "unmatched"));
        state.insert(
            ("argocd".to_owned(), "api".to_owned()),
            live("api", &"f".repeat(40), "Healthy", "dev/api/api").1,
        );
        assert_eq!(states(&judged("p2", &state))[1], ("api", "not-on-branch"));
        state.remove(&("argocd".to_owned(), "api".to_owned()));
        let evaluation = judged("p2", &state);
        assert_eq!(states(&evaluation)[1], ("api", "missing"));
        assert!(!evaluation.accepted());
        state.insert(
            ("argocd".to_owned(), "api".to_owned()),
            json!({"metadata": {"name": "api"}, "spec": {"source": live_source("dev/api/api")}}),
        );
        assert_eq!(states(&judged("p2", &state))[1], ("api", "never-synced"));
    }

    /// Catches a selective sync, a deleting Application, and one that syncs
    /// another directory vouching for the receipt's publication.
    #[test]
    fn test_mixed_deleting_and_other_source_applications_fail() {
        let mut state = running("p2", "p1", "p2");
        let web = ("argocd".to_owned(), "web".to_owned());
        let mut mixed = live("web", "p2", "Healthy", "dev/web/web").1;
        mixed["status"]["operationState"] = json!({
            "phase": "Succeeded", "startedAt": "2026-01-02T00:00:00Z",
            "operation": {"sync": {"resources": [{"kind": "Deployment", "name": "web"}]}}
        });
        state.insert(web.clone(), mixed);
        assert_eq!(states(&judged("p2", &state))[2], ("web", "mixed"));
        let mut deleting = live("web", "p2", "Healthy", "dev/web/web").1;
        deleting["metadata"]["deletionTimestamp"] = json!("2026-01-02T00:00:00Z");
        state.insert(web.clone(), deleting);
        assert_eq!(states(&judged("p2", &state))[2], ("web", "deleting"));
        state.insert(web.clone(), live("web", "p2", "Healthy", "elsewhere").1);
        assert_eq!(states(&judged("p2", &state))[2], ("web", "other-source"));
        // The spec says dev/web/web, but the last sync took another directory.
        let mut synced = live("web", "p2", "Healthy", "dev/web/web").1;
        synced["status"]["history"][0]["source"]["path"] = json!("dev/other");
        state.insert(web, synced);
        assert_eq!(states(&judged("p2", &state))[2], ("web", "other-source"));
    }

    /// Catches an Application edited in the cluster to render its directory
    /// differently (`directory.recurse: false`, an `exclude`, a tool block)
    /// passing because it still sources the same path, and an Argo CD
    /// default or an omitted empty value being taken for such an edit.
    #[test]
    fn test_an_application_whose_source_was_edited_is_not_accepted() {
        let web = ("argocd".to_owned(), "web".to_owned());
        let edits: [Edit; 3] = [
            ("recurse", |source| source["directory"]["recurse"] = json!(false)),
            ("exclude", |source| source["directory"]["exclude"] = json!("*.yaml")),
            ("tool", |source| source["kustomize"] = json!({"namePrefix": "x-"})),
        ];
        for (edit, apply) in edits {
            let mut state = running("p2", "p1", "p2");
            let mut edited = live("web", "p2", "Healthy", "dev/web/web").1;
            apply(&mut edited["spec"]["source"]);
            state.insert(web.clone(), edited.clone());
            let evaluation = judged("p2", &state);
            assert_eq!(states(&evaluation)[2], ("web", "other-source"), "{edit}");
            assert!(evaluation.findings[2]
                .reason
                .as_deref()
                .unwrap()
                .contains("differs from the one Nyl generated"));
            // The last sync took the edited source too.
            let mut synced = live("web", "p2", "Healthy", "dev/web/web").1;
            apply(&mut synced["status"]["history"][0]["source"]);
            state.insert(web.clone(), synced);
            assert_eq!(states(&judged("p2", &state))[2], ("web", "other-source"), "{edit}");
        }
        let mut state = running("p2", "p1", "p2");
        let mut defaulted = live("web", "p2", "Healthy", "dev/web/web").1;
        defaulted["spec"]["source"]["targetRevision"] = json!("another");
        defaulted["spec"]["source"]["helm"] = json!({});
        defaulted["spec"]["source"]["plugin"] = Value::Null;
        state.insert(web, defaulted);
        assert_eq!(states(&judged("p2", &state))[2], ("web", "accepted"));
    }

    /// Catches a dry run after the last full sync letting an Application
    /// attest: it may hide a selective sync.
    #[test]
    fn test_a_dry_run_after_the_last_full_sync_leaves_the_application_mixed() {
        let mut state = running("p2", "p1", "p2");
        let mut web = live("web", "p2", "Healthy", "dev/web/web").1;
        web["status"]["operationState"] = json!({
            "phase": "Succeeded", "startedAt": "2026-01-03T00:00:00Z",
            "operation": {"sync": {"dryRun": true}}
        });
        state.insert(("argocd".to_owned(), "web".to_owned()), web);
        let evaluation = judged("p2", &state);
        assert_eq!(states(&evaluation)[2], ("web", "mixed"));
        assert!(evaluation.findings[2].reason.as_deref().unwrap().contains("dry run"));
    }

    /// Catches a receipt whose publication is no longer on the branch
    /// attesting anything: every Application fails, never "unknown".
    #[test]
    fn test_a_receipt_publication_missing_from_the_branch_fails_everything() {
        let evaluation = judged(&"e".repeat(40), &running("p2", "p1", "p2"));
        assert!(evaluation
            .findings
            .iter()
            .all(|finding| finding.state == State::NotOnBranch));
        let outcomes = evaluation.outcomes(&observation());
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
    fn test_details_are_stable_and_leave_out_the_time_of_the_reading() {
        let state = running("foreign", "p1", "p2");
        let first = judged("p2", &state).outcomes(&observation());
        let second = judged("p2", &state).outcomes(&observation());
        assert_eq!(first, second);
        assert!(!first[0].details.to_string().contains("deployedAt"));
    }

    /// A target that generates no Application has nothing to accept.
    #[test]
    fn test_a_target_without_applications_attests_vacuously() {
        let mut empty = observation();
        empty.applications.clear();
        let evaluation = evaluate(&empty, "p2", &BTreeMap::new(), &branch()).unwrap();
        assert!(evaluation.accepted() && evaluation.healthy());
        assert_eq!(evaluation.details(&empty)["applications"], json!([]));
    }
}
