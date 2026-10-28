//! Approvals fence effects and retain their audit identity in Git state.
mod scenario;

use scenario::shell::{command, project, Harness, ENVIRONMENT, NYL_TOML};
use serde_json::Value;
use tempfile::TempDir;

fn result(report: &Value, unit: &str) -> String {
    report["environments"][0]["units"][unit]["result"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[test]
fn test_remote_manual_approval_records_asserted_identity_and_teardown_approval() {
    let effects = TempDir::new().unwrap();
    let marker = effects.path().join("effect");
    let unit = command(
        "job",
        &format!("touch {}", marker.display()),
        &format!("  approval: manual\n  teardown: [sh, -c, 'rm {}']\n", marker.display()),
    );
    let project = project(&[NYL_TOML, ("nyl/environment.yaml", ENVIRONMENT), ("nyl/job.yaml", &unit)]);
    let shell = Harness::new(project.path(), true);
    assert_eq!(shell.invoke(&["state", "init", "-e", "dev"]).exit, 0);
    let waiting = shell.invoke(&["reconcile", "-e", "dev", "-o", "json"]);
    assert_eq!(waiting.exit, 2, "{}", waiting.stderr);
    assert_eq!(result(&waiting.report, "job"), "awaiting-approval");
    assert!(!marker.exists());
    let approved = shell.invoke(&[
        "reconcile",
        "-e",
        "dev",
        "--approve",
        "job",
        "--approved-by",
        "alice,bob",
        "--approval-source",
        "https://ci.invalid/run/1",
        "-o",
        "json",
    ]);
    assert_eq!(approved.exit, 0, "{}", approved.stderr);
    assert!(marker.exists());
    let snapshot = shell.snapshot();
    let receipt = snapshot.observed.values().next().unwrap().receipt.as_ref().unwrap();
    let approval = receipt.approval.as_ref().unwrap();
    assert_eq!(
        serde_json::to_value(approval).unwrap(),
        serde_json::json!({"by":["alice","bob"],"identity":"asserted","source":"https://ci.invalid/run/1","digest":null,"reviewed":false})
    );
    assert!(shell
        .message("refs/heads/nyl/dev/observed")
        .contains("https://ci.invalid/run/1"));
    assert_eq!(shell.invoke(&["teardown", "-e", "dev", "--unit", "job"]).exit, 2);
    assert!(marker.exists());
    let removed = shell.invoke(&[
        "teardown",
        "-e",
        "dev",
        "--unit",
        "job",
        "--approve",
        "job",
        "--approved-by",
        "alice",
        "-o",
        "json",
    ]);
    assert_eq!(removed.exit, 0, "{}", removed.stderr);
    assert!(!marker.exists());
    assert_eq!(
        removed.report["environments"][0]["units"]["job"]["approval"]["by"],
        serde_json::json!(["alice"])
    );
}

#[test]
fn test_reviewed_apply_and_destroy_digests_fence_effects() {
    let backend = TempDir::new().unwrap();
    let state = backend.path().join("state.json");
    let unit = format!("apiVersion: units.gitops.nyl/v1\nkind: OpenTofu\nmetadata:\n  name: db\n  labels: {{dev: 'true'}}\nspec:\n  source: {{path: /infra}}\n  backend: {{path: '{}'}}\n  variables: {{region: east}}\n  approval: {{mode: manual, bind: plan, requireDigest: true}}\n", state.display());
    let project = project(&[
        NYL_TOML,
        ("nyl/environment.yaml", ENVIRONMENT),
        ("nyl/db.yaml", &unit),
        ("infra/main.tf", "# fake infrastructure"),
    ]);
    let shell = Harness::new(project.path(), true);
    assert_eq!(shell.invoke(&["state", "init", "-e", "dev"]).exit, 0);
    let plan = shell.invoke(&["plan", "-e", "dev", "-o", "json"]);
    let digest = plan.report["environments"][0]["units"]["db"]["changeDigest"]
        .as_str()
        .expect("manual plan has digest");
    let skipped = shell.invoke(&["reconcile", "-e", "dev", "--approve-all", "-o", "json"]);
    assert_eq!(skipped.exit, 2, "{}", skipped.stderr);
    assert_eq!(result(&skipped.report, "db"), "awaiting-approval");
    assert!(!state.exists());
    let wrong = format!("db=sha256:{}", "0".repeat(64));
    let mismatch = shell.invoke(&["reconcile", "-e", "dev", "--approve", &wrong, "-o", "json"]);
    assert_eq!(mismatch.exit, 2, "{}", mismatch.stderr);
    assert_eq!(
        mismatch.report["environments"][0]["units"]["db"]["changeDigest"],
        digest
    );
    assert!(!state.exists());
    let approved = format!("db={digest}");
    let applied = shell.invoke(&["reconcile", "-e", "dev", "--approve", &approved, "-o", "json"]);
    assert_eq!(applied.exit, 0, "{}", applied.stderr);
    assert!(state.exists());
    let waiting = shell.invoke(&[
        "teardown",
        "-e",
        "dev",
        "--unit",
        "db",
        "--approve",
        &wrong,
        "-o",
        "json",
    ]);
    assert_eq!(waiting.exit, 2, "{}", waiting.stderr);
    assert!(state.exists());
    let destroy = shell.invoke(&["plan", "-e", "dev", "--teardown", "--unit", "db", "-o", "json"]);
    let digest = destroy.report["environments"][0]["units"]["db"]["changeDigest"]
        .as_str()
        .expect("destroy plan digest");
    let removed = shell.invoke(&[
        "teardown",
        "-e",
        "dev",
        "--unit",
        "db",
        "--approve",
        &format!("db={digest}"),
        "-o",
        "json",
    ]);
    assert_eq!(removed.exit, 0, "{}", removed.stderr);
    assert!(!state.exists());
}

#[test]
fn test_failed_approval_lookup_prevents_effects() {
    let marker = TempDir::new().unwrap();
    let path = marker.path().join("effect");
    let unit = command("job", &format!("touch {}", path.display()), "  approval: manual\n");
    let project = project(&[
        ("nyl/nyl.toml", "[approval]\nlookup = ['sh', '-c', 'echo invalid']\n"),
        ("nyl/environment.yaml", ENVIRONMENT),
        ("nyl/job.yaml", &unit),
    ]);
    let shell = Harness::new(project.path(), true);
    assert_eq!(shell.invoke(&["state", "init", "-e", "dev"]).exit, 0);
    let before = shell.tip("refs/heads/nyl/dev/desired");
    let failure = shell.invoke(&["reconcile", "-e", "dev", "--approve", "job", "-o", "json"]);
    assert_eq!(failure.exit, 1, "{}", failure.stderr);
    assert!(failure.stderr.contains("NYL-APPROVAL-INVALID"));
    assert!(!path.exists());
    assert_eq!(shell.tip("refs/heads/nyl/dev/desired"), before);
}

#[test]
fn test_lookup_hook_receives_context_and_records_verified_approvers() {
    let config = r#"[approval]
lookup = ["sh", "-c", '''test "$NYL_ENVIRONMENT" = dev && test "$NYL_APPROVAL_UNITS" = '["job"]' && printf '%s' '{"by":["alice"],"source":"gate"}' ''']
"#;
    let unit = command("job", "true", "  approval: manual\n");
    let project = project(&[
        ("nyl/nyl.toml", config),
        ("nyl/environment.yaml", ENVIRONMENT),
        ("nyl/job.yaml", &unit),
    ]);
    let shell = Harness::new(project.path(), true);
    assert_eq!(shell.invoke(&["state", "init", "-e", "dev"]).exit, 0);
    let approved = shell.invoke(&["reconcile", "-e", "dev", "--approve", "job", "-o", "json"]);
    assert_eq!(approved.exit, 0, "{}", approved.stderr);
    let snapshot = shell.snapshot();
    let approval = snapshot
        .observed
        .values()
        .next()
        .unwrap()
        .receipt
        .as_ref()
        .unwrap()
        .approval
        .as_ref()
        .unwrap();
    assert_eq!(serde_json::to_value(approval).unwrap()["identity"], "verified");
    assert_eq!(approval.by, ["alice"]);
    assert_eq!(approval.source.as_deref(), Some("hook sh gate"));
}
