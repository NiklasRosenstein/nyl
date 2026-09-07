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
