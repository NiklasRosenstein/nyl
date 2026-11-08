//! Generated resources inherit the Argo CD sync wave and phase of the
//! resource that expands into them, through nested expansion.

use assert_cmd::Command;
use serde_json::{json, Value};
use std::fs;
use std::path::Path;
use tempfile::TempDir;

fn write_chart(directory: &Path, name: &str, templates: &[(&str, &str)]) {
    fs::create_dir_all(directory.join("templates")).unwrap();
    fs::write(
        directory.join("Chart.yaml"),
        format!("apiVersion: v2\nname: {name}\nversion: 1.0.0\n"),
    )
    .unwrap();
    for (file, content) in templates {
        fs::write(directory.join("templates").join(file), content).unwrap();
    }
}

fn annotations(documents: &[Value], kind: &str, name: &str) -> Value {
    documents
        .iter()
        .find(|document| document["kind"] == kind && document["metadata"]["name"] == name)
        .unwrap_or_else(|| panic!("{kind} {name} is not rendered"))["metadata"]["annotations"]
        .clone()
}

#[test]
fn test_render_inherits_sync_wave_and_phase_through_nested_expansion() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    fs::write(root.join("nyl.toml"), "[project]\nhelm_chart_search_paths = [\".\"]\n").unwrap();
    write_chart(
        &root.join("charts/outer"),
        "outer",
        &[
            (
                "config.yaml",
                "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: outer-config\n",
            ),
            (
                "job.yaml",
                "apiVersion: batch/v1\nkind: Job\nmetadata:\n  name: outer-job\n  annotations:\n    \
                 argocd.argoproj.io/sync-wave: \"1\"\n    argocd.argoproj.io/hook: Sync\n\
                 spec:\n  template:\n    spec:\n      restartPolicy: Never\n      containers: []\n",
            ),
            (
                "inner.yaml",
                "apiVersion: k8s.nyl/v1\nkind: HelmChart\nmetadata:\n  name: inner\nspec:\n  chart:\n    \
                 name: ./charts/inner\n",
            ),
        ],
    );
    write_chart(
        &root.join("charts/inner"),
        "inner",
        &[(
            "config.yaml",
            "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: inner-config\n",
        )],
    );
    fs::write(
        root.join("release.yaml"),
        r#"
apiVersion: k8s.nyl/v1
kind: HelmChart
metadata:
  name: outer
  namespace: default
  annotations:
    argocd.argoproj.io/sync-wave: "-2"
    argocd.argoproj.io/hook: PreSync
    argocd.argoproj.io/hook-delete-policy: BeforeHookCreation
spec:
  chart:
    name: ./charts/outer
"#,
    )
    .unwrap();

    let output = Command::new(assert_cmd::cargo::cargo_bin!("nyl"))
        .current_dir(root)
        .args([
            "render",
            "--offline",
            "--kube-version",
            "1.30.0",
            "--kube-api-versions",
            "v1,batch/v1",
            "release.yaml",
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let documents = serde_saphyr::from_multiple::<Value>(&String::from_utf8(output.stdout).unwrap()).unwrap();

    let inherited = json!({
        "argocd.argoproj.io/sync-wave": "-2",
        "argocd.argoproj.io/hook": "PreSync",
        "argocd.argoproj.io/hook-delete-policy": "BeforeHookCreation",
    });
    assert_eq!(annotations(&documents, "ConfigMap", "outer-config"), inherited);
    assert_eq!(annotations(&documents, "ConfigMap", "inner-config"), inherited);
    assert_eq!(
        annotations(&documents, "Job", "outer-job"),
        json!({"argocd.argoproj.io/sync-wave": "1", "argocd.argoproj.io/hook": "Sync"})
    );
}
