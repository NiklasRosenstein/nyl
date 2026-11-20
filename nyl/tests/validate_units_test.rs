//! `nyl validate` of a project with orchestration units, in the scenario test
//! world with the walkthroughs' shared project: a DeploymentTarget that an
//! environment's publication unit places compiles with placeholders for its
//! `$fromUnit` bindings, which are checked against that environment's units;
//! a target no publication unit places keeps rejecting them.

mod scenario;

use std::path::Path;

use assert_cmd::Command;
use predicates::prelude::*;
use scenario::world::World;

fn project() -> std::path::PathBuf {
    scenario::shell::shared_project()
}

fn nyl_validate(world: &World, checkout: &Path) -> Command {
    let mut command = Command::cargo_bin("nyl").unwrap();
    command
        .current_dir(checkout)
        .arg("validate")
        .env("NYL_CACHE_DIR", world.scenario_tmp().join("cache"))
        // CI forces colour; the assertions read plain text.
        .env("NO_COLOR", "1");
    command
}

#[test]
fn test_validate_accepts_a_target_placed_by_a_publication_unit() {
    let world = World::new(&project());
    let checkout = world.fresh_clone("main");
    nyl_validate(&world, &checkout)
        .assert()
        .success()
        .stdout(predicate::str::contains("GitOps configuration is valid").and(predicate::str::contains("4 unit(s)")));
}

/// Catches validation failing a correct bundle that reads an object input by
/// pointer when the target binds the input with `$fromUnit`: before a run the
/// input holds a placeholder, so what the pointer selects is unknown.
#[test]
fn test_validate_accepts_a_pointer_into_an_input_a_unit_fills() {
    let world = World::new(&project());
    let checkout = world.fresh_clone("main");
    let edit = |path: &str, from: &str, to: &str| {
        let path = checkout.join(path);
        let contents = std::fs::read_to_string(&path).unwrap().replace("\r\n", "\n");
        assert!(contents.contains(from), "{path:?} holds {from:?}");
        std::fs::write(&path, contents.replace(from, to)).unwrap();
    };
    edit(
        "nyl/platform.yaml",
        "image: {$fromUnit: {unit: web-image, artifact: image, pointer: /reference}}",
        "image: {$fromUnit: {unit: web-image, artifact: image}}",
    );
    edit(
        "applications/web/release.yaml",
        "image: {type: string}",
        "image: {type: object}",
    );
    edit(
        "applications/web/release.yaml",
        "image: {$fromInput: image}",
        "image: {$fromInput: {name: image, pointer: /reference}}",
    );

    nyl_validate(&world, &checkout)
        .assert()
        .success()
        .stdout(predicate::str::contains("GitOps configuration is valid"));
}

/// Catches a pointer into a scalar input a unit fills passing validation: a
/// string holds nothing a pointer selects, whatever orchestration delivers.
#[test]
fn test_validate_rejects_a_pointer_into_a_string_input_a_unit_fills() {
    let world = World::new(&project());
    let checkout = world.fresh_clone("main");
    let release = checkout.join("applications/web/release.yaml");
    let contents = std::fs::read_to_string(&release).unwrap().replace("\r\n", "\n");
    let from = "value: {$fromInput: databaseHost}";
    assert!(contents.contains(from));
    std::fs::write(
        &release,
        contents.replace(from, "value: {$fromInput: {name: databaseHost, pointer: /x}}"),
    )
    .unwrap();

    nyl_validate(&world, &checkout)
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "$fromInput reads \"/x\" of Release input \"databaseHost\", which holds nothing there",
        ));
}

#[test]
fn test_validate_rejects_from_unit_on_a_target_without_a_publication_unit() {
    let world = World::new(&project());
    let checkout = world.fresh_clone("main");
    let units = checkout.join("nyl/units.yaml");
    // Windows checkouts may convert line endings.
    let contents = std::fs::read_to_string(&units).unwrap().replace("\r\n", "\n");
    let (kept, publication) = contents
        .split_once("---\napiVersion: units.gitops.nyl/v1\nkind: KubernetesPublication\n")
        .expect("the shared project declares a publication unit");
    assert!(publication.contains("name: kubernetes"));
    std::fs::write(&units, kept).unwrap();

    nyl_validate(&world, &checkout).assert().failure().stderr(
        predicate::str::contains("spec.releaseInputs.\"web/web\".image.$fromUnit needs orchestrated execution").and(
            predicate::str::contains("spec.releaseInputs.\"web/web\".databaseHost.$fromUnit"),
        ),
    );
}

#[test]
fn test_validate_rejects_a_placed_target_whose_unit_reference_is_undeclared() {
    let world = World::new(&project());
    let checkout = world.fresh_clone("main");
    let platform = checkout.join("nyl/platform.yaml");
    let contents = std::fs::read_to_string(&platform).unwrap();
    std::fs::write(&platform, contents.replace("output: host", "output: port")).unwrap();

    nyl_validate(&world, &checkout)
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "spec.releaseInputs.\"web/web\".databaseHost.$fromUnit names output port of unit database, which the unit \
         does not declare",
        ));
}

/// Catches `nyl validate` accepting units that `nyl plan` and `nyl reconcile`
/// reject: they share the state-free unit checks, so an invalid credential
/// declaration, a dependency cycle, and a reference to an output its
/// producer does not declare each fail validation with the run's code.
#[test]
fn test_validate_runs_the_unit_checks_plan_runs() {
    let world = World::new(&project());
    let cases = [
        (
            "kind: Command\nmetadata:\n  name: seed\n  labels: {dev: 'true'}\nspec:\n  command: [./seed.sh]\n  env:\n    secrets: {BAD-NAME: api-token}\n",
            "NYL-UNIT-INVALID",
        ),
        (
            "kind: Command\nmetadata:\n  name: a\n  labels: {dev: 'true'}\nspec:\n  command: [./a.sh]\n  dependsOn: [b]\n---\napiVersion: units.gitops.nyl/v1\nkind: Command\nmetadata:\n  name: b\n  labels: {dev: 'true'}\nspec:\n  command: [./b.sh]\n  dependsOn: [a]\n",
            "NYL-DEPENDENCY-CYCLE",
        ),
        (
            "kind: Command\nmetadata:\n  name: reader\n  labels: {dev: 'true'}\nspec:\n  command: [./read.sh]\n  values:\n    h: {$fromUnit: {unit: network, output: missing}}\n",
            "NYL-REFERENCE-INVALID",
        ),
        (
            "kind: Command\nmetadata:\n  name: waiter\n  labels: {dev: 'true'}\nspec:\n  command: [./wait.sh]\n  dependsOn: [{unit: network, attestations: [healthy]}]\n",
            "NYL-ATTESTATION-UNDECLARED",
        ),
    ];
    for (unit, code) in cases {
        let checkout = world.fresh_clone("main");
        let units = checkout.join("nyl/units.yaml");
        let mut contents = std::fs::read_to_string(&units).unwrap();
        contents.push_str("---\napiVersion: units.gitops.nyl/v1\n");
        contents.push_str(unit);
        std::fs::write(&units, contents).unwrap();
        nyl_validate(&world, &checkout)
            .assert()
            .failure()
            .stderr(predicate::str::contains(code));
    }
}

/// Catches `nyl validate` refusing a dependent that gates on an attestation
/// only `nyl attest --name` supplies: a producer's `spec.attestations` entry
/// with `from: external` is a name a dependent may require, by name or
/// through `evidence: attested`.
#[test]
fn test_validate_accepts_dependents_of_external_attestations() {
    let world = World::new(&project());
    let checkout = world.fresh_clone("main");
    let units = checkout.join("nyl/units.yaml");
    let mut contents = std::fs::read_to_string(&units).unwrap();
    contents.push_str("---\napiVersion: units.gitops.nyl/v1\nkind: Command\nmetadata:\n  name: gate\n  labels: {dev: 'true'}\nspec:\n  command: [./gate.sh]\n  attestations: [{name: qa, from: external}]\n---\napiVersion: units.gitops.nyl/v1\nkind: Command\nmetadata:\n  name: waiter\n  labels: {dev: 'true'}\nspec:\n  command: [./wait.sh]\n  dependsOn: [{unit: gate, attestations: [qa]}, {unit: gate, evidence: attested}]\n");
    std::fs::write(&units, contents).unwrap();
    nyl_validate(&world, &checkout).assert().success();
}

/// Catches a value source an environment's values carry that no unit reads
/// passing silently: it never resolves, so validate warns, and `--strict`
/// fails.
#[test]
fn test_validate_warns_about_a_carried_source_no_unit_reads() {
    let world = World::new(&project());
    let checkout = world.fresh_clone("main");
    let platform = checkout.join("nyl/platform.yaml");
    let contents = std::fs::read_to_string(&platform).unwrap().replace("\r\n", "\n");
    let changed = contents.replace(
        "  values:\n    target: dev\n",
        "  values:\n    target: dev\n    unused: {$fromPromotion: {value: image}}\n",
    );
    assert_ne!(changed, contents, "the shared project's Environment declares values");
    std::fs::write(&platform, changed).unwrap();

    nyl_validate(&world, &checkout)
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "NYL-VALUES-SOURCE-UNREAD: spec.values.unused of Environment dev holds a $fromPromotion source that no unit of the environment reads",
        ));
    nyl_validate(&world, &checkout).arg("--strict").assert().failure();
}
