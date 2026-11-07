//! The real `KubernetesPublication` driver against the shared publication
//! world: the behaviors every publication kind has (`conformance`), and what
//! only the real driver does: record the target's effective teardown
//! settings at resolution, pin resolved `fromUnit` Release inputs into the
//! render and the ownership index, detect drift, and reject what it cannot
//! do yet.
//!
//! Contract: [Kubernetes publication unit](../../../design/orchestration-core.md#kubernetes-publication-unit)
//! and [Release inputs](../../../design/release-inputs.md).

mod publication_world;

use std::collections::BTreeMap;

use nyl_core::orchestration::codes::Code;
use nyl_core::orchestration::driver::{
    Driver, DriverOperation, DriverResult, IncarnationStatus, KeyInputs, Outcome, ResolutionContext, Supported,
};
use nyl_core::orchestration::ids::{EnvironmentName, ExecutionKey, RunId};
use nyl_core::orchestration::ports::CommitIdentity;
use nyl_core::orchestration::state::{Drift, Receipt};
use nyl_render::gitops::publication::TeardownSettings;
use nyl_units::kubernetes_publication::KubernetesPublicationDriver;
use publication_world::{conformance, context, context_with, desired, git, spec, Incarnations, NoTools, World};
use serde_json::{json, Value};

fn catalogue() -> nyl_render::config::ResourceCatalogue {
    let mut descriptors = nyl_render::resources::catalog::resource_catalogue();
    descriptors.extend_from_slice(nyl_units::catalog::RESOURCES);
    nyl_render::config::ResourceCatalogue(descriptors)
}

fn real() -> KubernetesPublicationDriver {
    KubernetesPublicationDriver::new(catalogue())
}

fn identity() -> CommitIdentity {
    "Nyl Orchestration <nyl@example.invalid>".parse().unwrap()
}

/// What resolution reads from the driver for `spec`.
fn resolve(driver: &dyn Driver, world: &World, spec: &Value) -> DriverResult<KeyInputs> {
    driver.execution_key_inputs(
        &ResolutionContext {
            instances: &nyl_core::orchestration::instance::NO_INSTANCES,
            source_roots: BTreeMap::default(),
            environment: EnvironmentName::new("dev").unwrap(),
            worktree: world.work(),
            project_dir: world.work().join("nyl"),
            env: BTreeMap::new(),
            deadline: chrono::Utc::now(),
            now: chrono::Utc::now(),
            tools: &NoTools,
            cache_dir: world.root.path().join("cache"),
        },
        spec,
    )
}

/// A unit resolved the way a run resolves it: the driver's recorded facts
/// join the spec, and the key covers the render's input record and that spec.
fn resolved_unit(driver: &dyn Driver, world: &World, authored: Value) -> nyl_core::orchestration::state::DesiredUnit {
    let inputs = resolve(driver, world, &authored).unwrap();
    let mut resolved = authored;
    inputs.record_into(&mut resolved).unwrap();
    // The key leaves out the fields the driver records but does not key.
    let mut keyed = resolved.clone();
    for pointer in &inputs.excluded_fields {
        if let Some((parent, name)) = pointer.rsplit_once('/') {
            if let Some(Value::Object(map)) = keyed.pointer_mut(parent) {
                map.remove(name);
            }
        }
    }
    let key = ExecutionKey::from_digest_of(
        serde_json::to_string(&json!({"spec": keyed, "files": inputs.files}))
            .unwrap()
            .as_bytes(),
    );
    desired(resolved, key)
}

#[test]
fn test_kubernetes_publication_publishes_the_target_tree_and_records_the_tree_artifact() {
    conformance::publishes_the_target_tree_and_records_the_tree_artifact(&real());
}

#[test]
fn test_kubernetes_publication_without_a_commit_identity_fails_before_effects() {
    conformance::publishing_without_a_commit_identity_fails_before_effects(&real());
}

#[test]
fn test_kubernetes_publication_tears_down_in_two_phases() {
    conformance::tears_down_in_two_phases(&real());
}

#[test]
fn test_kubernetes_publication_teardown_without_a_commit_identity_fails_before_effects() {
    conformance::teardown_without_a_commit_identity_fails_before_effects(&real());
}

#[test]
fn test_kubernetes_publication_plans_a_deleting_unit_as_destroy() {
    conformance::plans_a_deleting_unit_as_destroy(&real());
}

/// Catches readiness reading anything but the target's effective settings
/// as the render produced them, or a stored document that cannot answer
/// without rendering: resolution records the settings in the resolved
/// spec, and `teardown_readiness` evaluates them from the document alone.
#[test]
fn test_resolution_records_the_targets_effective_teardown_settings() {
    let world = World::new();
    let driver = real();
    let inputs = resolve(&driver, &world, &spec()).unwrap();
    let recorded = inputs
        .recorded
        .get("/teardownSettings")
        .expect("resolution records the teardown settings");
    let settings: TeardownSettings = serde_json::from_value(recorded.clone()).unwrap();
    // The walkthrough target keeps every default: a catalog that syncs
    // manually, cascades in the foreground, and Namespaces that confirm deletion.
    assert_eq!(settings.catalog_prunes, Some(false));
    assert_eq!(settings.application_deletion_policy.as_deref(), Some("Foreground"));
    assert!(!settings.catalog_owned_by_parent);
    assert_eq!(
        settings.namespace_delete_policies,
        BTreeMap::from([("web".to_owned(), "Confirm".to_owned())])
    );
    assert_eq!(
        settings.namespace_prune_policies,
        BTreeMap::from([("web".to_owned(), "Confirm".to_owned())])
    );

    // The stored document answers without rendering anything.
    let unit = resolved_unit(&driver, &world, spec());
    let Supported::Supported(readiness) = driver.teardown_readiness(&unit).unwrap() else {
        panic!("recorded settings give a readiness");
    };
    assert_eq!(
        readiness
            .blocking
            .iter()
            .map(|finding| finding.code)
            .collect::<Vec<_>>(),
        vec![Code::TeardownCatalogManualSync, Code::TeardownNamespaceConfirm]
    );
    assert_eq!(
        readiness
            .remnants
            .iter()
            .map(|finding| finding.code)
            .collect::<Vec<_>>(),
        vec![Code::TeardownNamespacesRemain, Code::TeardownCatalogRemains]
    );
    assert!(readiness.blocking[0].message.starts_with("dev/kubernetes: "));
    // A document resolved before settings were recorded has no requirements.
    let unrecorded = desired(spec(), ExecutionKey::from_digest_of(b"key"));
    assert_eq!(driver.teardown_readiness(&unrecorded).unwrap(), Supported::Unsupported);
}

/// Catches a target setting that changes what a teardown leaves behind
/// without changing the unit's key, so the stored settings go stale.
#[test]
fn test_recorded_teardown_settings_follow_the_target_and_its_key() {
    let world = World::new();
    let driver = real();
    let before = resolved_unit(&driver, &world, spec());

    let platform = world.work().join("nyl/platform.yaml");
    let text = std::fs::read_to_string(&platform).unwrap();
    std::fs::write(
        &platform,
        text.replace(
            "  publication:\n    repositoryRef: {name: platform}\n",
            "  catalogApplication:\n    syncPolicy: {automated: {prune: true}}\n    applicationDeletionPolicy: Background\n  publication:\n    repositoryRef: {name: platform}\n",
        ),
    )
    .unwrap();
    git(&world.work(), &["add", "--all"]);
    git(
        &world.work(),
        &["commit", "--quiet", "--message", "Prune from the catalog"],
    );
    let after = resolved_unit(&driver, &world, spec());

    let settings: TeardownSettings =
        serde_json::from_value(after.resolved_spec.as_ref().unwrap()["teardownSettings"].clone()).unwrap();
    assert_eq!(settings.catalog_prunes, Some(true));
    assert_eq!(settings.application_deletion_policy.as_deref(), Some("Background"));
    // A catalog that prunes leaves one blocking requirement: Namespaces that confirm.
    let Supported::Supported(readiness) = driver.teardown_readiness(&after).unwrap() else {
        panic!("recorded");
    };
    assert_eq!(
        readiness
            .blocking
            .iter()
            .map(|finding| finding.code)
            .collect::<Vec<_>>(),
        vec![Code::TeardownNamespaceConfirm]
    );
    assert_ne!(before.execution_key, after.execution_key);
}

/// The resolved `fromUnit` bindings: an artifact value (`image`, from a
/// `ContainerImage`) and an output value (`databaseHost`).
fn inputs_digest(world: &World) -> BTreeMap<String, String> {
    let index = git(&world.bare(), &["show", "deploy:dev/_nyl/index.json"]);
    let index: Value = serde_json::from_str(&index).unwrap();
    index["inputs"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(key, digest)| (key.clone(), digest.as_str().unwrap().to_owned()))
        .collect()
}

/// Catches a resolved `fromUnit` value (artifact or output form) missing
/// from the render, or from the published ownership index's provenance,
/// where `@input/<group>/<release>/<input>` records the digest of each
/// canonical value.
#[test]
fn test_resolved_from_unit_values_pin_the_render_and_enter_the_ownership_index() {
    let world = World::new();
    let driver = real();
    let unit = resolved_unit(&driver, &world, spec());
    let id = identity();
    driver
        .reconcile(&context(&world, DriverOperation::Reconcile, Some(&id)), &unit)
        .unwrap();

    let digests = inputs_digest(&world);
    for (input, value) in [
        ("image", "registry.invalid:5000/web@sha256:0123"),
        ("databaseHost", "db.vpc-walkthrough.internal"),
    ] {
        assert_eq!(
            digests.get(&format!("@input/web/web/{input}")),
            Some(&nyl_core::bindings::value_digest(&json!(value)).unwrap()),
            "{digests:?}"
        );
    }

    // A newly built image changes the key and the published tree.
    let mut rebuilt = spec();
    rebuilt["releases"]["web/web"]["image"] = json!("registry.invalid:5000/web@sha256:4567");
    let next = resolved_unit(&driver, &world, rebuilt);
    assert_ne!(unit.execution_key, next.execution_key);
    driver
        .reconcile(&context(&world, DriverOperation::Reconcile, Some(&id)), &next)
        .unwrap();
    let deployment = world
        .files("deploy")
        .iter()
        .filter(|path| path.starts_with("dev/web/"))
        .map(|path| git(&world.bare(), &["show", &format!("deploy:{path}")]))
        .find(|text| text.contains("kind: Deployment"))
        .unwrap();
    assert!(deployment.contains("sha256:4567") && !deployment.contains("sha256:0123"));
    assert_eq!(
        inputs_digest(&world).get("@input/web/web/image"),
        Some(&nyl_core::bindings::value_digest(&json!("registry.invalid:5000/web@sha256:4567")).unwrap())
    );
}

/// Catches a pinned input that the Release cannot take being rendered
/// anyway, or a binding left without a value producing a partial tree.
#[test]
fn test_unusable_resolved_inputs_fail_before_effects_and_publish_nothing() {
    let world = World::new();
    let driver = real();
    let id = identity();
    let key = ExecutionKey::from_digest_of(b"key");
    for (releases, expected) in [
        (json!({"web/web": {"image": 7, "databaseHost": "db"}}), "image"),
        (
            json!({"web/web": {"image": "registry.invalid:5000/web@sha256:0123"}}),
            "databaseHost",
        ),
    ] {
        let mut broken = spec();
        broken["releases"] = releases;
        let error = driver
            .reconcile(
                &context(&world, DriverOperation::Reconcile, Some(&id)),
                &desired(broken, key.clone()),
            )
            .unwrap_err();
        assert!(!error.effects_possible(), "{error}");
        assert!(error.to_string().contains(expected), "{error}");
    }
    assert!(git(&world.bare(), &["branch", "--list", "deploy"]).is_empty());
}

fn receipt(key: &ExecutionKey) -> Receipt {
    Receipt {
        execution_key: key.clone(),
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
        warnings: Vec::new(),
    }
}

fn drift_of(world: &World, driver: &dyn Driver, unit: &nyl_core::orchestration::state::DesiredUnit) -> Drift {
    let key = unit.execution_key.clone().unwrap();
    let Supported::Supported(detection) = driver
        .detect_drift(
            &context(world, DriverOperation::DetectDrift, None),
            unit,
            &receipt(&key),
        )
        .unwrap()
    else {
        panic!("a publication detects drift");
    };
    detection.drift
}

/// Catches drift detection answering `clean` without comparing the branch
/// with the tree the unit renders, and a comparison that publishes.
#[test]
fn test_drift_compares_the_branch_with_the_rendered_tree_without_publishing() {
    let world = World::new();
    let driver = real();
    let unit = resolved_unit(&driver, &world, spec());
    let id = identity();
    driver
        .reconcile(&context(&world, DriverOperation::Reconcile, Some(&id)), &unit)
        .unwrap();
    let head = git(&world.bare(), &["rev-parse", "refs/heads/deploy"]);
    assert_eq!(drift_of(&world, &driver, &unit), Drift::Clean);

    // The unit now renders another tree than the branch holds.
    let mut rebuilt = spec();
    rebuilt["releases"]["web/web"]["image"] = json!("registry.invalid:5000/web@sha256:4567");
    let Drift::Drifted { summary } = drift_of(&world, &driver, &resolved_unit(&driver, &world, rebuilt)) else {
        panic!("the branch holds another tree");
    };
    assert!(summary.contains("deploy"), "{summary}");
    assert_eq!(
        git(&world.bare(), &["rev-parse", "refs/heads/deploy"]),
        head,
        "nothing is published"
    );

    // A file edited outside Nyl is drift, and the summary says which.
    let owned = world
        .files("deploy")
        .into_iter()
        .find(|path| path.starts_with("dev/web/"))
        .unwrap();
    let other = world.root.path().join("other-tool");
    git(
        world.root.path(),
        &[
            "clone",
            "--quiet",
            "--branch",
            "deploy",
            &world.project_url(),
            "other-tool",
        ],
    );
    std::fs::write(other.join(&owned), "edited: true\n").unwrap();
    git(&other, &["commit", "--quiet", "--all", "--message", "Edit outside Nyl"]);
    git(&other, &["push", "--quiet", "origin", "HEAD:refs/heads/deploy"]);
    let Drift::Drifted { summary } = drift_of(&world, &driver, &unit) else {
        panic!("an owned file was edited outside Nyl");
    };
    assert!(summary.contains("modified outside Nyl"), "{summary}");
}

/// Catches a hand-written `teardownSettings` or `reconciler`, which only the
/// driver records at resolution, being accepted, the removed `mode` field
/// lingering, an `observe` with options, and a `delay` without a duration.
#[test]
fn test_validation_rejects_what_only_the_driver_records() {
    let driver = real();
    let with = |field: &str, value: Value| {
        let mut spec = spec();
        spec[field] = value;
        driver.validate_spec(&spec)
    };
    assert!(driver.validate_spec(&spec()).is_ok());
    assert!(with("observe", json!({})).is_ok());
    assert!(with("observe", json!({"interval": "5m"}))
        .unwrap_err()
        .contains("unknown field"));
    assert!(with("mode", json!("observe")).unwrap_err().contains("unknown field"));
    assert!(with("teardownWait", json!({"strategy": "observe"}))
        .unwrap_err()
        .contains("observe: {}"));
    assert!(with("teardownSettings", json!({"catalogPrunes": true}))
        .unwrap_err()
        .contains("must not be written by hand"));
    assert!(with("reconciler", json!({"instance": "x"}))
        .unwrap_err()
        .contains("must not be written by hand"));
    assert!(with("teardownWait", json!({"strategy": "delay"})).is_err());
}

fn unit_with(spec: Value, name: &str, uid: &str, key: &ExecutionKey) -> nyl_core::orchestration::state::DesiredUnit {
    let mut unit = desired(spec, key.clone());
    unit.unit.name = name.parse().unwrap();
    unit.unit.uid = Some(uid.parse().unwrap());
    unit
}

const FIRST: &str = "00000000-0000-7000-8000-000000000004";
const SECOND: &str = "00000000-0000-7000-8000-000000000005";

/// The ownership index published for `dev`, parsed.
fn published_index(world: &World) -> Value {
    serde_json::from_str(&git(&world.bare(), &["show", "deploy:dev/_nyl/index.json"])).unwrap()
}

fn owner(index: &Value) -> Value {
    index["owner"].clone()
}

/// A directory in which `dev/kubernetes` is `status`; every other unit is gone.
fn first_is(status: IncarnationStatus) -> Incarnations {
    Incarnations(BTreeMap::from([("dev/kubernetes".to_owned(), status)]))
}

/// Catches a published prefix that records no owner, or a publication that
/// rewrites it as a different version: an orchestrated publication writes a
/// version 3 index with the environment, the unit, and the incarnation's uid.
#[test]
fn test_publication_records_its_unit_incarnation_as_the_owner_of_the_prefix() {
    let world = World::new();
    let driver = real();
    let id = identity();
    let key = ExecutionKey::from_digest_of(b"key");
    let unit = unit_with(spec(), "kubernetes", FIRST, &key);
    driver
        .reconcile(&context(&world, DriverOperation::Reconcile, Some(&id)), &unit)
        .unwrap();
    let index = published_index(&world);
    assert_eq!(index["version"], 3);
    assert_eq!(
        owner(&index),
        json!({"environment": "dev", "unit": "kubernetes", "uid": FIRST})
    );
}

/// Catches a unit overwriting the prefix of another incarnation that still
/// exists, which would take its published tree away from it: the publication
/// fails with its own code, retryably because the next run reads state
/// again, and the branch is untouched. Once that incarnation is gone from
/// state the new owner takes the prefix over and records itself; when state
/// cannot say, the fence holds.
#[test]
fn test_a_prefix_owned_by_an_existing_incarnation_is_fenced_until_it_is_gone() {
    let world = World::new();
    let driver = real();
    let id = identity();
    let key = ExecutionKey::from_digest_of(b"key");
    driver
        .reconcile(
            &context(&world, DriverOperation::Reconcile, Some(&id)),
            &unit_with(spec(), "kubernetes", FIRST, &key),
        )
        .unwrap();
    let head = git(&world.bare(), &["rev-parse", "refs/heads/deploy"]);

    // Another incarnation of the unit, with a new image so it would change the tree.
    let mut changed = spec();
    changed["releases"]["web/web"]["image"] = json!("registry.invalid:5000/web@sha256:4567");
    let successor = unit_with(changed, "kubernetes", SECOND, &key);
    let publish = |directory: &Incarnations| {
        let Supported::Supported(outcome) = driver
            .reconcile(
                &context_with(&world, DriverOperation::Reconcile, Some(&id), directory),
                &successor,
            )
            .unwrap()
        else {
            panic!("the driver reconciles");
        };
        outcome
    };
    for (status, code) in [
        (IncarnationStatus::Exists, "NYL-KUBERNETES-OWNER-FENCED"),
        (
            IncarnationStatus::Unknown("the state of environment dev cannot be read".to_owned()),
            "NYL-KUBERNETES-OWNER-UNKNOWN",
        ),
    ] {
        let Outcome::Failed {
            category,
            retryable,
            message,
        } = publish(&first_is(status.clone()))
        else {
            panic!("a fenced publication fails: {status:?}");
        };
        assert_eq!(category, nyl_core::orchestration::state::FailureCategory::Ownership);
        assert!(retryable, "the next run reads state again");
        assert!(message.starts_with(&format!("{code}:")), "{message}");
        assert!(
            message.contains(&format!("unit dev/kubernetes (uid {FIRST})")),
            "{message}"
        );
        if code.ends_with("UNKNOWN") {
            // The way out of an owner that state can no longer answer for.
            assert!(message.contains("remove the `owner` member"), "{message}");
        }
        assert_eq!(git(&world.bare(), &["rev-parse", "refs/heads/deploy"]), head);
    }

    assert!(matches!(
        publish(&first_is(IncarnationStatus::Gone)),
        Outcome::Succeeded { .. }
    ));
    assert_eq!(
        owner(&published_index(&world)),
        json!({"environment": "dev", "unit": "kubernetes", "uid": SECOND})
    );
}

/// Catches a prefix published before orchestration owned it (index version 3,
/// as `publish-tree` writes it) being refused or left without an owner: the
/// first publication of a unit adopts it, without asking whether anyone else
/// exists, and records its owner in it.
#[test]
fn test_a_prefix_without_an_owner_is_adopted_and_upgraded() {
    let world = World::new();
    let driver = real();
    let id = identity();
    let key = ExecutionKey::from_digest_of(b"key");
    let unit = unit_with(spec(), "kubernetes", FIRST, &key);
    driver
        .reconcile(&context(&world, DriverOperation::Reconcile, Some(&id)), &unit)
        .unwrap();

    // The index as `publish-tree` wrote it: version 3, no owner.
    let other = world.root.path().join("publish-tree");
    git(
        world.root.path(),
        &[
            "clone",
            "--quiet",
            "--branch",
            "deploy",
            &world.project_url(),
            "publish-tree",
        ],
    );
    let mut index = published_index(&world);
    index.as_object_mut().unwrap().remove("owner");
    std::fs::write(
        other.join("dev/_nyl/index.json"),
        serde_json::to_string_pretty(&index).unwrap() + "\n",
    )
    .unwrap();
    git(
        &other,
        &["commit", "--quiet", "--all", "--message", "As publish-tree wrote it"],
    );
    git(&other, &["push", "--quiet", "origin", "HEAD:refs/heads/deploy"]);
    assert_eq!(published_index(&world)["version"], 3);

    // A directory that knows nothing is never asked.
    let successor = unit_with(spec(), "kubernetes", SECOND, &key);
    let Supported::Supported(outcome) = driver
        .reconcile(&context(&world, DriverOperation::Reconcile, Some(&id)), &successor)
        .unwrap()
    else {
        panic!("the driver reconciles");
    };
    assert!(matches!(outcome, Outcome::Succeeded { .. }), "{outcome:?}");
    let index = published_index(&world);
    assert_eq!(index["version"], 3);
    assert_eq!(
        owner(&index),
        json!({"environment": "dev", "unit": "kubernetes", "uid": SECOND})
    );
}

/// Catches a teardown deleting files another existing incarnation owns, in
/// either phase: both fail fenced and leave the branch as it was.
#[test]
fn test_teardown_of_a_fenced_prefix_removes_nothing() {
    let world = World::new();
    let driver = real();
    let id = identity();
    let key = ExecutionKey::from_digest_of(b"key");
    driver
        .reconcile(
            &context(&world, DriverOperation::Reconcile, Some(&id)),
            &unit_with(spec(), "kubernetes", FIRST, &key),
        )
        .unwrap();
    let head = git(&world.bare(), &["rev-parse", "refs/heads/deploy"]);
    let successor = publication_world::deleting(unit_with(spec(), "kubernetes", SECOND, &key));
    let directory = first_is(IncarnationStatus::Exists);

    let teardown = |completion| {
        let mut ctx = context_with(&world, DriverOperation::Teardown, Some(&id), &directory);
        ctx.teardown_wait = completion;
        let Supported::Supported(outcome) = driver.teardown(&ctx, &successor).unwrap() else {
            panic!("the driver tears down");
        };
        outcome
    };
    for completion in [None, Some(nyl_core::orchestration::driver::WaitCompletion::Delayed)] {
        let Outcome::Failed { message, .. } = teardown(completion.clone()) else {
            panic!("a fenced teardown fails: {completion:?}");
        };
        assert!(message.starts_with("NYL-KUBERNETES-OWNER-FENCED:"), "{message}");
        assert_eq!(git(&world.bare(), &["rev-parse", "refs/heads/deploy"]), head);
    }
}

/// The unit of the world with its target declared inline: the `preview`
/// target publishes to its own prefix, of the same cluster and repository.
fn inline_spec() -> Value {
    let mut spec = spec();
    spec["target"] = json!({
        "name": "preview",
        "sourceFile": "nyl/platform.yaml",
        "inline": {
            "reconciler": {"argocd": {}},
            "clusterRef": {"name": "dev"},
            "applicationGroupSelector": {"matchLabels": {"app": "web"}},
            "publication": {"repositoryRef": {"name": "platform"}, "revision": "deploy", "pathPrefix": "preview"},
            "values": {"hostname": "preview.example.com"},
            // What resolving the unit leaves of the `fromUnit` bindings: their values.
            "releaseInputs": {"web/web": {
                "image": "registry.invalid:5000/web@sha256:0123",
                "databaseHost": "db.vpc-walkthrough.internal",
            }},
        },
    });
    spec
}

/// The world whose only target is the inline one: no declared target
/// selects the ApplicationGroup too, so the Applications and AppProject the
/// group generates for the inline target are unique on the cluster, and no
/// Environment selects the unit whose inline spec the tests pass directly.
fn inline_only() -> World {
    let world = World::new();
    let edit = |path: &str, from: &str, to: &str| {
        let file = world.work().join(path);
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains(from), "{path} holds {from:?}");
        std::fs::write(file, text.replace(from, to)).unwrap();
    };
    let platform = world.work().join("nyl/platform.yaml");
    let text = std::fs::read_to_string(&platform).unwrap();
    let documents = text
        .split("---\n")
        .filter(|document| !document.contains("kind: DeploymentTarget"));
    std::fs::write(&platform, documents.collect::<Vec<_>>().join("---\n")).unwrap();
    edit(
        "nyl/units.yaml",
        "name: kubernetes\n  labels: {dev: 'true'}",
        "name: kubernetes\n  labels: {standalone: 'true'}",
    );
    git(&world.work(), &["add", "--all"]);
    git(
        &world.work(),
        &["commit", "--quiet", "--message", "Declare only the inline target"],
    );
    world
}

/// Catches an inline target going through any other path than a declared
/// one: it keys the unit like a target (the render's input record names the
/// target by its inline name), publishes beneath its own prefix with its own
/// name in the ownership index, records the unit as owner, and tears down in
/// two phases like every target.
#[test]
fn test_inline_target_publishes_and_tears_down_like_a_declared_one() {
    let world = inline_only();
    let driver = real();
    let id = identity();
    let unit = resolved_unit(&driver, &world, inline_spec());
    let inputs = resolve(&driver, &world, &inline_spec()).unwrap();
    assert!(
        inputs.files.contains_key("@resource:DeploymentTarget/preview"),
        "{:?}",
        inputs.files
    );

    driver
        .reconcile(&context(&world, DriverOperation::Reconcile, Some(&id)), &unit)
        .unwrap();
    let files = world.files("deploy");
    assert!(files.contains(&"preview/_nyl/index.json".to_owned()), "{files:?}");
    assert!(!files.iter().any(|path| path.starts_with("dev/")), "{files:?}");
    // The group the inline target selects is published: its workload
    // Application in the catalog and the Release's files beneath the prefix.
    let workload_applications = || {
        world
            .files("deploy")
            .iter()
            .filter(|path| path.starts_with("preview/_nyl/catalog/"))
            .map(|path| git(&world.bare(), &["show", &format!("deploy:{path}")]))
            .filter(|text| text.contains("kind: Application") && text.contains("path: preview/web"))
            .count()
    };
    assert!(
        workload_applications() > 0,
        "the group's Application is published: {files:?}"
    );
    let release_files = |branch_files: &[String]| {
        branch_files
            .iter()
            .filter(|path| path.starts_with("preview/web/"))
            .cloned()
            .collect::<Vec<_>>()
    };
    let published_files = release_files(&files);
    assert!(
        !published_files.is_empty(),
        "the Release's files are published: {files:?}"
    );
    let deployment = published_files
        .iter()
        .map(|path| git(&world.bare(), &["show", &format!("deploy:{path}")]))
        .find(|text| text.contains("kind: Deployment"))
        .expect("the Deployment is published");
    assert!(
        deployment.contains("registry.invalid:5000/web@sha256:0123"),
        "{deployment}"
    );
    let index: Value = serde_json::from_str(&git(&world.bare(), &["show", "deploy:preview/_nyl/index.json"])).unwrap();
    assert_eq!(index["target"], "preview");
    assert_eq!(index["version"], 3);
    assert_eq!(owner(&index)["unit"], "kubernetes");
    // The target's own values reach the render, and a changed one changes the key.
    let mut moved = inline_spec();
    moved["target"]["inline"]["values"]["hostname"] = json!("other.example.com");
    assert_ne!(resolved_unit(&driver, &world, moved).execution_key, unit.execution_key);

    // Two phases, the second removing exactly what the index owns.
    let deleting = publication_world::deleting(unit);
    let mut ctx = context(&world, DriverOperation::Teardown, Some(&id));
    let Supported::Supported(Outcome::Waiting { published, .. }) = driver.teardown(&ctx, &deleting).unwrap() else {
        panic!("phase 1 waits");
    };
    assert!(published.is_some());
    // Phase 1 removes the workload Application and keeps the Release's files.
    assert_eq!(workload_applications(), 0, "phase 1 removes the group's Application");
    assert_eq!(
        release_files(&world.files("deploy")),
        published_files,
        "phase 1 keeps the Release's files"
    );
    ctx.teardown_wait = Some(nyl_core::orchestration::driver::WaitCompletion::Delayed);
    driver.teardown(&ctx, &deleting).unwrap();
    assert!(!world.files("deploy").iter().any(|path| path.starts_with("preview/")));

    // Two units naming one inline target claim one ownership scope.
    let dev = EnvironmentName::new("dev").unwrap();
    let [first] = driver.ownership_scopes(&dev, &inline_spec()).try_into().unwrap();
    let [second] = driver.ownership_scopes(&dev, &inline_spec()).try_into().unwrap();
    assert!(first.overlaps(&second));
}

/// [`inline_spec`] for the preview `name`: its own prefix and name, and its
/// catalog shared as `previews` with `shared`.
fn preview_spec(name: &str, shared: Value) -> Value {
    let mut spec = inline_spec();
    spec["target"]["name"] = json!(name);
    spec["target"]["inline"]["publication"]["pathPrefix"] = json!(name);
    spec["target"]["inline"]["catalogApplication"] = json!({"shared": shared});
    spec
}

/// The artifact `tree` a publication recorded.
fn tree_artifact(outcome: &Outcome) -> Value {
    let Outcome::Succeeded { artifacts, .. } = outcome else {
        panic!("the publication succeeds: {outcome:?}");
    };
    artifacts
        .iter()
        .find(|artifact| artifact.metadata.name == "tree")
        .expect("a PublishedTree")
        .spec
        .clone()
}

/// A shared catalog end to end through the driver: the first target's
/// publication creates the manifest under its reserved prefix with the owner
/// `catalog:<name>` (the target's own tree holds no catalog Application),
/// later targets find it current and leave it alone, a target that renders
/// another manifest reports `differs` without overwriting it, teardown reads
/// the shared Application's settings and leaves the catalog standing, and
/// `nyl build -- --shared-catalog` is the one writer that replaces it. Catches
/// pull requests overwriting each other's manifest, a catalog removed with its
/// last target, and readiness read from a catalog that is not in the tree.
#[test]
fn test_a_shared_catalog_is_created_by_the_first_target_and_replaced_only_by_a_build() {
    let world = inline_only();
    let driver = real();
    let id = identity();
    let pruning = json!({"name": "previews", "syncPolicy": {"automated": {"prune": true}}});
    let publish = |name: &str, shared: Value| {
        let unit = resolved_unit(&driver, &world, preview_spec(name, shared));
        let Supported::Supported(outcome) = driver
            .reconcile(&context(&world, DriverOperation::Reconcile, Some(&id)), &unit)
            .unwrap()
        else {
            panic!("the driver reconciles");
        };
        (unit, outcome)
    };

    // Before the manifest exists nothing prunes yet, so the catalog rows are not checked.
    let before = resolved_unit(&driver, &world, preview_spec("pr-1", pruning.clone()));
    assert_eq!(
        before.resolved_spec.as_ref().unwrap()["sharedCatalogReading"]["result"],
        "missing"
    );
    assert_eq!(
        before.resolved_spec.as_ref().unwrap()["sharedCatalogReading"]["settings"]["catalogPrunes"],
        Value::Null
    );

    let (_, outcome) = publish("pr-1", pruning.clone());
    assert_eq!(tree_artifact(&outcome)["sharedCatalog"]["result"], "created");
    let files = world.files("deploy");
    for path in [
        "_nyl/shared/previews/catalog.yaml",
        "_nyl/shared/previews/_nyl/index.json",
        "pr-1/_nyl/index.json",
    ] {
        assert!(files.contains(&path.to_owned()), "{path} in {files:?}");
    }
    let manifest = git(&world.bare(), &["show", "deploy:_nyl/shared/previews/catalog.yaml"]);
    assert!(
        manifest.contains("name: previews") && manifest.contains("prune: true"),
        "{manifest}"
    );
    let shared_index: Value = serde_json::from_str(&git(
        &world.bare(),
        &["show", "deploy:_nyl/shared/previews/_nyl/index.json"],
    ))
    .unwrap();
    assert_eq!(shared_index["owner"], "catalog:previews");
    assert!(
        !files
            .iter()
            .filter(|path| path.starts_with("pr-1/_nyl/catalog/"))
            .any(|path| git(&world.bare(), &["show", &format!("deploy:{path}")]).contains("path: pr-1/_nyl/catalog")),
        "the target's tree holds no catalog Application of its own"
    );
    // Readiness is read from the shared Application the branch holds, which the project does not own.
    let first = resolved_unit(&driver, &world, preview_spec("pr-1", pruning.clone()));
    let reading = first.resolved_spec.as_ref().unwrap()["sharedCatalogReading"].clone();
    assert_eq!(reading["result"], "current", "{reading}");
    assert_eq!(reading["settings"]["catalogPrunes"], true, "{reading}");
    assert_eq!(reading["settings"]["catalogOwnedByParent"], true, "{reading}");
    // The branch's state is not part of the key: creating the manifest did not re-key the unit.
    assert_eq!(before.execution_key, first.execution_key);
    // A target that declares another manifest is read as published: this one prunes,
    // whatever the declaration says.
    let declared = resolved_unit(&driver, &world, preview_spec("pr-3", json!({"name": "previews"})));
    assert_eq!(
        declared.resolved_spec.as_ref().unwrap()["sharedCatalogReading"]["settings"]["catalogPrunes"],
        true
    );

    // A second target finds the same manifest and writes nothing outside its prefix.
    let head = git(&world.bare(), &["rev-parse", "deploy:_nyl/shared/previews"]);
    let (_, outcome) = publish("pr-2", pruning.clone());
    assert_eq!(tree_artifact(&outcome)["sharedCatalog"]["result"], "current");
    assert_eq!(git(&world.bare(), &["rev-parse", "deploy:_nyl/shared/previews"]), head);

    // A target that renders another manifest leaves the published one alone.
    let (_, outcome) = publish("pr-3", json!({"name": "previews"}));
    assert_eq!(tree_artifact(&outcome)["sharedCatalog"]["result"], "differs");
    let Outcome::Succeeded { warnings, .. } = &outcome else {
        panic!("the publication succeeds");
    };
    assert_eq!(
        warnings.iter().map(|finding| finding.code).collect::<Vec<_>>(),
        vec![Code::KubernetesSharedCatalogDiffers]
    );
    assert!(
        warnings[0].fix.contains("--shared-catalog --push"),
        "{}",
        warnings[0].fix
    );
    assert_eq!(git(&world.bare(), &["rev-parse", "deploy:_nyl/shared/previews"]), head);

    // The build is the writer: --diff reads, --push replaces.
    let build_unit = resolved_unit(&driver, &world, preview_spec("pr-3", json!({"name": "previews"})));
    let build = |arguments: &[&str]| {
        let arguments = arguments.iter().map(std::ffi::OsString::from).collect::<Vec<_>>();
        let mut ctx = context(&world, DriverOperation::Build, Some(&id));
        ctx.dirty_source = false;
        driver.build(&ctx, &build_unit, &arguments).unwrap()
    };
    let Supported::Supported(diff) = build(&["--shared-catalog", "--diff"]) else {
        panic!("the driver builds");
    };
    assert_eq!(diff[0].spec["changes"], "update");
    assert_eq!(diff[0].spec["owner"], "catalog:previews");
    assert_eq!(diff[0].spec["pathPrefix"], "_nyl/shared/previews");
    assert_eq!(git(&world.bare(), &["rev-parse", "deploy:_nyl/shared/previews"]), head);
    let Supported::Supported(pushed) = build(&["--shared-catalog", "--push"]) else {
        panic!("the driver builds");
    };
    assert_eq!(pushed[0].spec["target"], "catalog:previews");
    assert_ne!(git(&world.bare(), &["rev-parse", "deploy:_nyl/shared/previews"]), head);
    let replaced = git(&world.bare(), &["show", "deploy:_nyl/shared/previews/catalog.yaml"]);
    assert!(!replaced.contains("prune: true"), "{replaced}");

    // Teardown of the last target removes its own files; the catalog stays.
    let mut ctx = context(&world, DriverOperation::Teardown, Some(&id));
    let deleting = publication_world::deleting(first);
    driver.teardown(&ctx, &deleting).unwrap();
    ctx.teardown_wait = Some(nyl_core::orchestration::driver::WaitCompletion::Delayed);
    driver.teardown(&ctx, &deleting).unwrap();
    let files = world.files("deploy");
    assert!(!files.iter().any(|path| path.starts_with("pr-1/")), "{files:?}");
    assert!(
        files.contains(&"_nyl/shared/previews/catalog.yaml".to_owned()),
        "{files:?}"
    );

    // --shared-catalog of a target that shares none is refused with its code.
    let plain = resolved_unit(&driver, &world, inline_spec());
    let error = driver
        .build(
            &context(&world, DriverOperation::Build, Some(&id)),
            &plain,
            &[std::ffi::OsString::from("--shared-catalog")],
        )
        .expect_err("a target without a shared catalog has none to build");
    assert!(
        error.to_string().contains("NYL-KUBERNETES-BUILD-NO-SHARED-CATALOG"),
        "{error}"
    );
}

/// Catches teardown readiness trusting the instance's own declaration over
/// what Argo CD runs: the branch's manifest does not prune, a pull request
/// declares a pruning one, and the unit must still report that the catalog
/// does not prune, so its teardown is held instead of orphaning workloads.
/// Another shared catalog on the branch refuses with the conflict code.
#[test]
fn test_readiness_reads_the_published_shared_catalog_and_another_catalog_conflicts() {
    let world = inline_only();
    let driver = real();
    let id = identity();
    let unit = resolved_unit(&driver, &world, preview_spec("pr-1", json!({"name": "previews"})));
    driver
        .reconcile(&context(&world, DriverOperation::Reconcile, Some(&id)), &unit)
        .unwrap();
    let pruning = json!({"name": "previews", "syncPolicy": {"automated": {"prune": true}}});
    let declared = resolved_unit(&driver, &world, preview_spec("pr-2", pruning));
    let reading = &declared.resolved_spec.as_ref().unwrap()["sharedCatalogReading"];
    assert_eq!(reading["result"], "differs", "{reading}");
    assert_eq!(reading["settings"]["catalogPrunes"], false, "{reading}");
    let readiness = driver.teardown_readiness(&declared).unwrap();
    let Supported::Supported(readiness) = readiness else {
        panic!("the driver checks readiness");
    };
    assert!(
        readiness
            .blocking
            .iter()
            .any(|finding| finding.code == Code::TeardownCatalogManualSync),
        "{:?}",
        readiness.blocking
    );

    // A renamed catalog leaves the old one on the branch.
    let error = driver
        .execution_key_inputs(
            &ResolutionContext {
                instances: &nyl_core::orchestration::instance::NO_INSTANCES,
                source_roots: BTreeMap::default(),
                environment: EnvironmentName::new("dev").unwrap(),
                worktree: world.work(),
                project_dir: world.work().join("nyl"),
                env: BTreeMap::new(),
                deadline: chrono::Utc::now(),
                now: chrono::Utc::now(),
                tools: &NoTools,
                cache_dir: world.root.path().join("cache"),
            },
            &preview_spec("pr-2", json!({"name": "renamed"})),
        )
        .expect_err("two shared catalogs on one branch conflict");
    assert!(
        error.to_string().contains("NYL-KUBERNETES-SHARED-CATALOG-CONFLICT"),
        "{error}"
    );
    let outcome = driver.reconcile(
        &context(&world, DriverOperation::Reconcile, Some(&id)),
        &unit_with_renamed(&unit),
    );
    assert!(outcome.is_err(), "a publication refuses to add a second catalog");
}

/// `unit` published to a catalog named `renamed`.
fn unit_with_renamed(
    unit: &nyl_core::orchestration::state::DesiredUnit,
) -> nyl_core::orchestration::state::DesiredUnit {
    let mut renamed = unit.clone();
    let spec = renamed.resolved_spec.as_mut().unwrap();
    spec["target"]["inline"]["catalogApplication"]["shared"]["name"] = json!("renamed");
    renamed.spec = spec.clone();
    renamed
}

/// Catches the shared catalog's writer re-keying every preview, and a teardown
/// that trusts settings older than the writer's last push. The key follows what
/// the target renders, never the branch: a writer push that changes the
/// policy leaves every key alone. The stored settings of an instance that
/// expired are stale, so its teardown reads the branch again and is held, with
/// the readiness code and nothing published, while the catalog no longer
/// prunes; once it prunes again the teardown proceeds.
#[test]
fn test_the_writer_never_rekeys_previews_and_a_teardown_reads_the_catalog_again() {
    let world = inline_only();
    let driver = real();
    let id = identity();
    let pruning = json!({"name": "previews", "syncPolicy": {"automated": {"prune": true}}});
    let plain = json!({"name": "previews"});
    let first = resolved_unit(&driver, &world, preview_spec("pr-1", pruning.clone()));
    driver
        .reconcile(&context(&world, DriverOperation::Reconcile, Some(&id)), &first)
        .unwrap();
    let writer = |shared: &Value| {
        let unit = resolved_unit(&driver, &world, preview_spec("pr-9", shared.clone()));
        let arguments = ["--shared-catalog", "--push"].map(std::ffi::OsString::from);
        driver
            .build(&context(&world, DriverOperation::Build, Some(&id)), &unit, &arguments)
            .unwrap();
    };
    // Resolved while the catalog prunes, as the fleet's last run left it.
    let stored = resolved_unit(&driver, &world, preview_spec("pr-1", pruning.clone()));
    assert_eq!(
        stored.execution_key, first.execution_key,
        "the manifest's creation re-keys nothing"
    );
    // A writer push that stops pruning does not re-key the instance, but its readiness follows.
    writer(&plain);
    let again = resolved_unit(&driver, &world, preview_spec("pr-1", pruning.clone()));
    assert_eq!(again.execution_key, first.execution_key);
    assert_eq!(
        again.resolved_spec.as_ref().unwrap()["sharedCatalogReading"]["settings"]["catalogPrunes"],
        false
    );

    // The stored document is the one resolved while the catalog pruned.
    let stale = publication_world::deleting(stored);
    assert_eq!(
        stale.resolved_spec.as_ref().unwrap()["sharedCatalogReading"]["settings"]["catalogPrunes"],
        true
    );
    let head = git(&world.bare(), &["rev-parse", "refs/heads/deploy"]);
    let teardown = |unit: &nyl_core::orchestration::state::DesiredUnit| {
        let Supported::Supported(outcome) = driver
            .teardown(&context(&world, DriverOperation::Teardown, Some(&id)), unit)
            .unwrap()
        else {
            panic!("the driver tears down");
        };
        outcome
    };
    let Outcome::Failed { message, retryable, .. } = teardown(&stale) else {
        panic!("a teardown over a catalog that no longer prunes is held");
    };
    assert!(retryable);
    assert!(message.starts_with("NYL-TEARDOWN-CATALOG-MANUAL-SYNC:"), "{message}");
    assert_eq!(
        git(&world.bare(), &["rev-parse", "refs/heads/deploy"]),
        head,
        "nothing was published"
    );

    // Pruning again, the same stored document starts its teardown.
    writer(&pruning);
    assert!(matches!(teardown(&stale), Outcome::Waiting { .. }));
}

/// A branch that cannot be read, and a manifest that is no Application, are
/// recorded as readings of their own and block teardown readiness, rather than
/// failing the resolution or passing as "not checked": a catalog that cannot be
/// shown to prune is not trusted to. Catches both reading as harmless.
#[test]
fn test_an_unreadable_branch_and_an_unparseable_manifest_are_recorded_and_block_readiness() {
    let world = inline_only();
    let driver = real();
    let id = identity();
    let pruning = json!({"name": "previews", "syncPolicy": {"automated": {"prune": true}}});
    let first = resolved_unit(&driver, &world, preview_spec("pr-1", pruning.clone()));
    driver
        .reconcile(&context(&world, DriverOperation::Reconcile, Some(&id)), &first)
        .unwrap();
    let blocked = |unit: &nyl_core::orchestration::state::DesiredUnit| {
        let Supported::Supported(readiness) = driver.teardown_readiness(unit).unwrap() else {
            panic!("the driver checks readiness");
        };
        readiness.blocking.into_iter().find(|finding| {
            finding.code == Code::TeardownCatalogManualSync && finding.message.contains("cannot be shown to prune")
        })
    };
    assert!(blocked(&resolved_unit(&driver, &world, preview_spec("pr-1", pruning.clone()))).is_none());

    // A manifest on the branch that is no YAML at all.
    let checkout = world.root.path().join("edit");
    git(
        world.root.path(),
        &["clone", "--quiet", "--branch", "deploy", &world.project_url(), "edit"],
    );
    std::fs::write(checkout.join("_nyl/shared/previews/catalog.yaml"), "a: [unclosed\n").unwrap();
    git(
        &checkout,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.invalid",
            "commit",
            "--quiet",
            "-am",
            "break",
        ],
    );
    git(&checkout, &["push", "--quiet", "origin", "deploy"]);
    let unparseable = resolved_unit(&driver, &world, preview_spec("pr-1", pruning.clone()));
    let reading = &unparseable.resolved_spec.as_ref().unwrap()["sharedCatalogReading"];
    assert_eq!(reading["result"], "unparseable", "{reading}");
    assert!(reading["problem"].is_string(), "{reading}");
    assert!(blocked(&unparseable).is_some());

    // A branch that cannot be fetched.
    let moved = world.root.path().join("moved.git");
    std::fs::rename(world.bare(), &moved).unwrap();
    let unreadable = resolved_unit(&driver, &world, preview_spec("pr-1", pruning));
    let reading = &unreadable.resolved_spec.as_ref().unwrap()["sharedCatalogReading"];
    assert_eq!(reading["result"], "unreadable", "{reading}");
    assert!(blocked(&unreadable).is_some());
    std::fs::rename(moved, world.bare()).unwrap();
}

/// Catches a renamed target publishing a second tree and orphaning the first,
/// or failing in the ownership index with no way out: nothing in the engine
/// replaces a unit when its destination changes, so the driver fails closed
/// before any effect with `NYL-KUBERNETES-TARGET-CHANGED`, naming what changed
/// and how to replace the unit. It holds with an explicit prefix, which stays
/// while the name changes, and with the default prefix, which follows the name.
#[test]
fn test_a_changed_target_destination_fails_before_publishing_anything() {
    let driver = real();
    let id = identity();
    let key = ExecutionKey::from_digest_of(b"key");
    for (explicit_prefix, changed) in [
        (true, "target \"preview\" became \"preview-2\""),
        (false, "pathPrefix \"preview-1\" became \"preview-2\""),
    ] {
        let world = inline_only();
        let published = |spec: Value| desired(spec, key.clone());
        let mut before = inline_spec();
        if !explicit_prefix {
            before["target"]["inline"]["publication"]
                .as_object_mut()
                .unwrap()
                .remove("pathPrefix");
            before["target"]["name"] = json!("preview-1");
        }
        let Supported::Supported(Outcome::Succeeded { artifacts, .. }) = driver
            .reconcile(
                &context(&world, DriverOperation::Reconcile, Some(&id)),
                &published(before.clone()),
            )
            .unwrap()
        else {
            panic!("the first publication succeeds");
        };
        let head = git(&world.bare(), &["rev-parse", "refs/heads/deploy"]);
        assert_eq!(artifacts[0].spec["target"], before["target"]["name"]);

        let mut renamed = before.clone();
        renamed["target"]["name"] = json!("preview-2");
        let mut ctx = context(&world, DriverOperation::Reconcile, Some(&id));
        ctx.artifacts = artifacts
            .into_iter()
            .map(|artifact| ("tree".to_owned(), artifact))
            .collect();
        let Supported::Supported(Outcome::Failed { retryable, message, .. }) =
            driver.reconcile(&ctx, &published(renamed.clone())).unwrap()
        else {
            panic!("a changed destination fails");
        };
        assert!(retryable, "restoring the name needs no recovery");
        assert!(message.starts_with("NYL-KUBERNETES-TARGET-CHANGED:"), "{message}");
        assert!(message.contains(changed), "{message}");
        assert!(message.contains("nyl teardown -e dev --unit kubernetes"), "{message}");
        assert_eq!(
            git(&world.bare(), &["rev-parse", "refs/heads/deploy"]),
            head,
            "nothing was published"
        );

        // The unchanged destination still publishes.
        let Supported::Supported(outcome) = driver.reconcile(&ctx, &published(before)).unwrap() else {
            panic!("the driver reconciles");
        };
        assert!(matches!(outcome, Outcome::Succeeded { .. }), "{outcome:?}");
    }
}

/// Catches two units publishing one DeploymentTarget going unnoticed.
#[test]
fn test_units_claim_their_target_as_an_ownership_scope() {
    let driver = real();
    let dev = EnvironmentName::new("dev").unwrap();
    let scopes = |target: &str| driver.ownership_scopes(&dev, &json!({"target": target}));
    let [first] = scopes("dev").try_into().unwrap();
    let [again] = scopes("dev").try_into().unwrap();
    let [other] = scopes("prod").try_into().unwrap();
    assert!(first.overlaps(&again));
    assert!(!first.overlaps(&other));
}
