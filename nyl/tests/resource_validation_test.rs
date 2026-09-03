/// Integration tests for resource validation and error reporting
use nyl::resources::{HelmChart, Release};
use nyl::util::SourceContext;
use std::path::PathBuf;

#[test]
fn test_unknown_field_in_helmchart() {
    // Create a HelmChart with an unknown field
    let yaml = r#"
apiVersion: nyl.niklasrosenstein.github.com/v1
kind: HelmChart
metadata:
  name: test-app
spec:
  chart:
    name: mychart
  unknownField: this-should-fail
"#;

    let source_ctx = SourceContext::new(PathBuf::from("/test/manifests/app.yaml"));
    let result: nyl::Result<HelmChart> = source_ctx.parse_yaml(yaml);

    assert!(result.is_err());
    let err_msg = result.unwrap_err().to_string();
    println!("Error message: {err_msg}");

    // Verify error message contains file path
    assert!(err_msg.contains("app.yaml"), "Error should contain file path");
    // Verify it mentions unknown field
    assert!(
        err_msg.contains("unknown field") || err_msg.contains("Unknown field"),
        "Error should mention unknown field: {err_msg}"
    );
    // Verify it has a hint
    assert!(err_msg.contains("Hint:"), "Error should contain a hint");
}

#[test]
fn test_unknown_field_in_nested_struct() {
    // Create a HelmChart with an unknown field in the chart spec
    let yaml = r#"
apiVersion: nyl.niklasrosenstein.github.com/v1
kind: HelmChart
metadata:
  name: test-app
spec:
  chart:
    name: mychart
    version: "1.0.0"
    invalidField: should-fail
"#;

    let source_ctx = SourceContext::new(PathBuf::from("/test/manifests/nested.yaml"));
    let result: nyl::Result<HelmChart> = source_ctx.parse_yaml(yaml);

    assert!(result.is_err());
    let err_msg = result.unwrap_err().to_string();
    println!("Error message: {err_msg}");

    // Verify error message contains useful information
    assert!(err_msg.contains("nested.yaml"), "Error should contain file path");
    assert!(
        err_msg.contains("unknown field") || err_msg.contains("Unknown field"),
        "Error should mention unknown field: {err_msg}"
    );
    assert!(err_msg.contains("Hint:"), "Error should contain a hint");
}

#[test]
fn test_type_mismatch_error() {
    // Create a HelmChart with a type mismatch (metadata should be object, not string)
    let yaml = r#"
apiVersion: nyl.niklasrosenstein.github.com/v1
kind: HelmChart
metadata: "this should be an object not a string"
spec:
  chart:
    name: mychart
"#;

    let source_ctx = SourceContext::new(PathBuf::from("/test/manifests/types.yaml"));
    let result: nyl::Result<HelmChart> = source_ctx.parse_yaml(yaml);

    assert!(result.is_err());
    let err_msg = result.unwrap_err().to_string();
    println!("Error message: {err_msg}");

    // Verify error message contains file path and hints about type
    assert!(err_msg.contains("types.yaml"), "Error should contain file path");
    assert!(err_msg.contains("Hint:"), "Error should contain a hint");
}

#[test]
fn test_release_unknown_field() {
    // Create a Release with an unknown field
    let yaml = r#"
apiVersion: gitops.nyl/v1
kind: Release
metadata:
  name: my-release
  namespace: production
  extraField: not-allowed
"#;

    let source_ctx = SourceContext::new(PathBuf::from("/test/manifests/release.yaml"));
    let result: nyl::Result<Release> = source_ctx.parse_yaml(yaml);

    assert!(result.is_err());
    let err_msg = result.unwrap_err().to_string();
    println!("Error message: {err_msg}");

    assert!(err_msg.contains("release.yaml"), "Error should contain file path");
    assert!(
        err_msg.contains("unknown field") || err_msg.contains("Unknown field"),
        "Error should mention unknown field: {err_msg}"
    );
    assert!(err_msg.contains("Hint:"), "Error should contain a hint");
}

#[test]
fn test_valid_manifest_succeeds() {
    // Create a valid HelmChart
    let yaml = r#"
apiVersion: nyl.niklasrosenstein.github.com/v1
kind: HelmChart
metadata:
  name: test-app
spec:
  chart:
    name: mychart
  values:
    replicaCount: 2
"#;

    let source_ctx = SourceContext::new(PathBuf::from("/test/manifests/valid.yaml"));
    let result: nyl::Result<HelmChart> = source_ctx.parse_yaml(yaml);

    // This should succeed
    assert!(result.is_ok(), "Valid manifest should parse successfully");
    let chart = result.unwrap();
    assert_eq!(chart.metadata.name, "test-app");
}

#[test]
fn test_error_message_has_helpful_hint() {
    // Test that the error message provides actionable hints
    let yaml = r#"
apiVersion: nyl.niklasrosenstein.github.com/v1
kind: HelmChart
metadata:
  name: test-app
spec:
  chart:
    name: mychart
  char: wrong-field-name
"#;

    let source_ctx = SourceContext::new(PathBuf::from("/test/manifests/typo.yaml"));
    let result: nyl::Result<HelmChart> = source_ctx.parse_yaml(yaml);

    assert!(result.is_err());
    let err_msg = result.unwrap_err().to_string();
    println!("Error message: {err_msg}");

    // The hint should suggest checking for typos
    assert!(
        err_msg.contains("typo") || err_msg.contains("API documentation"),
        "Error should suggest checking for typos or documentation: {err_msg}"
    );
}
