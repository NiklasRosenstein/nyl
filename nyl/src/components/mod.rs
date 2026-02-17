/// Component discovery module
///
/// This module handles:
/// - Scanning filesystem for Helm components
/// - Caching component lookups
/// - Component type definitions
///
/// Components are discovered in the directory structure:
/// <components_search_path>/<apiVersion>/<kind>/Chart.yaml
use crate::{NylError, Result};
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;

/// A component represents a reusable template/chart
#[derive(Debug, Clone)]
pub enum Component {
    /// Helm chart component
    Helm(HelmComponent),
}

/// A Helm-based component
#[derive(Debug, Clone)]
pub struct HelmComponent {
    /// Path to the directory containing Chart.yaml
    pub path: PathBuf,

    /// API version this component implements
    pub api_version: String,

    /// Kind this component implements
    pub kind: String,
}

impl HelmComponent {
    /// Get the path to the Chart.yaml file
    pub fn chart_yaml_path(&self) -> PathBuf {
        self.path.join("Chart.yaml")
    }

    /// Verify the component is valid (Chart.yaml exists)
    pub fn verify(&self) -> Result<()> {
        let chart_path = self.chart_yaml_path();
        if !chart_path.exists() {
            return Err(NylError::Config(format!(
                "Chart.yaml not found at: {}",
                chart_path.display()
            )));
        }
        Ok(())
    }
}

/// Registry for discovering and caching components
///
/// The registry searches configured paths for components following the
/// structure: <search_path>/<apiVersion>/<kind>/Chart.yaml
pub struct ComponentRegistry {
    /// Directories to search for components
    search_paths: Vec<PathBuf>,

    /// Cache of component lookups
    /// Key: (apiVersion, kind)
    /// Value: Option<Component> - None means "checked and not found"
    cache: RefCell<HashMap<(String, String), Option<Component>>>,
}

impl ComponentRegistry {
    /// Create a new component registry
    ///
    /// # Arguments
    /// * `search_paths` - Directories to search for components
    pub fn new(search_paths: Vec<PathBuf>) -> Self {
        Self {
            search_paths,
            cache: RefCell::new(HashMap::new()),
        }
    }

    /// Find a component by API version and kind
    ///
    /// # Arguments
    /// * `api_version` - The API version (e.g., "v1.example.io")
    /// * `kind` - The kind name (e.g., "WebApp")
    ///
    /// # Returns
    /// * `Ok(Some(Component))` - Component found
    /// * `Ok(None)` - Component not found (not an error)
    /// * `Err(...)` - I/O error or other failure
    pub fn find_component(&self, api_version: &str, kind: &str) -> Result<Option<Component>> {
        let key = (api_version.to_string(), kind.to_string());

        // Check cache first
        if let Some(cached) = self.cache.borrow().get(&key) {
            return Ok(cached.clone());
        }

        // Search for the component
        let component = self.search_component(api_version, kind)?;

        // Cache the result (even if None)
        self.cache.borrow_mut().insert(key, component.clone());

        Ok(component)
    }

    /// Search for a component in all search paths
    fn search_component(&self, api_version: &str, kind: &str) -> Result<Option<Component>> {
        for search_path in &self.search_paths {
            // Look for <search_path>/<apiVersion>/<kind>/Chart.yaml
            let component_path = search_path.join(api_version).join(kind);

            let chart_path = component_path.join("Chart.yaml");

            if chart_path.exists() {
                let component = HelmComponent {
                    path: component_path,
                    api_version: api_version.to_string(),
                    kind: kind.to_string(),
                };

                // Verify it's valid
                component.verify()?;

                return Ok(Some(Component::Helm(component)));
            }
        }

        Ok(None)
    }

    /// Clear the component cache
    ///
    /// Useful for testing or when the filesystem may have changed
    pub fn clear_cache(&self) {
        self.cache.borrow_mut().clear();
    }

    /// Get the number of cached entries
    pub fn cache_size(&self) -> usize {
        self.cache.borrow().len()
    }

    /// Get the search paths
    pub fn search_paths(&self) -> &[PathBuf] {
        &self.search_paths
    }

    /// List all components in the search paths
    ///
    /// This performs a full filesystem scan and returns all discovered components.
    /// Results are cached for individual lookups.
    pub fn list_components(&self) -> Result<Vec<Component>> {
        let mut components = Vec::new();

        for search_path in &self.search_paths {
            let components_dir = search_path;
            if !components_dir.exists() {
                continue;
            }

            // Walk the components directory structure
            if let Ok(entries) = std::fs::read_dir(components_dir) {
                for api_version_entry in entries.flatten() {
                    let api_version_path = api_version_entry.path();
                    if !api_version_path.is_dir() {
                        continue;
                    }

                    let api_version = api_version_entry.file_name().to_string_lossy().to_string();

                    if let Ok(kind_entries) = std::fs::read_dir(&api_version_path) {
                        for kind_entry in kind_entries.flatten() {
                            let kind_path = kind_entry.path();
                            if !kind_path.is_dir() {
                                continue;
                            }

                            let chart_path = kind_path.join("Chart.yaml");
                            if chart_path.exists() {
                                let kind = kind_entry.file_name().to_string_lossy().to_string();

                                let component = HelmComponent {
                                    path: kind_path,
                                    api_version: api_version.clone(),
                                    kind: kind.clone(),
                                };

                                // Cache it
                                self.cache
                                    .borrow_mut()
                                    .insert((api_version.clone(), kind), Some(Component::Helm(component.clone())));

                                components.push(Component::Helm(component));
                            }
                        }
                    }
                }
            }
        }

        Ok(components)
    }
}

impl std::fmt::Debug for ComponentRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ComponentRegistry")
            .field("search_paths", &self.search_paths)
            .field("cache_size", &self.cache_size())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;
    use tempfile::TempDir;

    fn create_test_component(base: &Path, api_version: &str, kind: &str) {
        let component_dir = base.join(api_version).join(kind);
        fs::create_dir_all(&component_dir).unwrap();

        let chart_yaml = component_dir.join("Chart.yaml");
        fs::write(
            chart_yaml,
            format!("apiVersion: v2\nname: {}\nversion: 1.0.0\n", kind.to_lowercase()),
        )
        .unwrap();
    }

    #[test]
    fn test_helm_component_chart_yaml_path() {
        let component = HelmComponent {
            path: PathBuf::from("/test/path"),
            api_version: "v1".to_string(),
            kind: "Test".to_string(),
        };

        assert_eq!(component.chart_yaml_path(), PathBuf::from("/test/path/Chart.yaml"));
    }

    #[test]
    fn test_component_registry_new() {
        let paths = vec![PathBuf::from("/path1"), PathBuf::from("/path2")];
        let registry = ComponentRegistry::new(paths.clone());

        assert_eq!(registry.search_paths, paths);
        assert_eq!(registry.cache_size(), 0);
    }

    #[test]
    fn test_find_component_success() {
        let temp = TempDir::new().unwrap();
        create_test_component(temp.path(), "v1.example.io", "WebApp");

        let registry = ComponentRegistry::new(vec![temp.path().to_path_buf()]);
        let result = registry.find_component("v1.example.io", "WebApp").unwrap();

        assert!(result.is_some());
        match result.unwrap() {
            Component::Helm(helm) => {
                assert_eq!(helm.api_version, "v1.example.io");
                assert_eq!(helm.kind, "WebApp");
                assert!(helm.chart_yaml_path().exists());
            }
        }
    }

    #[test]
    fn test_find_component_not_found() {
        let temp = TempDir::new().unwrap();
        let registry = ComponentRegistry::new(vec![temp.path().to_path_buf()]);

        let result = registry.find_component("v1.example.io", "Missing").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_find_component_caching() {
        let temp = TempDir::new().unwrap();
        create_test_component(temp.path(), "v1.example.io", "WebApp");

        let registry = ComponentRegistry::new(vec![temp.path().to_path_buf()]);

        // First lookup - should cache
        let result1 = registry.find_component("v1.example.io", "WebApp").unwrap();
        assert!(result1.is_some());
        assert_eq!(registry.cache_size(), 1);

        // Second lookup - should hit cache
        let result2 = registry.find_component("v1.example.io", "WebApp").unwrap();
        assert!(result2.is_some());
        assert_eq!(registry.cache_size(), 1); // Cache size unchanged

        // Not found should also be cached
        let result3 = registry.find_component("v1.example.io", "Missing").unwrap();
        assert!(result3.is_none());
        assert_eq!(registry.cache_size(), 2);

        // Second lookup of missing - should hit cache
        let result4 = registry.find_component("v1.example.io", "Missing").unwrap();
        assert!(result4.is_none());
        assert_eq!(registry.cache_size(), 2);
    }

    #[test]
    fn test_clear_cache() {
        let temp = TempDir::new().unwrap();
        create_test_component(temp.path(), "v1.example.io", "WebApp");

        let registry = ComponentRegistry::new(vec![temp.path().to_path_buf()]);

        registry.find_component("v1.example.io", "WebApp").unwrap();
        assert_eq!(registry.cache_size(), 1);

        registry.clear_cache();
        assert_eq!(registry.cache_size(), 0);
    }

    #[test]
    fn test_multiple_search_paths() {
        let temp1 = TempDir::new().unwrap();
        let temp2 = TempDir::new().unwrap();

        create_test_component(temp1.path(), "v1.example.io", "App1");
        create_test_component(temp2.path(), "v2.example.io", "App2");

        let registry = ComponentRegistry::new(vec![temp1.path().to_path_buf(), temp2.path().to_path_buf()]);

        let app1 = registry.find_component("v1.example.io", "App1").unwrap();
        assert!(app1.is_some());

        let app2 = registry.find_component("v2.example.io", "App2").unwrap();
        assert!(app2.is_some());
    }

    #[test]
    fn test_list_components() {
        let temp = TempDir::new().unwrap();

        create_test_component(temp.path(), "v1.example.io", "WebApp");
        create_test_component(temp.path(), "v1.example.io", "Database");
        create_test_component(temp.path(), "v2.example.io", "Cache");

        let registry = ComponentRegistry::new(vec![temp.path().to_path_buf()]);

        let components = registry.list_components().unwrap();
        assert_eq!(components.len(), 3);

        // All should be cached now
        assert_eq!(registry.cache_size(), 3);
    }

    #[test]
    fn test_list_components_empty_dir() {
        let temp = TempDir::new().unwrap();
        let registry = ComponentRegistry::new(vec![temp.path().to_path_buf()]);

        let components = registry.list_components().unwrap();
        assert_eq!(components.len(), 0);
    }

    #[test]
    fn test_component_verify_missing_chart() {
        let temp = TempDir::new().unwrap();
        let component = HelmComponent {
            path: temp.path().to_path_buf(),
            api_version: "v1".to_string(),
            kind: "Test".to_string(),
        };

        let result = component.verify();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Chart.yaml not found"));
    }
}
