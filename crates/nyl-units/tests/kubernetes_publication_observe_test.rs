//! The real `KubernetesPublication` driver observing Argo CD, end to end
//! over the shared publication world and a local fake of Argo CD, once per
//! access method: resolution records the Argo CD facts, `attest` judges what
//! the fake reports against the publication the unit really pushed, and the
//! `observe` teardown wait reports which workload Applications remain.
//!
//! Contract: [Kubernetes publications](../../../design/promotion.md#kubernetes-publications)
//! and the [Kubernetes publication unit](../../../design/orchestration-core.md#kubernetes-publication-unit),
//! Teardown.

mod argocd_stub;
mod publication_world;

use std::collections::BTreeMap;

use argocd_stub::{
    application_with_source, applications, applications_none, argocd_api_full, kube_api_with_projects, kubeconfig,
    Applications, Denied, Response, Stub,
};
use nyl_core::orchestration::driver::{
    AttestationCheck, Driver, DriverOperation, KeyInputs, Outcome, RemovalContext, RemovalObservation,
    ResolutionContext, Supported,
};
use nyl_core::orchestration::ids::{EnvironmentName, ExecutionKey, RunId, UnitAddress};
use nyl_core::orchestration::ports::CommitIdentity;
use nyl_core::orchestration::state::{ArtifactDocument, AttestationResult, DesiredUnit, Receipt};
use nyl_render::gitops::observation::{ApplicationRole, ArgoCdObservation};
use nyl_units::kubernetes_publication::KubernetesPublicationDriver;
use publication_world::{context, desired, git, spec, NoTools, World};
use serde_json::{json, Value};

const TOKEN: &str = "argocd-api-token";

fn real() -> KubernetesPublicationDriver {
    let mut descriptors = nyl_render::resources::catalog::resource_catalogue();
    descriptors.extend_from_slice(nyl_units::catalog::RESOURCES);
    KubernetesPublicationDriver::new(nyl_render::config::ResourceCatalogue(descriptors))
}

fn identity() -> CommitIdentity {
    "Nyl Orchestration <nyl@example.invalid>".parse().unwrap()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Method {
    /// The Kubernetes API of the control-plane Cluster (the implicit instance's default).
    Kubernetes,
    /// The Argo CD API server of an explicit ArgoCDInstance.
    Api,
}

/// The world with an Argo CD to read: a fake of the method's endpoint, the
/// declaration in Git that points at it, and the admitted environment that
/// holds its credential.
struct Observed {
    world: World,
    stub: Stub,
    served: Applications,
    /// What the API server's `can-i` denies the token.
    denied: Denied,
    /// The AppProjects the fake serves.
    projects: Applications,
    env: BTreeMap<String, String>,
    _directory: tempfile::TempDir,
}

impl Observed {
    fn new(method: Method) -> Self {
        Self::with_catalog(method, true)
    }

    /// `catalog: false` is a target that generates no catalog Application
    /// (`catalogApplication.enabled: false`, as a shared catalog does).
    fn with_catalog(method: Method, catalog: bool) -> Self {
        Self::with_catalog_setting(method, (!catalog).then_some("enabled: false"))
    }

    /// A target whose `catalogApplication` is `setting`, with no catalog
    /// Application of its own: `enabled: false`, or a shared catalog.
    fn with_catalog_setting(method: Method, setting: Option<&str>) -> Self {
        let world = World::new();
        let served = applications(Vec::new());
        let projects = applications(Vec::new());
        let directory = tempfile::TempDir::new().unwrap();
        let denied = applications_none();
        let (stub, env) = match method {
            Method::Kubernetes => {
                let stub = kube_api_with_projects(served.clone(), projects.clone());
                let file = directory.path().join("kubeconfig");
                kubeconfig(&file, "argocd-admin", &stub.url());
                (
                    stub,
                    BTreeMap::from([("KUBECONFIG".to_owned(), file.display().to_string())]),
                )
            }
            Method::Api => (
                argocd_api_full(TOKEN, served.clone(), denied.clone(), projects.clone()),
                BTreeMap::from([("ARGOCD_AUTH_TOKEN".to_owned(), TOKEN.to_owned())]),
            ),
        };
        let platform = world.work().join("nyl/platform.yaml");
        let mut text = std::fs::read_to_string(&platform).unwrap();
        // The control-plane Cluster declares its local context (used by the Kubernetes method).
        text = text.replace(
            "  kubernetes:\n    kubeVersion: 1.31.4",
            "  live: {context: argocd-admin}\n  kubernetes:\n    kubeVersion: 1.31.4",
        );
        if let Some(setting) = setting {
            text = text.replace(
                "  publication:\n    repositoryRef: {name: platform}\n",
                &format!(
                    "  catalogApplication: {{{setting}}}\n  publication:\n    repositoryRef: {{name: platform}}\n"
                ),
            );
        }
        if method == Method::Api {
            text = text.replace(
                "  clusterRef: {name: dev}\n  applicationGroupSelector",
                "  clusterRef: {name: dev}\n  argocdRef: {name: control}\n  applicationGroupSelector",
            );
            text.push_str(&format!(
                "---\napiVersion: k8s.gitops.nyl/v1\nkind: ArgoCDInstance\nmetadata: {{name: control}}\nspec:\n  clusterRef: {{name: dev}}\n  namespace: argocd\n  access: {{method: api, api: {{server: '{}'}}}}\n",
                stub.url()
            ));
        }
        std::fs::write(&platform, text).unwrap();
        git(&world.work(), &["add", "--all"]);
        git(
            &world.work(),
            &["commit", "--quiet", "--message", "Declare Argo CD access"],
        );
        Self {
            world,
            stub,
            served,
            denied,
            projects,
            env,
            _directory: directory,
        }
    }

    fn driver_spec(extra: Value) -> Value {
        let mut spec = spec();
        spec["mode"] = json!("observe");
        spec.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        spec
    }

    /// The unit as a run resolves it: the driver's recorded facts join the spec.
    fn resolve(&self, driver: &dyn Driver, authored: Value) -> DesiredUnit {
        let inputs: KeyInputs = driver
            .execution_key_inputs(
                &ResolutionContext {
                    instances: &nyl_core::orchestration::instance::NO_INSTANCES,
                    source_roots: BTreeMap::default(),
                    environment: EnvironmentName::new("dev").unwrap(),
                    worktree: self.world.work(),
                    project_dir: self.world.work().join("nyl"),
                    env: BTreeMap::new(),
                    deadline: chrono::Utc::now(),
                    now: chrono::Utc::now(),
                    tools: &NoTools,
                    cache_dir: self.world.root.path().join("cache"),
                },
                &authored,
            )
            .unwrap();
        let mut resolved = authored;
        inputs.record_into(&mut resolved).unwrap();
        let key = ExecutionKey::from_digest_of(
            serde_json::to_string(&json!({"spec": resolved, "files": inputs.files}))
                .unwrap()
                .as_bytes(),
        );
        desired(resolved, key)
    }

    /// Publish the unit; the `PublishedTree` artifact and the commit it names.
    fn publish(&self, driver: &dyn Driver, unit: &DesiredUnit) -> (ArtifactDocument, String) {
        let id = identity();
        let Supported::Supported(Outcome::Succeeded { artifacts, .. }) = driver
            .reconcile(&context(&self.world, DriverOperation::Reconcile, Some(&id)), unit)
            .unwrap()
        else {
            panic!("the publication succeeds");
        };
        let tree = artifacts.into_iter().next().unwrap();
        let commit = tree.spec["commit"].as_str().unwrap().to_owned();
        (tree, commit)
    }

    fn attest(&self, driver: &dyn Driver, unit: &DesiredUnit, tree: &ArtifactDocument) -> AttestationCheck {
        let mut ctx = context(&self.world, DriverOperation::Attest, None);
        ctx.env = self.env.clone();
        ctx.artifacts = BTreeMap::from([("tree".to_owned(), tree.clone())]);
        let Supported::Supported(check) = driver.attest(&ctx, unit, &receipt(unit)).unwrap() else {
            panic!("an observing unit attests");
        };
        check
    }

    fn removal(&self, driver: &dyn Driver, unit: &DesiredUnit) -> RemovalObservation {
        let ctx = RemovalContext {
            unit: "dev/kubernetes".parse::<UnitAddress>().unwrap(),
            env: self.env.clone(),
            secrets: BTreeMap::new(),
            now: chrono::Utc::now(),
            deadline: chrono::Utc::now() + chrono::Duration::seconds(30),
        };
        let Supported::Supported(observation) = driver.observe_removal(&ctx, unit).unwrap() else {
            panic!("the driver observes removal");
        };
        observation
    }

    /// Serve every Application the unit's facts name, each running the
    /// revision `running(name)` returns and healthy.
    fn serve(
        &self,
        unit: &DesiredUnit,
        running: impl Fn(&str) -> Option<String>,
        health: impl Fn(&str) -> &'static str,
    ) {
        let facts = facts(unit);
        *self.served.lock().unwrap() = facts
            .applications
            .iter()
            .filter_map(|app| {
                let revision = running(&app.name)?;
                // As Argo CD serves it: the generated source, with its target revision.
                let mut source = app.source.clone();
                source["targetRevision"] = json!("deploy");
                Some(application_with_source(
                    &app.name,
                    &app.namespace,
                    &source,
                    &[&revision],
                    health(&app.name),
                ))
            })
            .collect();
    }
}

/// Commit `path` to the `deploy` branch from a clone of its own, as another
/// target or tool would.
fn commit_as_another_tool(world: &World, clone: &str, path: &str) {
    let directory = world.root.path().join(format!("tool-{clone}"));
    git(
        world.root.path(),
        &[
            "clone",
            "--quiet",
            "--branch",
            "deploy",
            &world.project_url(),
            &format!("tool-{clone}"),
        ],
    );
    let file = directory.join(path);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(file, "kept: true\n").unwrap();
    git(&directory, &["add", "--all"]);
    git(&directory, &["commit", "--quiet", "--message", "Another tool"]);
    git(&directory, &["push", "--quiet", "origin", "HEAD:refs/heads/deploy"]);
}

fn facts(unit: &DesiredUnit) -> ArgoCdObservation {
    serde_json::from_value(unit.resolved_spec.as_ref().unwrap()["argocd"].clone()).unwrap()
}

fn receipt(unit: &DesiredUnit) -> Receipt {
    Receipt {
        execution_key: unit.execution_key.clone().unwrap(),
        behavior_version: 1,
        run: RunId::new("0192f0c4-8d0a-7c3e-9a51-3c1f0e7d2b90").unwrap(),
        read_desired: None,
        started_at: chrono::Utc::now(),
        finished_at: chrono::Utc::now(),
        versions: BTreeMap::new(),
        approval: None,
        provenance: BTreeMap::new(),
        outputs: json!({}),
        artifacts: Vec::new(),
    }
}

fn outcomes(check: AttestationCheck) -> Vec<(String, AttestationResult, Option<String>, Value)> {
    match check {
        AttestationCheck::Attested { outcomes } => outcomes
            .into_iter()
            .map(|outcome| (outcome.name, outcome.result, outcome.reason, outcome.details))
            .collect(),
        AttestationCheck::Unknown { reason } => panic!("the check is unknown: {reason}"),
    }
}

fn unknown(check: AttestationCheck) -> String {
    match check {
        AttestationCheck::Unknown { reason } => reason,
        AttestationCheck::Attested { .. } => panic!("nothing could be attested"),
    }
}

/// Catches resolution recording nothing, or the wrong thing, for the unit's
/// observation: the instance, how it is read, the publication destination,
/// and every Application the target generates (the catalog and each
/// workload, with its release), which `attest` and the teardown wait read
/// from the stored document. A publish-mode unit records none, so its key
/// is unchanged.
#[test]
fn test_resolution_records_the_argocd_facts_of_an_observing_unit() {
    let driver = real();
    for method in [Method::Kubernetes, Method::Api] {
        let observed = Observed::new(method);
        let unit = observed.resolve(&driver, Observed::driver_spec(json!({})));
        let facts = facts(&unit);
        assert_eq!(facts.location.namespace, "argocd");
        assert_eq!(facts.location.cluster, "dev");
        assert_eq!(facts.location.context.as_deref(), Some("argocd-admin"));
        assert_eq!(facts.publication.branch, "deploy");
        assert_eq!(facts.publication.path_prefix, "dev");
        assert_eq!(facts.publication.url, observed.world.project_url());
        match method {
            Method::Kubernetes => {
                assert_eq!(facts.location.instance, "implicit:dev");
                assert_eq!(
                    facts.location.access.method,
                    nyl_render::resources::ArgoCDAccessMethod::Kubernetes
                );
            }
            Method::Api => {
                assert_eq!(facts.location.instance, "control");
                let api = facts.location.access.api.as_ref().unwrap();
                assert_eq!(api.server, observed.stub.url());
                assert_eq!(api.token_env, "ARGOCD_AUTH_TOKEN");
            }
        }
        let catalog = facts
            .applications
            .iter()
            .find(|app| app.role == ApplicationRole::Catalog)
            .unwrap();
        assert_eq!(catalog.path, "dev/_nyl/catalog");
        let workloads = facts.workload_applications().collect::<Vec<_>>();
        assert_eq!(workloads.len(), 1, "{:?}", facts.applications);
        assert_eq!(workloads[0].path, "dev/web/web");
        assert_eq!(workloads[0].release.as_deref(), Some("web/web"));
        // The declaration, not the process, decided it: a publish-mode unit records nothing.
        let publishing = observed.resolve(&driver, spec());
        assert!(publishing.resolved_spec.as_ref().unwrap().get("argocd").is_none());
        // A unit that only waits by observing records the facts too.
        let waiting = observed.resolve(&driver, {
            let mut spec = spec();
            spec["teardownWait"] = json!({"strategy": "observe"});
            spec
        });
        assert!(waiting.resolved_spec.as_ref().unwrap().get("argocd").is_some());
    }
}

/// Catches an observe-mode unit declaring no attestations, or a publish-mode
/// one declaring (or being asked for) some it cannot produce.
#[test]
fn test_observe_mode_declares_accepted_and_healthy_and_attests() {
    let driver = real();
    let observing = Observed::driver_spec(json!({}));
    assert_eq!(driver.attestations(&observing), ["accepted", "healthy"]);
    assert!(driver.capabilities(&observing).attest);
    assert!(driver.attestations(&spec()).is_empty());
    assert!(!driver.capabilities(&spec()).attest);
}

/// The whole path for each access method: every Application runs the
/// pushed publication and is Healthy, so both attestations pass, and their
/// details name each Application's running revision, matched publication,
/// and health. Then a newer publication that no Application has reached
/// fails `accepted` for the receipt that runs it, and a Degraded Application
/// fails `healthy` only.
#[test]
fn test_attest_judges_what_argocd_runs_against_the_pushed_publication() {
    let driver = real();
    for method in [Method::Kubernetes, Method::Api] {
        let observed = Observed::new(method);
        let unit = observed.resolve(&driver, Observed::driver_spec(json!({})));
        let (tree, first) = observed.publish(&driver, &unit);
        observed.serve(&unit, |_| Some(first.clone()), |_| "Healthy");
        let result = outcomes(observed.attest(&driver, &unit, &tree));
        let names = result
            .iter()
            .map(|(name, result, ..)| (name.as_str(), *result))
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                ("accepted", AttestationResult::Pass),
                ("healthy", AttestationResult::Pass)
            ],
            "{method:?}: {result:?}"
        );
        let details = &result[0].3;
        assert_eq!(details["publication"]["commit"], json!(first));
        assert_eq!(
            details["instance"]["access"],
            json!(if method == Method::Api { "api" } else { "kubernetes" })
        );
        let applications = details["applications"].as_array().unwrap();
        assert_eq!(applications.len(), 2);
        for application in applications {
            assert_eq!(application["state"], "accepted", "{application}");
            assert_eq!(application["running"], json!(first));
            assert_eq!(application["publication"], json!(first));
            assert_eq!(application["health"], "Healthy");
        }
        // The reading used the declared endpoint with the admitted credential.
        let requests = observed.stub.requests();
        assert!(!requests.is_empty());
        if method == Method::Api {
            assert!(requests
                .iter()
                .all(|request| request.headers["authorization"] == format!("Bearer {TOKEN}")));
        }

        // Degraded health fails `healthy` and keeps `accepted`.
        observed.serve(
            &unit,
            |_| Some(first.clone()),
            |name| if name == "dev-catalog" { "Healthy" } else { "Degraded" },
        );
        let degraded = outcomes(observed.attest(&driver, &unit, &tree));
        assert_eq!(degraded[0].1, AttestationResult::Pass);
        assert_eq!(degraded[1].1, AttestationResult::Fail);
        assert!(
            degraded[1].2.as_deref().unwrap().contains("web is Degraded"),
            "{:?}",
            degraded[1].2
        );

        // A second publication (another image) is the receipt now; Argo CD
        // still runs the first one, so nothing is accepted yet.
        let mut second_spec = Observed::driver_spec(json!({}));
        second_spec["releases"]["web/web"]["image"] = json!("registry.invalid:5000/web@sha256:4567");
        let second_unit = observed.resolve(&driver, second_spec);
        let (second_tree, second) = observed.publish(&driver, &second_unit);
        assert_ne!(first, second);
        let behind = outcomes(observed.attest(&driver, &second_unit, &second_tree));
        assert_eq!(behind[0].1, AttestationResult::Fail, "{method:?}");
        assert_eq!(behind[1].1, AttestationResult::Fail);
        let reason = behind[0].2.as_deref().unwrap();
        assert!(reason.contains(&format!("do not run publication {second}")), "{reason}");
        assert!(reason.contains(&format!("another publication ({first})")), "{reason}");
        // The catalog and the other Application did not change, so they run the second publication too.
        let states = behind[0].3["applications"]
            .as_array()
            .unwrap()
            .iter()
            .map(|application| {
                (
                    application["name"].as_str().unwrap().to_owned(),
                    application["state"].as_str().unwrap().to_owned(),
                )
            })
            .collect::<BTreeMap<_, _>>();
        assert_eq!(states["web"], "other-publication");

        // After syncing, the second receipt is accepted.
        observed.serve(&second_unit, |_| Some(second.clone()), |_| "Healthy");
        assert!(outcomes(observed.attest(&driver, &second_unit, &second_tree))
            .iter()
            .all(|(_, result, ..)| *result == AttestationResult::Pass));
    }
}

/// Catches the matching trusting the commit Argo CD reports instead of the
/// Application's directory: on a shared branch, another target's commit
/// that leaves the directory alone is the same publication, and a commit
/// that edits the directory without publishing is not.
#[test]
fn test_attest_matches_by_application_directory_on_a_shared_branch() {
    let driver = real();
    let observed = Observed::new(Method::Kubernetes);
    let unit = observed.resolve(&driver, Observed::driver_spec(json!({})));
    let (tree, published) = observed.publish(&driver, &unit);

    commit_as_another_tool(&observed.world, "first", "other-target/state.yaml");
    let foreign = git(&observed.world.bare(), &["rev-parse", "refs/heads/deploy"]);
    assert_ne!(foreign, published);
    observed.serve(&unit, |_| Some(foreign.clone()), |_| "Healthy");
    let passing = outcomes(observed.attest(&driver, &unit, &tree));
    assert!(
        passing.iter().all(|(_, result, ..)| *result == AttestationResult::Pass),
        "{passing:?}"
    );
    assert_eq!(passing[0].3["applications"][0]["running"], json!(foreign));
    assert_eq!(passing[0].3["applications"][0]["publication"], json!(published));

    // Another tool commits a file inside the workload's directory.
    commit_as_another_tool(&observed.world, "second", "dev/web/web/hotfix.yaml");
    let edited = git(&observed.world.bare(), &["rev-parse", "refs/heads/deploy"]);
    observed.serve(
        &unit,
        |name| {
            Some(if name == "web" {
                edited.clone()
            } else {
                published.clone()
            })
        },
        |_| "Healthy",
    );
    let failing = outcomes(observed.attest(&driver, &unit, &tree));
    assert_eq!(failing[0].1, AttestationResult::Fail);
    let web = failing[0].3["applications"]
        .as_array()
        .unwrap()
        .iter()
        .find(|application| application["name"] == "web")
        .unwrap();
    assert_eq!(web["state"], "unmatched");
    assert_eq!(web["running"], json!(edited));
}

/// Catches an attestation recorded when Argo CD could not be read: each
/// failure to read, whatever its cause, is an unknown check that records
/// nothing, with the fix in its reason, never a failing attestation, and
/// never a partial one.
#[test]
fn test_attest_that_cannot_read_argocd_is_unknown_with_the_fix() {
    let driver = real();
    for method in [Method::Kubernetes, Method::Api] {
        let mut observed = Observed::new(method);
        let unit = observed.resolve(&driver, Observed::driver_spec(json!({})));
        let (tree, published) = observed.publish(&driver, &unit);
        observed.serve(&unit, |_| Some(published.clone()), |_| "Healthy");

        // No credential admitted.
        let admitted = std::mem::take(&mut observed.env);
        let reason = unknown(observed.attest(&driver, &unit, &tree));
        match method {
            Method::Kubernetes => assert!(
                reason.contains("KUBECONFIG") && reason.contains("env.passthrough"),
                "{reason}"
            ),
            Method::Api => assert!(
                reason.contains("ARGOCD_AUTH_TOKEN") && reason.contains("env.secrets"),
                "{reason}"
            ),
        }

        // A rejected token (expiry) or kubeconfig credential.
        if method == Method::Api {
            observed.env = BTreeMap::from([("ARGOCD_AUTH_TOKEN".to_owned(), "expired".to_owned())]);
            let reason = unknown(observed.attest(&driver, &unit, &tree));
            assert!(
                reason.contains("rejected the API token") && reason.contains("has expired"),
                "{reason}"
            );
            assert!(!reason.contains("expired\""), "{reason}");
        }
        observed.env = admitted;

        // Argo CD not answering.
        let stub = std::mem::replace(&mut observed.stub, Stub::start(|_| Response::text(500, "unused")));
        drop(stub);
        let reason = unknown(observed.attest(&driver, &unit, &tree));
        match method {
            Method::Kubernetes => assert!(reason.contains("cannot list Applications through context"), "{reason}"),
            Method::Api => assert!(
                reason.contains("cannot read Argo CD at") && reason.contains("spec.access.api.server"),
                "{reason}"
            ),
        }
    }
}

/// The `observe` teardown wait: the workload Applications Argo CD still has
/// remain (not the catalog), none remain once they are gone, and a reading
/// that fails is a failure with the fix, which the shell reports as
/// `observation-failed` rather than as removal.
#[test]
fn test_observe_removal_reports_the_workload_applications_that_remain() {
    let driver = real();
    for method in [Method::Kubernetes, Method::Api] {
        let mut observed = Observed::new(method);
        let unit = observed.resolve(&driver, {
            let mut spec = spec();
            spec["teardownWait"] = json!({"strategy": "observe"});
            spec
        });
        observed.serve(&unit, |_| Some("c".repeat(40)), |_| "Healthy");
        assert_eq!(
            observed.removal(&driver, &unit),
            RemovalObservation::Remaining(vec!["web".to_owned()]),
            "{method:?}"
        );
        // The catalog Application alone left: phase 1 removed the workloads.
        observed.serve(
            &unit,
            |name| (name == "dev-catalog").then(|| "c".repeat(40)),
            |_| "Healthy",
        );
        assert_eq!(
            observed.removal(&driver, &unit),
            RemovalObservation::Remaining(Vec::new())
        );

        let admitted = std::mem::take(&mut observed.env);
        let RemovalObservation::Failed(reason) = observed.removal(&driver, &unit) else {
            panic!("no credential admitted");
        };
        assert!(
            reason.contains("env.passthrough") || reason.contains("env.secrets"),
            "{reason}"
        );
        observed.env = admitted;
        let stub = std::mem::replace(&mut observed.stub, Stub::start(|_| Response::text(500, "unused")));
        drop(stub);
        assert!(
            matches!(observed.removal(&driver, &unit), RemovalObservation::Failed(_)),
            "{method:?}"
        );
    }
}

/// A document resolved before the unit observed Argo CD cannot attest or
/// observe removal, and says what to do, rather than guessing a location.
#[test]
fn test_a_unit_without_recorded_facts_cannot_attest_or_observe() {
    let driver = real();
    let observed = Observed::new(Method::Kubernetes);
    let mut unit = desired(Observed::driver_spec(json!({})), ExecutionKey::from_digest_of(b"key"));
    unit.resolved_spec = unit.resolved_spec.take();
    let tree = ArtifactDocument {
        api_version: "artifacts.gitops.nyl/v1".to_owned(),
        kind: "PublishedTree".to_owned(),
        metadata: nyl_core::orchestration::state::ArtifactMetadata {
            name: "tree".to_owned(),
            unit: nyl_core::orchestration::state::ArtifactUnit {
                environment: EnvironmentName::new("dev").unwrap(),
                name: "kubernetes".parse().unwrap(),
                uid: "00000000-0000-7000-8000-000000000004".parse().unwrap(),
            },
        },
        execution_key: unit.execution_key.clone().unwrap(),
        spec: json!({"commit": "a".repeat(40)}),
    };
    assert!(unknown(observed.attest(&driver, &unit, &tree)).contains("reconcile it so it records them"));
    let RemovalObservation::Failed(reason) = observed.removal(&driver, &unit) else {
        panic!("no facts, no observation");
    };
    assert!(reason.contains("records no Argo CD facts"), "{reason}");
}

/// The unit's key as a run computes it: the resolved spec without the fields
/// the driver excludes, and the files.
fn key_of(inputs: &KeyInputs, resolved: &Value) -> String {
    let mut spec = resolved.clone();
    for pointer in &inputs.excluded_fields {
        if let Some((parent, last)) = pointer.rsplit_once('/') {
            if let Some(Value::Object(map)) = spec.pointer_mut(parent) {
                map.remove(last);
            }
        }
    }
    serde_json::to_string(&json!({"spec": spec, "files": inputs.files})).unwrap()
}

impl Observed {
    fn inputs(&self, driver: &dyn Driver) -> (KeyInputs, Value) {
        let authored = Self::driver_spec(json!({}));
        let inputs = driver
            .execution_key_inputs(
                &ResolutionContext {
                    instances: &nyl_core::orchestration::instance::NO_INSTANCES,
                    source_roots: BTreeMap::default(),
                    environment: EnvironmentName::new("dev").unwrap(),
                    worktree: self.world.work(),
                    project_dir: self.world.work().join("nyl"),
                    env: BTreeMap::new(),
                    deadline: chrono::Utc::now(),
                    now: chrono::Utc::now(),
                    tools: &NoTools,
                    cache_dir: self.world.root.path().join("cache"),
                },
                &authored,
            )
            .unwrap();
        let mut resolved = authored;
        inputs.record_into(&mut resolved).unwrap();
        (inputs, resolved)
    }

    fn edit_platform(&self, edit: impl Fn(&str) -> String) {
        let platform = self.world.work().join("nyl/platform.yaml");
        let text = std::fs::read_to_string(&platform).unwrap();
        std::fs::write(&platform, edit(&text)).unwrap();
        git(&self.world.work(), &["add", "--all"]);
        git(&self.world.work(), &["commit", "--quiet", "--message", "Edit"]);
    }

    fn index_bytes(&self) -> String {
        git(&self.world.bare(), &["show", "deploy:dev/_nyl/index.json"])
    }
}

/// How Argo CD is read changes no result, so it must change no key and no
/// published byte: editing the instance's `access` (the server), moving the
/// Cluster's kube context, or naming another token variable leaves the unit's
/// key and the published ownership index as they were, while the Applications
/// the target generates stay part of the key. Catches credential plumbing
/// re-executing units, resetting their attestations, and republishing.
#[test]
fn test_how_argocd_is_read_keys_nothing_and_publishes_nothing() {
    let driver = real();
    let observed = Observed::new(Method::Api);
    let unit = observed.resolve(&driver, Observed::driver_spec(json!({})));
    observed.publish(&driver, &unit);
    let (inputs, resolved) = observed.inputs(&driver);
    let key = key_of(&inputs, &resolved);
    let index = observed.index_bytes();
    let head = git(&observed.world.bare(), &["rev-parse", "refs/heads/deploy"]);

    // Edit access (another token variable and server) and the Cluster's context.
    observed.edit_platform(|text| {
        text.replace(
            "access: {method: api, api: {server: '",
            "access: {method: api, api: {tokenEnv: OTHER_TOKEN, server: 'http://127.0.0.1:9/#",
        )
        .replace("http://127.0.0.1:9/#http://", "http://")
        .replace("live: {context: argocd-admin}", "live: {context: renamed-context}")
    });
    let (after_inputs, after_resolved) = observed.inputs(&driver);
    assert_ne!(
        after_resolved["argocd"]["access"], resolved["argocd"]["access"],
        "the edit reached the facts"
    );
    assert_ne!(after_resolved["argocd"]["context"], resolved["argocd"]["context"]);
    assert_eq!(
        key_of(&after_inputs, &after_resolved),
        key,
        "the key moved with credential plumbing"
    );
    observed.publish(&driver, &observed.resolve(&driver, Observed::driver_spec(json!({}))));
    assert_eq!(
        observed.index_bytes(),
        index,
        "the index moved with credential plumbing"
    );
    assert_eq!(
        git(&observed.world.bare(), &["rev-parse", "refs/heads/deploy"]),
        head,
        "a commit was published"
    );

    // The generated Applications are keyed: a group that outputs elsewhere
    // generates another directory.
    let group = observed.world.work().join("nyl/web.yaml");
    let text = std::fs::read_to_string(&group).unwrap();
    std::fs::write(
        &group,
        text.replace(
            "  applicationNamespace: argocd\n",
            "  applicationNamespace: argocd\n  outputPath: elsewhere\n",
        ),
    )
    .unwrap();
    git(&observed.world.work(), &["add", "--all"]);
    git(
        &observed.world.work(),
        &["commit", "--quiet", "--message", "Output elsewhere"],
    );
    let (moved_inputs, moved_resolved) = observed.inputs(&driver);
    assert_ne!(
        moved_resolved["argocd"]["applications"],
        after_resolved["argocd"]["applications"]
    );
    assert_ne!(key_of(&moved_inputs, &moved_resolved), key);
}

/// Catches an Argo CD that lists nothing for a token that cannot see the
/// target's Applications ending a teardown wait while workloads still run:
/// removal is concluded only from Applications the reading could see. The
/// catalog Application stays through phase 1, so a reading without it proves
/// nothing, and (API server) an Application missing from the list needs
/// Argo CD's own `can-i` to say the token may get it.
#[test]
fn test_observe_removal_is_never_concluded_from_what_the_credential_cannot_see() {
    let driver = real();
    for method in [Method::Kubernetes, Method::Api] {
        let observed = Observed::new(method);
        let unit = observed.resolve(&driver, {
            let mut spec = spec();
            spec["teardownWait"] = json!({"strategy": "observe"});
            spec
        });
        let failed = |observation: RemovalObservation| match observation {
            RemovalObservation::Failed(reason) => reason,
            other => panic!("{method:?}: {other:?}"),
        };

        // A token (or context) that sees none of the target's Applications.
        *observed.denied.lock().unwrap() = vec!["*".to_owned()];
        let reason = failed(observed.removal(&driver, &unit));
        assert!(
            reason.contains("catalog Application argocd/dev-catalog is not in the reading"),
            "{method:?}: {reason}"
        );

        // It sees the catalog but not the workload that is gone from its list.
        observed.serve(
            &unit,
            |name| (name == "dev-catalog").then(|| "c".repeat(40)),
            |_| "Healthy",
        );
        if method == Method::Api {
            let reason = failed(observed.removal(&driver, &unit));
            assert!(reason.contains("may not get Application web/web"), "{reason}");
            assert!(
                reason.contains("grant its account or project role `get` on web/web"),
                "{reason}"
            );
            // Argo CD says it may: the workload really is gone.
            observed.denied.lock().unwrap().clear();
        }
        assert_eq!(
            observed.removal(&driver, &unit),
            RemovalObservation::Remaining(Vec::new()),
            "{method:?}"
        );
    }
}

/// The same rule for `accepted`: for the API server an Application the token
/// could not see is unknown, not a recorded failure, while one it could see
/// and that is gone fails as missing.
#[test]
fn test_attest_does_not_record_a_failure_for_applications_the_token_cannot_see() {
    let driver = real();
    let observed = Observed::new(Method::Api);
    let unit = observed.resolve(&driver, Observed::driver_spec(json!({})));
    let (tree, published) = observed.publish(&driver, &unit);
    observed.serve(&unit, |name| (name != "web").then(|| published.clone()), |_| "Healthy");
    *observed.denied.lock().unwrap() = vec!["web/web".to_owned()];
    let reason = unknown(observed.attest(&driver, &unit, &tree));
    assert!(reason.contains("may not get Application web/web"), "{reason}");
    observed.denied.lock().unwrap().clear();
    let result = outcomes(observed.attest(&driver, &unit, &tree));
    assert_eq!(result[0].1, AttestationResult::Fail);
    assert_eq!(result[0].3["applications"][1]["state"], "missing");
}

impl Observed {
    /// The AppProject `name` exists in the fake Argo CD.
    fn serve_project(&self, name: &str) {
        self.projects
            .lock()
            .unwrap()
            .push(json!({"apiVersion": "argoproj.io/v1alpha1", "kind": "AppProject", "metadata": {"name": name, "namespace": "argocd"}}));
    }
}

/// A target without a catalog Application (`catalogApplication.enabled:
/// false`, or a shared catalog) has no Application phase 1 keeps, so its
/// AppProjects anchor the teardown wait instead, under both methods. Catches a
/// context for another Argo CD (an empty list that is a success) completing the
/// wait while the workloads still run: phase 2 and the teardown of the
/// databases and roles behind them would run under live workloads.
#[test]
fn test_removal_without_a_catalog_application_is_anchored_by_the_projects() {
    let driver = real();
    // `enabled: false` leaves the catalog to a parent the project writes; a
    // shared catalog is rendered by Nyl but is no Application of the target:
    // its Application sources the whole branch, so it anchors nothing here and
    // the target's own AppProjects, which phase 1 keeps, do.
    for (method, setting) in [Method::Kubernetes, Method::Api]
        .into_iter()
        .flat_map(|method| ["enabled: false", "shared: {name: previews}"].map(|setting| (method, setting)))
    {
        let observed = Observed::with_catalog_setting(method, Some(setting));
        let unit = observed.resolve(&driver, {
            let mut spec = spec();
            spec["teardownWait"] = json!({"strategy": "observe"});
            spec
        });
        let facts = facts(&unit);
        assert!(
            facts
                .applications
                .iter()
                .all(|app| app.role == ApplicationRole::Workload),
            "{:?}",
            facts.applications
        );
        assert_eq!(facts.applications[0].project, "web");

        // The credential reaches another Argo CD: no project of this target.
        let RemovalObservation::Failed(reason) = observed.removal(&driver, &unit) else {
            panic!("{method:?}: an empty reading from another Argo CD completed the wait");
        };
        assert!(
            reason.contains("AppProject web is not visible in namespace argocd"),
            "{method:?}: {reason}"
        );
        // The same context for attest: unknown, never a recorded failure.
        let tree = {
            let (tree, _) = observed.publish(&driver, &unit);
            tree
        };
        if method == Method::Kubernetes {
            let reason = unknown(observed.attest(&driver, &unit, &tree));
            assert!(reason.contains("AppProject web is not visible"), "{reason}");
        }

        // Its project is visible: the workload is gone, or it still runs.
        observed.serve_project("web");
        assert_eq!(
            observed.removal(&driver, &unit),
            RemovalObservation::Remaining(Vec::new()),
            "{method:?}"
        );
        observed.serve(&unit, |_| Some("c".repeat(40)), |_| "Healthy");
        assert_eq!(
            observed.removal(&driver, &unit),
            RemovalObservation::Remaining(vec!["web".to_owned()]),
            "{method:?}"
        );
    }
}

/// With neither a catalog Application nor an AppProject of its own to show
/// (every Application in `default`, which every Argo CD has), nothing proves a
/// Kubernetes context reaches this target: the wait fails closed. The API
/// method relies on `can-i` for each Application, so an Application it may get
/// that is gone completes the wait.
#[test]
fn test_removal_with_nothing_to_anchor_fails_closed_for_kubernetes_only() {
    let driver = real();
    for method in [Method::Kubernetes, Method::Api] {
        let observed = Observed::with_catalog(method, false);
        let mut unit = observed.resolve(&driver, {
            let mut spec = spec();
            spec["teardownWait"] = json!({"strategy": "observe"});
            spec
        });
        unit.resolved_spec.as_mut().unwrap()["argocd"]["applications"][0]["project"] = json!("default");
        match method {
            Method::Kubernetes => {
                let RemovalObservation::Failed(reason) = observed.removal(&driver, &unit) else {
                    panic!("a Kubernetes context was trusted with nothing to anchor it");
                };
                assert!(
                    reason.contains("nothing shows that the Kubernetes context reaches the Argo CD"),
                    "{reason}"
                );
            }
            Method::Api => {
                assert_eq!(
                    observed.removal(&driver, &unit),
                    RemovalObservation::Remaining(Vec::new())
                );
                *observed.denied.lock().unwrap() = vec!["*".to_owned()];
                assert!(matches!(
                    observed.removal(&driver, &unit),
                    RemovalObservation::Failed(_)
                ));
            }
        }
    }
}

/// `attest` with the Kubernetes method and a context for another Argo CD is
/// unknown, not a recorded `missing` failure: the catalog Application is not in
/// the reading, which no honest Argo CD of this target lacks once it applied it.
#[test]
fn test_attest_with_a_context_for_another_argocd_is_unknown() {
    let driver = real();
    let observed = Observed::new(Method::Kubernetes);
    let unit = observed.resolve(&driver, Observed::driver_spec(json!({})));
    let (tree, _) = observed.publish(&driver, &unit);
    let reason = unknown(observed.attest(&driver, &unit, &tree));
    assert!(
        reason.contains("the catalog Application argocd/dev-catalog is not in the reading"),
        "{reason}"
    );
}

/// The control-plane Cluster's `live.context` is local configuration like the
/// workload Cluster's: moving it changes no published byte (so no drift, no
/// republication). Catches the control-plane Cluster being hashed raw into
/// the index.
#[test]
fn test_moving_the_control_plane_context_changes_no_published_byte() {
    let driver = real();
    let observed = Observed::new(Method::Api);
    // The instance's control-plane Cluster is another one, with its own `live`.
    observed.edit_platform(|text| {
        let text = text.replace("  clusterRef: {name: dev}\n  namespace: argocd", "  clusterRef: {name: control}\n  namespace: argocd");
        format!(
            "{text}---\napiVersion: k8s.nyl/v1\nkind: Cluster\nmetadata:\n  name: control\nspec:\n  destination:\n    server: https://kubernetes.default.svc\n  live: {{context: control-admin}}\n  kubernetes:\n    kubeVersion: 1.31.4\n    apiVersions: [v1, apps/v1, v1/Namespace, apps/v1/Deployment]\n    clusterScopedKinds: [core/Namespace]\n"
        )
    });
    let unit = observed.resolve(&driver, Observed::driver_spec(json!({})));
    observed.publish(&driver, &unit);
    let index = observed.index_bytes();
    let head = git(&observed.world.bare(), &["rev-parse", "refs/heads/deploy"]);

    observed.edit_platform(|text| text.replace("control-admin", "renamed-control-admin"));
    let moved = observed.resolve(&driver, Observed::driver_spec(json!({})));
    assert_eq!(facts(&moved).location.cluster, "control");
    assert_eq!(facts(&moved).location.context.as_deref(), Some("renamed-control-admin"));
    observed.publish(&driver, &moved);
    assert_eq!(
        observed.index_bytes(),
        index,
        "the index moved with the control-plane context"
    );
    assert_eq!(git(&observed.world.bare(), &["rev-parse", "refs/heads/deploy"]), head);
}
