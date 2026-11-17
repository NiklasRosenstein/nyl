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
        known: paths,
        only: None,
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

fn reader(environment: &str, path: Option<&str>, value: &str, expected: Option<ValueType>) -> ValueReader {
    ValueReader {
        location: format!("unit web of Environment {environment} at \"/replicas\""),
        environment: environment_name(environment),
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
        reader("prod", Some("dev-to-prod"), "replicas", Some(ValueType::Integer)),
        reader("prod", None, "replicas", Some(ValueType::Number)),
        reader("prod", None, "replicas", Some(ValueType::String)),
        reader("prod", Some("dev-to-prod"), "image", Some(ValueType::Boolean)),
    ];
    let findings = run(&world, &paths, &readers);
    assert_eq!(
        codes(&findings),
        [Code::PromoteValueType, Code::PromoteValueType],
        "{findings:?}"
    );
    assert!(
        findings[0].message.contains("is a integer, but") && findings[0].message.contains("accepts a string"),
        "{}",
        findings[0]
    );
    assert!(findings[0].fix.contains("type string"), "{}", findings[0]);
    // The image's `/reference` is a string, as its kind declares the artifact.
    assert!(
        findings[1].message.contains("\"image\" is a string, but") && findings[1].message.contains("accepts a boolean"),
        "{}",
        findings[1]
    );
}

/// A selector's pointer into an artifact is typed by the schema its
/// producing kind declares for that artifact (`Driver::artifact_schema`):
/// a field it lacks is an invalid selector, and an artifact the kind does
/// not describe is checked only once a receipt is read. Catches an image
/// field bound where an integer belongs, and a typo in an artifact pointer,
/// both found only after a promotion was recorded.
#[test]
fn test_artifact_pointers_are_typed_by_the_producing_kind() {
    let paths = [path(base(json!({
        "size": {"select": {"unit": "web", "artifact": "image", "pointer": "/size"}},
        "typo": {"select": {"unit": "web", "artifact": "image", "pointer": "/referense"}},
        "other": {"select": {"unit": "web", "artifact": "sbom", "pointer": "/anything"}},
        "untyped": {"select": {"unit": "db", "artifact": "image", "pointer": "/anything"}}
    })))];
    let readers = [
        reader("prod", None, "size", Some(ValueType::String)),
        reader("prod", None, "other", Some(ValueType::String)),
        reader("prod", None, "untyped", Some(ValueType::Integer)),
    ];
    let findings = run(&world(), &paths, &readers);
    assert_eq!(
        codes(&findings),
        [Code::PromoteSelectorInvalid, Code::PromoteValueType],
        "{findings:?}"
    );
    assert!(findings[0].message.contains("\"/referense\""), "{}", findings[0]);
    assert!(findings[1].message.contains("\"size\" is a integer"), "{}", findings[1]);
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
        reader("prod", Some("dev-to-prod"), "replicas", None),
        reader("prod", None, "replicas", None),
        reader("prod", Some("dev-to-prod"), "nope", None),
        reader("prod", Some("other-path"), "replicas", None),
        reader("dev", None, "replicas", None),
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
        findings[2]
            .message
            .contains("no PromotionPath into Environment dev declares"),
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
    assert_eq!(replicas.environment, environment("prod"));
    let nested = readers.iter().find(|reader| reader.binding.value == "other").unwrap();
    assert_eq!(
        nested.expected, None,
        "a field the schema does not know has no type to check"
    );
    assert!(nested.location.contains("/extra/list/0"));
}

fn named(name: &str, from: &str, to: &str, values: &[&str]) -> PromotionPath {
    let mut path = path(json!({
        "from": {"environment": from}, "to": {"environment": to},
        "values": values.iter().map(|value| ((*value).to_owned(), json!({"select": {"unit": "web", "input": "/replicas"}}))).collect::<serde_json::Map<_, _>>()
    }));
    path.metadata.name = name.to_owned();
    path
}

/// A chain dev -> staging -> prod: staging reads its own path, prod reads the
/// next one. Catches checking a binding against only the paths of the
/// environment being planned.
#[test]
fn test_a_chain_resolves_every_binding_against_all_paths() {
    let mut world = world();
    let staging = world.sources[&environment("prod")].clone();
    world.sources.insert(environment("staging"), staging);
    world.declared.insert(environment("staging"));
    let paths = [
        named("dev-to-staging", "dev", "staging", &["replicas"]),
        named("staging-to-prod", "staging", "prod", &["replicas"]),
    ];
    let readers = [
        reader("staging", Some("dev-to-staging"), "replicas", None),
        reader("prod", Some("staging-to-prod"), "replicas", None),
        reader("prod", None, "replicas", None),
    ];
    // Only the first path is checked (as `plan -e dev` does), against all of them.
    let findings = check(&PathChecks {
        paths: &paths[..1],
        known: &paths,
        only: None,
        declared: &world.declared,
        sources: &world.sources,
        drivers: &drivers(),
        readers: &readers,
    });
    assert!(findings.is_empty(), "{findings:?}");
}

/// A binding that names a path reads only a path into its own environment.
#[test]
fn test_a_named_path_must_promote_into_the_readers_environment() {
    let mut world = world();
    let staging = world.sources[&environment("prod")].clone();
    world.sources.insert(environment("staging"), staging);
    world.declared.insert(environment("staging"));
    let paths = [
        named("dev-to-staging", "dev", "staging", &["replicas"]),
        named("dev-to-prod", "dev", "prod", &["replicas"]),
    ];
    let findings = run(
        &world,
        &paths,
        &[reader("staging", Some("dev-to-prod"), "replicas", None)],
    );
    assert_eq!(codes(&findings), [Code::PromoteBindingUnknown], "{findings:?}");
    assert!(
        findings[0]
            .message
            .contains("promotes into prod, not into Environment staging"),
        "{}",
        findings[0]
    );
    assert!(findings[0].message.contains("dev-to-staging"), "{}", findings[0]);
}

/// In an environment whose source is promoted from several paths, every
/// source path must define the values path-less bindings read, and only the
/// source paths count; otherwise one path defining it suffices.
#[test]
fn test_every_source_path_defines_what_path_less_bindings_read() {
    let mut world = world();
    let mut prod = world.sources[&environment("prod")].clone();
    prod.environment = serde_json::from_value(json!({
        "apiVersion": "gitops.nyl/v1", "kind": "Environment", "metadata": {"name": "prod"},
        "spec": {"source": {"fromPromotion": {"paths": ["dev-to-prod", "hotfix-to-prod"]}}}
    }))
    .unwrap();
    world.sources.insert(environment("prod"), prod);
    world
        .sources
        .insert(environment("hotfix"), world.sources[&environment("dev")].clone());
    world.declared.insert(environment("hotfix"));
    let paths = [
        named("dev-to-prod", "dev", "prod", &["replicas", "image"]),
        named("hotfix-to-prod", "hotfix", "prod", &["replicas"]),
    ];
    let readers = [
        reader("prod", None, "replicas", None),
        reader("prod", None, "image", None),
    ];
    let findings = run(&world, &paths, &readers);
    assert_eq!(codes(&findings), [Code::PromoteBindingUnknown], "{findings:?}");
    assert!(
        findings[0].message.contains("\"image\"") && findings[0].message.contains("hotfix-to-prod"),
        "{}",
        findings[0]
    );
    assert!(
        findings[0].message.contains("every source path must"),
        "{}",
        findings[0]
    );

    // A path outside the source paths does not count for a path-less binding.
    let outside = [
        named("dev-to-prod", "dev", "prod", &["replicas"]),
        named("hotfix-to-prod", "hotfix", "prod", &["replicas"]),
        named("vendor-to-prod", "dev", "prod", &["vendor"]),
    ];
    let findings = run(&world, &outside, &[reader("prod", None, "vendor", None)]);
    assert_eq!(codes(&findings), [Code::PromoteBindingUnknown], "{findings:?}");
}

/// `source.fromPromotion` names declared paths that promote into the environment.
#[test]
fn test_a_promoted_source_names_paths_into_its_environment() {
    let mut world = world();
    let mut prod = world.sources[&environment("prod")].clone();
    prod.environment = serde_json::from_value(json!({
        "apiVersion": "gitops.nyl/v1", "kind": "Environment", "metadata": {"name": "prod"},
        "spec": {"source": {"fromPromotion": {"paths": ["dev-to-prod", "ghost", "prod-to-dev"]}}}
    }))
    .unwrap();
    world.sources.insert(environment("prod"), prod);
    let paths = [
        named("dev-to-prod", "dev", "prod", &[]),
        named("prod-to-dev", "prod", "dev", &[]),
    ];
    let findings = run(&world, &paths, &[]);
    assert_eq!(codes(&findings), [Code::PromoteBindingUnknown; 2], "{findings:?}");
    assert!(
        findings[0].message.contains("ghost") && findings[0].message.contains("does not declare"),
        "{}",
        findings[0]
    );
    assert!(
        findings[1].message.contains("prod-to-dev") && findings[1].message.contains("promotes into dev"),
        "{}",
        findings[1]
    );
}

/// A malformed selector parses and is reported with its code, whether or not
/// the source environment is declared.
#[test]
fn test_malformed_selectors_are_findings_not_parse_errors() {
    let mut world = world();
    let paths = [path(json!({
        "from": {"environment": "dev"}, "to": {"environment": "prod"},
        "values": {
            "none": {"select": {"unit": "web"}},
            "badPointer": {"select": {"unit": "web", "input": "replicas"}}
        }
    }))];
    let findings = run(&world, &paths, &[]);
    assert_eq!(codes(&findings), [Code::PromoteSelectorInvalid; 2], "{findings:?}");
    assert!(findings[0].message.contains("badPointer") || findings[1].message.contains("badPointer"));
    world.declared.clear();
    let findings = run(&world, &paths, &[]);
    assert_eq!(
        findings
            .iter()
            .filter(|finding| finding.code == Code::PromoteSelectorInvalid)
            .count(),
        2,
        "{findings:?}"
    );
}

/// A command working on one environment checks that environment's bindings
/// only: a broken binding of a partner environment is for its own commands.
#[test]
fn test_only_checks_the_bindings_of_one_environment() {
    let world = world();
    let paths = [named("dev-to-prod", "dev", "prod", &["replicas"])];
    let readers = [reader("prod", None, "ghost", None), reader("dev", None, "ghost", None)];
    let check_for = |only: Option<&EnvironmentName>| {
        check(&PathChecks {
            paths: &paths,
            known: &paths,
            only,
            declared: &world.declared,
            sources: &world.sources,
            drivers: &drivers(),
            readers: &readers,
        })
    };
    assert_eq!(check_for(None).len(), 2);
    let prod = environment("prod");
    let findings = check_for(Some(&prod));
    assert_eq!(codes(&findings), [Code::PromoteBindingUnknown]);
    assert!(findings[0].message.contains("Environment prod"), "{}", findings[0]);
}

/// `plan` lists the values two paths into one environment promote that the
/// supersede check cannot check, because nothing names their source
/// commit: a command output, or a Release input written as a literal. An
/// artifact of a unit built from Git, and a Release input a `fromUnit`
/// reference fills from one, are checked; a value only one path carries is
/// never compared. Catches a plan that stays silent about a value whose
/// supersede check `nyl promote` will skip.
#[test]
fn test_unchecked_lists_values_without_a_source_commit() {
    let mut drivers = drivers();
    drivers.register(Arc::new(TestDriver { kind: "Built" }));
    let dev = source(
        "dev",
        &[],
        vec![
            ("image", unit("Built", true, json!({"source": {"path": "/app"}}))),
            (
                "cmd",
                unit("Test", true, json!({"outputs": {"out": {"type": "string"}}})),
            ),
            (
                "pub",
                unit(
                    "Test",
                    true,
                    json!({"releases": {
                        "web/web": {"image": {"fromUnit": {"unit": "image", "artifact": "image", "pointer": "/reference"}}},
                        "web/api": {"image": "registry.example/api:1"}
                    }}),
                ),
            ),
        ],
    );
    let sources = BTreeMap::from([(environment("dev"), dev)]);
    let values = json!({
        "img": {"select": {"unit": "image", "artifact": "image", "pointer": "/reference"}},
        "out": {"select": {"unit": "cmd", "output": "out"}},
        "web": {"select": {"unit": "pub", "input": "/releases/web~1web/image"}},
        "api": {"select": {"unit": "pub", "input": "/releases/web~1api/image"}},
    });
    let named = |name: &str, values: Value| {
        let mut path = path(json!({"from": {"environment": "dev"}, "to": {"environment": "prod"}, "values": values}));
        path.metadata.name = name.to_owned();
        path
    };
    let paths = [
        named("a", values.clone()),
        named("b", values),
        named("c", json!({"only": {"select": {"unit": "cmd", "output": "out"}}})),
    ];
    let findings = unchecked(&paths, &environment("prod"), &sources, &drivers);
    let listed: Vec<&str> = findings
        .iter()
        .map(|finding| {
            assert_eq!(finding.code, Code::PromoteUnchecked);
            finding.message.split('"').nth(1).unwrap()
        })
        .collect();
    assert_eq!(listed, ["api", "out", "api", "out"], "{findings:?}");
    assert!(unchecked(&paths, &environment("dev"), &sources, &drivers).is_empty());
}
