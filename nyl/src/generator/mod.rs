/// Generator module for manifest generation
///
/// This module handles:
/// - Component discovery and instantiation
/// - Dispatch logic for different resource types
/// - Resource generation workflow
///
/// Phase 2: Component instantiation to HelmChart
/// Phase 3: Template rendering and full generation pipeline
use crate::components::{Component, ComponentRegistry, HelmComponent};
use crate::config::ProjectConfig;
use crate::resources::{ChartRef, HelmChart};
use crate::Result;
use std::path::PathBuf;

/// Generator for Kubernetes manifests
///
/// The generator coordinates component discovery, template rendering,
/// and manifest generation.
pub struct Generator {
    /// Component registry for discovering components
    registry: ComponentRegistry,

    /// Project configuration
    config: ProjectConfig,
}

impl Generator {
    /// Create a new generator
    ///
    /// # Arguments
    /// * `config` - Project configuration
    pub fn new(config: ProjectConfig) -> Self {
        let search_paths = Self::build_search_paths(&config);
        let registry = ComponentRegistry::new(search_paths);

        Self { registry, config }
    }

    /// Build search paths from configuration
    fn build_search_paths(config: &ProjectConfig) -> Vec<PathBuf> {
        config.get_components_search_paths().to_vec()
    }

    /// Find a component by API version and kind
    ///
    /// # Arguments
    /// * `api_version` - The API version (e.g., "v1.example.io")
    /// * `kind` - The kind name (e.g., "WebApp")
    pub fn find_component(&self, api_version: &str, kind: &str) -> Result<Option<Component>> {
        self.registry.find_component(api_version, kind)
    }

    /// Instantiate a component to a HelmChart resource
    ///
    /// # Arguments
    /// * `component` - The component to instantiate
    /// * `name` - Name for the resource
    /// * `values` - Values to pass to the component
    pub fn instantiate_component(
        &self,
        component: &Component,
        name: &str,
        values: &serde_json::Value,
    ) -> Result<HelmChart> {
        match component {
            Component::Helm(helm_component) => Self::instantiate_helm_component(helm_component, name, values),
        }
    }

    /// Instantiate a Helm component to a HelmChart
    fn instantiate_helm_component(
        component: &HelmComponent,
        name: &str,
        values: &serde_json::Value,
    ) -> Result<HelmChart> {
        // Create ChartRef pointing to the component's chart
        let chart_ref = ChartRef {
            name: Some(component.path.to_string_lossy().to_string()),
            ..Default::default()
        };

        // Create HelmChart resource
        let mut helm_chart = HelmChart::new(name, chart_ref);

        // Set API version and kind from component
        helm_chart.api_version.clone_from(&component.api_version);
        helm_chart.kind.clone_from(&component.kind);

        // Set values
        helm_chart = helm_chart.with_values(values.clone());

        Ok(helm_chart)
    }

    /// List all available components
    pub fn list_components(&self) -> Result<Vec<Component>> {
        self.registry.list_components()
    }

    /// Generate manifests (Phase 3)
    ///
    /// Phase 2: Stubbed
    /// Phase 3: Will render templates and generate manifests
    pub fn generate(&self) -> Result<Vec<String>> {
        // Stub implementation for Phase 2
        Ok(vec![])
    }

    /// Get the component registry
    pub fn registry(&self) -> &ComponentRegistry {
        &self.registry
    }

    /// Get the project configuration
    pub fn config(&self) -> &ProjectConfig {
        &self.config
    }
}

impl Default for Generator {
    fn default() -> Self {
        Self::new(ProjectConfig::load(None).unwrap_or_else(|_| ProjectConfig {
            file: None,
            config: crate::config::ProjectFile::default(),
        }))
    }
}

impl std::fmt::Debug for Generator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Generator")
            .field("registry", &self.registry)
            .field("config_file", &self.config.file)
            .finish()
    }
}

/// Helper function for instantiating components
///
/// Convenience function that doesn't require a Generator instance
pub fn instantiate_component(component: &Component, name: &str, values: &serde_json::Value) -> Result<HelmChart> {
    match component {
        Component::Helm(helm_component) => instantiate_helm_component(helm_component, name, values),
    }
}

/// Instantiate a Helm component to a HelmChart
fn instantiate_helm_component(component: &HelmComponent, name: &str, values: &serde_json::Value) -> Result<HelmChart> {
    // Create ChartRef pointing to the component's chart
    let chart_ref = ChartRef {
        name: Some(component.path.to_string_lossy().to_string()),
        ..Default::default()
    };

    // Create HelmChart resource
    let mut helm_chart = HelmChart::new(name, chart_ref);

    // Set API version and kind from component
    helm_chart.api_version.clone_from(&component.api_version);
    helm_chart.kind.clone_from(&component.kind);

    // Set values
    helm_chart = helm_chart.with_values(values.clone());

    Ok(helm_chart)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn create_test_config(base: &std::path::Path) -> ProjectConfig {
        let config_path = base.join("nyl.toml");
        fs::write(&config_path, "[project]\ncomponents_search_paths = [\"components\"]\n").unwrap();

        ProjectConfig::load(Some(config_path)).unwrap()
    }

    fn create_test_component(base: &std::path::Path, api_version: &str, kind: &str) {
        let component_dir = base.join("components").join(api_version).join(kind);
        fs::create_dir_all(&component_dir).unwrap();

        let chart_yaml = component_dir.join("Chart.yaml");
        fs::write(
            chart_yaml,
            format!("apiVersion: v2\nname: {}\nversion: 1.0.0\n", kind.to_lowercase()),
        )
        .unwrap();
    }

    #[test]
    fn test_generator_new() {
        let temp = TempDir::new().unwrap();
        let config = create_test_config(temp.path());

        let generator = Generator::new(config);
        assert!(generator.config().file.is_some());
    }

    #[test]
    fn test_generator_find_component() {
        let temp = TempDir::new().unwrap();
        create_test_component(temp.path(), "v1.example.io", "WebApp");

        let config = create_test_config(temp.path());
        let generator = Generator::new(config);

        let component = generator.find_component("v1.example.io", "WebApp").unwrap();
        assert!(component.is_some());
    }

    #[test]
    fn test_generator_instantiate_component() {
        let temp = TempDir::new().unwrap();
        create_test_component(temp.path(), "v1.example.io", "WebApp");

        let config = create_test_config(temp.path());
        let generator = Generator::new(config);

        let component = generator.find_component("v1.example.io", "WebApp").unwrap().unwrap();

        let values = serde_json::json!({
            "replicas": 3,
            "image": "nginx:latest"
        });

        let helm_chart = generator
            .instantiate_component(&component, "my-webapp", &values)
            .unwrap();

        assert_eq!(helm_chart.metadata.name, "my-webapp");
        assert_eq!(helm_chart.api_version, "v1.example.io");
        assert_eq!(helm_chart.kind, "WebApp");
        assert_eq!(helm_chart.spec.values["replicas"], 3);
        assert_eq!(helm_chart.release_name(), "my-webapp");
    }

    #[test]
    fn test_generator_list_components() {
        let temp = TempDir::new().unwrap();
        create_test_component(temp.path(), "v1.example.io", "WebApp");
        create_test_component(temp.path(), "v1.example.io", "Database");

        let config = create_test_config(temp.path());
        let generator = Generator::new(config);

        let components = generator.list_components().unwrap();
        assert_eq!(components.len(), 2);
    }

    #[test]
    fn test_instantiate_component_helper() {
        let helm_component = HelmComponent {
            path: PathBuf::from("/test/charts/myapp"),
            api_version: "v1.test.io".to_string(),
            kind: "TestApp".to_string(),
        };

        let component = Component::Helm(helm_component);

        let values = serde_json::json!({
            "replicas": 2
        });

        let helm_chart = instantiate_component(&component, "test-instance", &values).unwrap();

        assert_eq!(helm_chart.metadata.name, "test-instance");
        assert_eq!(helm_chart.api_version, "v1.test.io");
        assert_eq!(helm_chart.kind, "TestApp");
        assert_eq!(helm_chart.spec.values["replicas"], 2);
    }

    #[test]
    fn test_instantiate_helm_component_direct() {
        let helm_component = HelmComponent {
            path: PathBuf::from("/charts/webapp"),
            api_version: "v2.apps.io".to_string(),
            kind: "WebApplication".to_string(),
        };

        let values = serde_json::json!({
            "port": 8080,
            "environment": "production"
        });

        let helm_chart = instantiate_helm_component(&helm_component, "prod-app", &values).unwrap();

        assert_eq!(helm_chart.metadata.name, "prod-app");
        assert_eq!(helm_chart.api_version, "v2.apps.io");
        assert_eq!(helm_chart.kind, "WebApplication");
        assert_eq!(helm_chart.spec.chart.name, Some("/charts/webapp".to_string()));
        assert_eq!(helm_chart.spec.values["port"], 8080);
        assert_eq!(helm_chart.spec.values["environment"], "production");
    }

    #[test]
    fn test_generator_default() {
        let generator = Generator::default();
        assert!(generator.config().file.is_none());
    }

    #[test]
    fn test_build_search_paths() {
        let temp = TempDir::new().unwrap();
        let config_path = temp.path().join("nyl.toml");

        let toml = format!(
            r#"[project]
components_search_paths = ['{}']
"#,
            temp.path().join("lib").display(),
        );

        fs::write(&config_path, toml).unwrap();

        let config = ProjectConfig::load(Some(config_path)).unwrap();
        let paths = Generator::build_search_paths(&config);

        assert_eq!(paths.len(), 1);
        assert!(paths[0].ends_with("lib"));
    }
}
