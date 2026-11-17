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
        // An undeclared output fails whatever driver serves the kind; pointer
        // resolution against a kind's schema is tested in the rule, with a
        // closed test schema, because test builds' fake drivers keep theirs open.
        (
            "  from: {environment: dev}\n  to: {environment: prod}\n  values:\n    v:\n      select: {unit: database, output: nope}\n",
            "NYL-PROMOTE-SELECTOR-INVALID",
            "declares no output \"nope\"",
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

/// `coverage` is checked against what each named unit runs, as its driver
/// lists it: every unknown name is `NYL-PROMOTE-COVERAGE-UNKNOWN`, and
/// `groups` on this value-only path is `NYL-PROMOTE-COVERAGE-INEFFECTIVE`.
#[test]
fn test_validate_checks_coverage_names_against_the_unit() {
    let world = World::new(&scenario::shell::shared_project());
    let path = "  from: {environment: dev}\n  to: {environment: prod}\n";
    let cases = [
        (
            "  coverage:\n    ghost: {exclude: [web]}\n",
            "NYL-PROMOTE-COVERAGE-UNKNOWN",
            "names unit \"ghost\" in `coverage`",
        ),
        (
            "  coverage:\n    database: {exclude: [web]}\n",
            "NYL-PROMOTE-COVERAGE-UNKNOWN",
            "runs no deployments",
        ),
        (
            "  coverage:\n    kubernetes: {exclude: [nope]}\n",
            "NYL-PROMOTE-COVERAGE-UNKNOWN",
            "deployment \"nope\" in coverage.kubernetes.exclude, but unit kubernetes runs no such deployment (it runs: dev-catalog, web)",
        ),
        (
            "  coverage:\n    kubernetes: {require: [nope]}\n",
            "NYL-PROMOTE-COVERAGE-UNKNOWN",
            "deployment \"nope\" in coverage.kubernetes.require",
        ),
        (
            "  coverage:\n    kubernetes: {groups: [wbe]}\n",
            "NYL-PROMOTE-COVERAGE-UNKNOWN",
            "group \"wbe\" in coverage.kubernetes.groups",
        ),
        (
            "  coverage:\n    kubernetes: {groups: [web]}\n",
            "NYL-PROMOTE-COVERAGE-INEFFECTIVE",
            "promotes only values",
        ),
    ];
    for (coverage, code, text) in cases {
        let checkout = checkout_with(&world, &format!("{path}{coverage}"));
        nyl(&world, &checkout, &["validate"]).assert().failure().stderr(
            predicate::str::contains(code)
                .and(predicate::str::contains(text))
                .and(predicate::str::contains("dev-to-prod")),
        );
    }
    // The deployments the driver lists pass: the Release's Application and the catalog.
    let checkout = checkout_with(
        &world,
        &format!("{path}  coverage:\n    kubernetes: {{exclude: [web], require: [dev-catalog]}}\n"),
    );
    nyl(&world, &checkout, &["validate"]).assert().success();
}

/// `plan` checks coverage names through the same path as `validate`.
#[test]
fn test_plan_reports_unknown_coverage_names() {
    let world = World::new(&scenario::shell::shared_project());
    let checkout = checkout_with(
        &world,
        "  from: {environment: dev}\n  to: {environment: prod}\n  coverage:\n    kubernetes: {require: [nope]}\n",
    );
    nyl(&world, &checkout, &["plan", "-e", "dev"])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("NYL-PROMOTE-COVERAGE-UNKNOWN")
                .and(predicate::str::contains("coverage.kubernetes.require")),
        );
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

/// dev -> staging -> prod: the walkthroughs' project plus two Environments,
/// each with an OpenTofu unit whose `size` variable reads a promoted value
/// of the path into it, and the two paths. `edit` changes the added text.
fn chain(world: &World, edit: impl Fn(String) -> String) -> std::path::PathBuf {
    let checkout = world.fresh_clone("main");
    let platform = checkout.join("nyl/platform.yaml");
    let mut contents = std::fs::read_to_string(&platform).unwrap().replace("\r\n", "\n");
    for name in ["staging", "prod"] {
        contents.push_str(&format!(
            "---\napiVersion: gitops.nyl/v1\nkind: Environment\nmetadata:\n  name: {name}\nspec:\n  unitSelector:\n    matchLabels: {{{name}: 'true'}}\n  values:\n    backendDir: '%SCENARIO_TMP%/tofu'\n  state:\n    repositoryRef: {{name: platform}}\n"
        ));
    }
    let paths = edit(
        "---\napiVersion: gitops.nyl/v1\nkind: PromotionPath\nmetadata: {name: dev-to-staging}\nspec:\n  from: {environment: dev}\n  to: {environment: staging}\n  values:\n    dbSize:\n      select: {unit: database, input: /timeout}\n---\napiVersion: gitops.nyl/v1\nkind: PromotionPath\nmetadata: {name: staging-to-prod}\nspec:\n  from: {environment: staging}\n  to: {environment: prod}\n  values:\n    dbSize:\n      select: {unit: stage-db, input: /timeout}\n"
            .to_owned(),
    );
    contents.push_str(&paths);
    std::fs::write(&platform, contents).unwrap();
    let units = checkout.join("nyl/units.yaml");
    let mut text = std::fs::read_to_string(&units).unwrap().replace("\r\n", "\n");
    for (unit, label, path) in [
        ("stage-db", "staging", "dev-to-staging"),
        ("prod-db", "prod", "staging-to-prod"),
    ] {
        text.push_str(&edit(format!(
            "---\napiVersion: units.gitops.nyl/v1\nkind: OpenTofu\nmetadata:\n  name: {unit}\n  labels: {{{label}: 'true'}}\nspec:\n  source: {{path: ../infra/database}}\n  backend: {{path: '{{{{ values.backendDir }}}}/{unit}.tfstate'}}\n  variables:\n    size: {{fromPromotion: {{path: {path}, value: dbSize}}}}\n  timeout: 60m\n"
        )));
    }
    std::fs::write(&units, text).unwrap();
    // `plan` renders the committed source: commit what the test declared.
    for args in [
        vec!["add", "-A"],
        vec![
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-q",
            "-m",
            "chain",
        ],
    ] {
        let status = std::process::Command::new("git")
            .current_dir(&checkout)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success());
    }
    checkout
}

#[test]
fn test_a_chain_of_three_environments_validates_and_plans_without_false_findings() {
    let world = World::new(&scenario::shell::shared_project());
    let checkout = chain(&world, |text| text);
    nyl(&world, &checkout, &["validate"])
        .assert()
        .success()
        .stdout(predicate::str::contains("GitOps configuration is valid"));
    // `plan -e dev` checks the dev-to-staging path against every declared path:
    // prod's binding of staging-to-prod is not a finding there.
    for environment in ["dev", "staging", "prod"] {
        let output = nyl(&world, &checkout, &["plan", "-e", environment]).output().unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !stderr.contains("NYL-PROMOTE") && !stderr.contains("NYL-ATTESTATION"),
            "{environment}: {stderr}"
        );
    }
}

/// A binding no path supplies is reported once by `validate`, even though a
/// publication unit's target copies bindings into its spec; `plan` reports it
/// for the environment that reads it and for no other.
#[test]
fn test_binding_unknown_is_reported_once_and_only_where_it_applies() {
    let world = World::new(&scenario::shell::shared_project());
    let checkout = chain(&world, |text| {
        text.replace("value: dbSize}}}}", "value: ghost}}}}")
            .replace("value: dbSize}}", "value: ghost}}")
    });
    let output = nyl(&world, &checkout, &["validate"]).output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert_eq!(stderr.matches("NYL-PROMOTE-BINDING-UNKNOWN").count(), 2, "{stderr}");
    assert!(
        stderr.contains("reads value \"ghost\"") && stderr.contains("staging-to-prod"),
        "{stderr}"
    );
    nyl(&world, &checkout, &["plan", "-e", "prod"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("NYL-PROMOTE-BINDING-UNKNOWN"));
    let unrelated = nyl(&world, &checkout, &["plan", "-e", "dev"]).output().unwrap();
    assert!(
        !String::from_utf8_lossy(&unrelated.stderr).contains("NYL-PROMOTE"),
        "dev reads no promoted value"
    );
}

/// A binding naming a path that promotes into another environment is rejected.
#[test]
fn test_a_named_path_into_another_environment_is_rejected() {
    let world = World::new(&scenario::shell::shared_project());
    let checkout = chain(&world, |text| {
        text.replace("path: staging-to-prod, value", "path: dev-to-staging, value")
    });
    nyl(&world, &checkout, &["validate"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "promotes into staging, not into Environment prod",
        ));
}

/// The type a selector reads must fit the field a binding replaces: a string
/// `timeout` cannot become OpenTofu's `source`, which is an object.
#[test]
fn test_value_type_mismatch_is_reported() {
    let world = World::new(&scenario::shell::shared_project());
    let checkout = chain(&world, |text| {
        text        .replace("  source: {path: ../infra/database}\n  backend: {path: '{{ values.backendDir }}/stage-db.tfstate'}", "  source: {fromPromotion: {path: dev-to-staging, value: dbSize}}\n  backend: {path: '{{ values.backendDir }}/stage-db.tfstate'}")
    });
    nyl(&world, &checkout, &["validate"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("NYL-PROMOTE-VALUE-TYPE").and(predicate::str::contains("is a string, but")));
}

/// `reconcile` refuses what `plan` refuses: both call the one session check.
#[test]
fn test_reconcile_refuses_an_invalid_path_like_plan() {
    let world = World::new(&scenario::shell::shared_project());
    let checkout = checkout_with(&world, "  from: {environment: staging}\n  to: {environment: dev}\n");
    // `reconcile` writes state commits, so it needs an identity; a CI runner's
    // clone has none configured.
    let reconcile = ["reconcile", "-e", "dev", "--author", "Nyl Test <test@example.invalid>"];
    for command in [&["plan", "-e", "dev"][..], &reconcile[..]] {
        nyl(&world, &checkout, command)
            .assert()
            .failure()
            .stderr(predicate::str::contains("NYL-PROMOTE-ENVIRONMENT-UNKNOWN"));
    }
}

/// A malformed selector is a coded finding, and `nyl get` still lists the path.
#[test]
fn test_a_malformed_selector_is_a_finding_and_the_path_is_still_listed() {
    let world = World::new(&scenario::shell::shared_project());
    let checkout = checkout_with(
        &world,
        "  from: {environment: dev}\n  to: {environment: prod}\n  values:\n    v:\n      select: {unit: database}\n",
    );
    nyl(&world, &checkout, &["validate"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("NYL-PROMOTE-SELECTOR-INVALID"));
    nyl(&world, &checkout, &["get", "promotion-paths"])
        .assert()
        .success()
        .stdout(predicate::str::contains("dev-to-prod"));
}
