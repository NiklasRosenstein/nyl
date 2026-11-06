//! The real `KubernetesPublication` driver through the orchestration shell:
//! a reconcile of the walkthrough environment whose publication consumes an
//! image artifact and a database output, and its teardown. Images and
//! infrastructure are the fake kinds; publication is the built-in driver,
//! pushing to the world's `project.git`.
//!
//! Contract: [Kubernetes publication unit](../../design/orchestration-core.md#kubernetes-publication-unit)
//! and [Release inputs](../../design/release-inputs.md).

mod scenario;

use nyl::orchestration::run::{CommandOutcome, Wiring};
use nyl_orchestration::fault::NoFaults;
use nyl_orchestration::shell::run::Cancellation;
use nyl_units::process::ProcessRunner;
use scenario::runner::{invoke_in, AUTHOR};
use scenario::shell::{self as harness, shared_project};
use scenario::world::git_output;
use serde_json::Value;

fn real_publication_harness() -> harness::Harness {
    let mut harness = harness::Harness::new(&shared_project(), true);
    harness.drivers = nyl_units::registry::with_real_publication(&harness.script, nyl::resource_catalogue());
    harness
}

fn invoke(harness: &harness::Harness, args: &[&str]) -> CommandOutcome {
    let tools = ProcessRunner::new();
    let wiring = Wiring {
        clock: harness.clock.clone(),
        ids: &harness.ids,
        drivers: &harness.drivers,
        tools: &tools,
        faults: &NoFaults,
        runner: "test-runner".to_owned(),
        author: Some(AUTHOR.parse().unwrap()),
        process_env: harness.process_env.clone(),
        cancellation: Cancellation::default(),
        prompt: None,
        observer: None,
        progress: None,
    };
    let args = args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
    invoke_in(&harness.world, &wiring, &args, "main", "").unwrap()
}

fn result<'a>(outcome: &'a CommandOutcome, unit: &str) -> &'a Value {
    &outcome.report["environments"][0]["units"][unit]["result"]
}

/// The stored desired document of `unit`, as the state ref holds it.
fn desired(harness: &harness::Harness, unit: &str) -> Value {
    let text = git_output(
        &harness.world.bare(),
        &["show", &format!("nyl/dev/desired:desired/units/{unit}.yaml")],
    );
    serde_saphyr::from_str(&text).unwrap()
}

/// The `Deployment` published under `dev/web/`.
fn published_deployment(harness: &harness::Harness) -> String {
    harness
        .files("refs/heads/deploy")
        .lines()
        .filter(|path| path.starts_with("dev/web/"))
        .map(|path| harness.show("refs/heads/deploy", path))
        .find(|text| text.contains("kind: Deployment"))
        .expect("the Deployment is published")
}

/// End to end: the publication waits for the image build and the database,
/// renders the target with the artifact and output values the shared
/// resolver pinned into its resolved spec, publishes them with their digests
/// in the ownership index, and records the target's teardown settings in the
/// stored document, where `status` reads readiness without rendering. A
/// second reconcile finds the receipt current and publishes nothing. Catches
/// a `fromUnit` value that reaches the render by any other path, a published
/// index that lacks the input provenance, or readiness invented rather than
/// read from the target.
#[test]
fn test_reconcile_publishes_resolved_from_unit_values_and_records_teardown_settings() {
    let harness = real_publication_harness();
    assert_eq!(invoke(&harness, &["state", "init", "-e", "dev"]).exit, 0);
    let reconcile = invoke(&harness, &["reconcile", "-e", "dev", "-o", "json"]);
    assert_eq!(reconcile.exit, 0, "{}\n{}", reconcile.stderr, reconcile.stdout);
    assert_eq!(result(&reconcile, "kubernetes"), "executed", "{}", reconcile.report);

    let unit = desired(&harness, "kubernetes");
    let inputs = &unit["resolvedSpec"]["releases"]["web/web"];
    let image = inputs["image"]
        .as_str()
        .expect("the image artifact's reference is pinned");
    let host = inputs["databaseHost"].as_str().expect("the database output is pinned");
    assert!(image.contains("@sha256:"), "{image}");
    let deployment = published_deployment(&harness);
    assert!(deployment.contains(image) && deployment.contains(host), "{deployment}");

    let index: Value = serde_json::from_str(&harness.show("refs/heads/deploy", "dev/_nyl/index.json")).unwrap();
    for (input, value) in [("image", image), ("databaseHost", host)] {
        assert_eq!(
            index["inputs"][format!("@input/web/web/{input}")],
            nyl_core::bindings::value_digest(&Value::String(value.to_owned())).unwrap()
        );
    }

    // The effective settings of the target's default catalog and Namespaces.
    let settings = &unit["resolvedSpec"]["teardownSettings"];
    assert_eq!(settings["catalogPrunes"], false, "{settings}");
    assert_eq!(settings["applicationDeletionPolicy"], "Foreground", "{settings}");
    assert_eq!(settings["namespaceDeletePolicies"]["web"], "Confirm", "{settings}");
    let status = invoke(&harness, &["status", "-e", "dev", "-o", "json"]);
    let readiness = status.report["environments"][0]["units"]["kubernetes"]["teardownReadiness"].to_string();
    for code in ["NYL-TEARDOWN-CATALOG-MANUAL-SYNC", "NYL-TEARDOWN-NAMESPACE-CONFIRM"] {
        assert!(readiness.contains(code), "{readiness}");
    }

    // `plan` and `reconcile` warn about every teardown requirement the
    // target does not meet and about what a teardown leaves, each with the
    // setting that fixes it, long before a teardown runs.
    for report in [
        &reconcile.report,
        &invoke(&harness, &["plan", "-e", "dev", "-o", "json"]).report,
    ] {
        let warnings = report["environments"][0]["warnings"].as_array().unwrap();
        let codes = warnings
            .iter()
            .map(|warning| warning["code"].as_str().unwrap())
            .filter(|code| code.starts_with("NYL-TEARDOWN-"))
            .collect::<Vec<_>>();
        assert_eq!(
            codes,
            [
                "NYL-TEARDOWN-CATALOG-MANUAL-SYNC",
                "NYL-TEARDOWN-NAMESPACE-CONFIRM",
                "NYL-TEARDOWN-NAMESPACES-REMAIN",
                "NYL-TEARDOWN-CATALOG-REMAINS"
            ],
            "{warnings:?}"
        );
        assert!(warnings
            .iter()
            .all(|warning| warning["fix"].as_str().is_some_and(|fix| !fix.is_empty())));
    }

    let deploy = harness.tip("refs/heads/deploy");
    let again = invoke(&harness, &["reconcile", "-e", "dev", "-o", "json"]);
    assert_eq!(again.exit, 0, "{}", again.stderr);
    assert_eq!(harness.tip("refs/heads/deploy"), deploy);
    assert_ne!(result(&again, "kubernetes"), "executed", "{}", again.report);
}

/// A target that lets its teardown complete, through the real driver: phase 1
/// publishes the catalog without workload Applications and the manual wait
/// exits 2, then `--confirm-removed` removes the owned files and keeps
/// what another tool committed. Catches a recorded setting that blocks a
/// ready target, or a phase that removes files the index does not own.
#[test]
fn test_ready_target_tears_down_in_two_phases() {
    let harness = real_publication_harness();
    let read = |path: &str| {
        std::fs::read_to_string(shared_project().join(path))
            .unwrap()
            .replace("\r\n", "\n")
    };
    let platform = read("nyl/platform.yaml").replace(
        "  publication:\n    repositoryRef: {name: platform}\n",
        "  catalogApplication:\n    syncPolicy: {automated: {prune: true}}\n  publication:\n    repositoryRef: {name: platform}\n",
    );
    let web = read("nyl/web.yaml") + "  namespace: {deletePolicy: Automatic}\n";
    harness.world.commit_files(
        "main",
        "A target that tears down cleanly",
        &[("nyl/platform.yaml", &platform), ("nyl/web.yaml", &web)],
    );
    assert_eq!(invoke(&harness, &["state", "init", "-e", "dev"]).exit, 0);
    let reconcile = invoke(&harness, &["reconcile", "-e", "dev", "-o", "json"]);
    assert_eq!(reconcile.exit, 0, "{}\n{}", reconcile.stderr, reconcile.stdout);
    let settings = &desired(&harness, "kubernetes")["resolvedSpec"]["teardownSettings"];
    assert_eq!(settings["catalogPrunes"], true, "{settings}");
    assert_eq!(settings["namespaceDeletePolicies"]["web"], "Automatic", "{settings}");
    // Only the self-managing catalog is left behind, which warns without blocking.
    let warnings = reconcile.report["environments"][0]["warnings"].to_string();
    assert!(warnings.contains("NYL-TEARDOWN-CATALOG-REMAINS"), "{warnings}");
    for blocking in ["MANUAL-SYNC", "ORPHANED", "NAMESPACE-CONFIRM"] {
        assert!(!warnings.contains(blocking), "{warnings}");
    }

    let phase1 = invoke(
        &harness,
        &["teardown", "-e", "dev", "--unit", "kubernetes", "-o", "json"],
    );
    assert_eq!(phase1.exit, 2, "{}\n{}", phase1.stderr, phase1.stdout);
    assert_eq!(result(&phase1, "kubernetes"), "tearing-down", "{}", phase1.report);
    let files = harness.files("refs/heads/deploy");
    assert!(files.lines().any(|path| path.starts_with("dev/web/")), "{files}");
    assert!(
        files.lines().any(|path| path.starts_with("dev/_nyl/catalog/")),
        "the catalog stays: {files}"
    );

    let confirmed = invoke(
        &harness,
        &[
            "teardown",
            "-e",
            "dev",
            "--unit",
            "kubernetes",
            "--confirm-removed",
            "--reason",
            "checked in Argo CD",
            "-o",
            "json",
        ],
    );
    assert_eq!(confirmed.exit, 0, "{}\n{}", confirmed.stderr, confirmed.stdout);
    assert_eq!(result(&confirmed, "kubernetes"), "removed", "{}", confirmed.report);
    let files = harness.files("refs/heads/deploy");
    assert!(!files.lines().any(|path| path.starts_with("dev/")), "{files}");
}

/// The index the unit published, as the `deploy` branch holds it.
fn deployed_index(harness: &harness::Harness) -> Value {
    serde_json::from_str(&harness.show("refs/heads/deploy", "dev/_nyl/index.json")).unwrap()
}

/// Rewrite the owner of the published index, as another unit incarnation
/// having published the prefix would leave it.
fn rewrite_owner(harness: &harness::Harness, environment: &str, unit: &str, uid: &str) {
    let mut index = deployed_index(harness);
    index["owner"] = serde_json::json!({"environment": environment, "unit": unit, "uid": uid});
    harness.world.commit_files(
        "deploy",
        "Another incarnation published the prefix",
        &[(
            "dev/_nyl/index.json",
            &(serde_json::to_string_pretty(&index).unwrap() + "\n"),
        )],
    );
}

/// Owner fencing through the shell, with the real directory of incarnations
/// read from state: a prefix owned by an incarnation that still exists in its
/// environment's state (here the unit `web-image` of `dev`) refuses the
/// publication, one owned by an environment the source does not declare
/// fails closed, and one owned by an incarnation state no longer holds is
/// taken over. Catches a unit publishing over, or removing, a tree another
/// live unit owns, and a stale owner that blocks a target forever.
#[test]
fn test_owner_fencing_follows_whether_the_owning_incarnation_still_exists_in_state() {
    let harness = real_publication_harness();
    assert_eq!(invoke(&harness, &["state", "init", "-e", "dev"]).exit, 0);
    assert_eq!(invoke(&harness, &["reconcile", "-e", "dev"]).exit, 0);
    let uid = desired(&harness, "kubernetes")["unit"]["uid"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        deployed_index(&harness)["owner"],
        serde_json::json!({"environment": "dev", "unit": "kubernetes", "uid": uid})
    );
    // The default target is not teardown-ready, so each teardown accepts what may remain.
    let teardown = |harness: &harness::Harness| {
        invoke(
            harness,
            &[
                "teardown",
                "-e",
                "dev",
                "--unit",
                "kubernetes",
                "-o",
                "json",
                "--allow-incomplete",
            ],
        )
    };

    let image_uid = desired(&harness, "web-image")["unit"]["uid"]
        .as_str()
        .unwrap()
        .to_owned();
    for (environment, unit, owner_uid, why) in [
        (
            "dev",
            "web-image",
            image_uid.as_str(),
            "still exists in the state of environment dev",
        ),
        ("ghost", "kubernetes", uid.as_str(), "environment ghost is not declared"),
    ] {
        rewrite_owner(&harness, environment, unit, owner_uid);
        let deploy = harness.tip("refs/heads/deploy");
        let fenced = teardown(&harness);
        assert_eq!(fenced.exit, 3, "{}\n{}", fenced.stderr, fenced.stdout);
        let message = fenced.report.to_string();
        assert!(message.contains("NYL-KUBERNETES-OWNER-FENCED"), "{message}");
        assert!(message.contains(why), "{message}");
        assert_eq!(harness.tip("refs/heads/deploy"), deploy, "nothing was published");
    }

    // An incarnation of dev/kubernetes that state does not hold is gone: the
    // unit takes the prefix over, and its teardown (both phases, as it accepts
    // what may remain) removes the files.
    rewrite_owner(&harness, "dev", "kubernetes", "00000000-0000-7000-8000-0000000000aa");
    let taken = teardown(&harness);
    assert_eq!(taken.exit, 0, "{}\n{}", taken.stderr, taken.stdout);
    assert_eq!(result(&taken, "kubernetes"), "removed", "{}", taken.report);
    let files = harness.files("refs/heads/deploy");
    assert!(!files.lines().any(|path| path.starts_with("dev/")), "{files}");
}

/// Commit the walkthrough project with the unit declaring its `dev` target
/// inline (which is then no declared target): a target that tears down
/// cleanly, with a prefix named by its environment. `extra` is appended to
/// the inline target's spec, indented as a child of `inline`.
fn declare_target_inline(harness: &harness::Harness, extra: &str) {
    let read = |path: &str| {
        std::fs::read_to_string(shared_project().join(path))
            .unwrap()
            .replace("\r\n", "\n")
    };
    let platform = read("nyl/platform.yaml");
    let (before, rest) = platform
        .split_once("---\napiVersion: k8s.gitops.nyl/v1\nkind: DeploymentTarget")
        .unwrap();
    let (target, after) = rest.split_once("---\n").unwrap();
    let spec = target.split_once("spec:\n").unwrap().1;
    let inline = spec
        .lines()
        .map(|line| format!("    {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    let platform = format!("{before}---\n{after}");
    let units = read("nyl/units.yaml").replace(
        "  target: '{{ values.target }}'\n",
        &format!(
            "  target:\n    inline:\n{}\n{extra}",
            inline.replace("    pathPrefix: dev", "    pathPrefix: '{{ environment.name }}'")
                + "\n      catalogApplication:\n        syncPolicy: {automated: {prune: true}}"
        ),
    );
    let web = read("nyl/web.yaml") + "  namespace: {deletePolicy: Automatic}\n";
    harness.world.commit_files(
        "main",
        "Declare the target inline",
        &[
            ("nyl/platform.yaml", &platform),
            ("nyl/units.yaml", &units),
            ("nyl/web.yaml", &web),
        ],
    );
}

/// A unit that declares its target inline, end to end: the same compile and
/// publish path as a named target, so it publishes beneath its prefix with
/// its own name and its unit as owner, keeps its key between runs, records
/// its teardown settings, and tears down in two phases. Catches an inline
/// target that renders or publishes unlike a declared one.
#[test]
fn test_inline_target_reconciles_and_tears_down_through_the_shell() {
    let harness = real_publication_harness();
    declare_target_inline(&harness, "");
    assert_eq!(invoke(&harness, &["state", "init", "-e", "dev"]).exit, 0);
    let reconcile = invoke(&harness, &["reconcile", "-e", "dev", "-o", "json"]);
    assert_eq!(reconcile.exit, 0, "{}\n{}", reconcile.stderr, reconcile.stdout);
    assert_eq!(result(&reconcile, "kubernetes"), "executed", "{}", reconcile.report);

    let unit = desired(&harness, "kubernetes");
    assert_eq!(unit["spec"]["target"]["name"], "dev-kubernetes");
    let index = deployed_index(&harness);
    assert_eq!(index["target"], "dev-kubernetes");
    assert_eq!(index["version"], 4);
    assert_eq!(index["owner"]["unit"], "kubernetes");
    let deployment = published_deployment(&harness);
    assert!(deployment.contains("@sha256:"), "{deployment}");
    assert!(unit["resolvedSpec"]["teardownSettings"].is_object(), "{unit}");

    // The same render keeps its key: nothing is published again.
    let deploy = harness.tip("refs/heads/deploy");
    let again = invoke(&harness, &["reconcile", "-e", "dev", "-o", "json"]);
    assert_eq!(again.exit, 0, "{}", again.stderr);
    assert_ne!(result(&again, "kubernetes"), "executed", "{}", again.report);
    assert_eq!(harness.tip("refs/heads/deploy"), deploy);

    // Two phases: the catalog stays until the removal is confirmed.
    let phase1 = invoke(
        &harness,
        &["teardown", "-e", "dev", "--unit", "kubernetes", "-o", "json"],
    );
    assert_eq!(phase1.exit, 2, "{}\n{}", phase1.stderr, phase1.stdout);
    assert!(harness
        .files("refs/heads/deploy")
        .lines()
        .any(|path| path.starts_with("dev/_nyl/catalog/")));
    let confirmed = invoke(
        &harness,
        &[
            "teardown",
            "-e",
            "dev",
            "--unit",
            "kubernetes",
            "--confirm-removed",
            "-o",
            "json",
        ],
    );
    assert_eq!(confirmed.exit, 0, "{}\n{}", confirmed.stderr, confirmed.stdout);
    assert!(!harness
        .files("refs/heads/deploy")
        .lines()
        .any(|path| path.starts_with("dev/")));
}

/// `nyl validate` compiles an inline target with every other target, so a
/// spec that is no DeploymentTarget, or a `fromUnit` binding naming a unit the
/// environment does not select, fails before any run, naming the unit's file.
#[test]
fn test_validate_checks_inline_targets() {
    let harness = real_publication_harness();
    declare_target_inline(&harness, "");
    let validate = |harness: &harness::Harness| {
        let checkout = harness.world.fresh_clone("main");
        let home = checkout.with_extension("home");
        std::fs::create_dir_all(&home).unwrap();
        scenario::nyl_binary(&checkout, &home, &["validate"]).output().unwrap()
    };
    let valid = validate(&harness);
    assert!(valid.status.success(), "{}", String::from_utf8_lossy(&valid.stderr));

    let units = std::fs::read_to_string(shared_project().join("nyl/units.yaml"))
        .unwrap()
        .replace("\r\n", "\n");
    harness.world.commit_files(
        "main",
        "An inline target that is no DeploymentTarget",
        &[(
            "nyl/units.yaml",
            &units.replace(
                "  target: '{{ values.target }}'\n",
                "  target:\n    inline:\n      bogus: true\n",
            ),
        )],
    );
    let invalid = validate(&harness);
    assert!(!invalid.status.success());
    let stderr = String::from_utf8_lossy(&invalid.stderr);
    assert!(
        stderr.contains("bogus") && stderr.contains("dev-kubernetes"),
        "{stderr}"
    );
}

/// The same Release renders with static inputs outside orchestration: a
/// second target of the walkthrough project binds `value` inputs and renders
/// with `render-tree`, where the target its environment's publication owns
/// is refused because its `fromUnit` bindings need orchestrated execution.
/// Catches orchestration changing what a project without it renders.
#[test]
fn test_the_same_release_renders_with_static_inputs_outside_orchestration() {
    let harness = real_publication_harness();
    let platform = std::fs::read_to_string(shared_project().join("nyl/platform.yaml"))
        .unwrap()
        .replace("\r\n", "\n");
    // Its own Cluster keeps the generated Argo CD names apart from dev's.
    let static_target = "---\napiVersion: k8s.nyl/v1\nkind: Cluster\nmetadata:\n  name: static\nspec:\n  destination:\n    server: https://static.example.invalid\n  kubernetes:\n    kubeVersion: 1.31.4\n    apiVersions: [v1, apps/v1, v1/Namespace, apps/v1/Deployment]\n    clusterScopedKinds: [core/Namespace]\n---\napiVersion: k8s.gitops.nyl/v1\nkind: DeploymentTarget\nmetadata:\n  name: static\nspec:\n  clusterRef: {name: static}\n  applicationGroupSelector:\n    matchLabels: {app: web}\n  publication:\n    repositoryRef: {name: platform}\n    revision: static\n    pathPrefix: static\n  releaseInputs:\n    web/web:\n      image: {value: 'registry.example.com/web@sha256:feed'}\n      databaseHost: {value: db.static.internal}\n";
    harness.world.commit_files(
        "main",
        "A target without orchestration",
        &[("nyl/platform.yaml", &(platform + static_target))],
    );
    let checkout = harness.world.fresh_clone("main");
    let home = checkout.with_extension("home");
    std::fs::create_dir_all(&home).unwrap();
    let render = |target: &str, output: &str| {
        scenario::nyl_binary(
            &checkout,
            &home,
            &["render-tree", "--target", target, "--output-dir", output],
        )
        .output()
        .unwrap()
    };

    let rendered = render("static", "out");
    assert!(
        rendered.status.success(),
        "{}",
        String::from_utf8_lossy(&rendered.stderr)
    );
    let deployment = walkdir::WalkDir::new(checkout.join("out/static/web"))
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| std::fs::read_to_string(entry.path()).unwrap())
        .find(|text| text.contains("kind: Deployment"))
        .expect("the Deployment is rendered");
    assert!(deployment.contains("registry.example.com/web@sha256:feed") && deployment.contains("db.static.internal"));

    let refused = render("dev", "refused");
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("fromUnit needs orchestrated execution"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
    assert!(!checkout.join("refused").exists());
}
