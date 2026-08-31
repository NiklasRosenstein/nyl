use clap::{Args, Subcommand};
use std::fs;
use std::path::{Path, PathBuf};
use tracing::{debug, info};

use crate::config::ProjectConfig;
use crate::resources::GitOpsResourceKind;
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
    },
    /// Create a new component
    Component {
        /// Component API version (e.g., v1.example.io)
        api_version: String,

        /// Component kind (e.g., MyApp)
        kind: String,
    },
    /// Create a Kubernetes-shaped rendered GitOps configuration resource.
    Resource(ResourceScaffoldArgs),
    /// Create rendered GitOps configuration resources using kind-specific aliases.
    Gitops {
        #[command(subcommand)]
        command: NewGitopsSubcommand,
    },
}

#[derive(Args, Debug, Clone)]
struct ResourceScaffoldArgs {
    /// Resource kind.
    #[arg(value_enum)]
    kind: GitOpsResourceKind,
    /// Local resource name.
    name: String,
    /// Exact output file path.
    #[arg(long, conflicts_with = "colocate")]
    output: Option<PathBuf>,
    /// ApplicationGroup source directory.
    #[arg(long)]
    source: Option<PathBuf>,
    /// Place an ApplicationGroup in SOURCE/_application-group.yaml.
    #[arg(long, requires = "source", conflicts_with = "output")]
    colocate: bool,
}

#[derive(Args, Debug, Clone)]
struct AliasScaffoldArgs {
    name: String,
    #[arg(long, conflicts_with = "colocate")]
    output: Option<PathBuf>,
    #[arg(long)]
    source: Option<PathBuf>,
    #[arg(long, requires = "source", conflicts_with = "output")]
    colocate: bool,
}

#[derive(Subcommand, Debug)]
enum NewGitopsSubcommand {
    Repository(AliasScaffoldArgs),
    Cluster(AliasScaffoldArgs),
    Target(AliasScaffoldArgs),
    Project(AliasScaffoldArgs),
    ApplicationGroup(AliasScaffoldArgs),
}

pub fn execute(args: NewArgs) -> Result<()> {
    match args.command {
        NewSubcommand::Project { dir } => create_project(&dir),
        NewSubcommand::Component { api_version, kind } => create_component(&api_version, &kind),
        NewSubcommand::Resource(args) => scaffold_resource(args, None),
        NewSubcommand::Gitops { command } => {
            let (kind, args) = match command {
                NewGitopsSubcommand::Repository(args) => (GitOpsResourceKind::GitRepository, args),
                NewGitopsSubcommand::Cluster(args) => (GitOpsResourceKind::Cluster, args),
                NewGitopsSubcommand::Target(args) => (GitOpsResourceKind::GitOpsTarget, args),
                NewGitopsSubcommand::Project(args) => (GitOpsResourceKind::AppProjectDefinition, args),
                NewGitopsSubcommand::ApplicationGroup(args) => (GitOpsResourceKind::ApplicationGroup, args),
            };
            scaffold_resource(
                ResourceScaffoldArgs {
                    kind,
                    name: args.name,
                    output: args.output,
                    source: args.source,
                    colocate: args.colocate,
                },
                None,
            )
        }
    }
}

fn scaffold_resource(args: ResourceScaffoldArgs, project_dir: Option<&Path>) -> Result<()> {
    validate_resource_name(&args.name)?;
    if args.kind != GitOpsResourceKind::ApplicationGroup && (args.source.is_some() || args.colocate) {
        return Err(NylError::config(
            "--source and --colocate are only valid for ApplicationGroup",
        ));
    }
    let config = ProjectConfig::load_from_dir(None, project_dir)?;
    let output = if let Some(output) = args.output {
        output
    } else if args.colocate {
        args.source
            .as_ref()
            .expect("clap requires --source with --colocate")
            .join("_application-group.yaml")
    } else {
        let directory = match args.kind {
            GitOpsResourceKind::GitRepository => "repositories",
            GitOpsResourceKind::Cluster => "clusters",
            GitOpsResourceKind::GitOpsTarget => "targets",
            GitOpsResourceKind::AppProjectDefinition => "projects",
            GitOpsResourceKind::ApplicationGroup => "application-groups",
        };
        config
            .get_gitops_scaffold_path()
            .join(directory)
            .join(format!("{}.yaml", args.name))
    };
    if output.exists() {
        return Err(NylError::config(format!(
            "Refusing to overwrite existing resource: {}",
            output.display()
        )));
    }
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)?;
    }
    let source = if args.colocate {
        None
    } else {
        args.source.as_deref().map(|path| path.to_string_lossy())
    };
    let yaml = render_resource_scaffold(args.kind, &args.name, source.as_deref());
    fs::write(&output, yaml)?;
    println!("✓ Created {}: {}", args.kind.as_str(), output.display());
    Ok(())
}

fn render_resource_scaffold(kind: GitOpsResourceKind, name: &str, source: Option<&str>) -> String {
    let schema = format!(
        "https://niklasrosenstein.github.io/nyl/reference/schemas/{}",
        kind.schema_filename()
    );
    let body = match kind {
        GitOpsResourceKind::GitRepository => format!(
            "apiVersion: gitops.nyl/v1\nkind: GitRepository\nmetadata:\n  name: {name}\nspec:\n  repoURL: https://example.invalid/{name}.git\n"
        ),
        GitOpsResourceKind::Cluster => format!(
            "apiVersion: gitops.nyl/v1\nkind: Cluster\nmetadata:\n  name: {name}\nspec:\n  destination:\n    server: https://kubernetes.default.svc\n  # Populate from the selected context with: nyl cluster update {name}\n  kubernetes:\n    apiVersions: []\n  values: {{}}\n  live:\n    context: {name}\n"
        ),
        GitOpsResourceKind::GitOpsTarget => format!(
            "apiVersion: gitops.nyl/v1\nkind: GitOpsTarget\nmetadata:\n  name: {name}\nspec:\n  clusterRef:\n    name: {name}\n  values:\n    environment: {name}\n  publication:\n    repositoryRef:\n      name: deploy\n    revision: deploy/{name}\n    pathPrefix: {name}\n"
        ),
        GitOpsResourceKind::AppProjectDefinition => format!(
            "apiVersion: gitops.nyl/v1\nkind: AppProjectDefinition\nmetadata:\n  name: {name}\nspec:\n  management: Rendered\n  manifest:\n    apiVersion: argoproj.io/v1alpha1\n    kind: AppProject\n    metadata:\n      name: {name}\n      namespace: argocd\n    spec:\n      sourceRepos: []\n      destinations: []\n"
        ),
        GitOpsResourceKind::ApplicationGroup => {
            let source = source.map_or_else(String::new, |source| format!("  source:\n    path: {source}\n"));
            format!(
                "apiVersion: gitops.nyl/v1\nkind: ApplicationGroup\nmetadata:\n  name: {name}\nspec:\n  projectRef: {name}\n  applicationNamespace: argocd\n{source}  destinationNamespace: default\n"
            )
        }
    };
    format!("# yaml-language-server: $schema={schema}\n{body}")
}

fn validate_resource_name(name: &str) -> Result<()> {
    let valid = !name.is_empty()
        && name.len() <= 253
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-'))
        && name.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric)
        && name.as_bytes().last().is_some_and(u8::is_ascii_alphanumeric);
    if valid {
        Ok(())
    } else {
        Err(NylError::config(format!(
            "Resource name {name:?} must be a Kubernetes DNS subdomain"
        )))
    }
}

/// Create a new nyl project
fn create_project(project_path: &Path) -> Result<()> {
    info!("Creating new project at: {}", project_path.display());

    // Create project directory if it doesn't exist
    if project_path.exists() {
        // Check if a nyl project already exists in this directory
        let toml_config = project_path.join("nyl.toml");
        if toml_config.exists() {
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

    let project_directories = [
        project_path.join("components"),
        project_path.join("applications"),
        project_path.join("config/repositories"),
        project_path.join("config/clusters"),
        project_path.join("config/targets"),
        project_path.join("config/projects"),
        project_path.join("config/application-groups"),
    ];
    for directory in project_directories {
        fs::create_dir_all(&directory)?;
        println!("✓ Created directory: {}", directory.display());
    }

    let config_content = r#"#:schema https://niklasrosenstein.github.io/nyl/reference/schemas/nyl.schema.json

[project]
components_search_paths = ["components"]
helm_chart_search_paths = ["."]
gitops_scaffold_path = "config"
"#;
    let config_path = project_path.join("nyl.toml");
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
    create_component_in_dir(api_version, kind, None)
}

/// Create a new component in a specific directory (useful for testing)
fn create_component_in_dir(api_version: &str, kind: &str, project_dir: Option<&Path>) -> Result<()> {
    info!("Creating new component: {}/{}", api_version, kind);

    // Load project config to find components directory
    let config = ProjectConfig::load_from_dir(None, project_dir)?;
    let components_base = config.get_components_search_paths()[0].clone();

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

        let result = create_project(&project_dir);
        assert!(result.is_ok());

        assert!(project_dir.exists());
        assert!(project_dir.join("components").exists());
        assert!(project_dir.join("applications").exists());
        assert!(project_dir.join("config/repositories").exists());
        assert!(project_dir.join("config/clusters").exists());
        assert!(project_dir.join("config/targets").exists());
        assert!(project_dir.join("config/projects").exists());
        assert!(project_dir.join("config/application-groups").exists());
        assert!(project_dir.join("nyl.toml").exists());

        let config_content = fs::read_to_string(project_dir.join("nyl.toml")).unwrap();
        assert!(config_content
            .contains("#:schema https://niklasrosenstein.github.io/nyl/reference/schemas/nyl.schema.json"));
        assert!(config_content.contains(r#"components_search_paths = ["components"]"#));
        assert!(config_content.contains(r#"helm_chart_search_paths = ["."]"#));
        assert!(config_content.contains(r#"gitops_scaffold_path = "config""#));
    }

    #[test]
    fn test_create_project_already_exists() {
        let temp = TempDir::new().unwrap();
        let project_dir = temp.path().join("test-project");
        fs::create_dir(&project_dir).unwrap();
        fs::write(project_dir.join("nyl.toml"), "").unwrap();

        let result = create_project(&project_dir);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already exists"));
    }

    #[test]
    fn test_create_component() {
        let temp = TempDir::new().unwrap();

        // Create a project first
        let config_path = temp.path().join("nyl.toml");
        fs::write(&config_path, "[project]\ncomponents_search_paths = [\"components\"]\n").unwrap();

        let components_dir = temp.path().join("components");
        fs::create_dir(&components_dir).unwrap();

        let result = create_component_in_dir("v1.example.io", "MyApp", Some(temp.path()));

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
        let config_path = temp.path().join("nyl.toml");
        fs::write(&config_path, "[project]\ncomponents_search_paths = [\"components\"]\n").unwrap();

        let component_dir = temp.path().join("components").join("v1.example.io").join("MyApp");
        fs::create_dir_all(&component_dir).unwrap();

        let result = create_component_in_dir("v1.example.io", "MyApp", Some(temp.path()));

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already exists"));
    }

    #[test]
    fn test_scaffold_gitops_resources_and_refuse_overwrite() {
        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join("nyl.toml"), "[project]\n").unwrap();
        let args = ResourceScaffoldArgs {
            kind: GitOpsResourceKind::GitRepository,
            name: "deploy".to_string(),
            output: None,
            source: None,
            colocate: false,
        };
        scaffold_resource(args.clone(), Some(temp.path())).unwrap();
        let path = temp.path().join("config/repositories/deploy.yaml");
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("git-repository.schema.json"));
        assert!(content.contains("kind: GitRepository"));
        assert!(scaffold_resource(args, Some(temp.path())).is_err());
    }

    #[test]
    fn test_scaffold_cluster_and_target_use_cluster_model() {
        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join("nyl.toml"), "[project]\n").unwrap();
        scaffold_resource(
            ResourceScaffoldArgs {
                kind: GitOpsResourceKind::Cluster,
                name: "production".to_string(),
                output: None,
                source: None,
                colocate: false,
            },
            Some(temp.path()),
        )
        .unwrap();
        scaffold_resource(
            ResourceScaffoldArgs {
                kind: GitOpsResourceKind::GitOpsTarget,
                name: "production".to_string(),
                output: None,
                source: None,
                colocate: false,
            },
            Some(temp.path()),
        )
        .unwrap();

        let cluster = fs::read_to_string(temp.path().join("config/clusters/production.yaml")).unwrap();
        assert!(cluster.contains("kind: Cluster"));
        assert!(cluster.contains("context: production"));
        let target = fs::read_to_string(temp.path().join("config/targets/production.yaml")).unwrap();
        assert!(target.contains("clusterRef:\n    name: production"));
        assert!(target.contains("publication:"));
        assert!(!target.contains("profile:"));
    }

    #[test]
    fn test_scaffold_colocated_application_group() {
        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join("nyl.toml"), "[project]\n").unwrap();
        let source = temp.path().join("applications/platform");
        scaffold_resource(
            ResourceScaffoldArgs {
                kind: GitOpsResourceKind::ApplicationGroup,
                name: "platform".to_string(),
                output: None,
                source: Some(source.clone()),
                colocate: true,
            },
            Some(temp.path()),
        )
        .unwrap();
        assert!(source.join("_application-group.yaml").is_file());
    }

    #[test]
    fn test_resource_name_rejects_path_traversal() {
        assert!(validate_resource_name("../deploy").is_err());
        assert!(validate_resource_name("Production").is_err());
        assert!(validate_resource_name("production").is_ok());
    }
}
