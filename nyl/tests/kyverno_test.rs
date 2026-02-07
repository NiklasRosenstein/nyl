/// Integration tests for Kyverno post-processor
use assert_cmd::Command;
use predicates::prelude::*;
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
fn test_kyverno_resources_are_filtered() {
    let temp = TempDir::new().unwrap();

    // Create a manifest with Kyverno resource
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
apiVersion: post-processing.nyl.niklasrosenstein.github.com/v1
kind: Kyverno
metadata:
  name: test-policy
spec:
  scope: Global
  mutatingPolicyRules:
    - name: add-label
      match:
        resources:
          kinds:
            - ConfigMap
      mutate:
        patchStrategicMerge:
          metadata:
            labels:
              managed-by: nyl
"#,
    )
    .unwrap();

    let mut cmd = Command::cargo_bin("nyl").unwrap();
    cmd.arg("render")
        .arg(temp.path().join("manifest.yaml"))
        .arg("--offline")
        .arg("--kube-version")
        .arg("v1.28.0")
        .arg("--kube-api-versions")
        .arg("v1");

    let output = cmd.output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Kyverno resource should NOT be in the output
    assert!(stdout.contains("ConfigMap"));
    assert!(stdout.contains("test-config"));
    assert!(!stdout.contains("kind: Kyverno"));

    // If Kyverno CLI is available, verify the label was added
    if is_kyverno_available() {
        assert!(
            stdout.contains("managed-by: nyl"),
            "Expected label 'managed-by: nyl' to be added by Kyverno policy"
        );
    } else {
        // If Kyverno is not available, verify we got the warning
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("Kyverno CLI is not installed"),
            "Expected warning about Kyverno CLI not being installed"
        );
    }
}

#[test]
fn test_multiple_kyverno_resources() {
    let temp = TempDir::new().unwrap();

    // Create a manifest with multiple Kyverno resources
    fs::write(
        temp.path().join("manifest.yaml"),
        r#"
apiVersion: v1
kind: Service
metadata:
  name: test-svc
spec:
  type: ClusterIP
  ports:
    - port: 80
---
apiVersion: post-processing.nyl.niklasrosenstein.github.com/v1
kind: Kyverno
metadata:
  name: policy-1
spec:
  scope: Global
  clusterPolicyRules:
    - name: rule1
---
apiVersion: post-processing.nyl.niklasrosenstein.github.com/v1
kind: Kyverno
metadata:
  name: policy-2
spec:
  scope: Local
  mutatingPolicyRules:
    - name: rule2
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: test-app
spec:
  replicas: 1
  selector:
    matchLabels:
      app: test
  template:
    metadata:
      labels:
        app: test
    spec:
      containers:
        - name: nginx
          image: nginx
"#,
    )
    .unwrap();

    let mut cmd = Command::cargo_bin("nyl").unwrap();
    cmd.arg("render")
        .arg(temp.path().join("manifest.yaml"))
        .arg("--offline")
        .arg("--kube-version")
        .arg("v1.28.0")
        .arg("--kube-api-versions")
        .arg("v1,apps/v1");

    // Both Kyverno resources should be filtered out
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("Service"))
        .stdout(predicate::str::contains("Deployment"))
        .stdout(predicate::str::contains("Kyverno").not())
        .stdout(predicate::str::contains("policy-1").not())
        .stdout(predicate::str::contains("policy-2").not());
}

#[test]
fn test_kyverno_with_inline_policies() {
    let temp = TempDir::new().unwrap();

    // Create a manifest with inline policies
    fs::write(
        temp.path().join("manifest.yaml"),
        r#"
apiVersion: v1
kind: Pod
metadata:
  name: test-pod
spec:
  containers:
    - name: nginx
      image: nginx
---
apiVersion: post-processing.nyl.niklasrosenstein.github.com/v1
kind: Kyverno
metadata:
  name: inline-policy
spec:
  scope: Global
  inlinePolicies:
    - apiVersion: kyverno.io/v1
      kind: ClusterPolicy
      metadata:
        name: test-policy
      spec:
        rules:
          - name: test-rule
            match:
              resources:
                kinds:
                  - Pod
"#,
    )
    .unwrap();

    let mut cmd = Command::cargo_bin("nyl").unwrap();
    cmd.arg("render")
        .arg(temp.path().join("manifest.yaml"))
        .arg("--offline")
        .arg("--kube-version")
        .arg("v1.28.0")
        .arg("--kube-api-versions")
        .arg("v1");

    cmd.assert()
        .success()
        .stdout(predicate::str::contains("Pod"))
        .stdout(predicate::str::contains("test-pod"))
        // Kyverno resource should not be in output
        .stdout(predicate::str::contains("kind: Kyverno").not());
}

#[test]
fn test_kyverno_scope_variations() {
    let temp = TempDir::new().unwrap();

    // Test all scope types
    for scope in &["Local", "Root", "Global"] {
        let manifest = format!(
            r#"
apiVersion: v1
kind: ConfigMap
metadata:
  name: test-config
data:
  scope: {}
---
apiVersion: post-processing.nyl.niklasrosenstein.github.com/v1
kind: Kyverno
metadata:
  name: test-policy
spec:
  scope: {}
  clusterPolicyRules:
    - name: test-rule
"#,
            scope, scope
        );

        fs::write(temp.path().join("manifest.yaml"), manifest).unwrap();

        let mut cmd = Command::cargo_bin("nyl").unwrap();
        cmd.arg("render")
            .arg(temp.path().join("manifest.yaml"))
            .arg("--offline")
            .arg("--kube-version")
            .arg("v1.28.0")
            .arg("--kube-api-versions")
            .arg("v1");

        cmd.assert()
            .success()
            .stdout(predicate::str::contains("ConfigMap"))
            .stdout(predicate::str::contains("Kyverno").not());
    }
}

#[test]
#[ignore] // Only runs when Kyverno CLI is installed
fn test_kyverno_validation_failure() {
    if !is_kyverno_available() {
        eprintln!("Skipping test: Kyverno CLI not available");
        return;
    }

    let temp = TempDir::new().unwrap();

    // Create a manifest with a validation policy that will reject the resource
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
apiVersion: post-processing.nyl.niklasrosenstein.github.com/v1
kind: Kyverno
metadata:
  name: require-label-policy
spec:
  scope: Global
  validatingPolicyRules:
    - name: require-environment-label
      match:
        resources:
          kinds:
            - ConfigMap
      validate:
        message: "ConfigMap must have 'environment' label"
        pattern:
          metadata:
            labels:
              environment: "?*"
"#,
    )
    .unwrap();

    let mut cmd = Command::cargo_bin("nyl").unwrap();
    cmd.arg("render")
        .arg(temp.path().join("manifest.yaml"))
        .arg("--offline")
        .arg("--kube-version")
        .arg("v1.28.0")
        .arg("--kube-api-versions")
        .arg("v1");

    // The command should fail because the validation policy rejects the resource
    let output = cmd.output().unwrap();

    // Kyverno apply should fail when validation doesn't pass
    // This tests that we properly handle validation failures
    assert!(
        !output.status.success() || String::from_utf8_lossy(&output.stderr).contains("validation"),
        "Expected validation failure or error message about validation"
    );
}

#[test]
#[ignore] // Only runs when Kyverno CLI is installed
fn test_kyverno_mutation_applied() {
    if !is_kyverno_available() {
        eprintln!("Skipping test: Kyverno CLI not available");
        return;
    }

    let temp = TempDir::new().unwrap();

    // Create a manifest with a mutation policy
    fs::write(
        temp.path().join("manifest.yaml"),
        r#"
apiVersion: v1
kind: Service
metadata:
  name: test-service
spec:
  type: LoadBalancer
  ports:
    - port: 80
---
apiVersion: post-processing.nyl.niklasrosenstein.github.com/v1
kind: Kyverno
metadata:
  name: mutate-loadbalancer
spec:
  scope: Global
  mutatingPolicyRules:
    - name: add-loadbalancer-class
      match:
        resources:
          kinds:
            - Service
      mutate:
        patchStrategicMerge:
          spec:
            (type): LoadBalancer
            loadBalancerClass: ngrok
            allocateLoadBalancerNodePorts: false
"#,
    )
    .unwrap();

    let mut cmd = Command::cargo_bin("nyl").unwrap();
    cmd.arg("render")
        .arg(temp.path().join("manifest.yaml"))
        .arg("--offline")
        .arg("--kube-version")
        .arg("v1.28.0")
        .arg("--kube-api-versions")
        .arg("v1");

    let output = cmd.output().unwrap();
    assert!(output.status.success(), "Command should succeed");

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Verify the mutation was applied
    assert!(
        stdout.contains("loadBalancerClass: ngrok"),
        "Expected loadBalancerClass to be added by Kyverno mutation policy"
    );
    assert!(
        stdout.contains("allocateLoadBalancerNodePorts: false"),
        "Expected allocateLoadBalancerNodePorts to be set by Kyverno mutation policy"
    );
}
