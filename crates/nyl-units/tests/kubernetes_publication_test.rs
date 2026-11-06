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

use nyl_core::orchestration::driver::{
    Driver, DriverOperation, DriverResult, KeyInputs, ResolutionContext, Supported, TeardownSettings,
};
use nyl_core::orchestration::ids::{EnvironmentName, ExecutionKey, RunId};
use nyl_core::orchestration::ports::CommitIdentity;
use nyl_core::orchestration::state::{Drift, Receipt};
use nyl_units::kubernetes_publication::KubernetesPublicationDriver;
use publication_world::{conformance, context, desired, git, spec, NoTools, World};
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
    let key = ExecutionKey::from_digest_of(
        serde_json::to_string(&json!({"spec": resolved, "files": inputs.files}))
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
/// spec, and `teardown_settings` reads them back from the document alone.
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

    // The stored document answers without rendering anything.
    let unit = resolved_unit(&driver, &world, spec());
    assert_eq!(driver.teardown_settings(&unit).unwrap(), Supported::Supported(settings));
    // A document resolved before settings were recorded has no requirements.
    let unrecorded = desired(spec(), ExecutionKey::from_digest_of(b"key"));
    assert_eq!(driver.teardown_settings(&unrecorded).unwrap(), Supported::Unsupported);
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

    let Supported::Supported(settings) = driver.teardown_settings(&after).unwrap() else {
        panic!("recorded");
    };
    assert_eq!(settings.catalog_prunes, Some(true));
    assert_eq!(settings.application_deletion_policy.as_deref(), Some("Background"));
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

    // A file edited outside Nyl cannot be compared; the reason says why.
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
    let Drift::Unknown { reason } = drift_of(&world, &driver, &unit) else {
        panic!("the comparison fails");
    };
    assert!(reason.contains("modified outside Nyl"), "{reason}");
}

/// Catches observe mode, inline targets, and a hand-written
/// `teardownSettings` being accepted before the driver can honor them.
#[test]
fn test_validation_rejects_what_the_driver_cannot_do_yet() {
    let driver = real();
    let with = |field: &str, value: Value| {
        let mut spec = spec();
        spec[field] = value;
        driver.validate_spec(&spec)
    };
    assert!(driver.validate_spec(&spec()).is_ok());
    let observe = with("mode", json!("observe")).unwrap_err();
    assert!(observe.starts_with("NYL-KUBERNETES-MODE-UNSUPPORTED"), "{observe}");
    assert!(with("target", json!({"inline": {}}))
        .unwrap_err()
        .contains("inline is not supported yet"));
    assert!(with("teardownSettings", json!({"catalogPrunes": true}))
        .unwrap_err()
        .contains("must not be written by hand"));
    assert!(with("teardownWait", json!({"strategy": "delay"})).is_err());
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
