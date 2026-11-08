//! Preview instances reading declared environments, end to end through the
//! orchestration shell with Command units and Git state: a cross-environment
//! reference from an instance blocks until the declared producer has a current
//! receipt, a producer other environments read is protected against teardown
//! and omission, and an instance is never a producer.
//!
//! Contract: [Cross-environment references](../../design/orchestration-core.md#cross-environment-references)
//! and [Environment templates and previews](../../design/orchestration-core.md#environment-templates-and-previews).

mod scenario;

use clap::Parser;
use nyl::cli::Cli;
use nyl::orchestration::run::{CommandOutcome, Wiring};
use nyl_orchestration::fault::NoFaults;
use nyl_orchestration::shell::run::Cancellation;
use nyl_units::process::ProcessRunner;
use scenario::runner::AUTHOR;
use scenario::shell::{self, project, NYL_TOML};
use scenario::world::git_output;
use serde_json::Value;
use tempfile::TempDir;

const PLATFORM: &str = "apiVersion: gitops.nyl/v1\nkind: GitRepository\nmetadata:\n  name: platform\nspec:\n  repoURL: '%PROJECT_URL%'\n  publishURL: '%PROJECT_URL%'\n";

const DEV: &str = "\
apiVersion: gitops.nyl/v1
kind: Environment
metadata: {name: dev}
spec:
  unitSelector: {matchLabels: {scope: dev}}
  allowUnprotectedSource: true
";

const TEMPLATE: &str = "\
apiVersion: gitops.nyl/v1
kind: EnvironmentTemplate
metadata: {name: preview}
spec:
  parameters:
    pr: {type: integer}
    peer: {type: string, default: dev}
  unitSelector: {matchLabels: {scope: preview}}
  values:
    peer: '{{ params.peer }}'
  state:
    repositoryRef: {name: platform}
    desiredRef: nyl/previews
    observedRef: nyl/previews
    path: '{{ environment.name }}'
  allowUnprotectedSource: true
  deletionPolicy: Teardown
  allowTeardown: true
";

/// dev's `network`, which previews read, and the preview's `site`, which reads
/// the `peer` environment's `network`: dev unless a parameter says otherwise.
const UNITS: &str = r#"apiVersion: units.gitops.nyl/v1
kind: Command
metadata:
  name: network
  labels: {scope: dev}
spec:
  command: [sh, -c, 'printf %s ''{"id":"vpc-one"}'' > "$NYL_OUTPUTS"']
  idempotent: true
  teardown: [sh, -c, 'true']
  outputs:
    id: {type: string}
---
apiVersion: units.gitops.nyl/v1
kind: Command
metadata:
  name: site
  labels: {scope: preview}
spec:
  command: [sh, -c, 'true']
  idempotent: true
  teardown: [sh, -c, 'true']
  values:
    network: {fromUnit: {environment: '{{ values.peer }}', unit: network, output: id}}
"#;

struct Harness {
    shell: shell::Harness,
    _project: TempDir,
}

impl Harness {
    /// A project with `dev`, the preview template, and `units`, with dev's
    /// state initialized.
    fn new(units: &str) -> Self {
        Self::with_files(units, &[])
    }

    /// [`Harness::new`] with more project files.
    fn with_files(units: &str, extra: &[(&str, &str)]) -> Self {
        let mut files = vec![
            NYL_TOML,
            ("nyl/platform.yaml", PLATFORM),
            ("nyl/dev.yaml", DEV),
            ("nyl/preview.yaml", TEMPLATE),
            ("nyl/units.yaml", units),
        ];
        files.extend_from_slice(extra);
        let project = project(&files);
        let harness = Self {
            shell: shell::Harness::new(project.path(), true),
            _project: project,
        };
        let initialized = harness.invoke(&["state", "init", "-e", "dev"]);
        assert_eq!(initialized.exit, 0, "{}", initialized.stderr);
        harness
    }

    fn invoke(&self, args: &[&str]) -> CommandOutcome {
        self.try_invoke(args).unwrap()
    }

    fn try_invoke(&self, args: &[&str]) -> Result<CommandOutcome, String> {
        let clone = self.shell.world.fresh_clone("main");
        let tools = ProcessRunner::new();
        let wiring = Wiring {
            clock: self.shell.clock.clone(),
            ids: &self.shell.ids,
            drivers: &self.shell.drivers,
            tools: &tools,
            faults: &NoFaults,
            runner: "test-runner".to_owned(),
            author: Some(AUTHOR.parse().unwrap()),
            process_env: self.shell.process_env.clone(),
            cancellation: Cancellation::default(),
            prompt: None,
            progress: None,
        };
        let argv = std::iter::once("nyl".to_owned())
            .chain(args.iter().map(|arg| (*arg).to_owned()))
            .chain(std::iter::once(clone.display().to_string()));
        Cli::try_parse_from(argv)
            .unwrap()
            .execute_in_process(&wiring)
            .map_err(|error| error.to_string())
    }

    /// Create the preview instance `pr-<number>` and bring its state to exist.
    fn preview(&self, number: &str, extra: &[&str]) {
        let name = format!("pr-{number}");
        let param = format!("pr={number}");
        let mut args = vec!["state", "init", "-e", &name, "--template", "preview", "--param", &param];
        args.extend_from_slice(extra);
        let created = self.invoke(&args);
        assert_eq!(created.exit, 0, "{}\n{}", created.stderr, created.stdout);
    }

    /// `site` of `pr-1` as its desired document stores it.
    fn site(&self, instance: &str) -> String {
        git_output(
            &self.shell.world.bare(),
            &["show", &format!("nyl/previews:{instance}/desired/units/site.yaml")],
        )
    }
}

fn unit<'a>(outcome: &'a CommandOutcome, environment: &str, name: &str) -> &'a Value {
    let section = outcome.report["environments"]
        .as_array()
        .unwrap()
        .iter()
        .find(|section| section["name"] == environment)
        .unwrap_or_else(|| panic!("{environment} in {}", outcome.report));
    &section["units"][name]
}

/// An instance's unit that reads a declared environment's unit blocks, with
/// the producer named by its environment, until the producer has a current
/// receipt; separate calls converge in either order, and the value it read
/// is the producer's output. Catches an instance that cannot resolve a
/// reference to a declared environment, or one that resolves without waiting.
#[test]
fn test_an_instance_reads_a_declared_environments_unit_and_waits_for_it() {
    let harness = Harness::new(UNITS);
    harness.preview("1", &[]);

    let waiting = harness.invoke(&["reconcile", "-e", "pr-1", "-o", "json"]);
    assert_eq!(waiting.exit, 2, "{}\n{}", waiting.stderr, waiting.stdout);
    let site = unit(&waiting, "pr-1", "site");
    assert_eq!(site["result"], "blocked", "{site}");
    assert!(site.to_string().contains("dev/network"), "{site}");

    let produced = harness.invoke(&["reconcile", "-e", "dev", "-o", "json"]);
    assert_eq!(produced.exit, 0, "{}\n{}", produced.stderr, produced.stdout);

    let ran = harness.invoke(&["reconcile", "-e", "pr-1", "-o", "json"]);
    assert_eq!(ran.exit, 0, "{}\n{}", ran.stderr, ran.stdout);
    assert_eq!(unit(&ran, "pr-1", "site")["result"], "executed");
    let stored = harness.site("pr-1");
    assert!(
        stored.contains("vpc-one"),
        "the producer's output is the instance's input: {stored}"
    );
    assert!(stored.contains("dev"), "{stored}");

    // The producer changing its output makes the instance's unit run again.
    harness.shell.world.commit_files(
        "main",
        "dev's network changes",
        &[("nyl/units.yaml", &UNITS.replacen("vpc-one", "vpc-two", 1))],
    );
    assert_eq!(harness.invoke(&["reconcile", "-e", "dev", "-o", "json"]).exit, 0);
    let again = harness.invoke(&["reconcile", "-e", "pr-1", "-o", "json"]);
    assert_eq!(again.exit, 0, "{}\n{}", again.stderr, again.stdout);
    assert!(harness.site("pr-1").contains("vpc-two"), "{}", harness.site("pr-1"));
}

/// A world in which `pr-1`'s `site` has read dev's `network`.
fn consuming() -> Harness {
    let harness = Harness::new(UNITS);
    harness.preview("1", &[]);
    assert_eq!(harness.invoke(&["reconcile", "-e", "dev"]).exit, 0);
    let ran = harness.invoke(&["reconcile", "-e", "pr-1", "-o", "json"]);
    assert_eq!(ran.exit, 0, "{}\n{}", ran.stderr, ran.stdout);
    harness
}

/// What dev's `network` is after a refused request: still in state with its
/// resources in place, waiting for `--allow-dependents`, never removed.
fn assert_waits_for_the_instance(harness: &Harness) {
    let waiting = harness.invoke(&["reconcile", "-e", "dev", "--allow-teardown", "-o", "json"]);
    assert_eq!(waiting.exit, 2, "{}\n{}", waiting.stderr, waiting.stdout);
    let network = unit(&waiting, "dev", "network");
    assert_eq!(network["result"], "pending-teardown", "{network}");
    assert!(network.to_string().contains("pr-1/site"), "{network}");
    assert!(network.to_string().contains("NYL-TEARDOWN-HAS-DEPENDENTS"), "{network}");
}

/// A declared producer that an instance reads is protected like a referenced
/// unit in its own environment: an explicit teardown of it and `state delete
/// --teardown` of its environment each refuse with `NYL-TEARDOWN-HAS-DEPENDENTS`
/// naming the instance's unit and removing nothing, until `--allow-dependents`,
/// which the transition commit records; so does dropping the producer from its
/// environment in source. The instance's own next run then fails with
/// `NYL-REFERENCE-INVALID`, naming the unit its producer no longer selects.
/// Catches producer protection that does not see instance consumers, which
/// only declared environments' state would show.
#[test]
fn test_a_declared_producers_teardown_and_omission_see_instance_consumers() {
    // An explicit teardown of the producer, then the override.
    let harness = consuming();
    let refused = harness.invoke(&["teardown", "-e", "dev", "--unit", "network", "-o", "json"]);
    assert_eq!(refused.exit, 2, "{}\n{}", refused.stderr, refused.stdout);
    assert!(
        refused.stderr.contains("NYL-TEARDOWN-HAS-DEPENDENTS"),
        "{}",
        refused.stderr
    );
    assert!(refused.stderr.contains("pr-1/site"), "{}", refused.stderr);
    assert_waits_for_the_instance(&harness);
    let allowed = harness.invoke(&[
        "teardown",
        "-e",
        "dev",
        "--unit",
        "network",
        "--allow-dependents",
        "-o",
        "json",
    ]);
    assert_eq!(allowed.exit, 0, "{}\n{}", allowed.stderr, allowed.stdout);
    assert_eq!(unit(&allowed, "dev", "network")["result"], "removed");
    let message = git_output(
        &harness.shell.world.bare(),
        &["log", "-1", "--format=%B", "refs/heads/nyl/dev/desired"],
    );
    assert!(
        message.contains("allowExternalDependents") && message.contains("pr-1/site"),
        "{message}"
    );

    // Decommissioning the producer's environment.
    let harness = consuming();
    let refused = harness.invoke(&["state", "delete", "-e", "dev", "--teardown", "-o", "json"]);
    assert_eq!(refused.exit, 2, "{}\n{}", refused.stderr, refused.stdout);
    let text = format!("{}{}", refused.stderr, refused.stdout);
    assert!(
        text.contains("NYL-TEARDOWN-HAS-DEPENDENTS") && text.contains("pr-1/site"),
        "{text}"
    );
    assert_waits_for_the_instance(&harness);

    // Dropping the producer from its environment in source: refused like a
    // teardown, until `--allow-dependents`. The instance's own next run then fails.
    let harness = consuming();
    harness.shell.world.commit_files(
        "main",
        "dev drops network",
        &[("nyl/units.yaml", &UNITS.replacen("scope: dev", "scope: gone", 1))],
    );
    assert_waits_for_the_instance(&harness);
    let allowed = harness.invoke(&[
        "reconcile",
        "-e",
        "dev",
        "--allow-teardown",
        "--allow-dependents",
        "-o",
        "json",
    ]);
    assert_eq!(allowed.exit, 0, "{}\n{}", allowed.stderr, allowed.stdout);
    assert_eq!(unit(&allowed, "dev", "network")["result"], "removed");
    let message = git_output(
        &harness.shell.world.bare(),
        &["log", "-1", "--format=%B", "refs/heads/nyl/dev/desired"],
    );
    assert!(
        message.contains("allowExternalDependents") && message.contains("pr-1/site"),
        "{message}"
    );
    let orphaned = harness.invoke(&["reconcile", "-e", "pr-1", "-o", "json"]);
    assert_eq!(orphaned.exit, 1, "{}\n{}", orphaned.stderr, orphaned.stdout);
    let text = format!("{}{}", orphaned.stderr, orphaned.stdout);
    assert!(
        text.contains("NYL-REFERENCE-INVALID") && text.contains("pr-1/site") && text.contains("dev/network"),
        "{text}"
    );
}

/// An instance is a consumer only: a unit that names one as the producer of a
/// reference, from a declared environment or from another instance, is
/// refused with `NYL-REFERENCE-INSTANCE-PRODUCER` and the fix, and nothing runs.
/// Catches a short-lived instance becoming something another unit depends on.
#[test]
fn test_an_instance_is_never_a_producer() {
    let from_dev = format!(
        "{UNITS}---\n{}",
        UNITS
            .split("---\n")
            .nth(1)
            .unwrap()
            .replace("name: site", "name: peek")
            .replace("scope: preview", "scope: dev")
            .replace("'{{ values.peer }}'", "pr-1")
            .replace("unit: network", "unit: site")
    );
    let harness = Harness::new(&from_dev);
    harness.preview("1", &[]);

    // From a declared environment.
    let refused = harness.invoke(&["plan", "-e", "dev", "-o", "json"]);
    assert_eq!(refused.exit, 1, "{}\n{}", refused.stderr, refused.stdout);
    let text = format!("{}{}", refused.stderr, refused.stdout);
    assert!(text.contains("NYL-REFERENCE-INSTANCE-PRODUCER"), "{text}");
    assert!(text.contains("pr-1") && text.contains("declared Environment"), "{text}");
    let refused = harness.invoke(&["reconcile", "-e", "dev", "-o", "json"]);
    assert_eq!(refused.exit, 1, "{}\n{}", refused.stderr, refused.stdout);
    assert!(
        refused.stderr.contains("NYL-REFERENCE-INSTANCE-PRODUCER"),
        "{}",
        refused.stderr
    );

    // From another instance: `pr-2` names `pr-1` as its peer.
    let harness = Harness::new(UNITS);
    harness.preview("1", &[]);
    harness.preview("2", &["--param", "peer=pr-1"]);
    let refused = harness.invoke(&["reconcile", "-e", "pr-2", "-o", "json"]);
    assert_eq!(refused.exit, 1, "{}\n{}", refused.stderr, refused.stdout);
    let text = format!("{}{}", refused.stderr, refused.stdout);
    assert!(text.contains("NYL-REFERENCE-INSTANCE-PRODUCER"), "{text}");
    assert!(text.contains("pr-2/site"), "{text}");
    // The refusal is pr-2's alone: dev still reconciles.
    let dev = harness.invoke(&["reconcile", "-e", "dev", "-o", "json"]);
    assert_eq!(dev.exit, 0, "{}\n{}", dev.stderr, dev.stdout);
    assert_eq!(unit(&dev, "dev", "network")["result"], "executed");
    // Nothing was written for pr-2's unit.
    assert!(!git_output(
        &harness.shell.world.bare(),
        &["ls-tree", "-r", "--name-only", "nyl/previews"]
    )
    .contains("pr-2/observed/units/site"),);
}

/// The shared-catalog rules hold for the tree commands too: a project whose
/// template shares a catalog on `previews` and which also publishes a static
/// target there fails `render-tree` of that target with the conflict code, as
/// `nyl validate` does, although the tree commands compile from a plain
/// inventory without instances. Catches a rule that only the orchestrated
/// entry points enforce.
#[test]
fn test_tree_commands_enforce_the_shared_catalog_rules_over_template_instances() {
    const SHARED_UNIT: &str = "\
apiVersion: units.gitops.nyl/v1
kind: KubernetesPublication
metadata: {name: kubernetes, labels: {scope: preview}}
spec:
  target:
    inline:
      reconciler: {argocd: {}}
      clusterRef: {name: dev}
      applicationGroupSelector: {matchLabels: {app: web}}
      catalogApplication: {shared: {name: previews}}
      publication: {repositoryRef: {name: platform}, revision: previews, pathPrefix: '{{ environment.name }}'}
";
    const CLUSTER_AND_GROUP: &str = "\
apiVersion: k8s.nyl/v1
kind: Cluster
metadata: {name: dev}
spec:
  destination: {server: 'https://kubernetes.default.svc'}
  kubernetes: {kubeVersion: 1.31.4, apiVersions: [v1, v1/ConfigMap, v1/Namespace], clusterScopedKinds: [core/Namespace]}
---
apiVersion: k8s.gitops.nyl/v1
kind: ApplicationGroup
metadata: {name: web, labels: {app: web}}
spec:
  applicationNamespace: argocd
  source: {path: ../applications/web}
  applicationNameTemplate: '${ target.metadata.name }-${ release.metadata.name }'
";
    const RELEASE: &str = "apiVersion: k8s.nyl/v1\nkind: Release\nmetadata: {name: web, namespace: web}\n---\napiVersion: v1\nkind: ConfigMap\nmetadata: {name: web, namespace: web}\n";
    let target = |revision: &str| {
        format!(
            "apiVersion: k8s.gitops.nyl/v1\nkind: DeploymentTarget\nmetadata: {{name: stray}}\nspec:\n  reconciler: {{argocd: {{}}}}\n  clusterRef: {{name: dev}}\n  applicationGroupSelector: {{matchLabels: {{app: web}}}}\n  publication: {{repositoryRef: {{name: platform}}, revision: {revision}, pathPrefix: stray}}\n"
        )
    };
    let world = |revision: &str| {
        Harness::with_files(
            SHARED_UNIT,
            &[
                ("nyl/cluster.yaml", CLUSTER_AND_GROUP),
                ("nyl/stray.yaml", &target(revision)),
                ("applications/web/release.yaml", RELEASE),
            ],
        )
    };
    let run = |revision: &str| {
        let harness = world(revision);
        let clone = harness.shell.world.fresh_clone("main");
        let output = TempDir::new().unwrap();
        let output_dir = output.path().display().to_string();
        let home = clone.with_extension("home");
        std::fs::create_dir_all(&home).unwrap();
        let mut command = scenario::nyl_binary(
            &clone,
            &home,
            &[
                "render-tree",
                "--target",
                "stray",
                "--output-dir",
                &output_dir,
                "--check",
            ],
        );
        let output = command.output().unwrap();
        (
            output.status.success(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    };
    // On the revision that carries the shared catalog it is refused.
    let (succeeded, text) = run("previews");
    assert!(!succeeded, "{text}");
    assert!(text.contains("NYL-KUBERNETES-SHARED-CATALOG-CONFLICT"), "{text}");
    assert!(text.contains("stray"), "{text}");
    // On another branch the same target renders.
    let (succeeded, text) = run("elsewhere");
    assert!(succeeded, "{text}");
}
