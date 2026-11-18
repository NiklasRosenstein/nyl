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

/// [`checkout_with`] whose publication unit `kubernetes` observes, so it
/// attests what each deployment runs and takes coverage.
fn observing_checkout_with(world: &World, spec: &str) -> std::path::PathBuf {
    let checkout = checkout_with(world, spec);
    let units = checkout.join("nyl/units.yaml");
    let text = std::fs::read_to_string(&units).unwrap().replace("\r\n", "\n");
    let observed = text.replace(
        "  target: '{{ values.target }}'\n",
        "  target: '{{ values.target }}'\n  observe: {}\n",
    );
    assert_ne!(observed, text, "the publication unit's spec changed shape");
    std::fs::write(&units, observed).unwrap();
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
        // The image driver closes its artifact's schema: a misspelled field
        // or artifact name is caught before any receipt exists.
        (
            "  from: {environment: dev}\n  to: {environment: prod}\n  values:\n    v:\n      select: {unit: web-image, artifact: image, pointer: /referense}\n",
            "NYL-PROMOTE-SELECTOR-INVALID",
            "names no field of artifact image",
        ),
        (
            "  from: {environment: dev}\n  to: {environment: prod}\n  values:\n    v:\n      select: {unit: web-image, artifact: imgae, pointer: /reference}\n",
            "NYL-PROMOTE-SELECTOR-INVALID",
            "publishes no artifact named \"imgae\"",
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
/// `groups` on this value-only path, or any entry of a unit that attests
/// nothing per deployment, is `NYL-PROMOTE-COVERAGE-INEFFECTIVE`.
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
            "NYL-PROMOTE-COVERAGE-INEFFECTIVE",
            "records no attestation of what each of its deployments runs",
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
        let checkout = observing_checkout_with(&world, &format!("{path}{coverage}"));
        nyl(&world, &checkout, &["validate"]).assert().failure().stderr(
            predicate::str::contains(code)
                .and(predicate::str::contains(text))
                .and(predicate::str::contains("dev-to-prod")),
        );
    }
    // The deployments the driver lists pass: the Release's Application and the catalog.
    let checkout = observing_checkout_with(
        &world,
        &format!("{path}  coverage:\n    kubernetes: {{exclude: [web], require: [dev-catalog]}}\n"),
    );
    nyl(&world, &checkout, &["validate"]).assert().success();
}

/// `plan` checks coverage names through the same path as `validate`.
#[test]
fn test_plan_reports_unknown_coverage_names() {
    let world = World::new(&scenario::shell::shared_project());
    let checkout = observing_checkout_with(
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

/// When a coverage unit's target does not render, its names cannot be
/// checked: `nyl validate` fails with `NYL-PROMOTE-COVERAGE-UNCHECKED` and
/// the render error, while `nyl plan` of the target environment, which does
/// not need dev's render, warns and plans (with a warning about prod's
/// uninitialized state, which is not what this test is about). Catches one environment's broken
/// render blocking commands for another.
#[test]
fn test_an_unrenderable_coverage_unit_fails_validate_and_warns_plan() {
    let world = World::new(&scenario::shell::shared_project());
    let checkout = observing_checkout_with(
        &world,
        "  from: {environment: dev}\n  to: {environment: prod}\n  coverage:\n    kubernetes: {exclude: [web]}\n",
    );
    let release = checkout.join("applications/web/release.yaml");
    let text = std::fs::read_to_string(&release).unwrap();
    std::fs::write(
        &release,
        text.replace("{{ inputs.databaseHost }}", "{{ inputs.databaseHost | nope }}"),
    )
    .unwrap();
    nyl(&world, &checkout, &["validate"]).assert().failure().stderr(
        predicate::str::contains("NYL-PROMOTE-COVERAGE-UNCHECKED").and(predicate::str::contains("cannot be listed")),
    );
    // `plan` renders the committed source.
    for args in [
        &["add", "-A"][..],
        &[
            "commit",
            "-q",
            "--no-gpg-sign",
            "-m",
            "Break the web Release's template",
        ],
    ] {
        let status = std::process::Command::new("git")
            .current_dir(&checkout)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(["-c", "user.name=Nyl Test", "-c", "user.email=test@example.invalid"])
            .args(args)
            .status()
            .unwrap();
        assert!(status.success());
    }
    nyl(&world, &checkout, &["plan", "-e", "prod"])
        .assert()
        .success()
        .stderr(predicate::str::contains("NYL-PROMOTE-COVERAGE-UNCHECKED"));
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

/// [`checkout_with`] plus a prod Cluster and DeploymentTarget whose
/// `releaseInputs` are `bindings` (YAML at the Release-key level), placed by
/// a publication unit prod selects, and the extra ApplicationGroup documents
/// `groups`. Commits the result, as `plan` renders the committed source.
fn bound_checkout(world: &World, path: &str, bindings: &str, groups: &str) -> std::path::PathBuf {
    let checkout = checkout_with(world, path);
    let platform = checkout.join("nyl/platform.yaml");
    let mut contents = std::fs::read_to_string(&platform).unwrap();
    contents.push_str(&format!(
        "---\napiVersion: k8s.nyl/v1\nkind: Cluster\nmetadata:\n  name: prod\nspec:\n  destination:\n    server: https://prod.invalid\n  kubernetes:\n    kubeVersion: 1.31.4\n    apiVersions: [v1, apps/v1, v1/Namespace, apps/v1/Deployment, v1/ConfigMap]\n    clusterScopedKinds: [core/Namespace]\n---\napiVersion: k8s.gitops.nyl/v1\nkind: DeploymentTarget\nmetadata:\n  name: prod\nspec:\n  reconciler: {{argocd: {{}}}}\n  clusterRef: {{name: prod}}\n  applicationGroupSelector:\n    matchLabels: {{app: web}}\n  publication:\n    repositoryRef: {{name: platform}}\n    revision: deploy\n    pathPrefix: prod\n  releaseInputs:\n{bindings}{groups}"
    ));
    std::fs::write(&platform, contents).unwrap();
    let units = checkout.join("nyl/units.yaml");
    let mut text = std::fs::read_to_string(&units).unwrap();
    text.push_str("---\napiVersion: units.gitops.nyl/v1\nkind: KubernetesPublication\nmetadata:\n  name: prod-kubernetes\n  labels: {prod: 'true'}\nspec:\n  target: prod\n  timeout: 10m\n");
    std::fs::write(&units, text).unwrap();
    git(&checkout, &["add", "-A"]);
    git(
        &checkout,
        &["commit", "-q", "--no-gpg-sign", "-m", "Bind prod's Release inputs"],
    );
    checkout
}

/// Run Git in `directory` with no user or system configuration.
fn git(directory: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .current_dir(nyl_git::cli_path(directory))
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(["-c", "user.name=Nyl Test", "-c", "user.email=test@example.invalid"])
        .args(args)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

/// A Git repository with a Release `api` declaring the integer input
/// `replicas` beneath `releases/`, and the ApplicationGroup `api` sourcing
/// it at its commit, as a document to append.
fn remote_group(directory: &Path) -> String {
    std::fs::create_dir_all(directory.join("releases")).unwrap();
    std::fs::write(
        directory.join("releases/api.yaml"),
        "apiVersion: k8s.nyl/v1\nkind: Release\nmetadata:\n  name: api\n  namespace: api\nspec:\n  inputs:\n    replicas: {type: integer, default: 1}\n---\napiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: api\n  namespace: api\ndata:\n  replicas: '{{ inputs.replicas }}'\n",
    )
    .unwrap();
    git(directory, &["init", "-q", "-b", "main"]);
    git(directory, &["add", "-A"]);
    git(directory, &["commit", "-q", "--no-gpg-sign", "-m", "Release api"]);
    let commit = String::from_utf8(
        std::process::Command::new("git")
            .current_dir(nyl_git::cli_path(directory))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    let url = reqwest::Url::from_directory_path(directory).unwrap();
    format!(
        "---\napiVersion: k8s.gitops.nyl/v1\nkind: ApplicationGroup\nmetadata:\n  name: api\n  labels: {{app: web}}\nspec:\n  applicationNamespace: argocd\n  source:\n    repository: {{repoURL: '{url}'}}\n    revision: HEAD\n    commit: '{}'\n    path: releases\n",
        commit.trim()
    )
}

/// A promoted value is compared with the declared type of the Release input
/// a DeploymentTarget's `fromPromotion` binding fills, which the Release in
/// the target's selected ApplicationGroup declares: a string fits the
/// string input `image`, and an object does not. Catches the mismatch
/// passing `validate` and surfacing only when prod's publication resolves.
#[test]
fn test_validate_compares_promoted_values_with_release_input_types() {
    let world = World::new(&scenario::shell::shared_project());
    let bindings = "    web/web:\n      image: {fromPromotion: {value: webImage}}\n      databaseHost: {fromPromotion: {value: dbHost}}\n";
    let sound = bound_checkout(&world, SOUND, bindings, "");
    nyl(&world, &sound, &["validate"])
        .assert()
        .success()
        .stdout(predicate::str::contains("GitOps configuration is valid"));
    let object = SOUND.replace("input: /source/path}", "input: /source}");
    let mismatch = bound_checkout(&world, &object, bindings, "");
    nyl(&world, &mismatch, &["validate"]).assert().failure().stderr(
        predicate::str::contains("NYL-PROMOTE-VALUE-TYPE:")
            .and(predicate::str::contains("value \"dbHost\" is a object"))
            .and(predicate::str::contains("/releases/web~1web/databaseHost"))
            .and(predicate::str::contains("accepts a string")),
    );
}

/// A Release input declared in a remote ApplicationGroup source is typed
/// from that source: `validate` fetches it, and `validate --offline` reads
/// it from the cache only, so before any fetch the input is unchecked
/// (`NYL-PROMOTE-VALUE-TYPE-UNCHECKED`, an error in `validate`), and after
/// one the offline check finds the mismatch too. Catches `--offline`
/// reaching the network, and an unreadable declaration passing as checked.
#[test]
fn test_validate_offline_types_remote_inputs_from_the_cache_only() {
    let world = World::new(&scenario::shell::shared_project());
    let remote = world.scenario_tmp().join("api-source");
    let group = remote_group(&remote);
    let bindings = "    web/web:\n      image: {fromPromotion: {value: webImage}}\n    api/api:\n      replicas: {fromPromotion: {value: webImage}}\n";
    let checkout = bound_checkout(&world, SOUND, bindings, &group);
    nyl(&world, &checkout, &["validate", "--offline"])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("NYL-PROMOTE-VALUE-TYPE-UNCHECKED")
                .and(predicate::str::contains("/releases/api~1api/replicas"))
                .and(predicate::str::contains("ApplicationGroup \"api\"")),
        );
    let mismatch = || {
        predicate::str::contains("NYL-PROMOTE-VALUE-TYPE:")
            .and(predicate::str::contains("/releases/api~1api/replicas"))
            .and(predicate::str::contains("accepts a integer"))
            .and(predicate::str::contains("NYL-PROMOTE-VALUE-TYPE-UNCHECKED").not())
    };
    nyl(&world, &checkout, &["validate"])
        .assert()
        .failure()
        .stderr(mismatch());
    nyl(&world, &checkout, &["validate", "--offline"])
        .assert()
        .failure()
        .stderr(mismatch());
}

/// When a remote group's source can be neither fetched nor read from the
/// cache, `validate` refuses the unchecked input while `plan` warns and goes
/// on: resolution still checks the value. Catches one environment's
/// unreachable source blocking its plan.
#[test]
fn test_an_unreadable_remote_input_type_fails_validate_and_warns_plan() {
    let world = World::new(&scenario::shell::shared_project());
    let remote = world.scenario_tmp().join("api-source");
    let group = remote_group(&remote);
    let bindings = "    api/api:\n      replicas: {fromPromotion: {value: webImage}}\n";
    let checkout = bound_checkout(&world, SOUND, bindings, &group);
    std::fs::remove_dir_all(&remote).unwrap();
    nyl(&world, &checkout, &["validate"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("NYL-PROMOTE-VALUE-TYPE-UNCHECKED"));
    let output = nyl(&world, &checkout, &["plan", "-e", "prod"]).output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    // A warning, beside prod's uninitialized state, which is not what this test is about.
    assert!(
        stderr.contains("warning  PromotionPath dev-to-prod value \"webImage\"")
            && stderr.contains("NYL-PROMOTE-VALUE-TYPE-UNCHECKED"),
        "{stderr}"
    );
    assert!(!stderr.contains("PromotionPaths are invalid"), "{stderr}");
}

/// `validate --offline` lists a coverage unit's deployments without the
/// network too: with dev's target selecting a remote ApplicationGroup that
/// nothing cached yet, the listing fails (`NYL-PROMOTE-COVERAGE-UNCHECKED`)
/// even though the remote is reachable; once `validate` cached it, the
/// offline listing succeeds after the remote is gone. Catches the coverage
/// listing fetching sources offline, or never finding the cached copy.
#[test]
fn test_validate_offline_lists_coverage_from_the_cache_only() {
    let world = World::new(&scenario::shell::shared_project());
    let remote = world.scenario_tmp().join("api-source");
    let group = remote_group(&remote);
    let checkout = observing_checkout_with(
        &world,
        "  from: {environment: dev}\n  to: {environment: prod}\n  coverage:\n    kubernetes: {exclude: [web], require: [api]}\n",
    );
    let platform = checkout.join("nyl/platform.yaml");
    let mut contents = std::fs::read_to_string(&platform).unwrap();
    contents.push_str(&group);
    std::fs::write(&platform, contents).unwrap();
    nyl(&world, &checkout, &["validate", "--offline"])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("NYL-PROMOTE-COVERAGE-UNCHECKED")
                .and(predicate::str::contains("cannot be listed")),
        );
    nyl(&world, &checkout, &["validate"]).assert().success();
    std::fs::remove_dir_all(&remote).unwrap();
    nyl(&world, &checkout, &["validate", "--offline"]).assert().success();
}
