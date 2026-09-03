use clap::{Args, Subcommand};
use std::fs;
use std::path::{Path, PathBuf};
use tracing::{debug, info};

use crate::cli::resource_file::{append_document, atomic_replace};
use crate::config::ProjectConfig;
use crate::gitops::{discover_gitops_inventory, GitOpsInventoryKey};
use crate::resources::GitOpsResourceKind;
use crate::util::path_for_display;
use crate::{NylError, Result};

fn display_path(path: &Path) -> String {
    path_for_display(path)
        .display()
        .to_string()
        .replace(std::path::MAIN_SEPARATOR, "/")
}

/// Create a component or GitOps resource.
#[derive(Args, Debug)]
pub struct CreateArgs {
    #[command(subcommand)]
    command: CreateCommand,
}

#[derive(Subcommand, Debug)]
enum CreateCommand {
    /// Create a new component
    Component {
        /// Component API version (e.g., v1.example.io)
        api_version: String,

        /// Component kind (e.g., MyApp)
        kind: String,
    },
    /// Create a Git repository declaration.
    Repository(RepositoryScaffoldArgs),
    /// Create a Kubernetes cluster declaration.
    Cluster(ClusterScaffoldArgs),
    /// Create an Argo CD instance declaration.
    #[command(name = "argocd-instance", alias = "argocd")]
    ArgoCDInstance(AliasScaffoldArgs),
    /// Create a deployment target declaration.
    Target(AliasScaffoldArgs),
    /// Create an AppProject definition.
    #[command(name = "app-project")]
    AppProject(AliasScaffoldArgs),
    /// Create an application group declaration.
    ApplicationGroup(AliasScaffoldArgs),
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

#[derive(Args, Debug)]
struct ClusterScaffoldArgs {
    name: String,
    #[arg(long)]
    output: Option<PathBuf>,
    /// Local kubeconfig context. Defaults to the Cluster name.
    #[arg(long)]
    context: Option<String>,
}

#[derive(Args, Debug)]
struct RepositoryScaffoldArgs {
    name: String,
    #[arg(long)]
    output: Option<PathBuf>,
    /// Credential-free URL used for reads and generated Argo CD Applications.
    #[arg(long)]
    repo_url: String,
    /// Optional distinct URL used for publication writes.
    #[arg(long)]
    publish_url: Option<String>,
}

pub fn execute(args: CreateArgs) -> Result<()> {
    match args.command {
        CreateCommand::Component { api_version, kind } => create_component(&api_version, &kind),
        CreateCommand::Cluster(args) => scaffold_cluster(args),
        CreateCommand::Repository(args) => scaffold_repository(args),
        CreateCommand::ArgoCDInstance(args) => scaffold_alias_resource(GitOpsResourceKind::ArgoCDInstance, args),
        CreateCommand::Target(args) => scaffold_alias_resource(GitOpsResourceKind::DeploymentTarget, args),
        CreateCommand::AppProject(args) => scaffold_alias_resource(GitOpsResourceKind::AppProjectDefinition, args),
        CreateCommand::ApplicationGroup(args) => scaffold_alias_resource(GitOpsResourceKind::ApplicationGroup, args),
    }
}

fn scaffold_alias_resource(kind: GitOpsResourceKind, args: AliasScaffoldArgs) -> Result<()> {
    scaffold_resource(
        ResourceScaffoldArgs {
            kind,
            name: args.name,
            output: args.output,
            source: args.source,
            colocate: args.colocate,
        },
        None,
        None,
        None,
    )
    .map(|_| ())
}

fn scaffold_repository(args: RepositoryScaffoldArgs) -> Result<()> {
    crate::resources::validate_repository_coordinates(&args.repo_url, args.publish_url.as_deref())?;
    let repository_urls = (args.repo_url.as_str(), args.publish_url.as_deref());
    scaffold_resource(
        ResourceScaffoldArgs {
            kind: GitOpsResourceKind::GitRepository,
            name: args.name,
            output: args.output,
            source: None,
            colocate: false,
        },
        None,
        None,
        Some(repository_urls),
    )
    .map(|_| ())
}

fn scaffold_cluster(args: ClusterScaffoldArgs) -> Result<()> {
    let context = args.context.unwrap_or_else(|| args.name.clone());
    if context.trim().is_empty() {
        return Err(NylError::config("--context must not be empty"));
    }
    let name = args.name;
    scaffold_resource(
        ResourceScaffoldArgs {
            kind: GitOpsResourceKind::Cluster,
            name: name.clone(),
            output: args.output,
            source: None,
            colocate: false,
        },
        None,
        Some(&context),
        None,
    )
    .map(|_| ())
}

fn scaffold_resource(
    args: ResourceScaffoldArgs,
    project_dir: Option<&Path>,
    cluster_context: Option<&str>,
    repository_urls: Option<(&str, Option<&str>)>,
) -> Result<PathBuf> {
    validate_resource_name(&args.name)?;
    if args.kind != GitOpsResourceKind::ApplicationGroup && (args.source.is_some() || args.colocate) {
        return Err(NylError::config(
            "--source and --colocate are only valid for ApplicationGroup",
        ));
    }
    let start_dir = project_dir.unwrap_or_else(|| Path::new("."));
    let inventory = discover_gitops_inventory(start_dir, None)?;
    let key = GitOpsInventoryKey::new(args.kind, &args.name);
    if let Some(existing) = inventory.resources.get(&key) {
        return Err(NylError::config(format!(
            "{} {:?} already exists in {} document {}",
            args.kind.as_str(),
            args.name,
            existing.source_path.display(),
            existing.document_index
        )));
    }
    let config = &inventory.project_config;
    let primary = inventory.project_root.join("gitops.yaml");
    let use_primary = args.output.is_none() && !args.colocate && primary.exists();
    let output = if let Some(output) = args.output {
        output
    } else if args.colocate {
        args.source
            .as_ref()
            .expect("clap requires --source with --colocate")
            .join("_application-group.yaml")
    } else if use_primary {
        primary
    } else {
        let directory = match args.kind {
            GitOpsResourceKind::GitRepository => "repositories",
            GitOpsResourceKind::Cluster => "clusters",
            GitOpsResourceKind::ArgoCDInstance => "argocd-instances",
            GitOpsResourceKind::DeploymentTarget => "targets",
            GitOpsResourceKind::AppProjectDefinition => "projects",
            GitOpsResourceKind::ApplicationGroup => "application-groups",
        };
        config
            .get_gitops_scaffold_path()
            .join(directory)
            .join(format!("{}.yaml", args.name))
    };
    if output.exists() && !use_primary {
        return Err(NylError::config(format!(
            "Refusing to overwrite existing resource: {}",
            display_path(&output)
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
    let yaml = render_resource_scaffold(
        args.kind,
        &args.name,
        source.as_deref(),
        cluster_context,
        repository_urls,
    );
    if use_primary {
        let relative = output.strip_prefix(&inventory.project_root).map_err(|_| {
            NylError::config(format!(
                "Primary GitOps file {} is outside the project",
                output.display()
            ))
        })?;
        if !inventory.yaml_files.iter().any(|path| path == relative) {
            return Err(NylError::config(format!(
                "Primary GitOps file {} is not visible to GitOps discovery",
                output.display()
            )));
        }
        let contents = fs::read_to_string(&output)?;
        let updated = append_document(&contents, &yaml);
        atomic_replace(&output, &contents, &updated)?;
        println!("✓ Created {} in {}", args.kind.as_str(), display_path(&output));
    } else {
        fs::write(&output, yaml)?;
        println!("✓ Created {}: {}", args.kind.as_str(), display_path(&output));
    }
    Ok(output)
}

fn render_resource_scaffold(
    kind: GitOpsResourceKind,
    name: &str,
    source: Option<&str>,
    cluster_context: Option<&str>,
    repository_urls: Option<(&str, Option<&str>)>,
) -> String {
    let schema = format!(
        "https://niklasrosenstein.github.io/nyl/reference/schemas/{}",
        kind.schema_filename()
    );
    let body = match kind {
        GitOpsResourceKind::GitRepository => {
            let (repo_url, publish_url) = repository_urls.map_or_else(
                || (format!("https://example.invalid/{name}.git"), None),
                |(repo_url, publish_url)| (repo_url.to_owned(), publish_url.map(ToOwned::to_owned)),
            );
            let repo_url = serde_json::to_string(&repo_url).expect("string serialization cannot fail");
            let publish_url = publish_url.map_or_else(String::new, |publish_url| {
                format!(
                    "  publishURL: {}\n",
                    serde_json::to_string(&publish_url).expect("string serialization cannot fail")
                )
            });
            format!(
                "apiVersion: gitops.nyl/v1\nkind: GitRepository\nmetadata:\n  name: {name}\nspec:\n  repoURL: {repo_url}\n{publish_url}"
            )
        }
        GitOpsResourceKind::Cluster => {
            let context = cluster_context.unwrap_or(name);
            format!(
                "apiVersion: gitops.nyl/v1\nkind: Cluster\nmetadata:\n  name: {name}\nspec:\n  destination:\n    server: https://kubernetes.default.svc\n  # Populate from the selected context with: nyl update cluster {name}\n  kubernetes:\n    apiVersions: []\n  values: {{}}\n  live:\n    context: {context}\n"
            )
        }
        GitOpsResourceKind::ArgoCDInstance => format!(
            "apiVersion: gitops.nyl/v1\nkind: ArgoCDInstance\nmetadata:\n  name: {name}\nspec:\n  clusterRef:\n    name: {name}\n  namespace: argocd\n"
        ),
        GitOpsResourceKind::DeploymentTarget => format!(
            "apiVersion: gitops.nyl/v1\nkind: DeploymentTarget\nmetadata:\n  name: {name}\nspec:\n  publication:\n    repositoryRef:\n      name: deploy\n    revision: deploy/{name}\n"
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

/// Create a component
fn create_component(api_version: &str, kind: &str) -> Result<()> {
    create_component_in_dir(api_version, kind, None)
}

/// Create a new component in a specific directory (useful for testing)
fn create_component_in_dir(api_version: &str, kind: &str, project_dir: Option<&Path>) -> Result<()> {
    info!("Creating new component: {}/{}", api_version, kind);

    // Load project config to find components directory
    let config_file = ProjectConfig::find(project_dir)?.ok_or_else(|| NylError::ConfigNotFound("nyl.toml".into()))?;
    let config = ProjectConfig::load(Some(config_file))?;
    let components_base = config.get_components_search_paths()[0].clone();

    debug!("Components base path: {}", components_base.display());

    // Create component directory structure
    let component_dir = components_base.join(api_version).join(kind);

    if component_dir.exists() {
        return Err(NylError::Config(format!(
            "Component already exists: {}",
            display_path(&component_dir)
        )));
    }

    fs::create_dir_all(&component_dir)?;
    println!("✓ Created component directory: {}", display_path(&component_dir));

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
    println!(
        "  Edit {}/Chart.yaml to customize metadata",
        display_path(&component_dir)
    );
    println!(
        "  Edit {}/values.yaml to define component values",
        display_path(&component_dir)
    );
    println!(
        "  Edit {}/templates/deployment.yaml to customize Kubernetes resources",
        display_path(&component_dir)
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
    println!("✓ Created Chart.yaml: {}", display_path(&chart_path));
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
    println!("✓ Created values.yaml: {}", display_path(&values_path));
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
    println!("✓ Created values.schema.json: {}", display_path(&schema_path));
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
    println!(
        "✓ Created templates/deployment.yaml: {}",
        display_path(&deployment_path)
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

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
        git2::Repository::init(temp.path()).unwrap();
        fs::write(temp.path().join("nyl.toml"), "[project]\n").unwrap();
        let args = ResourceScaffoldArgs {
            kind: GitOpsResourceKind::GitRepository,
            name: "deploy".to_string(),
            output: None,
            source: None,
            colocate: false,
        };
        scaffold_resource(args.clone(), Some(temp.path()), None, None).unwrap();
        let path = temp.path().join("config/repositories/deploy.yaml");
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("git-repository.schema.json"));
        assert!(content.contains("kind: GitRepository"));
        assert!(scaffold_resource(args, Some(temp.path()), None, None).is_err());
    }

    #[test]
    fn test_scaffold_cluster_and_target_use_cluster_model() {
        let temp = TempDir::new().unwrap();
        git2::Repository::init(temp.path()).unwrap();
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
            Some("admin@production"),
            None,
        )
        .unwrap();
        scaffold_resource(
            ResourceScaffoldArgs {
                kind: GitOpsResourceKind::DeploymentTarget,
                name: "production".to_string(),
                output: None,
                source: None,
                colocate: false,
            },
            Some(temp.path()),
            None,
            None,
        )
        .unwrap();
        scaffold_resource(
            ResourceScaffoldArgs {
                kind: GitOpsResourceKind::ArgoCDInstance,
                name: "central".to_string(),
                output: None,
                source: None,
                colocate: false,
            },
            Some(temp.path()),
            None,
            None,
        )
        .unwrap();

        let cluster = fs::read_to_string(temp.path().join("config/clusters/production.yaml")).unwrap();
        assert!(cluster.contains("kind: Cluster"));
        assert!(cluster.contains("context: admin@production"));
        let target = fs::read_to_string(temp.path().join("config/targets/production.yaml")).unwrap();
        assert!(target.contains("kind: DeploymentTarget"));
        assert!(target.contains("publication:"));
        assert!(!target.contains("clusterRef:"));
        assert!(!target.contains("pathPrefix:"));
        let instance = fs::read_to_string(temp.path().join("config/argocd-instances/central.yaml")).unwrap();
        assert!(instance.contains("kind: ArgoCDInstance"));
        assert!(instance.contains("namespace: argocd"));
    }

    #[test]
    fn test_scaffold_colocated_application_group() {
        let temp = TempDir::new().unwrap();
        git2::Repository::init(temp.path()).unwrap();
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
            None,
            None,
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
