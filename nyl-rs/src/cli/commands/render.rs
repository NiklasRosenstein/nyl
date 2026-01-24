use clap::Args;
use std::path::Path;
use walkdir::WalkDir;

use crate::{
    config::ProjectConfig,
    generator::Generator,
    helm::{HelmChartResolver, HelmTemplateExecutor},
    profiles::{deep_merge_value, ProfileConfig},
    resources::{HelmChart, ReleaseMetadata},
    secrets::SecretsConfig,
    template::TemplateContext,
    NylError, Result,
};

/// Render Kubernetes manifests to stdout
#[derive(Args, Debug)]
pub struct RenderArgs {
    /// Path to the project directory
    #[arg(default_value = ".")]
    pub path: String,

    /// Component to render (if not specified, renders all)
    #[arg(short, long)]
    pub component: Option<String>,

    /// Environment to render for
    #[arg(short, long)]
    pub environment: Option<String>,
}

/// Output format for rendered manifests
#[derive(Debug, Clone, Copy)]
enum OutputFormat {
    Yaml,
    #[allow(dead_code)]
    Json,
}

pub fn execute(args: RenderArgs) -> Result<()> {
    // 1. Load project configuration
    let project_config = ProjectConfig::load(None)?;

    // 2. Load profile configuration
    let profile_config = ProfileConfig::load(None)?;

    // 3. Select environment/profile
    let env_name = args.environment.as_deref().unwrap_or("default");
    let profile = profile_config
        .get(env_name)
        .ok_or_else(|| NylError::Config(format!("Profile '{}' not found", env_name)))?;

    // 4. Load secrets
    let secrets_config = SecretsConfig::load(None)?;

    // 5. Build template context
    let context = TemplateContext::build(profile, &secrets_config, env_name)?;

    // 6. Create generator
    let generator = Generator::new(project_config.clone());

    // 7. Load and filter resources
    let resources = load_resources(&args.path)?;
    let filtered = filter_resources(resources, args.component.as_deref())?;

    // 8. Generate manifests
    let mut all_manifests = Vec::new();
    for resource in filtered {
        let manifests = generate_resource(&generator, &resource, &context, &project_config)?;
        all_manifests.extend(manifests);
    }

    // 9. Output results
    output_manifests(&all_manifests, OutputFormat::Yaml)?;

    Ok(())
}

/// Load all YAML/JSON resources from a directory
fn load_resources(path: &str) -> Result<Vec<serde_json::Value>> {
    let path = Path::new(path);
    let mut resources = Vec::new();

    for entry in WalkDir::new(path)
        .follow_links(true)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let file_path = entry.path();
        if !file_path.is_file() {
            continue;
        }

        let ext = file_path.extension().and_then(|s| s.to_str());
        if !matches!(ext, Some("yaml") | Some("yml") | Some("json")) {
            continue;
        }

        let content = std::fs::read_to_string(file_path)
            .map_err(|e| NylError::Config(format!("Failed to read file: {}", e)))?;
        let docs = parse_yaml_documents(&content)?;
        resources.extend(docs);
    }

    Ok(resources)
}

/// Parse YAML multi-document stream
fn parse_yaml_documents(yaml_str: &str) -> Result<Vec<serde_json::Value>> {
    let mut documents = Vec::new();

    for doc in yaml_str.split("\n---\n") {
        let trimmed = doc.trim();

        // Skip empty or comment-only documents
        if trimmed.is_empty()
            || trimmed
                .lines()
                .all(|line| line.trim().starts_with('#') || line.trim().is_empty())
        {
            continue;
        }

        let value: serde_json::Value =
            serde_norway::from_str(trimmed).map_err(|e| NylError::Yaml(e))?;

        if !value.is_null() {
            documents.push(value);
        }
    }

    Ok(documents)
}

/// Filter resources by component type
fn filter_resources(
    resources: Vec<serde_json::Value>,
    component_filter: Option<&str>,
) -> Result<Vec<serde_json::Value>> {
    if let Some(filter) = component_filter {
        Ok(resources
            .into_iter()
            .filter(|r| {
                let kind = r.get("kind").and_then(|k| k.as_str()).unwrap_or("");
                let api_version = r.get("apiVersion").and_then(|a| a.as_str()).unwrap_or("");
                filter == kind || filter == format!("{}/{}", api_version, kind)
            })
            .collect())
    } else {
        Ok(resources)
    }
}

/// Generate manifests from a resource
fn generate_resource(
    _generator: &Generator,
    resource: &serde_json::Value,
    context: &TemplateContext,
    config: &ProjectConfig,
) -> Result<Vec<serde_json::Value>> {
    // Check if it's a HelmChart resource
    let kind = resource.get("kind").and_then(|k| k.as_str());
    let api_version = resource.get("apiVersion").and_then(|a| a.as_str());

    if kind == Some("HelmChart") && api_version.map_or(false, |v| v.starts_with("v1.")) {
        // Parse as HelmChart and render
        let chart: HelmChart = serde_json::from_value(resource.clone())
            .map_err(|e| NylError::Config(format!("Failed to parse HelmChart: {}", e)))?;
        render_helm_chart(&chart, context, config)
    } else {
        // For Phase 3, pass through other resources as-is
        // Phase 4+: Use generator for component instantiation
        Ok(vec![resource.clone()])
    }
}

/// Render a Helm chart
fn render_helm_chart(
    chart: &HelmChart,
    context: &TemplateContext,
    config: &ProjectConfig,
) -> Result<Vec<serde_json::Value>> {
    let working_dir = std::env::current_dir()
        .map_err(|e| NylError::Config(format!("Failed to get current directory: {}", e)))?;

    let resolver = HelmChartResolver::new(
        config.config.settings.search_path.clone(),
        working_dir,
    );
    let resolved = resolver.resolve_chart(&chart.spec.chart)?;

    // Merge context values into chart values
    let merged_values = deep_merge_value(Some(chart.spec.values.clone()), context.values.clone());

    let release = chart
        .spec
        .release
        .clone()
        .unwrap_or_else(|| ReleaseMetadata::new(chart.effective_release_name()));

    let executor = HelmTemplateExecutor::new()
        .with_kube_version(chart.spec.kube_version.clone().unwrap_or_default())
        .with_api_versions(chart.spec.api_versions.clone());

    executor.template(&resolved, &release, &merged_values)
}

/// Output manifests in the specified format
fn output_manifests(manifests: &[serde_json::Value], format: OutputFormat) -> Result<()> {
    match format {
        OutputFormat::Yaml => {
            for (i, manifest) in manifests.iter().enumerate() {
                if i > 0 {
                    println!("---");
                }
                let yaml = serde_norway::to_string(manifest)
                    .map_err(|e| NylError::Yaml(e))?;
                print!("{}", yaml);
            }
        }
        OutputFormat::Json => {
            let json = serde_json::to_string_pretty(manifests)
                .map_err(|e| NylError::Config(format!("Failed to serialize JSON: {}", e)))?;
            println!("{}", json);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(docs[0]["kind"], "ConfigMap");
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
kind: Service
metadata:
  name: test2
"#;
        let docs = parse_yaml_documents(yaml).unwrap();
        assert_eq!(docs.len(), 2);
        assert_eq!(docs[0]["kind"], "ConfigMap");
        assert_eq!(docs[1]["kind"], "Service");
    }

    #[test]
    fn test_filter_resources_no_filter() {
        let resources = vec![
            serde_json::json!({
                "apiVersion": "v1",
                "kind": "ConfigMap"
            }),
            serde_json::json!({
                "apiVersion": "v1",
                "kind": "Service"
            }),
        ];

        let filtered = filter_resources(resources.clone(), None).unwrap();
        assert_eq!(filtered.len(), 2);
    }

    #[test]
    fn test_filter_resources_by_kind() {
        let resources = vec![
            serde_json::json!({
                "apiVersion": "v1",
                "kind": "ConfigMap"
            }),
            serde_json::json!({
                "apiVersion": "v1",
                "kind": "Service"
            }),
        ];

        let filtered = filter_resources(resources, Some("ConfigMap")).unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0]["kind"], "ConfigMap");
    }

    #[test]
    fn test_filter_resources_by_api_kind() {
        let resources = vec![
            serde_json::json!({
                "apiVersion": "v1",
                "kind": "ConfigMap"
            }),
            serde_json::json!({
                "apiVersion": "apps/v1",
                "kind": "Deployment"
            }),
        ];

        let filtered = filter_resources(resources, Some("apps/v1/Deployment")).unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0]["kind"], "Deployment");
    }
}
