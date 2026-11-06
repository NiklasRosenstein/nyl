//! `nyl attest` and the `observe` teardown wait of a `KubernetesPublication`
//! in observe mode, end to end through the orchestration shell with the real
//! publication driver: it publishes to the world's `project.git`, and Argo CD
//! is a local fake of the Kubernetes API of the control-plane Cluster. The
//! Cluster's local context and the kubeconfig the unit admits are the only
//! way to it; the process's own kube configuration is never read.
//!
//! Contract: [Kubernetes publications](../../design/promotion.md#kubernetes-publications),
//! [Attestations](../../design/orchestration-core.md#attestations), and the
//! [Kubernetes publication unit](../../design/orchestration-core.md#kubernetes-publication-unit),
//! Teardown.

#[path = "../../crates/nyl-units/tests/argocd_stub/mod.rs"]
mod argocd_stub;
mod scenario;

use std::path::Path;

use argocd_stub::{application, applications, kube_api, kubeconfig, Applications, Stub};
use scenario::shell::{self as harness, shared_project};
use scenario::world::git_output;
use serde_json::Value;
use tempfile::TempDir;

const OBSERVED: &str = "refs/heads/nyl/dev/observed";

fn copy(from: &Path, to: &Path) {
    for entry in walkdir::WalkDir::new(from) {
        let entry = entry.unwrap();
        let target = to.join(entry.path().strip_prefix(from).unwrap());
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&target).unwrap();
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// The walkthrough project with its publication in observe mode, admitting
/// the kubeconfig, over a control-plane Cluster that declares its context.
fn observing_project(teardown_wait: &str) -> TempDir {
    let project = TempDir::new().unwrap();
    copy(&shared_project(), project.path());
    let units = project.path().join("nyl/units.yaml");
    let text = std::fs::read_to_string(&units).unwrap();
    assert!(text.contains("  mode: publish\n  timeout: 10m"));
    std::fs::write(
        &units,
        text.replace(
            "  mode: publish\n  timeout: 10m",
            &format!("  mode: observe\n  teardownWait: {teardown_wait}\n  env: {{passthrough: [KUBECONFIG]}}\n  timeout: 10m"),
        ),
    )
    .unwrap();
    let platform = project.path().join("nyl/platform.yaml");
    let text = std::fs::read_to_string(&platform).unwrap();
    // The catalog prunes removed Applications, so a teardown is ready to complete.
    let text = text
        .replace(
            "  kubernetes:\n    kubeVersion: 1.31.4",
            "  live: {context: argocd-admin}\n  kubernetes:\n    kubeVersion: 1.31.4",
        )
        .replace(
            "  publication:\n    repositoryRef: {name: platform}\n",
            "  catalogApplication:\n    syncPolicy: {automated: {prune: true}}\n  publication:\n    repositoryRef: {name: platform}\n",
        );
    std::fs::write(&platform, text).unwrap();
    let group = project.path().join("nyl/web.yaml");
    let text = std::fs::read_to_string(&group).unwrap();
    std::fs::write(
        &group,
        text.replace(
            "  applicationNamespace: argocd\n",
            "  applicationNamespace: argocd\n  namespace: {deletePolicy: Automatic}\n",
        ),
    )
    .unwrap();
    project
}

struct Observed {
    shell: harness::Harness,
    stub: Option<Stub>,
    served: Applications,
    _project: TempDir,
    _config: TempDir,
}

impl Observed {
    fn new(teardown_wait: &str) -> Self {
        let project = observing_project(teardown_wait);
        let mut shell = harness::Harness::new(project.path(), true);
        shell.drivers = nyl_units::registry::with_real_publication(&shell.script, nyl::resource_catalogue());
        let served = applications(Vec::new());
        let stub = kube_api(served.clone());
        let config = TempDir::new().unwrap();
        let file = config.path().join("kubeconfig");
        kubeconfig(&file, "argocd-admin", &stub.url());
        shell
            .process_env
            .insert("KUBECONFIG".to_owned(), file.display().to_string());
        let observed = Self {
            shell,
            stub: Some(stub),
            served,
            _project: project,
            _config: config,
        };
        assert_eq!(observed.shell.invoke(&["state", "init", "-e", "dev"]).exit, 0);
        let reconcile = observed.shell.invoke(&["reconcile", "-e", "dev", "-o", "json"]);
        assert_eq!(reconcile.exit, 0, "{}", reconcile.stderr);
        observed
    }

    fn head(&self) -> String {
        git_output(&self.shell.world.bare(), &["rev-parse", "refs/heads/deploy"])
    }

    /// The Applications the stored unit records, as `(name, namespace, path, role)`.
    fn applications(&self) -> Vec<(String, String, String, String)> {
        let text = git_output(
            &self.shell.world.bare(),
            &["show", "refs/heads/nyl/dev/desired:desired/units/kubernetes.yaml"],
        );
        let unit: Value = serde_saphyr::from_str(&text).unwrap();
        unit["resolvedSpec"]["argocd"]["applications"]
            .as_array()
            .unwrap()
            .iter()
            .map(|app| {
                let text = |field: &str| app[field].as_str().unwrap().to_owned();
                (text("name"), text("namespace"), text("path"), text("role"))
            })
            .collect()
    }

    /// Argo CD reports every Application running `revision` with `health`,
    /// except the ones `without` names.
    fn serve(&self, revision: &str, health: &str, without: &[&str]) {
        *self.served.lock().unwrap() = self
            .applications()
            .iter()
            .filter(|(name, ..)| !without.contains(&name.as_str()))
            .map(|(name, namespace, path, _)| application(name, namespace, path, &[revision], health))
            .collect();
    }
}

fn unit(report: &Value) -> &Value {
    &report["environments"][0]["units"]["kubernetes"]
}

/// End to end: a reconcile publishes and records the Argo CD facts; `nyl
/// attest` reads the Applications through the declared context, matches
/// what they run to the pushed publication, and records `accepted` and
/// `healthy` with each Application's running publication and health; a
/// repeated attest writes nothing; a degraded Application fails `healthy`
/// (exit 3) and keeps `accepted`; an Argo CD that cannot be read exits 2 and
/// records nothing. Catches an attestation that never reaches state, one
/// recorded from a failed reading, and a repeat that commits only because
/// time moved on.
#[test]
fn test_attest_records_accepted_and_healthy_from_argocd() {
    let observed = Observed::new("{strategy: manual}");
    let published = observed.head();
    let tip = || git_output(&observed.shell.world.bare(), &["rev-parse", OBSERVED]);
    let before = tip();

    observed.serve(&published, "Healthy", &[]);
    let attested = observed.shell.invoke(&["attest", "-e", "dev", "-o", "json"]);
    assert_eq!(attested.exit, 0, "{}", attested.stderr);
    assert_eq!(unit(&attested.report)["check"], "attested", "{}", attested.report);
    assert_eq!(unit(&attested.report)["attestations"]["accepted"], "pass");
    assert_eq!(unit(&attested.report)["attestations"]["healthy"], "pass");
    let record = git_output(
        &observed.shell.world.bare(),
        &[
            "show",
            &format!("{OBSERVED}:observed/units/kubernetes/attestations.yaml"),
        ],
    );
    assert!(record.contains("driver: attest"), "{record}");
    let document: Value = serde_saphyr::from_str(&record).unwrap();
    let details = &document["attestations"]["healthy"]["details"];
    assert_eq!(details["publication"]["commit"], published.as_str());
    let applications = details["applications"].as_array().unwrap();
    assert_eq!(applications.len(), 2, "{details}");
    for application in applications {
        assert_eq!(application["running"], published.as_str());
        assert_eq!(application["publication"], published.as_str());
        assert_eq!(application["health"], "Healthy");
    }
    let after = tip();
    assert_ne!(before, after);

    // Nothing changed but the time: no commit.
    assert_eq!(observed.shell.invoke(&["attest", "-e", "dev"]).exit, 0);
    assert_eq!(tip(), after);

    // A degraded workload fails `healthy`, not `accepted`.
    observed
        .shell
        .clock
        .advance(nyl_core::orchestration::time::Duration::from_mins(5));
    observed.serve(&published, "Degraded", &["dev-catalog"]);
    *observed.served.lock().unwrap() = {
        let mut apps = observed.served.lock().unwrap().clone();
        apps.push(application(
            "dev-catalog",
            "argocd",
            "dev/_nyl/catalog",
            &[&published],
            "Healthy",
        ));
        apps
    };
    let degraded = observed.shell.invoke(&["attest", "-e", "dev", "-o", "json"]);
    assert_eq!(degraded.exit, 3, "{}", degraded.stderr);
    assert_eq!(unit(&degraded.report)["attestations"]["accepted"], "pass");
    assert_eq!(unit(&degraded.report)["attestations"]["healthy"], "fail");
    assert!(
        degraded.stderr.contains("NYL-ATTESTATION-FAILED"),
        "{}",
        degraded.stderr
    );
    let failed = tip();

    // Argo CD not answering is unknown: exit 2, nothing recorded.
    drop(observed.stub);
    let unreachable = observed.shell.invoke(&["attest", "-e", "dev", "-o", "json"]);
    assert_eq!(unreachable.exit, 2, "{}", unreachable.stderr);
    assert!(
        unreachable.stderr.contains("NYL-ATTEST-UNKNOWN"),
        "{}",
        unreachable.stderr
    );
    assert!(
        unreachable.stderr.contains("cannot list Applications"),
        "{}",
        unreachable.stderr
    );
    assert_eq!(tip(), failed);
}

/// A kubeconfig the unit does not admit is never used: the reading is
/// unknown and names `env.passthrough`, even though the file exists on this
/// machine. Catches a fallback to the process's kube configuration.
#[test]
fn test_attest_without_an_admitted_kubeconfig_is_unknown_and_says_how_to_admit_it() {
    let project = observing_project("{strategy: manual}");
    let text = std::fs::read_to_string(project.path().join("nyl/units.yaml")).unwrap();
    std::fs::write(
        project.path().join("nyl/units.yaml"),
        text.replace("  env: {passthrough: [KUBECONFIG]}\n", ""),
    )
    .unwrap();
    let mut shell = harness::Harness::new(project.path(), true);
    shell.drivers = nyl_units::registry::with_real_publication(&shell.script, nyl::resource_catalogue());
    // The process has a kubeconfig; the unit does not admit it.
    shell
        .process_env
        .insert("KUBECONFIG".to_owned(), "/nonexistent/kubeconfig".to_owned());
    assert_eq!(shell.invoke(&["state", "init", "-e", "dev"]).exit, 0);
    assert_eq!(shell.invoke(&["reconcile", "-e", "dev"]).exit, 0);
    let attested = shell.invoke(&["attest", "-e", "dev", "-o", "json"]);
    assert_eq!(attested.exit, 2, "{}", attested.stderr);
    assert!(attested.stderr.contains("env.passthrough"), "{}", attested.stderr);
}

/// The `observe` teardown wait, end to end: phase 1 is published, the run
/// waits while Argo CD still has a workload Application (the catalog does
/// not count) and ends uncertain with `applications-remain`; once only the
/// catalog is left, the wait is observed and phase 2 removes the tree.
#[test]
fn test_the_observe_teardown_wait_follows_what_argocd_reports() {
    let observed = Observed::new("{strategy: observe}");
    let published = observed.head();
    observed.serve(&published, "Healthy", &[]);
    let remaining = observed
        .shell
        .invoke(&["teardown", "-e", "dev", "--unit", "kubernetes", "-o", "json"]);
    assert_eq!(remaining.exit, 4, "{}\n{}", remaining.stderr, remaining.stdout);
    assert_eq!(unit(&remaining.report)["result"], "uncertain");
    assert_eq!(unit(&remaining.report)["reason"], "applications-remain: web");
    let files = git_output(
        &observed.shell.world.bare(),
        &["ls-tree", "-r", "--name-only", "refs/heads/deploy"],
    );
    assert!(
        files.lines().any(|path| path.starts_with("dev/web/")),
        "phase 2 did not run: {files}"
    );
}

#[test]
fn test_the_observe_teardown_wait_completes_when_only_the_catalog_remains() {
    let observed = Observed::new("{strategy: observe}");
    let published = observed.head();
    observed.serve(&published, "Healthy", &["web"]);
    let torn_down = observed
        .shell
        .invoke(&["teardown", "-e", "dev", "--unit", "kubernetes", "-o", "json"]);
    assert_eq!(torn_down.exit, 0, "{}\n{}", torn_down.stderr, torn_down.stdout);
    assert_eq!(unit(&torn_down.report)["result"], "removed");
    let files = git_output(
        &observed.shell.world.bare(),
        &["ls-tree", "-r", "--name-only", "refs/heads/deploy"],
    );
    assert!(!files.lines().any(|path| path.starts_with("dev/")), "{files}");
    let message = git_output(&observed.shell.world.bare(), &["log", "-1", "--format=%B", OBSERVED]);
    assert!(message.contains("observed"), "{message}");
}

/// The same attestation through the Argo CD API server: the ArgoCDInstance
/// in Git declares the method and the server's address, and the token comes
/// only from the unit's admitted environment. A token the server rejects
/// (expired) is unknown, exit 2, and records nothing.
#[test]
fn test_attest_reads_the_argocd_api_server_with_the_admitted_token() {
    let served = applications(Vec::new());
    let stub = argocd_stub::argocd_api("api-token", served.clone());
    let project = observing_project("{strategy: manual}");
    let units = project.path().join("nyl/units.yaml");
    let text = std::fs::read_to_string(&units).unwrap();
    std::fs::write(
        &units,
        text.replace("passthrough: [KUBECONFIG]", "passthrough: [ARGOCD_AUTH_TOKEN]"),
    )
    .unwrap();
    let platform = project.path().join("nyl/platform.yaml");
    let mut text = std::fs::read_to_string(&platform).unwrap();
    text = text.replace(
        "  clusterRef: {name: dev}\n  applicationGroupSelector",
        "  clusterRef: {name: dev}\n  argocdRef: {name: control}\n  applicationGroupSelector",
    );
    text.push_str(&format!(
        "---\napiVersion: k8s.gitops.nyl/v1\nkind: ArgoCDInstance\nmetadata: {{name: control}}\nspec:\n  clusterRef: {{name: dev}}\n  namespace: argocd\n  access: {{method: api, api: {{server: '{}'}}}}\n",
        stub.url()
    ));
    std::fs::write(&platform, text).unwrap();
    let mut shell = harness::Harness::new(project.path(), true);
    shell.drivers = nyl_units::registry::with_real_publication(&shell.script, nyl::resource_catalogue());
    shell
        .process_env
        .insert("ARGOCD_AUTH_TOKEN".to_owned(), "api-token".to_owned());
    assert_eq!(shell.invoke(&["state", "init", "-e", "dev"]).exit, 0);
    let reconcile = shell.invoke(&["reconcile", "-e", "dev", "-o", "json"]);
    assert_eq!(reconcile.exit, 0, "{}", reconcile.stderr);

    let published = git_output(&shell.world.bare(), &["rev-parse", "refs/heads/deploy"]);
    let stored = git_output(
        &shell.world.bare(),
        &["show", "refs/heads/nyl/dev/desired:desired/units/kubernetes.yaml"],
    );
    let unit_document: Value = serde_saphyr::from_str(&stored).unwrap();
    assert_eq!(unit_document["resolvedSpec"]["argocd"]["access"]["method"], "api");
    *served.lock().unwrap() = unit_document["resolvedSpec"]["argocd"]["applications"]
        .as_array()
        .unwrap()
        .iter()
        .map(|app| {
            let text = |field: &str| app[field].as_str().unwrap().to_owned();
            application(
                &text("name"),
                &text("namespace"),
                &text("path"),
                &[&published],
                "Healthy",
            )
        })
        .collect();
    let attested = shell.invoke(&["attest", "-e", "dev", "-o", "json"]);
    assert_eq!(attested.exit, 0, "{}", attested.stderr);
    assert_eq!(unit(&attested.report)["attestations"]["accepted"], "pass");
    assert_eq!(unit(&attested.report)["attestations"]["healthy"], "pass");
    assert!(stub
        .requests()
        .iter()
        .all(|request| request.headers["authorization"] == "Bearer api-token"));

    let tip = git_output(&shell.world.bare(), &["rev-parse", OBSERVED]);
    shell
        .process_env
        .insert("ARGOCD_AUTH_TOKEN".to_owned(), "expired".to_owned());
    let expired = shell.invoke(&["attest", "-e", "dev", "-o", "json"]);
    assert_eq!(expired.exit, 2, "{}", expired.stderr);
    assert!(
        expired.stderr.contains("NYL-ATTEST-UNKNOWN") && expired.stderr.contains("has expired"),
        "{}",
        expired.stderr
    );
    assert_eq!(git_output(&shell.world.bare(), &["rev-parse", OBSERVED]), tip);
}
