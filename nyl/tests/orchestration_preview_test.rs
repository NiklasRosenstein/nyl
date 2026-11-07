//! A preview environment end to end with the real `KubernetesPublication`
//! driver: two instances of one EnvironmentTemplate share a state ref and a
//! deploy branch, publish workloads and Argo CD objects under their own
//! prefixes with names that differ per instance, cannot be pushed to
//! directly, and are removed one at a time. Images and infrastructure are the
//! fake kinds.
//!
//! Contract: [Environment templates and previews](../../design/orchestration-core.md#environment-templates-and-previews)
//! and scenario 2 of `design/reference-scenarios.md`.

mod scenario;

use clap::Parser;
use nyl::cli::Cli;
use nyl::orchestration::run::{CommandOutcome, Wiring};
use nyl_orchestration::fault::NoFaults;
use nyl_orchestration::shell::run::Cancellation;
use nyl_units::process::ProcessRunner;
use scenario::runner::{invoke_in, AUTHOR};
use scenario::shell::{self as harness, shared_project};
use serde_json::Value;

/// The walkthrough project with its units also selected by previews, the
/// template with the inline target of its publication, and a catalog that
/// lets teardown complete.
fn preview_project() -> tempfile::TempDir {
    let copy = tempfile::TempDir::new().unwrap();
    for entry in walkdir::WalkDir::new(shared_project()) {
        let entry = entry.unwrap();
        let target = copy.path().join(entry.path().strip_prefix(shared_project()).unwrap());
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(target).unwrap();
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
    let edit = |path: &str, from: &str, to: &str| {
        let file = copy.path().join(path);
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains(from), "{path} holds {from:?}");
        std::fs::write(file, text.replace(from, to)).unwrap();
    };
    for unit in ["network", "database", "web-image", "kubernetes"] {
        edit(
            "nyl/units.yaml",
            &format!("name: {unit}\n  labels: {{dev: 'true'}}"),
            &format!("name: {unit}\n  labels: {{dev: 'true', preview: 'true'}}"),
        );
    }
    // Not YAML before rendering, so the envelope is block style.
    edit(
        "nyl/units.yaml",
        "name: kubernetes\n  labels: {dev: 'true', preview: 'true'}\nspec:\n  target: '{{ values.target }}'",
        "name: kubernetes\n  labels:\n    dev: 'true'\n    preview: 'true'\nspec:\n  target: {{ values.target | tojson }}",
    );
    edit(
        "nyl/platform.yaml",
        "  publication:\n    repositoryRef: {name: platform}\n    revision: deploy",
        "  catalogApplication:\n    syncPolicy: {automated: {prune: true}}\n  publication:\n    repositoryRef: {name: platform}\n    revision: deploy",
    );
    // Names per target, so no instance collides with dev or another instance.
    edit(
        "nyl/web.yaml",
        "spec:\n",
        "spec:\n  namespace: {prunePolicy: Automatic, deletePolicy: Automatic}\n  applicationNameTemplate: '${ target.metadata.name }-${ release.metadata.name }'\n  projectRef: web\n",
    );
    // One project every target shares, kept outside Nyl's output.
    let platform = std::fs::read_to_string(copy.path().join("nyl/platform.yaml")).unwrap();
    std::fs::write(
        copy.path().join("nyl/platform.yaml"),
        format!("{platform}---\napiVersion: k8s.gitops.nyl/v1\nkind: AppProjectDefinition\nmetadata: {{name: web}}\nspec:\n  management: External\n  manifest:\n    apiVersion: argoproj.io/v1alpha1\n    kind: AppProject\n    metadata: {{name: web, namespace: argocd}}\n    spec:\n      sourceRepos: ['*']\n      destinations: [{{server: '*', namespace: '*'}}]\n"),
    )
    .unwrap();
    std::fs::write(
        copy.path().join("nyl/preview.yaml"),
        "\
apiVersion: gitops.nyl/v1
kind: EnvironmentTemplate
metadata: {name: preview}
spec:
  parameters: {pr: {type: integer}}
  unitSelector: {matchLabels: {preview: 'true'}}
  values:
    backendDir: '%SCENARIO_TMP%/tofu'
    target:
      inline:
        reconciler: {argocd: {}}
        clusterRef: {name: dev}
        applicationGroupSelector: {matchLabels: {app: web}}
        catalogApplication: {syncPolicy: {automated: {prune: true}}}
        publication: {repositoryRef: {name: platform}, revision: previews, pathPrefix: '{{ environment.name }}'}
        releaseInputs:
          web/web:
            image: {fromUnit: {unit: web-image, artifact: image, pointer: /reference}}
            databaseHost: {fromUnit: {unit: database, output: host}}
  state:
    repositoryRef: {name: platform}
    desiredRef: nyl/previews
    observedRef: nyl/previews
    path: '{{ environment.name }}'
  allowUnprotectedSource: true
  deletionPolicy: Teardown
  allowTeardown: true
  ttl: 7d
",
    )
    .unwrap();
    copy
}

fn real_publication_harness(project: &std::path::Path) -> harness::Harness {
    let mut harness = harness::Harness::new(project, true);
    harness.drivers = nyl_units::registry::with_real_publication(&harness.script, nyl::resource_catalogue());
    harness
}

fn wiring<'a>(harness: &'a harness::Harness, tools: &'a ProcessRunner) -> Wiring<'a> {
    Wiring {
        clock: harness.clock.clone(),
        ids: &harness.ids,
        drivers: &harness.drivers,
        tools,
        faults: &NoFaults,
        runner: "test-runner".to_owned(),
        author: Some(AUTHOR.parse().unwrap()),
        process_env: harness.process_env.clone(),
        cancellation: Cancellation::default(),
        prompt: None,
        progress: None,
    }
}

fn invoke(harness: &harness::Harness, args: &[&str]) -> CommandOutcome {
    let tools = ProcessRunner::new();
    let args = args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
    invoke_in(&harness.world, &wiring(harness, &tools), &args, "main", "").unwrap()
}

/// `nyl build kubernetes <options> [PATH] [-- <driver arguments>]` in a fresh clone of `main`.
fn build(harness: &harness::Harness, args: &[&str]) -> CommandOutcome {
    let clone = harness.world.fresh_clone("main");
    let (options, driver) = match args.iter().position(|arg| *arg == "--") {
        Some(at) => args.split_at(at),
        None => (args, &[][..]),
    };
    let path = clone.display().to_string();
    let argv = ["nyl", "build", "kubernetes"]
        .into_iter()
        .chain(options.iter().copied())
        .chain(std::iter::once(path.as_str()))
        .chain(driver.iter().copied());
    let tools = ProcessRunner::new();
    Cli::try_parse_from(argv)
        .unwrap()
        .execute_in_process(&wiring(harness, &tools))
        .unwrap()
}

fn previews(harness: &harness::Harness) -> Vec<String> {
    harness
        .files("refs/heads/previews")
        .lines()
        .map(str::to_owned)
        .collect()
}

fn application_names(harness: &harness::Harness, prefix: &str) -> Vec<String> {
    previews(harness)
        .into_iter()
        .filter(|path| path.starts_with(&format!("{prefix}/_nyl/catalog/")) && path.ends_with(".yaml"))
        .map(|path| {
            let document: Value = serde_saphyr::from_str(&harness.show("refs/heads/previews", &path)).unwrap();
            document["metadata"]["name"].as_str().unwrap_or_default().to_owned()
        })
        .collect()
}

/// Two instances publish into one branch, each under its prefix and with its
/// own Argo CD names; a direct `build --push` of an instance's target is
/// refused, because the instance owns it; and closing one instance removes
/// its files and state and leaves the other. Catches instances that overwrite
/// each other's published files or names, an instance a direct build can
/// publish over, and a teardown that touches a neighbour.
#[test]
fn test_two_instances_publish_side_by_side_and_close_independently() {
    let project = preview_project();
    let harness = real_publication_harness(project.path());
    for pr in ["1", "2"] {
        let reconcile = invoke(
            &harness,
            &[
                "reconcile",
                "-e",
                &format!("pr-{pr}"),
                "--template",
                "preview",
                "--param",
                &format!("pr={pr}"),
                "-o",
                "json",
            ],
        );
        assert_eq!(reconcile.exit, 0, "{}\n{}", reconcile.stderr, reconcile.stdout);
    }

    let files = previews(&harness);
    for prefix in ["pr-1", "pr-2"] {
        assert!(
            files.iter().any(|path| path.starts_with(&format!("{prefix}/web/"))),
            "{prefix}: {files:?}"
        );
        assert!(files.contains(&format!("{prefix}/_nyl/index.json")), "{files:?}");
        let index: Value =
            serde_json::from_str(&harness.show("refs/heads/previews", &format!("{prefix}/_nyl/index.json"))).unwrap();
        assert_eq!(index["target"], format!("{prefix}-kubernetes"));
        assert_eq!(index["owner"]["environment"], prefix);
    }
    let names = [application_names(&harness, "pr-1"), application_names(&harness, "pr-2")].concat();
    let unique = names.iter().collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        unique.len(),
        names.len(),
        "Argo CD names differ per instance: {names:?}"
    );
    assert!(
        names.iter().any(|name| name.contains("pr-1")) && names.iter().any(|name| name.contains("pr-2")),
        "{names:?}"
    );

    // The instance owns its target: a direct publication refuses.
    let refused = build(&harness, &["-e", "pr-1", "--", "--push"]);
    assert_eq!(refused.exit, 3, "{}", refused.stderr);
    assert!(
        refused.stderr.contains("NYL-KUBERNETES-BUILD-TARGET-OWNED"),
        "{}",
        refused.stderr
    );
    assert!(refused.stderr.contains("pr-1"), "{}", refused.stderr);

    // Closing pr-1: phase 1 waits for the removal to be confirmed.
    let closing = invoke(&harness, &["state", "delete", "-e", "pr-1", "--teardown", "-o", "json"]);
    assert_eq!(closing.exit, 2, "{}\n{}", closing.stderr, closing.stdout);
    assert!(previews(&harness)
        .iter()
        .any(|path| path.starts_with("pr-1/_nyl/catalog/")));
    let confirmed = invoke(
        &harness,
        &[
            "teardown",
            "-e",
            "pr-1",
            "--unit",
            "kubernetes",
            "--confirm-removed",
            "--reason",
            "checked",
            "-o",
            "json",
        ],
    );
    assert_eq!(confirmed.exit, 0, "{}\n{}", confirmed.stderr, confirmed.stdout);
    let finished = invoke(&harness, &["state", "delete", "-e", "pr-1", "--teardown", "-o", "json"]);
    assert_eq!(finished.exit, 0, "{}\n{}", finished.stderr, finished.stdout);

    let files = previews(&harness);
    assert!(!files.iter().any(|path| path.starts_with("pr-1/")), "{files:?}");
    assert!(files.iter().any(|path| path.starts_with("pr-2/web/")), "{files:?}");
    let remaining = invoke(&harness, &["status", "-e", "pr-2", "-o", "json"]);
    assert_eq!(remaining.exit, 0, "{}\n{}", remaining.stderr, remaining.stdout);
}

/// `nyl validate` cannot read state, so it checks each EnvironmentTemplate
/// through sample instances: the project of two previews passes, and the
/// same project with names that ignore the target (so every instance, and
/// dev, would generate the same Argo CD Applications) fails before any
/// instance exists. Catches a template whose instances collide being found
/// only when the second pull request opens.
#[test]
fn test_validate_checks_the_names_every_instance_would_generate() {
    let validate = |edit: &dyn Fn(&std::path::Path)| {
        let project = preview_project();
        let root = project.path();
        // The world substitutes these placeholders when it commits the project.
        for entry in walkdir::WalkDir::new(root)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_file())
        {
            if let Ok(text) = std::fs::read_to_string(entry.path()) {
                let text = text
                    .replace("%PROJECT_URL%", "https://example.invalid/platform.git")
                    .replace("%SCENARIO_TMP%", "/var/empty")
                    .replace("%REGISTRY%", "registry.invalid")
                    .replace("['%DOCKER_PASSTHROUGH%']", "[]");
                std::fs::write(entry.path(), text).unwrap();
            }
        }
        edit(root);
        let git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(root)
                .status()
                .unwrap();
            assert!(status.success(), "{args:?}");
        };
        git(&["init", "--quiet", "--initial-branch=main"]);
        git(&["add", "--all"]);
        git(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "project",
        ]);
        assert_cmd::Command::new(assert_cmd::cargo::cargo_bin!("nyl"))
            .current_dir(root.join("nyl"))
            .arg("validate")
            .output()
            .unwrap()
    };
    let passes = validate(&|_| {});
    assert!(
        passes.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&passes.stdout),
        String::from_utf8_lossy(&passes.stderr)
    );

    let collides = validate(&|root| {
        let web = root.join("nyl/web.yaml");
        let text = std::fs::read_to_string(&web).unwrap();
        std::fs::write(
            &web,
            text.replace(
                "applicationNameTemplate: '${ target.metadata.name }-${ release.metadata.name }'",
                "applicationNameTemplate: '${ release.metadata.name }'",
            ),
        )
        .unwrap();
    });
    let output = format!(
        "{}{}",
        String::from_utf8_lossy(&collides.stdout),
        String::from_utf8_lossy(&collides.stderr)
    );
    assert!(!collides.status.success(), "{output}");
    assert!(
        output.contains("sample"),
        "the message names the sample instances: {output}"
    );
}

fn instance_uid(harness: &harness::Harness, instance: &str, unit: &str) -> String {
    let text = scenario::world::git_output(
        &harness.world.bare(),
        &["show", &format!("nyl/previews:{instance}/desired/units/{unit}.yaml")],
    );
    let document: Value = serde_saphyr::from_str(&text).unwrap();
    document["unit"]["uid"].as_str().unwrap().to_owned()
}

fn reconcile_instance(harness: &harness::Harness, pr: &str) -> CommandOutcome {
    invoke(
        harness,
        &[
            "reconcile",
            "-e",
            &format!("pr-{pr}"),
            "--template",
            "preview",
            "--param",
            &format!("pr={pr}"),
            "-o",
            "json",
        ],
    )
}

/// An instance that starts over (`state init --fresh`) gets a new incarnation
/// of its publication, and takes over the prefix its previous incarnation
/// published: the fence must answer for the instance's own state, where the old
/// owner no longer exists. Catches an instance stuck on `NYL-KUBERNETES-OWNER-UNKNOWN`
/// after a fresh start.
#[test]
fn test_an_instance_that_starts_over_takes_over_its_own_prefix() {
    let project = preview_project();
    let harness = real_publication_harness(project.path());
    assert_eq!(reconcile_instance(&harness, "1").exit, 0);
    let before = instance_uid(&harness, "pr-1", "kubernetes");
    let fresh = invoke(
        &harness,
        &[
            "state",
            "init",
            "-e",
            "pr-1",
            "--template",
            "preview",
            "--param",
            "pr=1",
            "--fresh",
        ],
    );
    assert_eq!(fresh.exit, 0, "{}", fresh.stderr);
    let again = reconcile_instance(&harness, "1");
    assert_eq!(again.exit, 0, "{}\n{}", again.stderr, again.stdout);
    assert_ne!(
        instance_uid(&harness, "pr-1", "kubernetes"),
        before,
        "a new incarnation published"
    );
    assert!(harness
        .show("refs/heads/previews", "pr-1/_nyl/index.json")
        .contains(&instance_uid(&harness, "pr-1", "kubernetes")));
}

/// Another environment's publication meeting a prefix an instance owns is
/// fenced while the instance's incarnation exists in its state, and takes the
/// prefix over once the instance is deleted. Catches fencing that cannot see
/// instances: it would call a live instance's owner unknown.
#[test]
fn test_a_prefix_owned_by_a_live_instance_fences_until_the_instance_is_deleted() {
    let project = preview_project();
    let harness = real_publication_harness(project.path());
    assert_eq!(reconcile_instance(&harness, "1").exit, 0);
    let owner = instance_uid(&harness, "pr-1", "kubernetes");
    assert_eq!(invoke(&harness, &["state", "init", "-e", "dev"]).exit, 0);
    assert_eq!(invoke(&harness, &["reconcile", "-e", "dev", "-o", "json"]).exit, 0);

    // dev's prefix now carries the instance's incarnation as its owner.
    let index_path = "dev/_nyl/index.json";
    let mut index: Value = serde_json::from_str(&harness.show("refs/heads/deploy", index_path)).unwrap();
    index["owner"] = serde_json::json!({"environment": "pr-1", "unit": "kubernetes", "uid": owner});
    harness.world.commit_files(
        "deploy",
        "The instance published dev's prefix",
        &[(index_path, &(serde_json::to_string_pretty(&index).unwrap() + "\n"))],
    );
    let teardown = |harness: &harness::Harness| {
        invoke(
            harness,
            &[
                "teardown",
                "-e",
                "dev",
                "--unit",
                "kubernetes",
                "--allow-incomplete",
                "-o",
                "json",
            ],
        )
    };
    let fenced = teardown(&harness);
    assert_eq!(fenced.exit, 3, "{}\n{}", fenced.stderr, fenced.stdout);
    let message = fenced.report.to_string();
    assert!(message.contains("NYL-KUBERNETES-OWNER-FENCED"), "{message}");
    assert!(
        message.contains("still exists in the state of environment pr-1"),
        "{message}"
    );

    // Close the instance, then the prefix is nobody's.
    let closing = invoke(&harness, &["state", "delete", "-e", "pr-1", "--teardown", "-o", "json"]);
    assert_eq!(closing.exit, 2, "{}", closing.stderr);
    let confirmed = invoke(
        &harness,
        &[
            "teardown",
            "-e",
            "pr-1",
            "--unit",
            "kubernetes",
            "--confirm-removed",
            "--reason",
            "checked",
            "-o",
            "json",
        ],
    );
    assert_eq!(confirmed.exit, 0, "{}", confirmed.stderr);
    assert_eq!(
        invoke(&harness, &["state", "delete", "-e", "pr-1", "--teardown"]).exit,
        0
    );
    let taken = teardown(&harness);
    assert_ne!(taken.exit, 3, "{}\n{}", taken.stderr, taken.stdout);
    assert!(!taken.report.to_string().contains("OWNER-"), "{}", taken.report);
}

/// `state init --template` says what an instance's teardown would meet before
/// the instance exists: the publication's unmet readiness requirements, and
/// that its `manual` `teardownWait` stops an unattended removal, such as an
/// expiry, after the first phase. A unit that waits for another's outputs
/// cannot be resolved yet, so only drivers that read their requirements from
/// the declaration (the fake publication here) show them this early; the
/// real publication shows them from its first `plan`. Catches a template whose
/// previews can never be removed unattended being found only when the first
/// one expires.
#[test]
fn test_state_init_warns_what_an_unattended_removal_would_wait_for() {
    let project = preview_project();
    let units = project.path().join("nyl/units.yaml");
    let text = std::fs::read_to_string(&units).unwrap().replace("\r\n", "\n");
    let declared = text.replace(
        "  target: {{ values.target | tojson }}\n",
        "  target: {{ values.target | tojson }}\n  teardownSettings: {catalogPrunes: false}\n",
    );
    assert_ne!(text, declared);
    std::fs::write(&units, declared).unwrap();
    // The fake kinds, whose publication reads `teardownSettings` from the declaration.
    let harness = harness::Harness::new(project.path(), true);
    let init = invoke(
        &harness,
        &[
            "state",
            "init",
            "-e",
            "pr-1",
            "--template",
            "preview",
            "--param",
            "pr=1",
        ],
    );
    assert_eq!(init.exit, 0, "{}\n{}", init.stderr, init.stdout);
    assert!(
        init.stderr.contains("NYL-TEARDOWN-CATALOG-MANUAL-SYNC"),
        "{}",
        init.stderr
    );
    assert!(init.stderr.contains("NYL-TEARDOWN-WAIT-MANUAL"), "{}", init.stderr);
    assert!(init.stderr.contains("pr-1/kubernetes"), "{}", init.stderr);
}

/// A `manual` `teardownWait` is read from the declaration, so `state init
/// --template` warns about it for the real publication before any producer
/// ran and the unit could resolve; the unit's readiness requirements follow
/// from the first `plan`. Catches the warning depending on resolution that
/// a fresh preview cannot have.
#[test]
fn test_state_init_warns_a_manual_wait_before_the_publication_resolves() {
    let project = preview_project();
    let harness = real_publication_harness(project.path());
    let init = invoke(
        &harness,
        &[
            "state",
            "init",
            "-e",
            "pr-1",
            "--template",
            "preview",
            "--param",
            "pr=1",
        ],
    );
    assert_eq!(init.exit, 0, "{}\n{}", init.stderr, init.stdout);
    assert!(init.stderr.contains("NYL-TEARDOWN-WAIT-MANUAL"), "{}", init.stderr);
    assert!(init.stderr.contains("pr-1/kubernetes"), "{}", init.stderr);
}
