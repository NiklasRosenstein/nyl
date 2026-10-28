//! `nyl validate` of a project with orchestration units, in the scenario test
//! world with the walkthroughs' shared project: a DeploymentTarget that an
//! environment's publication unit places compiles with placeholders for its
//! `fromUnit` bindings, which are checked against that environment's units;
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
        predicate::str::contains("spec.releaseInputs.\"web/web\".image.fromUnit needs orchestrated execution").and(
            predicate::str::contains("spec.releaseInputs.\"web/web\".databaseHost.fromUnit"),
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
            "spec.releaseInputs.\"web/web\".databaseHost.fromUnit names output port of unit database, which the unit \
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
            "kind: Command\nmetadata:\n  name: reader\n  labels: {dev: 'true'}\nspec:\n  command: [./read.sh]\n  values:\n    h: {fromUnit: {unit: network, output: missing}}\n",
            "NYL-REFERENCE-INVALID",
        ),
        (
            "kind: Command\nmetadata:\n  name: waiter\n  labels: {dev: 'true'}\nspec:\n  command: [./wait.sh]\n  dependsOn: [{unit: network, evidence: attested}]\n",
            "NYL-ATTESTATIONS-UNSUPPORTED",
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
