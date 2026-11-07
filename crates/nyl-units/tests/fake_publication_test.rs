//! `FakePublication`, the test stand-in for `KubernetesPublication`, against
//! the shared publication world: the behaviors every publication kind has
//! (`conformance`) and what only the fake adds, `teardownSettings` that a
//! scenario declares and `mode: observe`.
//!
//! Contract: [Fake kinds](../../../design/reference-scenarios.md#fake-kinds)
//! and the [Kubernetes publication unit](../../../design/orchestration-core.md#kubernetes-publication-unit).

mod publication_world;

use std::collections::BTreeMap;

use nyl_core::orchestration::driver::{Driver, Supported, TeardownSettings};
use nyl_core::orchestration::ids::ExecutionKey;
use nyl_units::fake::{FakePublication, Script};
use publication_world::{conformance, desired, spec};
use serde_json::{json, Value};

fn fake() -> FakePublication {
    FakePublication::new(Script::new(), nyl_units::fake::test_catalogue())
}

#[test]
fn test_fake_publication_publishes_the_target_tree_and_records_the_tree_artifact() {
    conformance::publishes_the_target_tree_and_records_the_tree_artifact(&fake());
}

#[test]
fn test_fake_publication_without_a_commit_identity_fails_before_effects() {
    conformance::publishing_without_a_commit_identity_fails_before_effects(&fake());
}

#[test]
fn test_fake_publication_tears_down_in_two_phases() {
    conformance::tears_down_in_two_phases(&fake());
}

#[test]
fn test_fake_publication_teardown_without_a_commit_identity_fails_before_effects() {
    conformance::teardown_without_a_commit_identity_fails_before_effects(&fake());
}

#[test]
fn test_fake_publication_plans_a_deleting_unit_as_destroy() {
    conformance::plans_a_deleting_unit_as_destroy(&fake());
}

/// Catches `status` or `plan` showing readiness from anything but the
/// stored document, or claiming requirements for a document that records none.
#[test]
fn test_fake_publication_teardown_settings_come_from_the_stored_document() {
    let driver = fake();
    let mut recorded = spec();
    recorded["teardownSettings"] = json!({
        "catalogPrunes": false,
        "applicationDeletionPolicy": "Foreground",
        "namespaceDeletePolicies": {"web": "Confirm"},
        "catalogOwnedByParent": true,
    });
    let unit = desired(recorded, ExecutionKey::from_digest_of(b"key"));
    assert_eq!(
        driver.teardown_settings(&unit).unwrap(),
        Supported::Supported(TeardownSettings {
            catalog_prunes: Some(false),
            application_deletion_policy: Some("Foreground".to_owned()),
            namespace_delete_policies: BTreeMap::from([("web".to_owned(), "Confirm".to_owned())]),
            catalog_owned_by_parent: true,
        })
    );

    // A parent-owned catalog records its settings as external.
    let mut external = spec();
    external["teardownSettings"] = json!({"catalogOwnedByParent": true});
    let unit = desired(external, ExecutionKey::from_digest_of(b"key"));
    let Supported::Supported(settings) = driver.teardown_settings(&unit).unwrap() else {
        panic!("recorded settings are supported");
    };
    assert_eq!(
        (settings.catalog_prunes, settings.application_deletion_policy),
        (None, None)
    );

    let unit = desired(spec(), ExecutionKey::from_digest_of(b"key"));
    assert_eq!(driver.teardown_settings(&unit).unwrap(), Supported::Unsupported);
}

/// Catches a stored `teardownWait` that the wait rule cannot read, such
/// as a delay without a duration, reaching state.
#[test]
fn test_fake_publication_validates_teardown_wait_and_settings() {
    let driver = fake();
    let with = |field: &str, value: Value| {
        let mut spec = spec();
        spec[field] = value;
        driver.validate_spec(&spec)
    };
    for valid in [
        json!({"strategy": "observe"}),
        json!({"strategy": "manual"}),
        json!({"strategy": "delay", "duration": "10m"}),
    ] {
        assert!(with("teardownWait", valid.clone()).is_ok(), "{valid}");
    }
    for invalid in [
        json!({"strategy": "delay"}),
        json!({"strategy": "delay", "duration": "ten minutes"}),
        json!({"strategy": "observe", "duration": "10m"}),
        json!({"strategy": "wait"}),
        json!({"strategy": "manual", "until": "never"}),
        json!("manual"),
    ] {
        assert!(with("teardownWait", invalid.clone()).is_err(), "{invalid}");
    }
    assert!(with("teardownSettings", json!({"catalogPrunes": true})).is_ok());
    assert!(with("teardownSettings", json!({"catalogPrunes": "yes"})).is_err());
    assert!(with("teardownSettings", json!({"prune": true})).is_err());
}
