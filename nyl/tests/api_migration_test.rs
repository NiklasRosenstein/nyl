use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

#[test]
fn render_rejects_obsolete_expansion_and_release_envelopes() {
    let project = TempDir::new().unwrap();
    std::fs::write(project.path().join("nyl.toml"), "[project]\n").unwrap();
    for (kind, api, expected) in [
        ("Release", "gitops.nyl/v1", "k8s.gitops.nyl/v1"),
        ("HelmChart", "nyl.niklasrosenstein.github.com/v1", "k8s.nyl/v1"),
        ("RemoteManifest", "nyl.niklasrosenstein.github.com/v1", "k8s.nyl/v1"),
        (
            "example/v1/App",
            "components.nyl.niklasrosenstein.github.com/v1",
            "components.k8s.nyl/v1",
        ),
    ] {
        std::fs::write(
            project.path().join("resource.yaml"),
            format!("apiVersion: {api}\nkind: {kind}\nmetadata:\n  name: test\n  namespace: default\n"),
        )
        .unwrap();
        Command::new(assert_cmd::cargo::cargo_bin!("nyl"))
            .current_dir(project.path())
            .args(["render", "resource.yaml"])
            .assert()
            .failure()
            .stderr(predicate::str::contains(format!("set apiVersion to \"{expected}\"")));
    }
}

#[test]
fn discovery_rejects_obsolete_envelopes_with_structurally_templated_specs() {
    let project = TempDir::new().unwrap();
    git2::Repository::init(project.path()).unwrap();
    std::fs::write(project.path().join("nyl.toml"), "[project]\n").unwrap();
    for (kind, api, expected) in [
        ("ApplicationGroup", "gitops.nyl/v1", "k8s.gitops.nyl/v1"),
        ("Release", "gitops.nyl/v1", "k8s.gitops.nyl/v1"),
    ] {
        std::fs::write(project.path().join("resource.yaml"), format!("apiVersion: {api}\nkind: {kind}\nmetadata:\n  name: test\nspec:\n{{% if values.enabled %}}\n  enabled: true\n{{% endif %}}\n")).unwrap();
        let error = nyl::gitops::discover_gitops_inventory(project.path(), None)
            .unwrap_err()
            .to_string();
        assert!(error.contains(expected), "{error}");
    }
}

fn chart_emitting_helm_chart(api_version: &str) -> TempDir {
    let project = TempDir::new().unwrap();
    std::fs::write(
        project.path().join("nyl.toml"),
        "[project]\nhelm_chart_search_paths = [\".\"]\n",
    )
    .unwrap();
    let chart = project.path().join("charts/wrapper");
    std::fs::create_dir_all(chart.join("templates")).unwrap();
    std::fs::write(
        chart.join("Chart.yaml"),
        "apiVersion: v2\nname: wrapper\nversion: 1.0.0\n",
    )
    .unwrap();
    std::fs::write(
        chart.join("templates/chart.yaml"),
        format!("apiVersion: {api_version}\nkind: HelmChart\nmetadata:\n  name: generated\n  namespace: default\nspec:\n  chart: example\n"),
    ).unwrap();
    std::fs::write(
        project.path().join("resource.yaml"),
        "apiVersion: k8s.nyl/v1\nkind: HelmChart\nmetadata:\n  name: wrapper\n  namespace: default\nspec:\n  chart:\n    name: ./charts/wrapper\n",
    ).unwrap();
    project
}

fn render_chart(project: &TempDir) -> Command {
    let mut command = Command::new(assert_cmd::cargo::cargo_bin!("nyl"));
    command
        .current_dir(project.path())
        .timeout(std::time::Duration::from_secs(30))
        .args([
            "render",
            "resource.yaml",
            "--offline",
            "--no-cache",
            "--kube-version",
            "1.34.0",
            "--kube-api-versions",
            "v1,apps/v1",
        ]);
    command
}

#[test]
fn generated_api_error_identifies_source_parent_and_generated_resource() {
    let project = chart_emitting_helm_chart("nyl.niklasrosenstein.github.com/v1");
    render_chart(&project)
        .assert()
        .failure()
        .stderr(predicate::str::contains("resource.yaml (document 1)"))
        .stderr(predicate::str::contains(
            "Resource: k8s.nyl/v1 HelmChart default/wrapper (chart: ./charts/wrapper)",
        ))
        .stderr(predicate::str::contains(
            "Resource: nyl.niklasrosenstein.github.com/v1 HelmChart default/generated",
        ))
        .stderr(predicate::str::contains("set apiVersion to \"k8s.nyl/v1\""));
}

#[test]
fn generated_helm_chart_in_kubernetes_api_group_is_preserved() {
    let project = chart_emitting_helm_chart("helm.cattle.io/v1");
    render_chart(&project)
        .assert()
        .success()
        .stdout(predicate::str::contains("apiVersion: helm.cattle.io/v1"))
        .stdout(predicate::str::contains("kind: HelmChart"))
        .stdout(predicate::str::contains("name: generated"))
        .stdout(predicate::str::contains("chart: example"));
}
