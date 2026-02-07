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
    if !is_kyverno_available() {
        eprintln!("Skipping test: Kyverno CLI not available");
        return;
    }

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
  mutatingPolicies:
    - name: add-managed-by-label
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

    // Kyverno resource should NOT be in the output
    assert!(stdout.contains("ConfigMap"));
    assert!(stdout.contains("test-config"));
    assert!(!stdout.contains("kind: Kyverno"));

    // Verify the label was added by the mutation policy
    assert!(
        stdout.contains("managedBy: nyl") || stdout.contains("managedBy: \"nyl\""),
        "Expected label 'managedBy: nyl' to be added by Kyverno policy, got:\n{}",
        stdout
    );
}

#[test]
fn test_multiple_kyverno_resources() {
    if !is_kyverno_available() {
        eprintln!("Skipping test: Kyverno CLI not available");
        return;
    }

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
      match:
        any:
          - resources:
              kinds:
                - Service
      mutate:
        patchStrategicMerge:
          metadata:
            labels:
              cluster-policy-applied: "true"
---
apiVersion: post-processing.nyl.niklasrosenstein.github.com/v1
kind: Kyverno
metadata:
  name: policy-2
spec:
  scope: Local
  mutatingPolicies:
    - matchConstraints:
        resourceRules:
          - apiGroups: ['apps']
            apiVersions: ['v1']
            operations: ['CREATE']
            resources: ['deployments']
      mutations:
        - patchType: ApplyConfiguration
          applyConfiguration:
            expression: "object"
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

    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("nyl"));
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
    if !is_kyverno_available() {
        eprintln!("Skipping test: Kyverno CLI not available");
        return;
    }

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
              any:
                - resources:
                    kinds:
                      - Pod
            mutate:
              patchStrategicMerge:
                metadata:
                  labels:
                    inline-applied: "true"
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
    assert!(stdout.contains("Pod"));
    assert!(stdout.contains("test-pod"));
    assert!(!stdout.contains("kind: Kyverno"));

    // Verify the inline ClusterPolicy mutation was applied
    assert!(
        stdout.contains("inline-applied: \"true\"") || stdout.contains("inline-applied: 'true'"),
        "Expected label 'inline-applied: true' to be added by inline ClusterPolicy, got:\n{}",
        stdout
    );
}

#[test]
fn test_kyverno_scope_variations() {
    if !is_kyverno_available() {
        eprintln!("Skipping test: Kyverno CLI not available");
        return;
    }

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
  scope: {scope}
---
apiVersion: post-processing.nyl.niklasrosenstein.github.com/v1
kind: Kyverno
metadata:
  name: test-policy
spec:
  scope: {scope}
  clusterPolicyRules:
    - name: test-rule
      match:
        any:
          - resources:
              kinds:
                - ConfigMap
      mutate:
        patchStrategicMerge:
          metadata:
            labels:
              scope-tested: "true"
"#
        );

        fs::write(temp.path().join("manifest.yaml"), manifest).unwrap();

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
            "Command should succeed for scope {}, stderr: {}",
            scope,
            String::from_utf8_lossy(&output.stderr)
        );

        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("ConfigMap"),
            "Output should contain ConfigMap for scope {}",
            scope
        );
        assert!(
            !stdout.contains("kind: Kyverno"),
            "Kyverno resource should be filtered for scope {}",
            scope
        );
    }
}

#[test]
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
  validatingPolicies:
    - name: require-environment-label
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
  mutatingPolicies:
    - name: add-loadbalancer-class
      matchConstraints:
        resourceRules:
          - apiGroups: ['']
            apiVersions: ['v1']
            operations: ['CREATE']
            resources: ['services']
      mutations:
        - patchType: ApplyConfiguration
          applyConfiguration:
            expression: >-
              Object{spec: Object.spec{loadBalancerClass: "ngrok", allocateLoadBalancerNodePorts: false}}
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

    // Verify the mutation was applied
    assert!(
        stdout.contains("loadBalancerClass: ngrok") || stdout.contains("loadBalancerClass: \"ngrok\""),
        "Expected loadBalancerClass to be added by Kyverno mutation policy, got:\n{}",
        stdout
    );
    assert!(
        stdout.contains("allocateLoadBalancerNodePorts: false"),
        "Expected allocateLoadBalancerNodePorts to be set by Kyverno mutation policy, got:\n{}",
        stdout
    );
}

/// Verify that a ValidatingPolicy passes when the resource satisfies the validation expression.
#[test]
fn test_validating_policy_passes() {
    if !is_kyverno_available() {
        eprintln!("Skipping test: Kyverno CLI not available");
        return;
    }

    let temp = TempDir::new().unwrap();

    // Resource that satisfies the validation (has the required label)
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
apiVersion: post-processing.nyl.niklasrosenstein.github.com/v1
kind: Kyverno
metadata:
  name: require-env-label
spec:
  scope: Global
  validatingPolicies:
    - name: check-environment-label
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
    assert!(!stdout.contains("kind: Kyverno"));
}

/// Verify that a ClusterPolicy (kyverno.io/v1) mutation is actually applied to resources.
#[test]
fn test_cluster_policy_mutation_applied() {
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
apiVersion: post-processing.nyl.niklasrosenstein.github.com/v1
kind: Kyverno
metadata:
  name: add-labels
spec:
  scope: Global
  clusterPolicyRules:
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
    assert!(!stdout.contains("kind: Kyverno"));

    // Verify ClusterPolicy mutation was applied
    assert!(
        stdout.contains("team: platform"),
        "Expected label 'team: platform' to be added by ClusterPolicy mutation, got:\n{}",
        stdout
    );
}

// NOTE: GeneratingPolicy, DeletingPolicy, and ImageValidatingPolicy are not evaluable
// by `kyverno apply` in offline mode (kyverno CLI 1.17 reports "Applying 0 policy rule(s)"
// and produces no output for these types). Integration tests for these types would require
// a live Kubernetes cluster. The CRD generation for these types is verified by unit tests
// in src/resources/kyverno.rs.
