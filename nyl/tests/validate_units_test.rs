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
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/scenarios/walkthroughs/_project")
}

/// A fresh clone of the shared project. Its Cluster gains the kind scopes
/// `nyl capture cluster` would record, so the target's generated AppProject
/// can be derived.
fn checkout(world: &World) -> std::path::PathBuf {
    let checkout = world.fresh_clone("main");
    let platform = checkout.join("nyl/platform.yaml");
    let contents = std::fs::read_to_string(&platform).unwrap();
    let patched = contents.replace(
        "    apiVersions: [v1, apps/v1]\n",
        "    apiVersions: [v1, apps/v1, v1/Namespace, apps/v1/Deployment]\n    clusterScopedKinds: [core/Namespace]\n",
    );
    assert_ne!(patched, contents, "the shared project's Cluster lists its API versions");
    std::fs::write(&platform, patched).unwrap();
    checkout
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
    let checkout = checkout(&world);
    nyl_validate(&world, &checkout)
        .assert()
        .success()
        .stdout(predicate::str::contains("GitOps configuration is valid").and(predicate::str::contains("4 unit(s)")));
}

#[test]
fn test_validate_rejects_from_unit_on_a_target_without_a_publication_unit() {
    let world = World::new(&project());
    let checkout = checkout(&world);
    let units = checkout.join("nyl/units.yaml");
    let contents = std::fs::read_to_string(&units).unwrap();
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
    let checkout = checkout(&world);
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
