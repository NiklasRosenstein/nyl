use clap::{Args, Subcommand};
use std::fs;
use std::path::{Path, PathBuf};
use tracing::{debug, info};

use crate::config::ProjectConfig;
use crate::{NylError, Result};

/// Create a new nyl project or component
#[derive(Args, Debug)]
pub struct NewArgs {
    #[command(subcommand)]
    command: NewSubcommand,
}

#[derive(Subcommand, Debug)]
enum NewSubcommand {
    /// Create a new nyl project
    Project {
        /// Directory where to create the project
        #[arg(value_name = "DIR")]
        dir: PathBuf,

        /// Configuration format (toml or yaml)
        #[arg(short, long, default_value = "toml")]
        format: ConfigFormat,
    },
    /// Create a new component
    Component {
        /// Component API version (e.g., v1.example.io)
        api_version: String,

        /// Component kind (e.g., MyApp)
        kind: String,
    },
}

#[derive(Debug, Clone, clap::ValueEnum)]
enum ConfigFormat {
    Toml,
    Yaml,
}

pub fn execute(args: NewArgs) -> Result<()> {
    match args.command {
        NewSubcommand::Project { dir, format } => create_project(&dir, format),
        NewSubcommand::Component { api_version, kind } => create_component(&api_version, &kind),
    }
}

/// Create a new nyl project
fn create_project(project_path: &Path, format: ConfigFormat) -> Result<()> {
    info!("Creating new project at: {}", project_path.display());

    // Create project directory if it doesn't exist
    if project_path.exists() {
        // Check if a nyl project already exists in this directory
        let toml_config = project_path.join(".nyl-project.toml");
        let yaml_config = project_path.join(".nyl-project.yaml");
        if toml_config.exists() || yaml_config.exists() {
            return Err(NylError::Config(format!(
                "Project already exists at: {}",
                project_path.display()
            )));
        }
        println!("✓ Using existing directory: {}", project_path.display());
    } else {
        fs::create_dir_all(project_path)?;
        println!("✓ Created project directory: {}", project_path.display());
    }

    // Create components directory
    let components_dir = project_path.join("components");
    fs::create_dir(&components_dir)?;
    println!("✓ Created components directory: {}", components_dir.display());

    // Create config file based on format (hidden files with . prefix)
    let (config_filename, config_content) = match format {
        ConfigFormat::Toml => (
            ".nyl-project.toml",
            r#"[settings]
components_path = "components"
search_path = ["."]
"#,
        ),
        ConfigFormat::Yaml => (
            ".nyl-project.yaml",
            r"settings:
  components_path: components
  search_path:
    - .
",
        ),
    };

    let config_path = project_path.join(config_filename);
    fs::write(&config_path, config_content)?;
    println!("✓ Created configuration file: {}", config_path.display());

    println!("\n✓ Project created successfully at: {}", project_path.display());
    println!("\nNext steps:");

    // Show relative path if possible, otherwise absolute path
    if let Ok(current_dir) = std::env::current_dir() {
        if let Ok(relative) = project_path.strip_prefix(&current_dir) {
            if relative.as_os_str() != "." {
                println!("  cd {}", relative.display());
            }
        } else {
            println!("  cd {}", project_path.display());
        }
    } else {
        println!("  cd {}", project_path.display());
    }

    println!("  nyl new component <api-version> <kind>");

    Ok(())
}

/// Create a new component
fn create_component(api_version: &str, kind: &str) -> Result<()> {
    info!("Creating new component: {}/{}", api_version, kind);

    // Load project config to find components directory
    let config = ProjectConfig::load(None)?;
    let components_base = config.get_components_path();

    debug!("Components base path: {}", components_base.display());

    // Create component directory structure
    let component_dir = components_base.join(api_version).join(kind);

    if component_dir.exists() {
        return Err(NylError::Config(format!(
            "Component already exists: {}",
            component_dir.display()
        )));
    }

    fs::create_dir_all(&component_dir)?;
    println!("✓ Created component directory: {}", component_dir.display());

    // Create Chart.yaml
    create_chart_yaml(&component_dir, kind)?;

    // Create values.yaml
    create_values_yaml(&component_dir)?;

    // Create values.schema.json
    create_values_schema(&component_dir)?;

    // Create templates directory and deployment.yaml
    create_deployment_template(&component_dir, kind)?;

    println!("\n✓ Component '{}/{}' created successfully!", api_version, kind);
    println!("\nNext steps:");
    println!("  Edit {}/Chart.yaml to customize metadata", component_dir.display());
    println!(
        "  Edit {}/values.yaml to define component values",
        component_dir.display()
    );
    println!(
        "  Edit {}/templates/deployment.yaml to customize Kubernetes resources",
        component_dir.display()
    );

    Ok(())
}

/// Create Chart.yaml file
fn create_chart_yaml(component_dir: &Path, kind: &str) -> Result<()> {
    let chart_path = component_dir.join("Chart.yaml");
    let chart_content = format!(
        r#"apiVersion: v2
name: {}
description: A Helm chart for {}
type: application
version: 0.1.0
appVersion: "1.0"
"#,
        kind.to_lowercase(),
        kind
    );

    fs::write(&chart_path, chart_content)?;
    println!("✓ Created Chart.yaml: {}", chart_path.display());
    Ok(())
}

/// Create values.yaml file
fn create_values_yaml(component_dir: &Path) -> Result<()> {
    let values_path = component_dir.join("values.yaml");
    let values_content = r#"# Default values for the component
replicaCount: 1

image:
  repository: nginx
  pullPolicy: IfNotPresent
  tag: "latest"

service:
  type: ClusterIP
  port: 80

resources:
  limits:
    cpu: 100m
    memory: 128Mi
  requests:
    cpu: 100m
    memory: 128Mi
"#;

    fs::write(&values_path, values_content)?;
    println!("✓ Created values.yaml: {}", values_path.display());
    Ok(())
}

/// Create values.schema.json file
fn create_values_schema(component_dir: &Path) -> Result<()> {
    let schema_path = component_dir.join("values.schema.json");
    let schema_content = r#"{
  "$schema": "http://json-schema.org/draft-07/schema#",
  "type": "object",
  "properties": {
    "replicaCount": {
      "type": "integer",
      "minimum": 1
    },
    "image": {
      "type": "object",
      "properties": {
        "repository": {
          "type": "string"
        },
        "pullPolicy": {
          "type": "string",
          "enum": ["Always", "IfNotPresent", "Never"]
        },
        "tag": {
          "type": "string"
        }
      },
      "required": ["repository", "tag"]
    },
    "service": {
      "type": "object",
      "properties": {
        "type": {
          "type": "string",
          "enum": ["ClusterIP", "NodePort", "LoadBalancer"]
        },
        "port": {
          "type": "integer"
        }
      }
    }
  },
  "required": ["replicaCount", "image"]
}
"#;

    fs::write(&schema_path, schema_content)?;
    println!("✓ Created values.schema.json: {}", schema_path.display());
    Ok(())
}

/// Create templates directory and deployment.yaml
fn create_deployment_template(component_dir: &Path, kind: &str) -> Result<()> {
    let templates_dir = component_dir.join("templates");
    fs::create_dir(&templates_dir)?;

    let deployment_path = templates_dir.join("deployment.yaml");
    let deployment_content = format!(
        r#"apiVersion: apps/v1
kind: Deployment
metadata:
  name: {{{{ include "chart.fullname" . }}}}
  labels:
    app.kubernetes.io/name: {}
    app.kubernetes.io/instance: {{{{ .Release.Name }}}}
spec:
  replicas: {{{{ .Values.replicaCount }}}}
  selector:
    matchLabels:
      app.kubernetes.io/name: {}
      app.kubernetes.io/instance: {{{{ .Release.Name }}}}
  template:
    metadata:
      labels:
        app.kubernetes.io/name: {}
        app.kubernetes.io/instance: {{{{ .Release.Name }}}}
    spec:
      containers:
        - name: {{{{ .Chart.Name }}}}
          image: "{{{{ .Values.image.repository }}}}:{{{{ .Values.image.tag }}}}"
          imagePullPolicy: {{{{ .Values.image.pullPolicy }}}}
          ports:
            - name: http
              containerPort: {{{{ .Values.service.port }}}}
              protocol: TCP
          resources:
            {{{{- toYaml .Values.resources | nindent 12 }}}}
"#,
        kind.to_lowercase(),
        kind.to_lowercase(),
        kind.to_lowercase()
    );

    fs::write(&deployment_path, deployment_content)?;
    println!("✓ Created templates/deployment.yaml: {}", deployment_path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_create_project() {
        let temp = TempDir::new().unwrap();
        let project_dir = temp.path().join("test-project");

        let result = create_project(&project_dir, ConfigFormat::Toml);
        assert!(result.is_ok());

        assert!(project_dir.exists());
        assert!(project_dir.join("components").exists());
        assert!(project_dir.join(".nyl-project.toml").exists());

        // Verify config file content (TOML format)
        let config_content = fs::read_to_string(project_dir.join(".nyl-project.toml")).unwrap();
        assert!(config_content.contains(r#"components_path = "components""#));
        assert!(config_content.contains(r#"search_path = ["."]"#));
    }

    #[test]
    fn test_create_project_yaml() {
        let temp = TempDir::new().unwrap();
        let project_dir = temp.path().join("test-project-yaml");

        let result = create_project(&project_dir, ConfigFormat::Yaml);
        assert!(result.is_ok());

        assert!(project_dir.exists());
        assert!(project_dir.join("components").exists());
        assert!(project_dir.join(".nyl-project.yaml").exists());

        // Verify config file content (YAML format)
        let config_content = fs::read_to_string(project_dir.join(".nyl-project.yaml")).unwrap();
        assert!(config_content.contains("components_path: components"));
        assert!(config_content.contains("search_path:"));
    }

    #[test]
    fn test_create_project_already_exists() {
        let temp = TempDir::new().unwrap();
        let project_dir = temp.path().join("test-project");
        fs::create_dir(&project_dir).unwrap();
        // Create a config file to simulate an existing project
        fs::write(project_dir.join(".nyl-project.toml"), "").unwrap();

        let result = create_project(&project_dir, ConfigFormat::Toml);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already exists"));
    }

    #[test]
    fn test_create_component() {
        let temp = TempDir::new().unwrap();

        // Create a project first
        let config_path = temp.path().join(".nyl-project.yaml");
        fs::write(&config_path, "settings: {}").unwrap();

        let components_dir = temp.path().join("components");
        fs::create_dir(&components_dir).unwrap();

        // Change to temp directory so config can be found
        let original_dir = std::env::current_dir().unwrap();
        std::env::set_current_dir(temp.path()).unwrap();

        let result = create_component("v1.example.io", "MyApp");

        // Restore original directory before assertions
        std::env::set_current_dir(&original_dir).ok();

        assert!(result.is_ok());

        let component_dir = components_dir.join("v1.example.io").join("MyApp");
        assert!(component_dir.exists());
        assert!(component_dir.join("Chart.yaml").exists());
        assert!(component_dir.join("values.yaml").exists());
        assert!(component_dir.join("values.schema.json").exists());
        assert!(component_dir.join("templates").join("deployment.yaml").exists());
    }

    #[test]
    fn test_create_component_already_exists() {
        let temp = TempDir::new().unwrap();

        // Create a project first
        let config_path = temp.path().join(".nyl-project.yaml");
        fs::write(&config_path, "settings: {}").unwrap();

        let component_dir = temp.path().join("components").join("v1.example.io").join("MyApp");
        fs::create_dir_all(&component_dir).unwrap();

        // Change to temp directory
        let original_dir = std::env::current_dir().unwrap();
        std::env::set_current_dir(temp.path()).unwrap();

        let result = create_component("v1.example.io", "MyApp");

        // Restore original directory before assertions
        std::env::set_current_dir(&original_dir).ok();

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already exists"));
    }
}
