/// Kyverno policy application logic
///
/// This module implements the logic to apply Kyverno policies to Kubernetes manifests
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

use crate::resources::{is_kyverno_policy, AnnotatedKyvernoPolicy};
use crate::{NylError, Result};

/// Apply Kyverno policies to manifests
///
/// Policy resources themselves are excluded from mutation
pub fn apply_kyverno_policies(
    manifests: &[serde_json::Value],
    policies: &[AnnotatedKyvernoPolicy],
) -> Result<Vec<serde_json::Value>> {
    if policies.is_empty() {
        return Ok(manifests.to_vec());
    }

    // Separate policy resources from regular resources
    let (policy_manifests, regular_manifests): (Vec<_>, Vec<_>) = manifests.iter().partition(|m| is_kyverno_policy(m));

    // Only apply to regular resources
    let policy_resources: Vec<serde_json::Value> = policies.iter().map(|p| p.policy.clone()).collect();

    let mutated_manifests = apply_kyverno_policies_internal(&regular_manifests, &policy_resources)?;

    // Combine mutated resources with policy resources (unchanged)
    let mut result = mutated_manifests;
    result.extend(policy_manifests.into_iter().cloned());

    Ok(result)
}

/// Internal function to apply Kyverno policies using the CLI
fn apply_kyverno_policies_internal(
    manifests: &[&serde_json::Value],
    policies: &[serde_json::Value],
) -> Result<Vec<serde_json::Value>> {
    if manifests.is_empty() {
        return Ok(Vec::new());
    }

    // Check if kyverno is installed
    if !is_kyverno_installed() {
        return Err(NylError::Config(
            "Kyverno CLI is not installed but Kyverno policies were found. \
             Install from: https://kyverno.io/docs/kyverno-cli/"
                .to_string(),
        ));
    }

    // Create a temporary directory for policy and resource files
    let temp_dir = TempDir::new().map_err(|e| NylError::Config(format!("Failed to create temp directory: {}", e)))?;
    let policies_dir = temp_dir.path().join("policies");
    fs::create_dir(&policies_dir)
        .map_err(|e| NylError::Config(format!("Failed to create policies directory: {}", e)))?;

    // Write all policies to files
    let mut policy_files = Vec::new();
    for (idx, policy) in policies.iter().enumerate() {
        let policy_file = policies_dir.join(format!("policy-{}.yaml", idx));
        let policy_yaml = crate::yaml::serialize_yaml_document(policy).map_err(NylError::YamlEmit)?;
        fs::write(&policy_file, policy_yaml)
            .map_err(|e| NylError::Config(format!("Failed to write policy file: {}", e)))?;
        policy_files.push(policy_file);
    }

    // Write all resources to a single file
    let resources_file = temp_dir.path().join("resources.yaml");
    write_manifests_to_file(&resources_file, manifests)?;

    // Create output directory for kyverno to write mutated manifests
    let output_dir = temp_dir.path().join("output");
    fs::create_dir(&output_dir).map_err(|e| NylError::Config(format!("Failed to create output directory: {}", e)))?;

    // Execute kyverno apply
    execute_kyverno_apply(&policy_files, &resources_file, &output_dir)?;

    // Read back mutated manifests from output directory
    let original_vec: Vec<serde_json::Value> = manifests.iter().map(|&m| m.clone()).collect();
    read_kyverno_output(&output_dir, &original_vec)
}

/// Check if kyverno CLI is installed
fn is_kyverno_installed() -> bool {
    Command::new("kyverno")
        .arg("version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// Write manifests to a YAML file
fn write_manifests_to_file(path: &Path, manifests: &[&serde_json::Value]) -> Result<()> {
    let mut file =
        fs::File::create(path).map_err(|e| NylError::Config(format!("Failed to create resources file: {}", e)))?;

    for (i, manifest) in manifests.iter().enumerate() {
        if i > 0 {
            writeln!(file, "---").map_err(|e| NylError::Config(format!("Failed to write separator to file: {}", e)))?;
        }
        let yaml = crate::yaml::serialize_yaml_document(manifest).map_err(NylError::YamlEmit)?;
        write!(file, "{}", yaml).map_err(|e| NylError::Config(format!("Failed to write manifest to file: {}", e)))?;
    }

    Ok(())
}

/// Execute kyverno apply command, writing mutated manifests to the output directory
fn execute_kyverno_apply(policy_files: &[PathBuf], resources_file: &Path, output_dir: &Path) -> Result<()> {
    let mut cmd = Command::new("kyverno");
    cmd.arg("apply");

    // Add all policy files as positional arguments
    for policy_file in policy_files {
        cmd.arg(policy_file);
    }

    // Add the resources file
    cmd.arg("--resource").arg(resources_file);

    // Write mutated manifests to output directory
    cmd.arg("-o").arg(output_dir);

    tracing::debug!("Executing kyverno apply command: {:?}", cmd);

    let output = cmd
        .output()
        .map_err(|e| NylError::Config(format!("Failed to execute kyverno apply: {}", e)))?;

    let combined_output = String::from_utf8_lossy(&output.stdout);
    tracing::debug!("Kyverno output:\n{}", combined_output);

    if !output.status.success() {
        return Err(NylError::Config(format!(
            "Kyverno apply failed with exit code {:?}:\n{}",
            output.status.code(),
            combined_output
        )));
    }

    Ok(())
}

/// Read mutated manifests from the kyverno output directory
///
/// Kyverno writes one file per mutated resource to the output directory. Validation-only
/// policies don't produce output files. When the output directory is empty (e.g. only
/// validation policies were applied), the original manifests are returned unchanged.
fn read_kyverno_output(output_dir: &Path, original_manifests: &[serde_json::Value]) -> Result<Vec<serde_json::Value>> {
    let mut manifests = Vec::new();

    let entries: Vec<_> = fs::read_dir(output_dir)
        .map_err(|e| NylError::Config(format!("Failed to read kyverno output directory: {}", e)))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| NylError::Config(format!("Failed to read kyverno output directory entries: {}", e)))?;

    // Validation-only policies don't produce output files. If kyverno succeeded
    // (checked by the caller) but wrote no output, return the originals unchanged.
    if entries.is_empty() {
        tracing::debug!("Kyverno output directory is empty; returning original manifests unchanged");
        return Ok(original_manifests.to_vec());
    }

    for entry in &entries {
        let content = fs::read_to_string(entry.path()).map_err(|e| {
            NylError::Config(format!(
                "Failed to read kyverno output file {}: {}",
                entry.path().display(),
                e
            ))
        })?;

        // Each file may contain multiple YAML documents
        for doc in content.split("\n---") {
            let trimmed = doc.trim();
            if trimmed.is_empty() {
                continue;
            }
            match crate::yaml::parse_yaml_value_k8s_compatible(trimmed) {
                Ok(value) if !value.is_null() => manifests.push(value),
                Ok(_) => {}
                Err(e) => {
                    tracing::debug!("Failed to parse YAML document from kyverno output: {}", e);
                }
            }
        }
    }

    if manifests.len() != original_manifests.len() {
        return Err(NylError::Config(format!(
            "Unexpected behaviour of `kyverno apply` command: The number of resources generated in the \
             output folder ({}) does not match the number of input resources ({})",
            manifests.len(),
            original_manifests.len()
        )));
    }

    Ok(manifests)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_write_manifests_to_file() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test.yaml");

        let manifests = [
            json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "test1"}}),
            json!({"apiVersion": "v1", "kind": "Service", "metadata": {"name": "test2"}}),
        ];
        let manifest_refs: Vec<&serde_json::Value> = manifests.iter().collect();

        write_manifests_to_file(&file_path, &manifest_refs).unwrap();

        let content = fs::read_to_string(&file_path).unwrap();
        assert!(content.contains("ConfigMap"));
        assert!(content.contains("Service"));
        assert!(content.contains("---"));
    }

    #[test]
    fn test_read_kyverno_output() {
        let temp_dir = TempDir::new().unwrap();

        // Write two resource files like kyverno would
        fs::write(
            temp_dir.path().join("resource-0.yaml"),
            "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: test1\n",
        )
        .unwrap();
        fs::write(
            temp_dir.path().join("resource-1.yaml"),
            "apiVersion: v1\nkind: Service\nmetadata:\n  name: test2\n",
        )
        .unwrap();

        let originals = vec![
            json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "test1"}}),
            json!({"apiVersion": "v1", "kind": "Service", "metadata": {"name": "test2"}}),
        ];
        let manifests = read_kyverno_output(temp_dir.path(), &originals).unwrap();
        assert_eq!(manifests.len(), 2);
    }

    #[test]
    fn test_read_kyverno_output_empty_returns_originals() {
        let temp_dir = TempDir::new().unwrap();

        // Empty output dir (e.g. validation-only policies)
        let originals = vec![json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "test1"}})];
        let manifests = read_kyverno_output(temp_dir.path(), &originals).unwrap();
        assert_eq!(manifests.len(), 1);
        assert_eq!(manifests[0]["metadata"]["name"], "test1");
    }

    #[test]
    fn test_read_kyverno_output_count_mismatch() {
        let temp_dir = TempDir::new().unwrap();

        fs::write(
            temp_dir.path().join("resource-0.yaml"),
            "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: test1\n",
        )
        .unwrap();

        let originals = vec![
            json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "test1"}}),
            json!({"apiVersion": "v1", "kind": "Service", "metadata": {"name": "test2"}}),
        ];
        let result = read_kyverno_output(temp_dir.path(), &originals);
        assert!(result.is_err());
    }
}
