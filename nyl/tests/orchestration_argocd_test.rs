//! `nyl attest` and the `observe` teardown wait of a `KubernetesPublication`
//! with `observe: {}`, end to end through the orchestration shell with the real
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

use argocd_stub::{application_with_source, applications, kube_api, kubeconfig, Applications, Stub};
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

/// The walkthrough project with its publication observing the reconciler (`observe: {}`), admitting
/// the kubeconfig, over a control-plane Cluster that declares its context.
fn observing_project(teardown_wait: &str) -> TempDir {
    let project = TempDir::new().unwrap();
    copy(&shared_project(), project.path());
    let units = project.path().join("nyl/units.yaml");
    let text = std::fs::read_to_string(&units).unwrap();
    assert!(text.contains("  target: '{{ values.target }}'\n  timeout: 10m"));
    std::fs::write(
        &units,
        text.replace(
            "  target: '{{ values.target }}'\n  timeout: 10m",
            &format!("  target: '{{{{ values.target }}}}'\n  observe: {{}}\n  teardownWait: {teardown_wait}\n  env: {{passthrough: [KUBECONFIG]}}\n  timeout: 10m"),
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
        Self::with_units(teardown_wait, "", 0)
    }

    /// The observed publication project with `units` appended to its units,
    /// whose first reconcile exits with `exit`.
    fn with_units(teardown_wait: &str, units: &str, exit: u8) -> Self {
        let project = observing_project(teardown_wait);
        let file = project.path().join("nyl/units.yaml");
        let mut text = std::fs::read_to_string(&file).unwrap();
        text.push_str(units);
        std::fs::write(&file, text).unwrap();
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
        assert_eq!(reconcile.exit, exit, "{}", reconcile.stderr);
        observed
    }

    fn head(&self) -> String {
        git_output(&self.shell.world.bare(), &["rev-parse", "refs/heads/deploy"])
    }

    /// The Applications the stored unit records, as `(name, namespace, source)`
    /// with the source as Argo CD serves it (a target revision added).
    fn applications(&self) -> Vec<(String, String, Value)> {
        let text = git_output(
            &self.shell.world.bare(),
            &["show", "refs/heads/nyl/dev/desired:desired/units/kubernetes.yaml"],
        );
        let unit: Value = serde_saphyr::from_str(&text).unwrap();
        served_sources(&unit)
    }

    /// Argo CD reports every Application running `revision` with `health`,
    /// except the ones `without` names.
    fn serve(&self, revision: &str, health: &str, without: &[&str]) {
        self.serve_with(revision, |name| {
            if without.contains(&name) {
                None
            } else {
                Some(health.to_owned())
            }
        });
    }

    /// Argo CD reports each Application `health_of` names running `revision`.
    fn serve_with(&self, revision: &str, health_of: impl Fn(&str) -> Option<String>) {
        *self.served.lock().unwrap() = self
            .applications()
            .iter()
            .filter_map(|(name, namespace, source)| {
                Some(application_with_source(
                    name,
                    namespace,
                    source,
                    &[revision],
                    &health_of(name)?,
                ))
            })
            .collect();
    }
}

/// The `(name, namespace, source)` of each Application a stored unit records,
/// the source as Argo CD serves it.
fn served_sources(unit: &Value) -> Vec<(String, String, Value)> {
    unit["resolvedSpec"]["reconciler"]["expected"]
        .as_array()
        .unwrap()
        .iter()
        .map(|deployment| {
            let source = &deployment["source"];
            let mut served = source.get("options").cloned().unwrap_or_else(|| serde_json::json!({}));
            served["repoURL"] = source["repository"].clone();
            served["path"] = source["path"].clone();
            served["targetRevision"] = Value::String("deploy".to_owned());
            (
                deployment["locator"]["name"].as_str().unwrap().to_owned(),
                deployment["locator"]["namespace"].as_str().unwrap().to_owned(),
                served,
            )
        })
        .collect()
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
    let deployments = details["deployments"].as_array().unwrap();
    assert_eq!(deployments.len(), 2, "{details}");
    for deployment in deployments {
        assert_eq!(deployment["running"], published.as_str());
        assert_eq!(deployment["publication"], published.as_str());
        assert_eq!(deployment["health"], "Healthy");
    }
    let after = tip();
    assert_ne!(before, after);

    // Nothing changed but the time: no commit.
    assert_eq!(observed.shell.invoke(&["attest", "-e", "dev"]).exit, 0);
    assert_eq!(tip(), after);

    // An unchanged result is written again once the stored attestation is
    // older than the refresh interval (15 minutes), so an attestation a CI job
    // re-verifies stays fresh for `maxAttestationAge`: within the interval
    // nothing is written (above), beyond it the time moves.
    observed
        .shell
        .clock
        .advance(nyl_core::orchestration::time::Duration::from_mins(16));
    assert_eq!(observed.shell.invoke(&["attest", "-e", "dev"]).exit, 0);
    let refreshed = tip();
    assert_ne!(
        refreshed, after,
        "a stale attestation of an unchanged system is refreshed"
    );
    let record: Value = serde_saphyr::from_str(&git_output(
        &observed.shell.world.bare(),
        &[
            "show",
            &format!("{OBSERVED}:observed/units/kubernetes/attestations.yaml"),
        ],
    ))
    .unwrap();
    assert_eq!(
        record["attestations"]["healthy"]["details"],
        document["attestations"]["healthy"]["details"]
    );
    assert_eq!(observed.shell.invoke(&["attest", "-e", "dev"]).exit, 0);
    assert_eq!(tip(), refreshed);

    // A degraded workload fails `healthy`, not `accepted`.
    observed
        .shell
        .clock
        .advance(nyl_core::orchestration::time::Duration::from_mins(5));
    observed.serve_with(&published, |name| {
        Some(if name == "dev-catalog" { "Healthy" } else { "Degraded" }.to_owned())
    });
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
    assert_eq!(unit_document["resolvedSpec"]["reconciler"]["access"]["method"], "api");
    *served.lock().unwrap() = served_sources(&unit_document)
        .iter()
        .map(|(name, namespace, source)| application_with_source(name, namespace, source, &[&published], "Healthy"))
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

/// Moving how Argo CD is read, here the control-plane context's name, changes
/// no result: the next reconcile finds the receipt current, publishes nothing,
/// and the attestations of the receipt stay. Catches credential plumbing
/// re-executing a unit and resetting its evidence.
#[test]
fn test_renaming_the_kube_context_neither_executes_the_unit_nor_resets_its_attestations() {
    let observed = Observed::new("{strategy: manual}");
    let published = observed.head();
    observed.serve(&published, "Healthy", &[]);
    assert_eq!(observed.shell.invoke(&["attest", "-e", "dev"]).exit, 0);
    let record = || {
        git_output(
            &observed.shell.world.bare(),
            &[
                "show",
                &format!("{OBSERVED}:observed/units/kubernetes/attestations.yaml"),
            ],
        )
    };
    let before = record();

    let platform = git_output(&observed.shell.world.bare(), &["show", "main:nyl/platform.yaml"]);
    assert!(platform.contains("context: argocd-admin"));
    observed.shell.world.commit_files(
        "main",
        "Rename the context",
        &[(
            "nyl/platform.yaml",
            &platform.replace("context: argocd-admin", "context: renamed"),
        )],
    );
    let again = observed.shell.invoke(&["reconcile", "-e", "dev", "-o", "json"]);
    assert_eq!(again.exit, 0, "{}", again.stderr);
    assert_ne!(unit(&again.report)["result"], "executed", "{}", again.report);
    assert_eq!(observed.head(), published, "a commit was published");
    assert_eq!(record(), before, "the attestations were reset");
    let stored = git_output(
        &observed.shell.world.bare(),
        &["show", "refs/heads/nyl/dev/desired:desired/units/kubernetes.yaml"],
    );
    assert!(
        stored.contains("context: renamed"),
        "the stored document records the new context: {stored}"
    );
}

/// A smoke test that runs only against healthy workloads, end to end with the
/// real publication driver in observe mode: `reconcile` publishes and leaves
/// the smoke unit blocked on `kubernetes`'s `healthy` (a receipt is not
/// health), `attest` against a degraded Argo CD fails it and keeps the
/// smoke unit blocked as failing, and a later `attest` against a healthy one
/// unblocks it in the next `reconcile`, which executes it without executing
/// the publication again. Catches a dependent that runs on a bare receipt,
/// one that ignores the Argo CD reading, and one that stays blocked after the
/// attestation passes.
#[test]
fn test_a_dependent_waits_for_the_healthy_attestation_of_the_publication() {
    let observed = Observed::with_units(
        "{strategy: manual}",
        "---
apiVersion: units.gitops.nyl/v1
kind: Command
metadata:
  name: smoke
  labels: {dev: 'true'}
spec:
  command: [sh, -c, 'true']
  dependsOn: [{unit: kubernetes, attestations: [healthy]}]
",
        2,
    );
    let published = observed.head();
    let status = observed.shell.invoke(&["status", "-e", "dev", "-o", "json"]);
    assert_eq!(status.exit, 2, "{}", status.stderr);
    let smoke = &status.report["environments"][0]["units"]["smoke"];
    assert_eq!(smoke["state"], "blocked", "{}", status.report);
    assert_eq!(smoke["blockedBy"][0]["reason"], "attestation-pending");
    assert_eq!(smoke["blockedBy"][0]["unit"], "kubernetes");
    assert_eq!(smoke["blockedBy"][0]["attestation"], "healthy");

    // Argo CD reports a degraded workload: the attestation fails and so does the wait.
    observed.serve_with(&published, |name| {
        Some(if name == "dev-catalog" { "Healthy" } else { "Degraded" }.to_owned())
    });
    assert_eq!(observed.shell.invoke(&["attest", "-e", "dev"]).exit, 3);
    let waiting = observed.shell.invoke(&["reconcile", "-e", "dev", "-o", "json"]);
    assert_eq!(waiting.exit, 2, "{}", waiting.stderr);
    assert_eq!(unit(&waiting.report)["result"], "current");
    assert_eq!(
        waiting.report["environments"][0]["units"]["smoke"]["blockedBy"][0]["reason"],
        "attestation-failing"
    );
    assert!(waiting.stderr.contains("NYL-ATTESTATION-FAILING"), "{}", waiting.stderr);

    // It recovers; the next attest passes and the next reconcile runs the smoke test.
    observed.serve(&published, "Healthy", &[]);
    assert_eq!(observed.shell.invoke(&["attest", "-e", "dev"]).exit, 0);
    let ready = observed.shell.invoke(&["reconcile", "-e", "dev", "-o", "json"]);
    assert_eq!(ready.exit, 0, "{}", ready.stderr);
    assert_eq!(ready.report["environments"][0]["units"]["smoke"]["result"], "executed");
    assert_eq!(
        unit(&ready.report)["result"],
        "current",
        "the publication did not execute again"
    );
    assert_eq!(observed.head(), published);
}
