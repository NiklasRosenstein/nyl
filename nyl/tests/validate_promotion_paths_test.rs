//! PromotionPaths through the command line: `nyl validate` and `nyl plan`
//! report each failure with its stable code and fix, and `nyl get
//! promotion-paths` lists the declarations. The rule itself is tested in
//! `nyl_orchestration::rules::promotion_paths`; these tests cover the
//! composition: a project's resources, rendered units, and discovery.

mod scenario;

use std::path::Path;

use assert_cmd::Command;
use predicates::prelude::*;
use scenario::world::World;

fn nyl(world: &World, checkout: &Path, args: &[&str]) -> Command {
    let mut command = Command::cargo_bin("nyl").unwrap();
    command
        .current_dir(checkout)
        .args(args)
        .env("NYL_CACHE_DIR", world.scenario_tmp().join("cache"))
        .env("NO_COLOR", "1");
    command
}

/// The walkthroughs' project (one Environment, `dev`) plus a second one,
/// `prod`, and the PromotionPath `spec`.
fn checkout_with(world: &World, spec: &str) -> std::path::PathBuf {
    let checkout = world.fresh_clone("main");
    let platform = checkout.join("nyl/platform.yaml");
    let mut contents = std::fs::read_to_string(&platform).unwrap().replace("\r\n", "\n");
    contents.push_str(
        "---\napiVersion: gitops.nyl/v1\nkind: Environment\nmetadata:\n  name: prod\nspec:\n  unitSelector:\n    matchLabels: {prod: 'true'}\n  state:\n    repositoryRef: {name: platform}\n",
    );
    contents.push_str(&format!(
        "---\napiVersion: gitops.nyl/v1\nkind: PromotionPath\nmetadata:\n  name: dev-to-prod\nspec:\n{spec}"
    ));
    std::fs::write(&platform, contents).unwrap();
    checkout
}

const SOUND: &str = "  from: {environment: dev}\n  to: {environment: prod}\n  values:\n    webImage:\n      select: {unit: web-image, artifact: image, pointer: /reference}\n    dbHost:\n      select: {unit: database, input: /source/path}\n";

#[test]
fn test_validate_accepts_a_sound_path() {
    let world = World::new(&scenario::shell::shared_project());
    let checkout = checkout_with(&world, SOUND);
    nyl(&world, &checkout, &["validate"])
        .assert()
        .success()
        .stdout(predicate::str::contains("GitOps configuration is valid"));
}

/// Each failure reaches the user with its code, the path's name, and a fix.
#[test]
fn test_validate_reports_each_failure_with_its_code_and_fix() {
    let world = World::new(&scenario::shell::shared_project());
    let cases = [
        (
            "  from: {environment: staging}\n  to: {environment: prod}\n",
            "NYL-PROMOTE-ENVIRONMENT-UNKNOWN",
            "declare an Environment named staging",
        ),
        (
            "  from: {environment: dev}\n  to: {environment: prod}\n  changeGate: pullRequest\n",
            "NYL-PROMOTE-UNSUPPORTED",
            "M11",
        ),
        (
            "  from: {environment: dev}\n  to: {target: production}\n",
            "NYL-PROMOTE-UNSUPPORTED",
            "M11",
        ),
        (
            "  from: {environment: dev}\n  to: {environment: prod}\n  values:\n    v:\n      select: {unit: ghost, input: /source}\n",
            "NYL-PROMOTE-SELECTOR-UNIT",
            "no unit named ghost",
        ),
        (
            "  from: {environment: dev}\n  to: {environment: prod}\n  values:\n    v:\n      select: {unit: database, input: /nope}\n",
            "NYL-PROMOTE-SELECTOR-INVALID",
            "resolves neither",
        ),
        (
            "  from: {environment: dev}\n  to: {environment: prod}\n  evidence: attested\n  attestations: {environment: [release]}\n",
            "NYL-ATTESTATION-UNDECLARED",
            "release",
        ),
    ];
    for (spec, code, text) in cases {
        let checkout = checkout_with(&world, spec);
        nyl(&world, &checkout, &["validate"]).assert().failure().stderr(
            predicate::str::contains(code)
                .and(predicate::str::contains(text))
                .and(predicate::str::contains("dev-to-prod")),
        );
    }
}

/// `plan` reports the findings of the paths that touch its environment.
#[test]
fn test_plan_reports_the_findings_of_the_paths_touching_the_environment() {
    let world = World::new(&scenario::shell::shared_project());
    let checkout = checkout_with(&world, "  from: {environment: staging}\n  to: {environment: dev}\n");
    nyl(&world, &checkout, &["plan", "-e", "dev"])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("NYL-PROMOTE-ENVIRONMENT-UNKNOWN").and(predicate::str::contains("dev-to-prod")),
        );
}

#[test]
fn test_get_promotion_paths_lists_declarations() {
    let world = World::new(&scenario::shell::shared_project());
    let checkout = checkout_with(&world, SOUND);
    nyl(&world, &checkout, &["get", "promotion-paths"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("dev-to-prod")
                .and(predicate::str::contains("dev"))
                .and(predicate::str::contains("prod"))
                .and(predicate::str::contains("dbHost,webImage"))
                .and(predicate::str::contains("published")),
        );
}
