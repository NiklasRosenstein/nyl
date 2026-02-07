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
pub fn apply_kyverno_policies(
    manifests: &[serde_json::Value],
    policies: &[Kyverno],
) -> Result<Vec<serde_json::Value>> {
    if policies.is_empty() {
        return Ok(manifests.to_vec());
    }

    // Check if kyverno is installed
    if !is_kyverno_installed() {
        tracing::warn!("Kyverno CLI is not installed. Skipping policy application. Install from: https://kyverno.io/docs/kyverno-cli/");
        return Ok(manifests.to_vec());
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

    // Execute kyverno apply
    let output = execute_kyverno_apply(&policy_files, &resources_file)?;

    // Parse the output
    parse_kyverno_output(&output)
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
            writeln!(file, "---")
                .map_err(|e| NylError::Config(format!("Failed to write separator to file: {}", e)))?;
        }
        let yaml = serde_norway::to_string(manifest).map_err(NylError::Yaml)?;
        write!(file, "{}", yaml).map_err(|e| NylError::Config(format!("Failed to write manifest to file: {}", e)))?;
    }

    Ok(())
}

/// Execute kyverno apply command
fn execute_kyverno_apply(policy_files: &[PathBuf], resources_file: &Path) -> Result<String> {
    let mut cmd = Command::new("kyverno");
    cmd.arg("apply");

    // Add all policy files
    for policy_file in policy_files {
        cmd.arg("--policy").arg(policy_file);
    }

    // Add the resources file
    cmd.arg("--resource").arg(resources_file);

    // Output as JSON for easier parsing
    cmd.arg("--output").arg("json");

    tracing::debug!("Executing kyverno apply command: {:?}", cmd);

    let output = cmd
        .output()
        .map_err(|e| NylError::Config(format!("Failed to execute kyverno apply: {}", e)))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(NylError::Config(format!(
            "Kyverno apply failed with exit code {:?}: {}",
            output.status.code(),
            stderr
        )));
    }

    String::from_utf8(output.stdout)
        .map_err(|e| NylError::Config(format!("Failed to parse kyverno output: {}", e)))
}

/// Parse kyverno apply output and extract processed manifests
///
/// The kyverno apply command with JSON output returns a structured result.
/// We need to extract the mutated/validated resources from the output.
fn parse_kyverno_output(output: &str) -> Result<Vec<serde_json::Value>> {
    // Kyverno apply with --output json returns a JSON object with results
    // We need to handle different output formats depending on the kyverno version
    
    // Try to parse as JSON first
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(output) {
        // If it's an object with a "resources" field, extract that
        if let Some(resources) = json.get("resources").and_then(|r| r.as_array()) {
            return Ok(resources.clone());
        }
        
        // If it's directly an array of resources
        if json.is_array() {
            return Ok(json.as_array().unwrap().clone());
        }
        
        // If there's a "results" field with resources
        if let Some(results) = json.get("results").and_then(|r| r.as_array()) {
            let mut manifests = Vec::new();
            for result in results {
                if let Some(resource) = result.get("resource") {
                    manifests.push(resource.clone());
                }
            }
            if !manifests.is_empty() {
                return Ok(manifests);
            }
        }
    }
    
    // If JSON parsing doesn't work, try parsing as YAML multi-document
    // This handles the case where kyverno outputs YAML instead of JSON
    let mut manifests = Vec::new();
    for doc in output.split("\n---\n") {
        let trimmed = doc.trim();
        if trimmed.is_empty() || trimmed.lines().all(|line| line.trim().starts_with('#') || line.trim().is_empty()) {
            continue;
        }
        
        if let Ok(value) = serde_norway::from_str::<serde_json::Value>(trimmed) {
            if !value.is_null() {
                manifests.push(value);
            }
        }
    }
    
    if manifests.is_empty() {
        // If we couldn't parse anything, log a warning and return empty
        tracing::warn!("Could not parse kyverno output. This might be a version compatibility issue.");
        return Err(NylError::Config(
            "Failed to parse kyverno apply output. Please check kyverno version compatibility.".to_string(),
        ));
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
                validating_policy_rules: vec![],
                mutating_policy_rules: vec![],
                generating_policy_rules: vec![],
                deleting_policy_rules: vec![],
                image_validating_policy_rules: vec![],
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
                validating_policy_rules: vec![],
                mutating_policy_rules: vec![],
                generating_policy_rules: vec![],
                deleting_policy_rules: vec![],
                image_validating_policy_rules: vec![],
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
    fn test_parse_kyverno_output_json_array() {
        let output = r#"[
            {"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "test"}},
            {"apiVersion": "v1", "kind": "Service", "metadata": {"name": "svc"}}
        ]"#;

        let manifests = parse_kyverno_output(output).unwrap();
        assert_eq!(manifests.len(), 2);
        assert_eq!(manifests[0]["kind"], "ConfigMap");
        assert_eq!(manifests[1]["kind"], "Service");
    }

    #[test]
    fn test_parse_kyverno_output_yaml() {
        let output = r#"apiVersion: v1
kind: ConfigMap
metadata:
  name: test1
---
apiVersion: v1
kind: Service
metadata:
  name: test2"#;

        let manifests = parse_kyverno_output(output).unwrap();
        assert_eq!(manifests.len(), 2);
        assert_eq!(manifests[0]["kind"], "ConfigMap");
        assert_eq!(manifests[1]["kind"], "Service");
    }
}
