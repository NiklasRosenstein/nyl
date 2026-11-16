use std::sync::Arc;

use nyl_core::constants::API_VERSION_UNITS;
use nyl_core::resources::units::{UnitEnvelope, UnitMetadata};
use serde_json::{json, Value};

use super::*;
use crate::rules::fixtures::{commit, TestDriver};

fn drivers() -> DriverSet {
    let mut drivers = DriverSet::new();
    for kind in ["Test", "Typed"] {
        drivers.register(Arc::new(TestDriver { kind }));
    }
    drivers
}

fn unit(kind: &str, selected: bool, spec: Value) -> RenderedUnit {
    RenderedUnit {
        envelope: UnitEnvelope {
            api_version: API_VERSION_UNITS.to_owned(),
            kind: kind.to_owned(),
            metadata: UnitMetadata {
                name: "unused".to_owned(),
                labels: BTreeMap::new(),
            },
        },
        source_path: "/nyl/units.yaml".to_owned(),
        selected,
        spec: Some(spec),
    }
}

fn source(name: &str, attestations: &[&str], units: Vec<(&str, RenderedUnit)>) -> RenderedSource {
    RenderedSource {
        repositories: BTreeMap::new(),
        source_commit: commit(&"a".repeat(40)),
        project_dir: "/nyl".to_owned(),
        environment: serde_json::from_value(json!({
            "apiVersion": "gitops.nyl/v1", "kind": "Environment", "metadata": {"name": name},
            "spec": {"attestations": attestations}
        }))
        .unwrap(),
        instance: None,
        units: units
            .into_iter()
            .map(|(unit, rendered)| (nyl_core::orchestration::ids::UnitName::new(unit).unwrap(), rendered))
            .collect(),
    }
}

#[allow(clippy::needless_pass_by_value)] // Builders take the literal under test.
fn path(spec: Value) -> PromotionPath {
    let path: PromotionPath = serde_json::from_value(json!({
        "apiVersion": "gitops.nyl/v1", "kind": "PromotionPath",
        "metadata": {"name": "dev-to-prod"}, "spec": spec
    }))
    .unwrap();
    path.validate().unwrap();
    path
}

fn environment(name: &str) -> EnvironmentName {
    EnvironmentName::new(name).unwrap()
}

/// dev runs `web` (a typed kind), `db` (declaring outputs and attestations), and
/// an unselected `scratch`; prod runs `web` and `db`.
struct World {
    declared: BTreeSet<EnvironmentName>,
    sources: BTreeMap<EnvironmentName, RenderedSource>,
}

fn world() -> World {
    let web = || {
        unit(
            "Typed",
            true,
            json!({"replicas": 2, "source": {"path": "/app"}, "image": {"fromUnit": {"unit": "x", "artifact": "image"}}}),
        )
    };
    let db = || {
        unit(
            "Test",
            true,
            json!({
                "declares": ["healthy", "smoke"],
                "outputs": {"host": {"type": "string"}, "port": {"type": "integer"}, "password": {"type": "string", "sensitive": true}}
            }),
        )
    };
    let dev = source(
        "dev",
        &["qa"],
        vec![
            ("web", web()),
            ("db", db()),
            ("scratch", unit("Test", false, json!({}))),
        ],
    );
    let prod = source("prod", &[], vec![("web", web()), ("db", db())]);
    World {
        declared: [environment("dev"), environment("prod")].into(),
        sources: [(environment("dev"), dev), (environment("prod"), prod)].into(),
    }
}

fn run(world: &World, paths: &[PromotionPath], readers: &[ValueReader]) -> Vec<Finding> {
    check(&PathChecks {
        paths,
        declared: &world.declared,
        sources: &world.sources,
        drivers: &drivers(),
        readers,
    })
}

#[allow(clippy::needless_pass_by_value)] // Builders take the literal under test.
fn base(values: Value) -> Value {
    json!({"from": {"environment": "dev"}, "to": {"environment": "prod"}, "values": values})
}

fn codes(findings: &[Finding]) -> Vec<Code> {
    findings.iter().map(|finding| finding.code).collect()
}

fn reader(environment: Option<&str>, path: Option<&str>, value: &str, expected: Option<ValueType>) -> ValueReader {
    ValueReader {
        location: "unit web of Environment prod at \"/replicas\"".to_owned(),
        environment: environment.map(environment_name),
        binding: PromotionInputSource {
            value: value.to_owned(),
            path: path.map(str::to_owned),
        },
        expected,
    }
}

fn environment_name(name: &str) -> EnvironmentName {
    environment(name)
}

/// A path that names declared environments, selected units, resolvable
/// pointers, and declared attestations has no findings.
#[test]
fn test_a_sound_path_has_no_findings() {
    let paths = [path(json!({
        "from": {"environment": "dev"}, "to": {"environment": "prod"}, "evidence": "attested",
        "attestations": {"units": {"db": ["smoke"]}, "environment": ["qa"]},
        "values": {
            "replicas": {"select": {"unit": "web", "input": "/replicas"}},
            "host": {"select": {"unit": "db", "output": "host"}},
            "image": {"select": {"unit": "web", "artifact": "image", "pointer": "/reference"}}
        }
    }))];
    let findings = run(&world(), &paths, &[]);
    assert!(findings.is_empty(), "{findings:?}");
}

/// The source and the target must be declared Environments, and an instance
/// of a template is not one; the finding names the path.
#[test]
fn test_unknown_source_or_target_environment() {
    let mut world = world();
    let paths = [path(
        json!({"from": {"environment": "qa"}, "to": {"environment": "preview-1"}}),
    )];
    // `preview-1` is rendered (an instance) but not declared.
    let preview = world.sources[&environment("prod")].clone();
    world.sources.insert(environment("preview-1"), preview);
    let findings = run(&world, &paths, &[]);
    assert_eq!(
        codes(&findings),
        [Code::PromoteEnvironmentUnknown, Code::PromoteEnvironmentUnknown]
    );
    assert!(
        findings[0].message.contains("dev-to-prod") && findings[0].message.contains("\"qa\""),
        "{}",
        findings[0]
    );
    assert!(findings[0].fix.contains("declare an Environment named qa"));
    assert!(
        findings[1].message.contains("instance of an EnvironmentTemplate"),
        "{}",
        findings[1]
    );
}

/// `changeGate: pullRequest` and a target endpoint are unsupported in M6.
#[test]
fn test_unsupported_forms_are_reported_naming_m11() {
    let paths = [path(
        json!({"from": {"environment": "dev"}, "to": {"target": "production"}, "changeGate": "pullRequest"}),
    )];
    let findings = run(&world(), &paths, &[]);
    let unsupported = findings
        .iter()
        .filter(|finding| finding.code == Code::PromoteUnsupported)
        .count();
    assert_eq!(unsupported, 2, "{findings:?}");
    assert!(
        findings.iter().all(|finding| finding.message.contains("M11")),
        "{findings:?}"
    );
}

/// A selector names a unit the source environment selects: not an unselected
/// one, a missing one, or a disabled one.
#[test]
fn test_selector_unit_must_be_selected_by_the_source() {
    let mut world = world();
    world.sources.get_mut(&environment("dev")).unwrap().units.insert(
        nyl_core::orchestration::ids::UnitName::new("off").unwrap(),
        unit("Test", true, json!({"enabled": false})),
    );
    let paths = [path(base(json!({
        "a": {"select": {"unit": "scratch", "input": "/x"}},
        "b": {"select": {"unit": "ghost", "input": "/x"}},
        "c": {"select": {"unit": "off", "input": "/x"}}
    })))];
    let findings = run(&world, &paths, &[]);
    assert_eq!(codes(&findings), [Code::PromoteSelectorUnit; 3]);
    assert!(
        findings[0].message.contains("not selected by Environment dev"),
        "{}",
        findings[0]
    );
    assert!(findings[1].message.contains("no unit named ghost"), "{}", findings[1]);
    assert!(findings[2].message.contains("disabled"), "{}", findings[2]);
}

/// A pointer resolves in the rendered spec, or in the kind's schema when the
/// spec omits an optional field; one in neither is a finding. Output
/// selectors need a declared, non-sensitive output.
#[test]
fn test_pointers_resolve_against_the_spec_or_the_kind_schema() {
    let mut world = world();
    // `replicas` is optional in the schema: this web omits it.
    world.sources.get_mut(&environment("dev")).unwrap().units.insert(
        nyl_core::orchestration::ids::UnitName::new("lean").unwrap(),
        unit("Typed", true, json!({})),
    );
    let paths = [path(base(json!({
        "rendered": {"select": {"unit": "web", "input": "/source/path"}},
        "schema": {"select": {"unit": "lean", "input": "/replicas"}},
        "common": {"select": {"unit": "lean", "input": "/timeout"}},
        "missing": {"select": {"unit": "web", "input": "/replica"}},
        "deep": {"select": {"unit": "lean", "input": "/source/nope/deeper"}},
        "free": {"select": {"unit": "db", "input": "/anything/at/all"}},
        "undeclared": {"select": {"unit": "db", "output": "nothing"}},
        "sensitive": {"select": {"unit": "db", "output": "password"}}
    })))];
    let findings = run(&world, &paths, &[]);
    let messages = findings
        .iter()
        .map(|finding| finding.message.as_str())
        .collect::<Vec<_>>();
    assert_eq!(codes(&findings), [Code::PromoteSelectorInvalid; 3], "{messages:?}");
    assert!(
        messages[0].contains("\"/replica\"") && messages[0].contains("kind Typed"),
        "{messages:?}"
    );
    // `deep` passes: the schema's `source` object does not close itself.
    assert!(
        messages.iter().any(|m| m.contains("declares no output \"nothing\"")),
        "{messages:?}"
    );
    assert!(messages.iter().any(|m| m.contains("sensitive")), "{messages:?}");
}

/// A value's type must fit every binding that reads it: an integer fits an
/// integer and a number place, not a string; an unknown type fits anything.
#[test]
fn test_value_types_must_fit_every_reading_binding() {
    let paths = [path(base(json!({
        "replicas": {"select": {"unit": "web", "input": "/replicas"}},
        "image": {"select": {"unit": "web", "artifact": "image", "pointer": "/reference"}}
    })))];
    let world = world();
    let readers = [
        reader(Some("prod"), Some("dev-to-prod"), "replicas", Some(ValueType::Integer)),
        reader(Some("prod"), None, "replicas", Some(ValueType::Number)),
        reader(Some("prod"), None, "replicas", Some(ValueType::String)),
        reader(None, Some("dev-to-prod"), "image", Some(ValueType::Boolean)),
    ];
    let findings = run(&world, &paths, &readers);
    assert_eq!(codes(&findings), [Code::PromoteValueType], "{findings:?}");
    assert!(
        findings[0].message.contains("is a integer, but") && findings[0].message.contains("accepts a string"),
        "{}",
        findings[0]
    );
    assert!(findings[0].fix.contains("type string"), "{}", findings[0]);
}

/// A path's `attestations` scopes name units it covers and attestations the
/// scope declares; each failure is `NYL-ATTESTATION-UNDECLARED` or a unit finding.
#[test]
fn test_attestation_scopes_must_be_declared() {
    let paths = [path(json!({
        "from": {"environment": "dev"}, "to": {"environment": "prod"}, "evidence": "attested",
        "attestations": {"units": {"db": ["healthy", "ghost"], "scratch": [], "stranger": []}, "environment": ["qa", "release"]},
        "values": {}
    }))];
    let findings = run(&world(), &paths, &[]);
    assert_eq!(
        codes(&findings),
        [
            Code::AttestationUndeclared,
            Code::PromoteSelectorUnit,
            Code::PromoteSelectorUnit,
            Code::AttestationUndeclared,
        ],
        "{findings:?}"
    );
    assert!(
        findings[0].message.contains("\"ghost\"") && findings[0].message.contains("healthy, smoke"),
        "{}",
        findings[0]
    );
    assert!(
        findings[3].message.contains("\"release\"") && findings[3].message.contains("qa"),
        "{}",
        findings[3]
    );
}

/// A `fromPromotion` binding names a path and a value that exist.
#[test]
fn test_bindings_name_declared_paths_and_values() {
    let paths = [path(base(
        json!({"replicas": {"select": {"unit": "web", "input": "/replicas"}}}),
    ))];
    let readers = [
        reader(Some("prod"), Some("dev-to-prod"), "replicas", None),
        reader(Some("prod"), None, "replicas", None),
        reader(Some("prod"), Some("dev-to-prod"), "nope", None),
        reader(Some("prod"), Some("other-path"), "replicas", None),
        reader(Some("dev"), None, "replicas", None),
    ];
    let findings = run(&world(), &paths, &readers);
    assert_eq!(codes(&findings), [Code::PromoteBindingUnknown; 3], "{findings:?}");
    assert!(
        findings[0].message.contains("declares no such value"),
        "{}",
        findings[0]
    );
    assert!(
        findings[1].message.contains("other-path") && findings[1].message.contains("dev-to-prod"),
        "{}",
        findings[1]
    );
    assert!(
        findings[2].message.contains("no PromotionPath into its environment"),
        "{}",
        findings[2]
    );
}

/// Bindings in unit specs are found at any depth, with the type the kind's
/// schema gives the field they replace.
#[test]
fn test_unit_readers_find_bindings_with_the_type_their_field_accepts() {
    let spec = json!({
        "replicas": {"fromPromotion": {"value": "replicas", "path": "dev-to-prod"}},
        "extra": {"list": [{"fromPromotion": {"value": "other"}}]}
    });
    let sources = BTreeMap::from([(
        environment("prod"),
        source("prod", &[], vec![("web", unit("Typed", true, spec))]),
    )]);
    let readers = unit_readers(&sources, &drivers());
    assert_eq!(readers.len(), 2);
    let replicas = readers
        .iter()
        .find(|reader| reader.binding.value == "replicas")
        .unwrap();
    assert_eq!(replicas.binding.path.as_deref(), Some("dev-to-prod"));
    assert_eq!(replicas.expected, Some(ValueType::Integer));
    assert_eq!(replicas.environment, Some(environment("prod")));
    let nested = readers.iter().find(|reader| reader.binding.value == "other").unwrap();
    assert_eq!(
        nested.expected, None,
        "a field the schema does not know has no type to check"
    );
    assert!(nested.location.contains("/extra/list/0"));
}
