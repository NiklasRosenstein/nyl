/// Kyverno policy application logic
///
/// This module implements the logic to apply Kyverno policies to Kubernetes manifests
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

use crate::resources::{Kyverno, KyvernoScope};
use crate::{NylError, Result};

/// Context for tracking Kyverno resources and their scopes
#[derive(Debug)]
pub struct KyvernoContext {
    /// All Kyverno resources collected during rendering
    pub global_policies: Vec<Kyverno>,
    /// Policies with Root or Local scope (tracked per file/chart)
    pub scoped_policies: Vec<(Kyverno, String)>, // (policy, source_path)
}

impl KyvernoContext {
    /// Create a new empty context
    pub fn new() -> Self {
        Self {
            global_policies: Vec::new(),
            scoped_policies: Vec::new(),
        }
    }

    /// Add a Kyverno resource to the context
    pub fn add_kyverno(&mut self, kyverno: Kyverno, source_path: String) {
        match kyverno.spec.scope {
            KyvernoScope::Global => {
                self.global_policies.push(kyverno);
            }
            KyvernoScope::Local | KyvernoScope::Root => {
                self.scoped_policies.push((kyverno, source_path));
            }
        }
    }
}

impl Default for KyvernoContext {
    fn default() -> Self {
        Self::new()
    }
}

/// Apply Kyverno policies to a set of manifests
///
/// This function takes a list of manifests and applies all Global-scoped Kyverno
/// policies to them using the `kyverno apply` command.
pub fn apply_kyverno_policies(manifests: &[serde_json::Value], policies: &[Kyverno]) -> Result<Vec<serde_json::Value>> {
    if policies.is_empty() {
        return Ok(manifests.to_vec());
    }

    // Check if kyverno is installed
    if !is_kyverno_installed() {
        return Err(NylError::Config(
            "Kyverno CLI is not installed but Kyverno post-processor resources were found. \
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
    for (idx, kyverno) in policies.iter().enumerate() {
        // Collect all policy resources (inline + generated from shorthand rules)
        let all_policies = kyverno.get_all_policies();

        // Also handle policy file references
        if !kyverno.spec.policies.is_empty() {
            tracing::warn!("Policy file references in Kyverno spec are not yet implemented. Only inline and shorthand policies are supported.");
        }

        // Write each policy to a file
        for (policy_idx, policy) in all_policies.iter().enumerate() {
            let policy_file = policies_dir.join(format!("policy-{}-{}.yaml", idx, policy_idx));
            let policy_yaml = serde_norway::to_string(policy).map_err(NylError::Yaml)?;
            fs::write(&policy_file, policy_yaml)
                .map_err(|e| NylError::Config(format!("Failed to write policy file: {}", e)))?;
            policy_files.push(policy_file);
        }
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
    read_kyverno_output(&output_dir, manifests)
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
fn write_manifests_to_file(path: &Path, manifests: &[serde_json::Value]) -> Result<()> {
    let mut file =
        fs::File::create(path).map_err(|e| NylError::Config(format!("Failed to create resources file: {}", e)))?;

    for (i, manifest) in manifests.iter().enumerate() {
        if i > 0 {
            writeln!(file, "---").map_err(|e| NylError::Config(format!("Failed to write separator to file: {}", e)))?;
        }
        let yaml = serde_norway::to_string(manifest).map_err(NylError::Yaml)?;
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
            match serde_norway::from_str::<serde_json::Value>(trimmed) {
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
    fn test_kyverno_context_new() {
        let ctx = KyvernoContext::new();
        assert!(ctx.global_policies.is_empty());
        assert!(ctx.scoped_policies.is_empty());
    }

    #[test]
    fn test_kyverno_context_add_global() {
        let mut ctx = KyvernoContext::new();
        let kyverno = Kyverno {
            api_version: "post-processing.nyl.niklasrosenstein.github.com/v1".to_string(),
            kind: "Kyverno".to_string(),
            metadata: crate::resources::KyvernoMetadata {
                name: "test".to_string(),
                namespace: None,
            },
            spec: crate::resources::KyvernoSpec {
                scope: KyvernoScope::Global,
                policies: vec![],
                inline_policies: vec![],
                cluster_policy_rules: vec![],
                validating_policies: vec![],
                mutating_policies: vec![],
                generating_policies: vec![],
                deleting_policies: vec![],
                image_validating_policies: vec![],
            },
        };

        ctx.add_kyverno(kyverno, "test.yaml".to_string());
        assert_eq!(ctx.global_policies.len(), 1);
        assert_eq!(ctx.scoped_policies.len(), 0);
    }

    #[test]
    fn test_kyverno_context_add_local() {
        let mut ctx = KyvernoContext::new();
        let kyverno = Kyverno {
            api_version: "post-processing.nyl.niklasrosenstein.github.com/v1".to_string(),
            kind: "Kyverno".to_string(),
            metadata: crate::resources::KyvernoMetadata {
                name: "test".to_string(),
                namespace: None,
            },
            spec: crate::resources::KyvernoSpec {
                scope: KyvernoScope::Local,
                policies: vec![],
                inline_policies: vec![],
                cluster_policy_rules: vec![],
                validating_policies: vec![],
                mutating_policies: vec![],
                generating_policies: vec![],
                deleting_policies: vec![],
                image_validating_policies: vec![],
            },
        };

        ctx.add_kyverno(kyverno, "test.yaml".to_string());
        assert_eq!(ctx.global_policies.len(), 0);
        assert_eq!(ctx.scoped_policies.len(), 1);
    }

    #[test]
    fn test_write_manifests_to_file() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test.yaml");

        let manifests = vec![
            json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "test1"}}),
            json!({"apiVersion": "v1", "kind": "Service", "metadata": {"name": "test2"}}),
        ];

        write_manifests_to_file(&file_path, &manifests).unwrap();

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
