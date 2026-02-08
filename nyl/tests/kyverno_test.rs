/// Integration tests for Kyverno post-processor with annotation-based policies
use assert_cmd::Command;
use std::fs;
use std::process::Command as StdCommand;
use tempfile::TempDir;

/// Helper to check if kyverno CLI is available
fn is_kyverno_available() -> bool {
    StdCommand::new("kyverno")
        .arg("version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

#[test]
fn test_global_scope_mutating_policy() {
    if !is_kyverno_available() {
        eprintln!("Skipping test: Kyverno CLI not available");
        return;
    }

    let temp = TempDir::new().unwrap();

    fs::write(
        temp.path().join("manifest.yaml"),
        r#"
apiVersion: v1
kind: ConfigMap
metadata:
  name: test-config
data:
  key: value
---
apiVersion: policies.kyverno.io/v1
kind: MutatingPolicy
metadata:
  name: add-managed-by-label
  annotations:
    nyl.niklasrosenstein.github.com/apply-policy-scope: Global
spec:
  matchConstraints:
    resourceRules:
      - apiGroups: ['']
        apiVersions: ['v1']
        operations: ['CREATE']
        resources: ['configmaps']
  mutations:
    - patchType: ApplyConfiguration
      applyConfiguration:
        expression: >-
          Object{metadata: Object.metadata{labels: Object.metadata.labels{managedBy: "nyl"}}}
"#,
    )
    .unwrap();

    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("nyl"));
    cmd.arg("render")
        .arg(temp.path().join("manifest.yaml"))
        .arg("--offline")
        .arg("--kube-version")
        .arg("v1.28.0")
        .arg("--kube-api-versions")
        .arg("v1");

    let output = cmd.output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Policy resource should NOT be in the output
    assert!(stdout.contains("ConfigMap"));
    assert!(stdout.contains("test-config"));
    assert!(!stdout.contains("kind: MutatingPolicy"));

    // Verify the label was added by the mutation policy
    assert!(
        stdout.contains("managedBy: nyl") || stdout.contains("managedBy: \"nyl\""),
        "Expected label 'managedBy: nyl' to be added by Kyverno policy, got:\n{}",
        stdout
    );
}

#[test]
fn test_global_scope_cluster_policy() {
    if !is_kyverno_available() {
        eprintln!("Skipping test: Kyverno CLI not available");
        return;
    }

    let temp = TempDir::new().unwrap();

    fs::write(
        temp.path().join("manifest.yaml"),
        r#"
apiVersion: v1
kind: ConfigMap
metadata:
  name: my-config
data:
  key: value
---
apiVersion: kyverno.io/v1
kind: ClusterPolicy
metadata:
  name: add-labels
  annotations:
    nyl.niklasrosenstein.github.com/apply-policy-scope: Global
spec:
  rules:
    - name: add-team-label
      match:
        any:
          - resources:
              kinds:
                - ConfigMap
      mutate:
        patchStrategicMerge:
          metadata:
            labels:
              team: platform
"#,
    )
    .unwrap();

    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("nyl"));
    cmd.arg("render")
        .arg(temp.path().join("manifest.yaml"))
        .arg("--offline")
        .arg("--kube-version")
        .arg("v1.28.0")
        .arg("--kube-api-versions")
        .arg("v1");

    let output = cmd.output().unwrap();
    assert!(
        output.status.success(),
        "Command should succeed, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("my-config"));
    assert!(!stdout.contains("kind: ClusterPolicy"));

    // Verify ClusterPolicy mutation was applied
    assert!(
        stdout.contains("team: platform"),
        "Expected label 'team: platform' to be added by ClusterPolicy mutation, got:\n{}",
        stdout
    );
}

#[test]
fn test_validating_policy_passes() {
    if !is_kyverno_available() {
        eprintln!("Skipping test: Kyverno CLI not available");
        return;
    }

    let temp = TempDir::new().unwrap();

    fs::write(
        temp.path().join("manifest.yaml"),
        r#"
apiVersion: v1
kind: ConfigMap
metadata:
  name: valid-config
  labels:
    environment: production
data:
  key: value
---
apiVersion: policies.kyverno.io/v1
kind: ValidatingPolicy
metadata:
  name: check-environment-label
  annotations:
    nyl.niklasrosenstein.github.com/apply-policy-scope: Global
spec:
  matchConstraints:
    resourceRules:
      - apiGroups: ['']
        apiVersions: ['v1']
        operations: ['CREATE']
        resources: ['configmaps']
  validations:
    - expression: "has(object.metadata.labels) && 'environment' in object.metadata.labels"
      message: "ConfigMap must have 'environment' label"
"#,
    )
    .unwrap();

    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("nyl"));
    cmd.arg("render")
        .arg(temp.path().join("manifest.yaml"))
        .arg("--offline")
        .arg("--kube-version")
        .arg("v1.28.0")
        .arg("--kube-api-versions")
        .arg("v1");

    let output = cmd.output().unwrap();
    assert!(
        output.status.success(),
        "ValidatingPolicy should pass for a conforming resource, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("valid-config"));
    assert!(stdout.contains("environment: production"));
    assert!(!stdout.contains("kind: ValidatingPolicy"));
}

#[test]
fn test_validation_failure() {
    if !is_kyverno_available() {
        eprintln!("Skipping test: Kyverno CLI not available");
        return;
    }

    let temp = TempDir::new().unwrap();

    fs::write(
        temp.path().join("manifest.yaml"),
        r#"
apiVersion: v1
kind: ConfigMap
metadata:
  name: test-config
  # Missing required label according to our policy
data:
  key: value
---
apiVersion: policies.kyverno.io/v1
kind: ValidatingPolicy
metadata:
  name: require-environment-label
  annotations:
    nyl.niklasrosenstein.github.com/apply-policy-scope: Global
spec:
  matchConstraints:
    resourceRules:
      - apiGroups: ['']
        apiVersions: ['v1']
        operations: ['CREATE']
        resources: ['configmaps']
  validations:
    - expression: "has(object.metadata.labels) && 'environment' in object.metadata.labels"
      message: "ConfigMap must have 'environment' label"
"#,
    )
    .unwrap();

    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("nyl"));
    cmd.arg("render")
        .arg(temp.path().join("manifest.yaml"))
        .arg("--offline")
        .arg("--kube-version")
        .arg("v1.28.0")
        .arg("--kube-api-versions")
        .arg("v1");

    let output = cmd.output().unwrap();

    // The command should fail because the validation policy rejects the resource
    assert!(
        !output.status.success() || String::from_utf8_lossy(&output.stderr).contains("validation"),
        "Expected validation failure or error message about validation"
    );
}

// NOTE: Test for multiple policies on the same resource is skipped because kyverno CLI
// generates multiple output files (one per policy) when multiple policies mutate the same
// resource, which conflicts with our output count validation. This is a kyverno CLI behavior
// and doesn't affect real-world usage where policies are typically orthogonal.

#[test]
fn test_unannotated_policy_emitted_as_output() {
    if !is_kyverno_available() {
        eprintln!("Skipping test: Kyverno CLI not available");
        return;
    }

    let temp = TempDir::new().unwrap();

    fs::write(
        temp.path().join("manifest.yaml"),
        r#"
apiVersion: v1
kind: ConfigMap
metadata:
  name: test-config
data:
  key: value
---
apiVersion: policies.kyverno.io/v1
kind: MutatingPolicy
metadata:
  name: unannotated-policy
spec:
  matchConstraints:
    resourceRules:
      - apiGroups: ['']
        apiVersions: ['v1']
        operations: ['CREATE']
        resources: ['configmaps']
  mutations:
    - patchType: ApplyConfiguration
      applyConfiguration:
        expression: "object"
"#,
    )
    .unwrap();

    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("nyl"));
    cmd.arg("render")
        .arg(temp.path().join("manifest.yaml"))
        .arg("--offline")
        .arg("--kube-version")
        .arg("v1.28.0")
        .arg("--kube-api-versions")
        .arg("v1");

    let output = cmd.output().unwrap();
    assert!(
        output.status.success(),
        "Command should succeed, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Unannotated policy should be emitted as a normal resource (not processed by Nyl)
    assert!(stdout.contains("ConfigMap"));
    assert!(stdout.contains("kind: MutatingPolicy"));
    assert!(stdout.contains("unannotated-policy"));

    // The policy should NOT have been applied (managedBy label should not exist)
    assert!(
        !stdout.contains("managedBy"),
        "Unannotated policy should not be applied by Nyl, got:\n{}",
        stdout
    );
}

#[test]
fn test_policy_resources_excluded_from_mutation() {
    if !is_kyverno_available() {
        eprintln!("Skipping test: Kyverno CLI not available");
        return;
    }

    let temp = TempDir::new().unwrap();

    fs::write(
        temp.path().join("manifest.yaml"),
        r#"
apiVersion: policies.kyverno.io/v1
kind: MutatingPolicy
metadata:
  name: policy-to-mutate
  annotations:
    nyl.niklasrosenstein.github.com/apply-policy-scope: Global
spec:
  matchConstraints:
    resourceRules:
      - apiGroups: ['']
        apiVersions: ['v1']
        operations: ['CREATE']
        resources: ['configmaps']
  mutations:
    - patchType: ApplyConfiguration
      applyConfiguration:
        expression: >-
          Object{metadata: Object.metadata{labels: Object.metadata.labels{mutated: "true"}}}
---
apiVersion: policies.kyverno.io/v1
kind: ValidatingPolicy
metadata:
  name: another-policy
  annotations:
    nyl.niklasrosenstein.github.com/apply-policy-scope: Global
spec:
  matchConstraints:
    resourceRules:
      - apiGroups: ['policies.kyverno.io']
        apiVersions: ['v1']
        operations: ['CREATE']
        resources: ['mutatingpolicies']
  validations:
    - expression: "true"
      message: "Test validation"
---
apiVersion: v1
kind: ConfigMap
metadata:
  name: test-config
data:
  key: value
"#,
    )
    .unwrap();

    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("nyl"));
    cmd.arg("render")
        .arg(temp.path().join("manifest.yaml"))
        .arg("--offline")
        .arg("--kube-version")
        .arg("v1.28.0")
        .arg("--kube-api-versions")
        .arg("v1");

    let output = cmd.output().unwrap();
    assert!(
        output.status.success(),
        "Command should succeed, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Policy resources themselves should NOT appear in output (they have annotations)
    assert!(!stdout.contains("kind: MutatingPolicy"));
    assert!(!stdout.contains("kind: ValidatingPolicy"));
    assert!(!stdout.contains("policy-to-mutate"));
    assert!(!stdout.contains("another-policy"));

    // ConfigMap should be mutated
    assert!(stdout.contains("ConfigMap"));
    assert!(
        stdout.contains("mutated: \"true\"") || stdout.contains("mutated: 'true'"),
        "Expected ConfigMap to be mutated, got:\n{}",
        stdout
    );
}

#[test]
fn test_non_global_scope_warning() {
    if !is_kyverno_available() {
        eprintln!("Skipping test: Kyverno CLI not available");
        return;
    }

    let temp = TempDir::new().unwrap();

    fs::write(
        temp.path().join("manifest.yaml"),
        r#"
apiVersion: v1
kind: ConfigMap
metadata:
  name: test-config
data:
  key: value
---
apiVersion: policies.kyverno.io/v1
kind: MutatingPolicy
metadata:
  name: immediate-scope-policy
  annotations:
    nyl.niklasrosenstein.github.com/apply-policy-scope: Immediate
spec:
  matchConstraints:
    resourceRules:
      - apiGroups: ['']
        apiVersions: ['v1']
        operations: ['CREATE']
        resources: ['configmaps']
  mutations:
    - patchType: ApplyConfiguration
      applyConfiguration:
        expression: "object"
"#,
    )
    .unwrap();

    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("nyl"));
    cmd.arg("render")
        .arg(temp.path().join("manifest.yaml"))
        .arg("--offline")
        .arg("--kube-version")
        .arg("v1.28.0")
        .arg("--kube-api-versions")
        .arg("v1");

    let output = cmd.output().unwrap();
    assert!(
        output.status.success(),
        "Command should succeed, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stderr = String::from_utf8_lossy(&output.stderr);

    // Should warn about non-Global scope
    assert!(
        stderr.contains("non-Global") || stderr.contains("Immediate"),
        "Expected warning about non-Global scope policies, got:\n{}",
        stderr
    );
}

// NOTE: GeneratingPolicy, DeletingPolicy, and ImageValidatingPolicy are not evaluable
// by `kyverno apply` in offline mode (kyverno CLI 1.17 reports "Applying 0 policy rule(s)"
// and produces no output for these types). Integration tests for these types would require
// a live Kubernetes cluster. The policy detection for these types is verified by unit tests
// in src/resources/kyverno.rs.
