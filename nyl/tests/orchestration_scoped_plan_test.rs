//! A scoped producer plan still resolves the desired state that a reconcile
//! would publish, so omitted changes must withhold old receipts from consumers.
mod scenario;

use scenario::shell::{self, project, NYL_TOML};

const NETWORK: &str = r#"apiVersion: units.gitops.nyl/v1
kind: Command
metadata: {name: network, labels: {scope: infra}}
spec:
  command:
    - sh
    - -c
    - |
      printf '%s' '{"id":"vpc-one"}' > "$NYL_OUTPUTS"
  idempotent: true
  outputs:
    id: {type: string}
"#;

#[test]
fn test_scoped_plan_blocks_on_changed_producer_omitted_from_report() {
    let environment = |name: &str| {
        format!("apiVersion: gitops.nyl/v1\nkind: Environment\nmetadata: {{name: {name}}}\nspec:\n  unitSelector:\n    matchLabels: {{scope: {name}}}\n  allowUnprotectedSource: true\n")
    };
    let project = project(&[
        NYL_TOML,
        ("nyl/infra.yaml", &environment("infra")),
        ("nyl/app.yaml", &environment("app")),
        ("nyl/network.yaml", NETWORK),
        ("nyl/noop.yaml", "apiVersion: units.gitops.nyl/v1\nkind: Command\nmetadata: {name: noop, labels: {scope: infra}}\nspec:\n  command: [sh, -c, 'true']\n  idempotent: true\n"),
        ("nyl/consumer.yaml", "apiVersion: units.gitops.nyl/v1\nkind: Command\nmetadata: {name: app, labels: {scope: app}}\nspec:\n  command: [sh, -c, 'true']\n  idempotent: true\n  values:\n    network: {fromUnit: {environment: infra, unit: network, output: id}}\n"),
    ]);
    let harness = shell::Harness::new(project.path(), true);
    for name in ["infra", "app"] {
        let initialized = harness.invoke(&["state", "init", "-e", name]);
        assert_eq!(initialized.exit, 0, "{}", initialized.stderr);
    }
    let run = harness.invoke(&["reconcile", "-e", "infra", "-e", "app"]);
    assert_eq!(run.exit, 0, "{}", run.stderr);
    let current = harness.invoke(&["plan", "-e", "infra", "-e", "app", "--unit", "app/app", "-o", "json"]);
    assert_eq!(current.exit, 0, "{}", current.stderr);

    harness.world.commit_files(
        "main",
        "change producer excluded from plan output",
        &[("nyl/network.yaml", &NETWORK.replace("vpc-one", "vpc-two"))],
    );
    for selected in [vec!["app/app"], vec!["infra/noop", "app/app"]] {
        let mut args = vec!["plan", "-e", "infra", "-e", "app", "-o", "json"];
        for unit in &selected {
            args.extend(["--unit", unit]);
        }
        let plan = harness.invoke(&args);
        assert_eq!(plan.exit, 2, "{}", plan.stderr);
        let environments = plan.report["environments"].as_array().unwrap();
        let infra = environments.iter().find(|section| section["name"] == "infra").unwrap();
        assert!(
            infra["units"].get("network").is_none(),
            "the --unit output filter still applies"
        );
        assert_eq!(infra["units"].as_object().unwrap().len(), selected.len() - 1);
        let app = &environments.iter().find(|section| section["name"] == "app").unwrap()["units"]["app"];
        assert_eq!(app["state"], "blocked");
        assert!(
            app["blockedBy"]
                .to_string()
                .contains("infra/network has a planned change"),
            "{app}"
        );
    }
}
