use clap::Args;
use kube::Client;
use std::collections::HashMap;

use crate::{
    cli::commands::render::render_manifests,
    kubernetes::{
        extract_name, KubernetesReleaseStorage, ResourceKey, ReleaseStorage,
    },
    NylError, Result,
};

/// Show diff between rendered manifests and cluster state
#[derive(Args, Debug)]
pub struct DiffArgs {
    /// Path to the project directory
    #[arg(default_value = ".")]
    pub path: String,

    /// Component to diff (if not specified, diffs all)
    #[arg(short, long)]
    pub component: Option<String>,

    /// Environment to diff for
    #[arg(short, long)]
    pub environment: Option<String>,

    /// Kubernetes context to use
    #[arg(long)]
    pub context: Option<String>,
}

pub async fn execute(args: DiffArgs) -> Result<()> {
    // 1. Render desired state
    let (desired_manifests, _profile, _env_name) = render_manifests(
        &args.path,
        args.component.as_deref(),
        args.environment.as_deref(),
    )?;

    if desired_manifests.is_empty() {
        println!("No manifests to diff");
        return Ok(());
    }

    // 2. Determine component name from first resource
    let component_name = extract_component_name(&desired_manifests)?;

    // 3. Initialize Kubernetes client
    let config = if let Some(ctx) = &args.context {
        let kubeconfig = kube::config::Kubeconfig::read()?;
        kube::Config::from_custom_kubeconfig(
            kubeconfig,
            &kube::config::KubeConfigOptions {
                context: Some(ctx.clone()),
                ..Default::default()
            },
        )
        .await?
    } else {
        // Use profile's kubeconfig
        let kubeconfig = kube::config::Kubeconfig::read()?;
        kube::Config::from_custom_kubeconfig(kubeconfig, &Default::default()).await?
    };
    let client = Client::try_from(config)?;

    // 4. Initialize state storage
    let storage = KubernetesReleaseStorage::new(client, "nyl-state".to_string());

    // 5. Fetch previous release
    let previous_release = storage.get_latest_release(&component_name).await?;

    // 6. Compute diff
    let diff_result = if let Some(prev) = previous_release {
        compute_diff_from_state(&prev.manifest, &desired_manifests)?
    } else {
        // No previous state - all new
        DiffResult {
            added: desired_manifests
                .iter()
                .map(|m| ResourceKey::from_json_value(m))
                .collect::<Result<Vec<_>>>()?,
            modified: Vec::new(),
            deleted: Vec::new(),
            unchanged: Vec::new(),
        }
    };

    // 7. Display diff
    display_diff(&diff_result);

    Ok(())
}

/// Extract component name from manifests (use first resource name)
pub fn extract_component_name(manifests: &[serde_json::Value]) -> Result<String> {
    if manifests.is_empty() {
        return Err(NylError::Config("No manifests to diff".to_string()));
    }

    // Try to extract from first resource
    let first = &manifests[0];
    let name = extract_name(first)?;

    // Use the resource name as component name (could be improved)
    Ok(name)
}

/// Diff result categorization
#[derive(Debug)]
struct DiffResult {
    added: Vec<ResourceKey>,
    modified: Vec<ResourceKey>,
    deleted: Vec<ResourceKey>,
    unchanged: Vec<ResourceKey>,
}

/// Compute diff between previous manifest and desired manifests
fn compute_diff_from_state(
    previous_manifest: &str,
    desired_manifests: &[serde_json::Value],
) -> Result<DiffResult> {
    // Parse previous YAML into Vec<Value>
    let previous_docs = parse_yaml_documents(previous_manifest)?;

    // Build HashMaps for comparison
    let mut previous_map: HashMap<ResourceKey, serde_json::Value> = HashMap::new();
    for doc in &previous_docs {
        let key = ResourceKey::from_json_value(doc)?;
        previous_map.insert(key, doc.clone());
    }

    let mut desired_map: HashMap<ResourceKey, serde_json::Value> = HashMap::new();
    for doc in desired_manifests {
        let key = ResourceKey::from_json_value(doc)?;
        desired_map.insert(key, doc.clone());
    }

    // Categorize changes
    let mut added = Vec::new();
    let mut modified = Vec::new();
    let mut deleted = Vec::new();
    let mut unchanged = Vec::new();

    // Check desired resources
    for (key, desired_value) in &desired_map {
        if let Some(previous_value) = previous_map.get(key) {
            // Resource exists in both
            if are_resources_equivalent(previous_value, desired_value) {
                unchanged.push(key.clone());
            } else {
                modified.push(key.clone());
            }
        } else {
            // Resource only in desired
            added.push(key.clone());
        }
    }

    // Check for deleted resources (in previous but not desired)
    for key in previous_map.keys() {
        if !desired_map.contains_key(key) {
            deleted.push(key.clone());
        }
    }

    Ok(DiffResult {
        added,
        modified,
        deleted,
        unchanged,
    })
}

/// Parse YAML documents from a string (supports multi-document YAML)
fn parse_yaml_documents(yaml: &str) -> Result<Vec<serde_json::Value>> {
    let mut documents = Vec::new();

    for doc_str in yaml.split("\n---\n") {
        let trimmed = doc_str.trim();
        if trimmed.is_empty() {
            continue;
        }

        let value: serde_json::Value = serde_norway::from_str(trimmed)?;

        // Skip null/empty documents
        if !value.is_null() {
            documents.push(value);
        }
    }

    Ok(documents)
}

/// Check if two resources are equivalent (deep equality)
fn are_resources_equivalent(a: &serde_json::Value, b: &serde_json::Value) -> bool {
    // For now, use simple JSON equality
    // Could be enhanced to ignore certain fields like metadata.resourceVersion
    a == b
}

/// Display diff results
fn display_diff(diff: &DiffResult) {
    for key in &diff.added {
        println!("+ {}", key.to_string());
    }

    for key in &diff.modified {
        println!("~ {}", key.to_string());
    }

    for key in &diff.deleted {
        println!("- {}", key.to_string());
    }

    println!();
    println!(
        "Summary: {} added, {} modified, {} deleted, {} unchanged",
        diff.added.len(),
        diff.modified.len(),
        diff.deleted.len(),
        diff.unchanged.len()
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_parse_yaml_documents_single() {
        let yaml = r#"
apiVersion: v1
kind: ConfigMap
metadata:
  name: test
"#;
        let docs = parse_yaml_documents(yaml).unwrap();
        assert_eq!(docs.len(), 1);
    }

    #[test]
    fn test_parse_yaml_documents_multiple() {
        let yaml = r#"
apiVersion: v1
kind: ConfigMap
metadata:
  name: test1
---
apiVersion: v1
kind: ConfigMap
metadata:
  name: test2
"#;
        let docs = parse_yaml_documents(yaml).unwrap();
        assert_eq!(docs.len(), 2);
    }

    #[test]
    fn test_are_resources_equivalent() {
        let a = json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {"name": "test"}
        });
        let b = a.clone();
        assert!(are_resources_equivalent(&a, &b));

        let c = json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {"name": "different"}
        });
        assert!(!are_resources_equivalent(&a, &c));
    }

    #[test]
    fn test_compute_diff_all_new() {
        let previous = "";
        let desired = vec![json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {"name": "test", "namespace": "default"}
        })];

        let result = compute_diff_from_state(previous, &desired).unwrap();
        assert_eq!(result.added.len(), 1);
        assert_eq!(result.modified.len(), 0);
        assert_eq!(result.deleted.len(), 0);
    }

    #[test]
    fn test_compute_diff_unchanged() {
        let manifest = json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {"name": "test", "namespace": "default"}
        });
        let previous = serde_norway::to_string(&manifest).unwrap();
        let desired = vec![manifest];

        let result = compute_diff_from_state(&previous, &desired).unwrap();
        assert_eq!(result.added.len(), 0);
        assert_eq!(result.modified.len(), 0);
        assert_eq!(result.deleted.len(), 0);
        assert_eq!(result.unchanged.len(), 1);
    }

    #[test]
    fn test_compute_diff_modified() {
        let previous_manifest = json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {"name": "test", "namespace": "default"},
            "data": {"key": "old_value"}
        });
        let previous = serde_norway::to_string(&previous_manifest).unwrap();

        let desired = vec![json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {"name": "test", "namespace": "default"},
            "data": {"key": "new_value"}
        })];

        let result = compute_diff_from_state(&previous, &desired).unwrap();
        assert_eq!(result.added.len(), 0);
        assert_eq!(result.modified.len(), 1);
        assert_eq!(result.deleted.len(), 0);
    }
}

