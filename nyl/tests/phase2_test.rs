/// Phase 2 Integration Tests
///
/// These tests verify the complete Phase 2 implementation including:
/// - Profile loading with precedence
/// - Component discovery
/// - Helm chart resolution
/// - Component instantiation to HelmChart
use nyl::components::ComponentRegistry;
use nyl::config::ProjectConfig;
use nyl::generator::{instantiate_component, Generator};
use nyl::helm::HelmChartResolver;
use nyl::profiles::{KubeconfigSource, Profile, ProfileConfig};
use nyl::resources::ChartRef;
use nyl::secrets::SecretsConfig;
use std::fs;
use std::path::PathBuf;
use tempfile::TempDir;

/// Helper to create a test component
fn create_test_component(base: &std::path::Path, api_version: &str, kind: &str) {
    let component_dir = base.join("components").join(api_version).join(kind);
    fs::create_dir_all(&component_dir).unwrap();

    fs::write(
        component_dir.join("Chart.yaml"),
        format!("apiVersion: v2\nname: {}\nversion: 1.0.0\n", kind.to_lowercase()),
    )
    .unwrap();
}

/// Helper to create a test chart
fn create_test_chart(base: &std::path::Path, name: &str) {
    let chart_dir = base.join(name);
    fs::create_dir_all(&chart_dir).unwrap();
    fs::write(
        chart_dir.join("Chart.yaml"),
        format!("apiVersion: v2\nname: {name}\nversion: 1.0.0\n"),
    )
    .unwrap();
}

#[test]
fn test_component_discovery_integration() {
    let temp = TempDir::new().unwrap();

    // Create test components
    create_test_component(temp.path(), "v1.example.io", "WebApp");
    create_test_component(temp.path(), "v1.example.io", "Database");
    create_test_component(temp.path(), "v2.example.io", "Cache");

    // Create registry and discover components
    let registry = ComponentRegistry::new(vec![temp.path().to_path_buf()]);

    let webapp = registry.find_component("v1.example.io", "WebApp").unwrap();
    assert!(webapp.is_some());

    let database = registry.find_component("v1.example.io", "Database").unwrap();
    assert!(database.is_some());

    let cache = registry.find_component("v2.example.io", "Cache").unwrap();
    assert!(cache.is_some());

    // Verify caching works
    assert_eq!(registry.cache_size(), 3);

    // Second lookup should hit cache
    let webapp2 = registry.find_component("v1.example.io", "WebApp").unwrap();
    assert!(webapp2.is_some());
    assert_eq!(registry.cache_size(), 3); // No change

    // List all components
    let all = registry.list_components().unwrap();
    assert_eq!(all.len(), 3);
}

#[test]
fn test_helm_chart_resolution_integration() {
    let temp = TempDir::new().unwrap();

    // Create charts directory
    let charts_dir = temp.path().join("charts");
    fs::create_dir_all(&charts_dir).unwrap();

    // Create test charts
    create_test_chart(&charts_dir, "nginx");
    create_test_chart(&charts_dir, "postgres");

    // Test resolution via search path
    let resolver = HelmChartResolver::new(vec![charts_dir.clone()], temp.path().to_path_buf());

    // Resolve by name
    let chart_ref = ChartRef {
        name: Some("nginx".to_string()),
        ..Default::default()
    };

    let resolved = resolver.resolve_chart(&chart_ref).unwrap();
    assert!(resolved.path.join("Chart.yaml").exists());
    assert_eq!(resolved.path.file_name().unwrap(), "nginx");

    // Resolve by explicit path
    let chart_ref2 = ChartRef {
        path: Some(charts_dir.join("postgres").to_string_lossy().to_string()),
        ..Default::default()
    };

    let resolved2 = resolver.resolve_chart(&chart_ref2).unwrap();
    assert!(resolved2.path.join("Chart.yaml").exists());
}

#[test]
fn test_profile_loading_precedence() {
    let temp = TempDir::new().unwrap();

    // 1. Create project config with profiles
    let project_config = temp.path().join("nyl-project.yaml");
    fs::write(
        &project_config,
        r#"
settings: {}
profiles:
  dev:
    values:
      source: "project-config"
      fromProject: true
"#,
    )
    .unwrap();

    // 2. Create nyl-profiles.yaml (should override project config)
    let profiles_file = temp.path().join("nyl-profiles.yaml");
    fs::write(
        &profiles_file,
        r#"
dev:
  values:
    source: "profiles-file"
    fromProfiles: true
prod:
  values:
    source: "profiles-file"
    environment: "production"
"#,
    )
    .unwrap();

    // Change to temp directory to test file discovery
    let original_dir = std::env::current_dir().unwrap();
    std::env::set_current_dir(temp.path()).unwrap();

    // Load profiles - should prefer nyl-profiles.yaml
    let config = ProfileConfig::load(None).unwrap();

    // Restore original directory
    std::env::set_current_dir(original_dir).unwrap();

    assert!(config.file.is_some());
    assert_eq!(config.profiles.len(), 2);

    let dev = config.get("dev").unwrap();
    assert_eq!(dev.values.get("source").unwrap(), "profiles-file");
    assert_eq!(dev.values.get("fromProfiles").unwrap(), true);

    let prod = config.get("prod").unwrap();
    assert_eq!(prod.values.get("environment").unwrap(), "production");
}

#[test]
fn test_profile_with_kubeconfig_sources() {
    let temp = TempDir::new().unwrap();
    let profiles_path = temp.path().join("nyl-profiles.yaml");

    let yaml = r#"
dev:
  values:
    environment: development
  kubeconfig:
    type: local
    context: minikube

prod:
  values:
    environment: production
  kubeconfig:
    type: ssh
    user: admin
    host: k8s-master.prod.example.com
    port: 22
    path: /etc/kubernetes/admin.conf
    context: production-cluster
"#;

    fs::write(&profiles_path, yaml).unwrap();

    let config = ProfileConfig::load(Some(profiles_path)).unwrap();

    // Verify dev profile
    let dev = config.get("dev").unwrap();
    match &dev.kubeconfig {
        KubeconfigSource::Local { context, .. } => {
            assert_eq!(context.as_deref(), Some("minikube"));
        }
        _ => panic!("Expected Local kubeconfig"),
    }

    // Verify prod profile
    let prod = config.get("prod").unwrap();
    match &prod.kubeconfig {
        KubeconfigSource::Ssh {
            user,
            host,
            path,
            context,
            ..
        } => {
            assert_eq!(user, "admin");
            assert_eq!(host, "k8s-master.prod.example.com");
            assert_eq!(path, "/etc/kubernetes/admin.conf");
            assert_eq!(context.as_deref(), Some("production-cluster"));
        }
        _ => panic!("Expected SSH kubeconfig"),
    }
}

#[test]
fn test_secrets_config_integration() {
    let temp = TempDir::new().unwrap();
    let secrets_path = temp.path().join("nyl-secrets.yaml");

    fs::write(&secrets_path, "type: null\n").unwrap();

    let mut config = SecretsConfig::load(Some(secrets_path.clone())).unwrap();
    assert_eq!(config.file, Some(secrets_path));

    // Test secret operations
    config
        .set("api_key", nyl::secrets::SecretValue::from("secret-123"))
        .unwrap();
    config
        .set("db_password", nyl::secrets::SecretValue::from("pass456"))
        .unwrap();

    let keys = config.keys().unwrap();
    assert_eq!(keys.len(), 2);
    assert!(keys.contains(&"api_key".to_string()));
    assert!(keys.contains(&"db_password".to_string()));

    let api_key = config.get("api_key").unwrap();
    assert_eq!(api_key, nyl::secrets::SecretValue::String("secret-123".to_string()));
}

#[test]
fn test_component_to_helmchart_flow() {
    let temp = TempDir::new().unwrap();

    // Create component
    create_test_component(temp.path(), "v1.apps.io", "WebApplication");

    // Discover component
    let registry = ComponentRegistry::new(vec![temp.path().to_path_buf()]);
    let component = registry
        .find_component("v1.apps.io", "WebApplication")
        .unwrap()
        .unwrap();

    // Instantiate to HelmChart
    let values = serde_json::json!({
        "replicas": 3,
        "image": "nginx:1.21",
        "port": 8080
    });

    let helm_chart = instantiate_component(&component, "my-webapp", &values).unwrap();

    // Verify HelmChart structure
    assert_eq!(helm_chart.api_version, "v1.apps.io");
    assert_eq!(helm_chart.kind, "WebApplication");
    assert_eq!(helm_chart.metadata.name, "my-webapp");
    assert_eq!(helm_chart.spec.values["replicas"], 3);
    assert_eq!(helm_chart.spec.values["image"], "nginx:1.21");
    assert_eq!(helm_chart.spec.values["port"], 8080);

    // Verify release metadata
    let release = helm_chart.spec.release.as_ref().unwrap();
    assert_eq!(release.name, "my-webapp");

    // Verify chart path
    assert!(helm_chart.spec.chart.path.is_some());
    let chart_path = PathBuf::from(helm_chart.spec.chart.path.unwrap());
    assert!(chart_path.ends_with("WebApplication"));
}

#[test]
fn test_generator_end_to_end() {
    let temp = TempDir::new().unwrap();

    // Create project config - use relative paths that will be resolved relative to config location
    let config_path = temp.path().join("nyl-project.yaml");
    fs::write(
        &config_path,
        r#"
settings:
  components_path: components
  search_path: []
"#,
    )
    .unwrap();

    // Create components - the registry will look for components/<apiVersion>/<kind>
    // So we create them relative to temp.path() which is the config directory
    create_test_component(temp.path(), "v1.example.io", "WebApp");
    create_test_component(temp.path(), "v1.example.io", "Database");

    // Create charts
    let charts_dir = temp.path().join("charts");
    fs::create_dir_all(&charts_dir).unwrap();
    create_test_chart(&charts_dir, "redis");

    // Load config and create generator
    let config = ProjectConfig::load(Some(config_path)).unwrap();
    let generator = Generator::new(config);

    // Find and instantiate components
    let webapp = generator
        .find_component("v1.example.io", "WebApp")
        .unwrap()
        .expect("WebApp component should be found");

    let webapp_values = serde_json::json!({
        "replicas": 2,
        "port": 8080
    });

    let webapp_chart = generator
        .instantiate_component(&webapp, "my-app", &webapp_values)
        .unwrap();

    assert_eq!(webapp_chart.metadata.name, "my-app");
    assert_eq!(webapp_chart.api_version, "v1.example.io");
    assert_eq!(webapp_chart.kind, "WebApp");

    // List all components
    let all_components = generator.list_components().unwrap();
    assert_eq!(all_components.len(), 2);
}

#[test]
fn test_profile_merge() {
    let mut base_profile = Profile::default();
    base_profile
        .values
        .insert("key1".to_string(), serde_json::json!("value1"));
    base_profile.values.insert("key2".to_string(), serde_json::json!(42));

    let mut override_profile = Profile::default();
    override_profile
        .values
        .insert("key2".to_string(), serde_json::json!(99)); // Override
    override_profile
        .values
        .insert("key3".to_string(), serde_json::json!(true)); // New

    base_profile.merge(&override_profile);

    assert_eq!(base_profile.values.len(), 3);
    assert_eq!(base_profile.values["key1"], "value1"); // Preserved
    assert_eq!(base_profile.values["key2"], 99); // Overridden
    assert_eq!(base_profile.values["key3"], true); // Added
}

#[test]
fn test_typed_profiles_in_project_config() {
    let temp = TempDir::new().unwrap();
    let config_path = temp.path().join("nyl-project.yaml");

    let yaml = r#"
settings: {}
profiles:
  dev:
    values:
      environment: development
      debug: true
    kubeconfig:
      type: local
      context: minikube
  prod:
    values:
      environment: production
      debug: false
    kubeconfig:
      type: local
      context: prod-cluster
"#;

    fs::write(&config_path, yaml).unwrap();

    let config = ProjectConfig::load(Some(config_path)).unwrap();

    assert_eq!(config.config.profiles.len(), 2);

    let dev = config.config.profiles.get("dev").unwrap();
    assert_eq!(dev.values["environment"], "development");
    assert_eq!(dev.values["debug"], true);

    let prod = config.config.profiles.get("prod").unwrap();
    assert_eq!(prod.values["environment"], "production");
    assert_eq!(prod.values["debug"], false);
}

#[test]
fn test_component_not_found() {
    let temp = TempDir::new().unwrap();
    let registry = ComponentRegistry::new(vec![temp.path().to_path_buf()]);

    let result = registry.find_component("v1.example.io", "NonExistent").unwrap();
    assert!(result.is_none());

    // Should be cached as "not found"
    assert_eq!(registry.cache_size(), 1);
}

#[test]
fn test_helm_chart_resolution_errors() {
    let temp = TempDir::new().unwrap();
    let resolver = HelmChartResolver::new(vec![temp.path().to_path_buf()], temp.path().to_path_buf());

    // Missing chart
    let chart_ref = ChartRef {
        name: Some("missing-chart".to_string()),
        ..Default::default()
    };

    let result = resolver.resolve_chart(&chart_ref);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("not found"));

    // Note: Git chart references are now supported in Phase 2a/2b
    // Testing actual Git operations requires integration tests with real repositories
}

#[test]
fn test_multiple_search_paths_priority() {
    let temp = TempDir::new().unwrap();

    // Create two search paths with same component name
    let path1 = temp.path().join("path1");
    let path2 = temp.path().join("path2");

    create_test_component(&path1, "v1.test.io", "App");
    create_test_component(&path2, "v1.test.io", "App");

    // Registry should find from first path
    let registry = ComponentRegistry::new(vec![path1.clone(), path2.clone()]);

    let component = registry.find_component("v1.test.io", "App").unwrap().unwrap();

    match component {
        nyl::components::Component::Helm(helm) => {
            assert!(helm.path.starts_with(&path1));
        }
    }
}
