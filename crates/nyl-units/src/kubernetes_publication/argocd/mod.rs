//! Reading Argo CD: the Argo CD implementations of a
//! [`ReconcilerReader`](super::reconciler::ReconcilerReader), which map what
//! Argo CD serves into the neutral [`Reading`] the shared evaluation judges.
//!
//! Contract: [Kubernetes publications](../../../../../design/promotion.md#kubernetes-publications)
//! (observer, access methods) and [Credentials](../../../../../design/orchestration-core.md#credentials).
//!
//! Two readers ask Argo CD the same questions, which Applications exist in a
//! namespace as Argo CD serves them, whether the credential may `get` one,
//! and whether an AppProject is visible, and share everything that follows
//! ([`read_argocd`], and [`live`] for one Application), so it is written and
//! tested once whichever way Argo CD is read:
//!
//! - [`KubeApiReader`] lists `applications.argoproj.io` objects from the
//!   Kubernetes API of the instance's control-plane Cluster, through the
//!   kubeconfig the unit admits as `KUBECONFIG` and the Cluster's declared
//!   `spec.live.context`. The API fails a list the context may not make
//!   instead of filtering it, so its proof that a missing Application is
//!   really missing is the anchors the target records
//!   ([`Proof::Anchors`]).
//! - [`ApiServerReader`] lists them from the Argo CD API server the
//!   instance declares, with the token the unit admits (named by the
//!   declaration's `tokenEnv`). Argo CD leaves out what a token may not
//!   `get` and still answers success, so its proof is Argo CD's own `can-i`
//!   answer per Application ([`Proof::PerDeployment`]); the anchors are
//!   established as well when the caller concludes from absence.
//!
//! Which one, and the API server's address, come from the ArgoCDInstance in
//! Git. Each reading runs on a thread with a runtime of its own and ends by
//! its deadline.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use nyl_render::gitops::observation::{Anchor, ArgoCdAnchor, ArgoCdApplication, ArgoCdLocation, ExpectedDeployment};
use nyl_render::resources::ArgoCDAccessMethod;
use serde_json::Value;

use super::reconciler::{
    Health, Observed, Proof, ReadError, ReadErrorKind, ReadRequest, Reading, ReconcilerReader, Running, Visibility,
    KUBECONFIG_ENV,
};

mod api_server;
mod kube_api;
pub mod live;

pub use api_server::ApiServerReader;
pub use kube_api::{Connection, KubeApiReader};

/// The namespace Argo CD runs in unless the instance says otherwise.
const DEFAULT_NAMESPACE: &str = "argocd";

/// What the two readers ask Argo CD, so that one mapping into a [`Reading`]
/// serves both.
trait ArgoQueries {
    /// The Applications in `namespace` by name, each as Argo CD serves it.
    /// An Application the credential may not read is absent, as Argo CD's
    /// own API answers; a namespace without any is an empty map.
    fn applications(&self, namespace: &str) -> Result<BTreeMap<String, Value>, ReadError>;

    /// Whether the credential may `get` the Application Argo CD's policy
    /// names `rbac_name`. A listing silently leaves out what the credential
    /// may not get and still answers success, so an Application's absence
    /// from it proves nothing until this says the credential could have seen
    /// it.
    fn can_get(&self, rbac_name: &str) -> Result<bool, ReadError>;

    /// Whether the AppProject `name` of `namespace` is visible to the
    /// credential. Absence, or a credential that may not read it, is `false`.
    fn project_exists(&self, namespace: &str, name: &str) -> Result<bool, ReadError>;
}

/// The name Argo CD's policy and its `can-i` endpoint know an Application by:
/// `<project>/<name>` in the instance's own namespace, else
/// `<project>/<namespace>/<name>`.
pub fn rbac_name(project: &str, namespace: &str, name: &str, default_namespace: &str) -> String {
    if namespace.is_empty() || namespace == default_namespace {
        format!("{project}/{name}")
    } else {
        format!("{project}/{namespace}/{name}")
    }
}

/// The reader the recorded location asks for, built from the unit's admitted
/// environment, finishing each reading within `budget`.
pub fn reader(
    location: &ArgoCdLocation,
    env: &BTreeMap<String, String>,
    budget: Duration,
) -> Result<Box<dyn ReconcilerReader>, ReadError> {
    let access = &location.access;
    match access.method {
        ArgoCDAccessMethod::Kubernetes => Ok(Box::new(
            KubeApiReader::new(
                &location.cluster,
                location.context.as_deref(),
                env.get(KUBECONFIG_ENV).map(String::as_str),
                budget,
            )?
            .in_namespace(&location.namespace),
        )),
        ArgoCDAccessMethod::Api => {
            let api = access.api.as_ref().ok_or_else(|| {
                ReadError::new(
                    ReadErrorKind::Declaration,
                    format!(
                        "ArgoCDInstance {} declares access method api without spec.access.api",
                        location.instance
                    ),
                )
            })?;
            Ok(Box::new(
                ApiServerReader::new(
                    &api.server,
                    &api.token_env,
                    env.get(&api.token_env).map(String::as_str),
                    env.get("HTTPS_PROXY")
                        .or_else(|| env.get("https_proxy"))
                        .map(String::as_str),
                    budget,
                )?
                .in_namespace(&location.namespace),
            ))
        }
    }
}

fn locator_error(what: &str) -> ReadError {
    ReadError::new(
        ReadErrorKind::Declaration,
        format!(
            "the unit records no Argo CD locator for {what}: it was resolved before it did; reconcile it so it records it"
        ),
    )
}

/// Map Argo CD into a [`Reading`] of what `request` names: every Application
/// of the namespaces involved is listed once, each expected deployment is
/// parsed from the Application it names, and absence is proven the way the
/// reader's `proof` says.
///
/// - Each expected deployment the listing lacks gets the credential's
///   answer, from `can-i` for the API method (`Visible` for the Kubernetes
///   method, which has no such question). A deployment whose project the unit
///   did not record is a [`ReadErrorKind::Declaration`] error, since Argo CD's
///   policy names an Application by it.
/// - The anchors are established when the reader's proof rests on them or
///   the caller concludes from absence, before any `can-i`: the catalog
///   Application must be in the listing, and each AppProject must be visible.
///   The first one that is not ends the check, and nothing more is asked of
///   Argo CD, so that the anchor's error is the one shown.
/// - With no anchor at all, the Kubernetes method can show nothing about
///   which Argo CD its context reaches, so the reading is unprovable and says
///   to declare the instance's access with method api.
fn read_argocd(
    queries: &dyn ArgoQueries,
    namespace: &str,
    proof: Proof,
    request: &ReadRequest<'_>,
) -> Result<Reading, ReadError> {
    let applications = request
        .deployments
        .iter()
        .map(|deployment| {
            serde_json::from_value::<ArgoCdApplication>(deployment.locator.clone())
                .map(|locator| (deployment, locator))
                .map_err(|_| locator_error(&format!("deployment {}", deployment.id)))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let anchors = request
        .anchors
        .iter()
        .map(|anchor| {
            serde_json::from_value::<ArgoCdAnchor>(anchor.locator.clone())
                .map(|locator| (anchor, locator))
                .map_err(|_| locator_error(&format!("anchor {}", anchor.id)))
        })
        .collect::<Result<Vec<_>, _>>()?;
    // Whether a target needs anchors at all (it needs none without a workload
    // deployment) is the shared evaluation's rule, not the reader's.
    let establish = request.concluding_absence || proof == Proof::Anchors;

    let namespaces = applications
        .iter()
        .map(|(_, locator)| locator.namespace.as_str())
        .chain(
            anchors
                .iter()
                .filter(|_| establish)
                .filter_map(|(_, locator)| match locator {
                    ArgoCdAnchor::Application { namespace, .. } => Some(namespace.as_str()),
                    ArgoCdAnchor::Project { .. } => None,
                }),
        )
        .collect::<BTreeSet<_>>();
    let mut live = BTreeMap::new();
    for listed in namespaces {
        for (name, application) in queries.applications(listed)? {
            live.insert((listed.to_owned(), name), application);
        }
    }

    // The anchors come before the per-Application `can-i`: when one is not
    // visible, its error and fix are what the user must see, so nothing more
    // is asked of Argo CD.
    let mut established = BTreeMap::new();
    let mut unprovable = None;
    if establish {
        established = establish_anchors(queries, namespace, &anchors, &live)?;
        if proof == Proof::Anchors && anchors.is_empty() {
            unprovable = Some(
                "the target generates no catalog Application and no AppProject other than `default`, so nothing shows that \
                 the Kubernetes context reaches the Argo CD that runs it; Nyl will not conclude anything from an empty list: \
                 declare the ArgoCDInstance's access with method api, which asks Argo CD whether the token may get each \
                 Application"
                    .to_owned(),
            );
        }
    }
    let anchored = !established
        .values()
        .any(|visibility| matches!(visibility, Visibility::Hidden(_)));
    let (deployments, absent) = observe_each(queries, namespace, &applications, &live, anchored)?;
    Ok(Reading {
        deployments,
        anchors: established,
        absent,
        proof,
        unprovable,
    })
}

type Live = BTreeMap<(String, String), Value>;

/// The observed deployments by id, and the visibility of the absent ones.
type Observations = (BTreeMap<String, Observed>, BTreeMap<String, Visibility>);

/// Each expected deployment as an [`Observed`] one when `live` has its
/// Application, else the credential's answer about it (`ask`: whether to
/// put the question to Argo CD at all).
fn observe_each(
    queries: &dyn ArgoQueries,
    namespace: &str,
    applications: &[(&ExpectedDeployment, ArgoCdApplication)],
    live: &Live,
    ask: bool,
) -> Result<Observations, ReadError> {
    let mut deployments = BTreeMap::new();
    let mut absent = BTreeMap::new();
    for (deployment, locator) in applications {
        if let Some(application) = live.get(&(locator.namespace.clone(), locator.name.clone())) {
            let observed = live::parse(application).unwrap_or_else(|error| Observed {
                running: Running::Unreadable(error),
                health: Health::Unknown,
                reported: None,
                deleting: false,
                terminal: false,
                source: None,
                synced_source: None,
            });
            deployments.insert(deployment.id.clone(), observed);
            continue;
        }
        if !ask {
            absent.insert(
                deployment.id.clone(),
                Visibility::Hidden(
                    "not asked: an anchor of the reading is not visible, which has to be fixed first".to_owned(),
                ),
            );
            continue;
        }
        if locator.project.is_empty() {
            return Err(ReadError::new(
                ReadErrorKind::Declaration,
                "the unit records no Argo CD project for its Applications: it was resolved before it did; reconcile it so it records them".to_owned(),
            ));
        }
        let rbac = rbac_name(&locator.project, &locator.namespace, &locator.name, namespace);
        let visibility = if queries.can_get(&rbac)? {
            Visibility::Visible
        } else {
            Visibility::Hidden(format!(
                "the credential may not get Application {rbac}, so its absence from the reading proves nothing \
                 (Argo CD leaves out what a token may not get and still answers success): grant its account or \
                 project role `get` on {rbac}"
            ))
        };
        absent.insert(deployment.id.clone(), visibility);
    }
    Ok((deployments, absent))
}

/// Whether each anchor is visible, in order, ending at the first that is not.
fn establish_anchors(
    queries: &dyn ArgoQueries,
    namespace: &str,
    anchors: &[(&Anchor, ArgoCdAnchor)],
    live: &Live,
) -> Result<BTreeMap<String, Visibility>, ReadError> {
    let mut established = BTreeMap::new();
    for (anchor, locator) in anchors {
        let visibility = match locator {
            ArgoCdAnchor::Application {
                namespace: catalog_namespace,
                name,
            } => {
                if live.contains_key(&(catalog_namespace.clone(), name.clone())) {
                    Visibility::Visible
                } else {
                    Visibility::Hidden(format!(
                        "the catalog Application {catalog_namespace}/{name} is not in the reading, so the credential or context \
                         does not see this target's Applications and their absence would prove nothing (if the catalog was \
                         never applied, apply it first): check that the ArgoCDInstance's access reaches the Argo CD that runs \
                         this target and may read its Applications"
                    ))
                }
            }
            ArgoCdAnchor::Project { name } => {
                if queries.project_exists(namespace, name)? {
                    Visibility::Visible
                } else {
                    Visibility::Hidden(format!(
                        "the target generates no catalog Application, and its AppProject {name} is not visible in \
                         namespace {namespace}, so the credential or context does not see this target's Argo CD and the \
                         absence of its Applications would prove nothing: check that the ArgoCDInstance's access reaches \
                         the Argo CD that runs this target and may read its projects"
                    ))
                }
            }
        };
        let hidden = matches!(visibility, Visibility::Hidden(_));
        established.insert(anchor.id.clone(), visibility);
        if hidden {
            break;
        }
    }
    Ok(established)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use nyl_render::gitops::observation::{DeploymentRole, Source};
    use serde_json::json;

    use super::*;

    /// An Argo CD with a fixed set of Applications and projects, and a policy.
    struct Fake {
        applications: BTreeMap<String, Vec<(String, Value)>>,
        projects: BTreeSet<String>,
        denied: BTreeSet<String>,
        /// The `can-i` questions put to it.
        asked: RefCell<Vec<String>>,
    }

    impl ArgoQueries for Fake {
        fn applications(&self, namespace: &str) -> Result<BTreeMap<String, Value>, ReadError> {
            Ok(self
                .applications
                .get(namespace)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .collect())
        }
        fn can_get(&self, rbac_name: &str) -> Result<bool, ReadError> {
            self.asked.borrow_mut().push(rbac_name.to_owned());
            Ok(!self.denied.contains(rbac_name))
        }
        fn project_exists(&self, _: &str, name: &str) -> Result<bool, ReadError> {
            Ok(self.projects.contains(name))
        }
    }

    fn deployment(role: DeploymentRole, namespace: &str, name: &str, project: &str) -> ExpectedDeployment {
        ExpectedDeployment {
            id: if namespace == "argocd" {
                name.to_owned()
            } else {
                format!("{namespace}/{name}")
            },
            display: format!("{namespace}/{name}"),
            role,
            path: format!("dev/{name}"),
            release: None,
            source: Source {
                repository: "r".to_owned(),
                path: format!("dev/{name}"),
                options: Value::Null,
            },
            locator: json!({"namespace": namespace, "name": name, "project": project}),
        }
    }

    fn live_application(name: &str) -> (String, Value) {
        (
            name.to_owned(),
            json!({"metadata": {"name": name}, "status": {"health": {"status": "Healthy"},
                "history": [{"id": 1, "revision": "c1", "deployedAt": "2026-01-01T00:00:00Z"}]}}),
        )
    }

    /// Catches Argo CD's own vocabulary leaking into the shared judgement and
    /// the two proofs of absence being mixed up: a listed Application is
    /// parsed, a missing one gets the credential's `can-i` answer with its
    /// fix, the Kubernetes method establishes anchors itself while the API
    /// method does so only for a caller that concludes from absence.
    #[test]
    fn test_argocd_is_mapped_into_the_neutral_reading() {
        let web = deployment(DeploymentRole::Workload, "argocd", "web", "web");
        let api = deployment(DeploymentRole::Workload, "team-a", "api", "api");
        let fake = Fake {
            applications: BTreeMap::from([("argocd".to_owned(), vec![live_application("web")])]),
            projects: BTreeSet::new(),
            denied: BTreeSet::from(["api/team-a/api".to_owned()]),
            asked: RefCell::default(),
        };
        let anchors = [Anchor {
            id: "project/web".to_owned(),
            locator: json!({"project": {"name": "web"}}),
        }];
        let deployments = [web, api];
        let request = |concluding_absence| ReadRequest {
            deployments: &deployments,
            anchors: &anchors,
            concluding_absence,
        };
        let reading = read_argocd(&fake, "argocd", Proof::PerDeployment, &request(false)).unwrap();
        assert_eq!(reading.deployments.keys().collect::<Vec<_>>(), ["web"]);
        assert_eq!(
            reading.absent["team-a/api"],
            Visibility::Hidden(
                "the credential may not get Application api/team-a/api, so its absence from the reading proves nothing \
                 (Argo CD leaves out what a token may not get and still answers success): grant its account or \
                 project role `get` on api/team-a/api"
                    .to_owned()
            )
        );
        assert!(reading.anchors.is_empty() && reading.unprovable.is_none());
        let concluding = read_argocd(&fake, "argocd", Proof::PerDeployment, &request(true)).unwrap();
        assert!(matches!(&concluding.anchors["project/web"], Visibility::Hidden(reason)
            if reason.contains("AppProject web is not visible in namespace argocd")));
        let kubernetes = read_argocd(&fake, "argocd", Proof::Anchors, &request(false)).unwrap();
        assert!(kubernetes.anchors.contains_key("project/web"));
    }

    /// Catches a Kubernetes context that can show nothing about which Argo CD
    /// it reaches being trusted with an empty list, and an Application whose
    /// project was never recorded being asked about with a made-up name.
    #[test]
    fn test_nothing_to_anchor_and_no_recorded_project_are_declaration_errors() {
        let fake = Fake {
            applications: BTreeMap::new(),
            projects: BTreeSet::new(),
            denied: BTreeSet::new(),
            asked: RefCell::default(),
        };
        let deployments = [deployment(DeploymentRole::Workload, "argocd", "web", "default")];
        let request = ReadRequest {
            deployments: &deployments,
            anchors: &[],
            concluding_absence: false,
        };
        let reading = read_argocd(&fake, "argocd", Proof::Anchors, &request).unwrap();
        assert!(reading.unprovable.as_deref().unwrap().contains("method api"));
        let reading = read_argocd(&fake, "argocd", Proof::PerDeployment, &request).unwrap();
        assert!(reading.unprovable.is_none());
        let deployments = [deployment(DeploymentRole::Workload, "argocd", "web", "")];
        let error = read_argocd(
            &fake,
            "argocd",
            Proof::PerDeployment,
            &ReadRequest {
                deployments: &deployments,
                ..request
            },
        )
        .unwrap_err();
        assert_eq!(error.kind, ReadErrorKind::Declaration);
        assert!(error.message.contains("no Argo CD project"), "{error}");
    }

    /// Catches `can-i` being asked before the anchors, which would bury an
    /// anchor's error and fix under per-Application ones: with an anchor that
    /// is not visible nothing is asked of Argo CD, an absent deployment says
    /// so, and even an Application whose project was never recorded does not
    /// get ahead of the anchor.
    #[test]
    fn test_a_hidden_anchor_comes_before_any_can_i() {
        let fake = Fake {
            applications: BTreeMap::new(),
            projects: BTreeSet::new(),
            denied: BTreeSet::new(),
            asked: RefCell::default(),
        };
        let deployments = [
            deployment(DeploymentRole::Workload, "argocd", "web", "web"),
            deployment(DeploymentRole::Workload, "argocd", "api", ""),
        ];
        let anchors = [Anchor {
            id: "project/web".to_owned(),
            locator: json!({"project": {"name": "web"}}),
        }];
        let request = ReadRequest {
            deployments: &deployments,
            anchors: &anchors,
            concluding_absence: true,
        };
        let reading = read_argocd(&fake, "argocd", Proof::PerDeployment, &request).unwrap();
        assert!(
            matches!(&reading.anchors["project/web"], Visibility::Hidden(reason) if reason.contains("AppProject web"))
        );
        assert!(fake.asked.borrow().is_empty(), "{:?}", fake.asked.borrow());
        assert!(reading
            .absent
            .values()
            .all(|visibility| matches!(visibility, Visibility::Hidden(reason) if reason.starts_with("not asked"))));
        // Once the anchor is visible the question is asked, project or not.
        let visible = Fake {
            projects: BTreeSet::from(["web".to_owned()]),
            ..fake
        };
        let error = read_argocd(&visible, "argocd", Proof::PerDeployment, &request).unwrap_err();
        assert_eq!(error.kind, ReadErrorKind::Declaration);
        assert_eq!(visible.asked.borrow().as_slice(), ["web/web"]);
    }

    #[test]
    fn test_rbac_names_qualify_applications_outside_the_instance_namespace() {
        assert_eq!(rbac_name("web", "argocd", "web", "argocd"), "web/web");
        assert_eq!(rbac_name("web", "", "web", "argocd"), "web/web");
        assert_eq!(rbac_name("web", "team-a", "web", "argocd"), "web/team-a/web");
    }
}
