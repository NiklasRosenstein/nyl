use std::collections::BTreeSet;
use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use clap::Args;
use dialoguer::{Confirm, Input};
use git2::Repository;
use serde_json::{json, Value};

use crate::cli::commands::cluster::{self, ClusterUpdateArgs};
use crate::config::ProjectConfig;
use crate::resources::{parse_gitops_resource, validate_repository_coordinates};
use crate::util::path_for_display;
use crate::{NylError, Result};

const MINIMAL_PROJECT_CONFIG: &str = r"#:schema https://niklasrosenstein.github.io/nyl/reference/schemas/nyl.schema.json

[project]
";

#[derive(Args, Debug)]
pub struct InitArgs {
    /// Create only the project configuration and conventional directories.
    #[arg(long, conflicts_with_all = [
        "output", "repository_name", "repo_url", "publish_url", "cluster_name", "context",
        "no_context", "destination_server", "destination_name", "update_cluster", "no_update_cluster",
        "target_name", "revision", "path_prefix", "argocd_namespace", "project_name", "allowed_namespaces",
        "allowed_cluster_resources", "applications_path", "applications_name", "skip_applications", "yes"
    ])]
    minimal: bool,

    #[command(flatten)]
    gitops: GitOpsInitArgs,
}

#[derive(Args, Debug)]
#[allow(clippy::struct_excessive_bools)] // Paired positive/negative CLI switches preserve scriptable intent.
pub struct GitOpsInitArgs {
    /// Directory in or below the Nyl project to initialize.
    #[arg(value_name = "DIR", default_value = ".")]
    dir: PathBuf,
    /// Output file, relative to the project root. Use '-' for stdout.
    #[arg(long, default_value = "gitops.yaml", allow_hyphen_values = true)]
    output: PathBuf,
    #[arg(long)]
    /// Local name for the deployment Git repository.
    repository_name: Option<String>,
    /// Credential-free URL used by Argo CD and source reads.
    #[arg(long)]
    repo_url: Option<String>,
    /// Optional distinct URL used for publication writes.
    #[arg(long)]
    publish_url: Option<String>,
    #[arg(long)]
    /// Local name for the destination Cluster.
    cluster_name: Option<String>,
    /// Local kubeconfig context to record.
    #[arg(long, conflicts_with = "no_context")]
    context: Option<String>,
    /// Do not associate the Cluster with a local kubeconfig context.
    #[arg(long)]
    no_context: bool,
    #[arg(long, conflicts_with = "destination_name")]
    /// Argo CD destination server URL.
    destination_server: Option<String>,
    #[arg(long, conflicts_with = "destination_server")]
    /// Argo CD destination cluster name.
    destination_name: Option<String>,
    /// Fetch Kubernetes capabilities after writing the configuration.
    #[arg(long, conflicts_with = "no_update_cluster")]
    update_cluster: bool,
    /// Do not offer to fetch Kubernetes capabilities.
    #[arg(long)]
    no_update_cluster: bool,
    #[arg(long)]
    /// DeploymentTarget name. Defaults to the Cluster name.
    target_name: Option<String>,
    #[arg(long)]
    /// Git revision that receives and serves rendered manifests.
    revision: Option<String>,
    /// Rendered-tree prefix. An explicit empty value publishes at repository root.
    #[arg(long, allow_hyphen_values = true)]
    path_prefix: Option<String>,
    #[arg(long)]
    /// Namespace containing Argo CD Applications and AppProjects.
    argocd_namespace: Option<String>,
    #[arg(long)]
    /// Name of the least-privilege AppProject and its local definition.
    project_name: Option<String>,
    /// Namespace allowed by the generated AppProject. May be repeated.
    #[arg(long = "allow-namespace")]
    allowed_namespaces: Vec<String>,
    /// Cluster resource allowed by the AppProject as GROUP/KIND. May be repeated.
    #[arg(long = "allow-cluster-resource")]
    allowed_cluster_resources: Vec<String>,
    #[arg(long)]
    /// Project-relative directory containing Release manifests.
    applications_path: Option<PathBuf>,
    #[arg(long)]
    /// Name for the generated ApplicationGroup.
    applications_name: Option<String>,
    /// Do not create an ApplicationGroup or applications directory.
    #[arg(long)]
    skip_applications: bool,
    /// Accept detected values and defaults without prompting.
    #[arg(long, short = 'y')]
    yes: bool,
}

#[derive(Debug)]
struct GitOpsInitConfig {
    project_root: PathBuf,
    output: PathBuf,
    stdout: bool,
    create_project_config: bool,
    repository_name: String,
    repo_url: String,
    publish_url: Option<String>,
    cluster_name: String,
    context: Option<String>,
    destination_server: Option<String>,
    destination_name: Option<String>,
    target_name: String,
    revision: String,
    path_prefix: Option<String>,
    argocd_namespace: String,
    project_name: String,
    allowed_namespaces: Vec<String>,
    allowed_cluster_resources: Vec<(String, String)>,
    applications_path: Option<PathBuf>,
    applications_name: Option<String>,
    update_cluster: bool,
}

pub async fn execute(args: InitArgs) -> Result<()> {
    if args.minimal {
        return init_minimal(&args.gitops.dir);
    }
    prepare_gitops_directory(&args.gitops.dir)?;
    init_gitops(args.gitops).await
}

fn prepare_gitops_directory(path: &Path) -> Result<()> {
    if path.exists() {
        if path.is_dir() {
            return Ok(());
        }
        return Err(NylError::config(format!(
            "Initialization path is not a directory: {}",
            path.display()
        )));
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let repository = Repository::discover(parent)
        .map_err(|_| NylError::config(format!("{} is not inside a Git worktree", parent.display())))?;
    let worktree = repository
        .workdir()
        .ok_or_else(|| NylError::config("nyl init requires a non-bare Git worktree"))?;
    let parent = fs::canonicalize(parent)?;
    let worktree = fs::canonicalize(worktree)?;
    if !parent.starts_with(&worktree) {
        return Err(NylError::config(format!(
            "Initialization directory {} is outside Git worktree {}",
            path.display(),
            worktree.display()
        )));
    }
    fs::create_dir_all(path)?;
    Ok(())
}

fn init_minimal(path: &Path) -> Result<()> {
    if path.exists() && !path.is_dir() {
        return Err(NylError::config(format!(
            "Initialization path is not a directory: {}",
            path.display()
        )));
    }
    fs::create_dir_all(path)?;
    let config_path = path.join("nyl.toml");
    if config_path.exists() {
        return Err(NylError::config(format!(
            "Project already exists at {}",
            path_for_display(path).display()
        )));
    }
    for relative in [
        "components",
        "applications",
        "config/repositories",
        "config/clusters",
        "config/argocd-instances",
        "config/targets",
        "config/projects",
        "config/application-groups",
    ] {
        fs::create_dir_all(path.join(relative))?;
    }
    fs::write(
        &config_path,
        "#:schema https://niklasrosenstein.github.io/nyl/reference/schemas/nyl.schema.json\n\n[project]\ncomponents_search_paths = [\"components\"]\nhelm_chart_search_paths = [\".\"]\ngitops_scaffold_path = \"config\"\n",
    )?;
    println!("✓ Initialized Nyl project at {}", path_for_display(path).display());
    Ok(())
}

async fn init_gitops(args: GitOpsInitArgs) -> Result<()> {
    let config = resolve_config(args)?;
    let documents = build_documents(&config)?;
    let yaml = documents
        .iter()
        .map(crate::yaml::serialize_yaml_document)
        .collect::<std::result::Result<Vec<_>, _>>()?
        .join("---\n");

    if config.stdout {
        print!("{yaml}");
        return Ok(());
    }

    if config.create_project_config {
        fs::write(config.project_root.join("nyl.toml"), MINIMAL_PROJECT_CONFIG)?;
    }
    if let Some(parent) = config.output.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&config.output, yaml)?;
    if let Some(path) = &config.applications_path {
        fs::create_dir_all(config.project_root.join(path))?;
    }

    println!(
        "✓ Initialized rendered GitOps configuration at {}",
        path_for_display(&config.output).display()
    );
    if config.create_project_config {
        println!(
            "✓ Created {}",
            path_for_display(&config.project_root.join("nyl.toml")).display()
        );
    }
    if config.update_cluster {
        cluster::update_from_dir(
            ClusterUpdateArgs {
                name: config.cluster_name,
                context: config.context,
                check: false,
            },
            &config.project_root,
        )
        .await?;
    }
    Ok(())
}

#[allow(clippy::too_many_lines)] // This keeps the interactive questions in their user-visible order.
fn resolve_config(mut args: GitOpsInitArgs) -> Result<GitOpsInitConfig> {
    if !args.dir.is_dir() {
        return Err(NylError::config(format!(
            "Initialization directory does not exist or is not a directory: {}",
            args.dir.display()
        )));
    }
    let dir = fs::canonicalize(&args.dir)?;
    let repository = Repository::discover(&dir)
        .map_err(|_| NylError::config(format!("{} is not inside a Git worktree", dir.display())))?;
    let worktree = repository
        .workdir()
        .ok_or_else(|| NylError::config("nyl init requires a non-bare Git worktree"))?;
    let worktree = fs::canonicalize(worktree)?;
    let found_config = ProjectConfig::find(Some(&dir))?;
    let project_root = if let Some(path) = &found_config {
        fs::canonicalize(
            path.parent()
                .ok_or_else(|| NylError::config("nyl.toml has no parent directory"))?,
        )?
    } else {
        dir
    };
    if !project_root.starts_with(&worktree) {
        return Err(NylError::config(format!(
            "Project root {} is outside Git worktree {}",
            project_root.display(),
            worktree.display()
        )));
    }

    let stdout = args.output == Path::new("-");
    if stdout && args.update_cluster {
        return Err(NylError::config("--update-cluster cannot be used with --output -"));
    }
    let output = if stdout {
        args.output.clone()
    } else {
        validate_relative_path("--output", &args.output)?;
        project_root.join(&args.output)
    };
    if !stdout && output.exists() {
        return Err(NylError::config(format!(
            "Refusing to overwrite existing configuration: {}",
            path_for_display(&output).display()
        )));
    }
    let project_config_path = project_root.join("nyl.toml");
    let create_project_config = found_config.is_none();
    if create_project_config && !stdout && project_config_path.exists() {
        return Err(NylError::config(format!(
            "Refusing to overwrite existing configuration: {}",
            path_for_display(&project_config_path).display()
        )));
    }

    let interactive = !args.yes && std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    let origin = repository.find_remote("origin").ok();
    let detected_repo_url = origin.as_ref().and_then(|remote| remote.url()).map(ToOwned::to_owned);
    let detected_publish_url = origin
        .as_ref()
        .and_then(|remote| remote.pushurl())
        .filter(|url| Some(*url) != detected_repo_url.as_deref())
        .map(ToOwned::to_owned);
    let current_context = kube::config::Kubeconfig::read()
        .ok()
        .and_then(|config| config.current_context);

    let repository_name = prompt_string(
        args.repository_name.take(),
        "Git repository name",
        Some("deploy".to_owned()),
        interactive,
        true,
    )?;
    let repo_url = prompt_string(
        args.repo_url.take(),
        "Git repository read URL",
        detected_repo_url,
        interactive,
        true,
    )?;
    let publish_url = prompt_optional_string(
        args.publish_url.take().or(detected_publish_url),
        "Git repository publish URL (empty to use the read URL)",
        interactive,
    )?;
    validate_repository_coordinates(&repo_url, publish_url.as_deref())?;

    let inferred_cluster = current_context.clone().unwrap_or_else(|| "default".to_owned());
    let cluster_name = prompt_string(
        args.cluster_name.take(),
        "Cluster name",
        Some(inferred_cluster),
        interactive,
        true,
    )?;
    let context = if args.no_context {
        None
    } else {
        prompt_optional_string(
            args.context
                .take()
                .or(current_context)
                .or_else(|| Some(cluster_name.clone())),
            "Local kubeconfig context (empty for none)",
            interactive,
        )?
    };
    if let Some(context) = &context {
        warn_for_missing_context(context);
    }
    let target_name = prompt_string(
        args.target_name.take(),
        "Deployment target name",
        Some(cluster_name.clone()),
        interactive,
        true,
    )?;
    let revision = prompt_string(
        args.revision.take(),
        "Publication revision",
        Some("deploy/main".to_owned()),
        interactive,
        true,
    )?;
    let argocd_namespace = prompt_string(
        args.argocd_namespace.take(),
        "Argo CD namespace",
        Some("argocd".to_owned()),
        interactive,
        true,
    )?;
    let project_name = prompt_string(
        args.project_name.take(),
        "Argo CD project name",
        Some("default".to_owned()),
        interactive,
        true,
    )?;

    let applications_path = if args.skip_applications {
        None
    } else {
        let path = prompt_string(
            args.applications_path
                .take()
                .map(|path| path.to_string_lossy().into_owned()),
            "Applications path",
            Some("applications".to_owned()),
            interactive,
            true,
        )?;
        let path = PathBuf::from(path);
        validate_relative_path("--applications-path", &path)?;
        Some(path)
    };
    let applications_name = if let Some(path) = &applications_path {
        let default = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("applications")
            .to_owned();
        Some(prompt_string(
            args.applications_name.take(),
            "Application group name",
            Some(default),
            interactive,
            true,
        )?)
    } else {
        None
    };

    let allowed_namespaces = deduplicate(if args.allowed_namespaces.is_empty() {
        vec!["default".to_owned()]
    } else {
        args.allowed_namespaces
    });
    if allowed_namespaces.iter().any(|namespace| namespace == "*") {
        eprintln!("⚠ The generated AppProject allows deployments to every namespace");
    }
    let allowed_cluster_resources = if args.allowed_cluster_resources.is_empty() {
        vec![(String::new(), "Namespace".to_owned())]
    } else {
        args.allowed_cluster_resources
            .iter()
            .map(|value| parse_resource_pattern(value))
            .collect::<Result<Vec<_>>>()?
    };

    let update_cluster = if args.no_update_cluster || context.is_none() || stdout {
        false
    } else if args.update_cluster {
        true
    } else if interactive {
        Confirm::new()
            .with_prompt("Fetch Kubernetes version and API versions from this context now?")
            .default(true)
            .interact()
            .map_err(|error| NylError::Other(format!("Confirmation prompt failed: {error}")))?
    } else {
        false
    };

    Ok(GitOpsInitConfig {
        project_root,
        output,
        stdout,
        create_project_config,
        repository_name,
        repo_url,
        publish_url,
        cluster_name,
        context,
        destination_server: args.destination_server.or_else(|| {
            args.destination_name
                .is_none()
                .then(|| "https://kubernetes.default.svc".to_owned())
        }),
        destination_name: args.destination_name,
        target_name,
        revision,
        path_prefix: args.path_prefix,
        argocd_namespace,
        project_name,
        allowed_namespaces,
        allowed_cluster_resources,
        applications_path,
        applications_name,
        update_cluster,
    })
}

fn build_documents(config: &GitOpsInitConfig) -> Result<Vec<Value>> {
    let mut repository_spec = json!({"repoURL": config.repo_url});
    if let Some(publish_url) = &config.publish_url {
        repository_spec["publishURL"] = json!(publish_url);
    }
    let repository = resource("GitRepository", &config.repository_name, repository_spec);

    let mut destination = json!({});
    if let Some(server) = &config.destination_server {
        destination["server"] = json!(server);
    }
    if let Some(name) = &config.destination_name {
        destination["name"] = json!(name);
    }
    let mut cluster_spec = json!({
        "destination": destination,
        "kubernetes": {"apiVersions": []}
    });
    if let Some(context) = &config.context {
        cluster_spec["live"] = json!({"context": context});
    }
    let cluster = resource("Cluster", &config.cluster_name, cluster_spec);

    let mut target_spec = json!({
        "publication": {
            "repositoryRef": {"name": config.repository_name},
            "revision": config.revision
        }
    });
    if config.cluster_name != config.target_name {
        target_spec["clusterRef"] = json!({"name": config.cluster_name});
    }
    if let Some(path_prefix) = &config.path_prefix {
        target_spec["publication"]["pathPrefix"] = json!(path_prefix);
    }
    let target = resource("DeploymentTarget", &config.target_name, target_spec);

    let project_destinations = config
        .allowed_namespaces
        .iter()
        .map(|namespace| {
            if let Some(server) = &config.destination_server {
                json!({"server": server, "namespace": namespace})
            } else {
                json!({"name": config.destination_name, "namespace": namespace})
            }
        })
        .collect::<Vec<_>>();
    let cluster_resource_whitelist = config
        .allowed_cluster_resources
        .iter()
        .map(|(group, kind)| json!({"group": group, "kind": kind}))
        .collect::<Vec<_>>();
    let project = resource(
        "AppProjectDefinition",
        &config.project_name,
        json!({
            "management": "Rendered",
            "sourceRepositoryRefs": [{"name": config.repository_name}],
            "manifest": {
                "apiVersion": "argoproj.io/v1alpha1",
                "kind": "AppProject",
                "metadata": {
                    "name": config.project_name,
                    "namespace": config.argocd_namespace
                },
                "spec": {
                    "sourceRepos": [],
                    "destinations": project_destinations,
                    "clusterResourceWhitelist": cluster_resource_whitelist
                }
            }
        }),
    );

    let mut documents = vec![repository, cluster, target, project];
    if let (Some(path), Some(name)) = (&config.applications_path, &config.applications_name) {
        documents.push(resource(
            "ApplicationGroup",
            name,
            json!({
                "projectRef": config.project_name,
                "applicationNamespace": config.argocd_namespace,
                "source": {"path": path.to_string_lossy()}
            }),
        ));
    }
    for document in &documents {
        parse_gitops_resource(document)?
            .ok_or_else(|| NylError::config("Generated document is not a GitOps resource"))?;
    }
    Ok(documents)
}

fn resource(kind: &str, name: &str, spec: Value) -> Value {
    json!({
        "apiVersion": "gitops.nyl/v1",
        "kind": kind,
        "metadata": {"name": name},
        "spec": spec
    })
}

fn prompt_string(
    value: Option<String>,
    prompt: &str,
    default: Option<String>,
    interactive: bool,
    required: bool,
) -> Result<String> {
    if let Some(value) = value {
        if !required || !value.trim().is_empty() {
            return Ok(value);
        }
    }
    if interactive {
        let mut input = Input::<String>::new().with_prompt(prompt);
        if let Some(default) = default {
            input = input.default(default);
        }
        let value = input
            .interact_text()
            .map_err(|error| NylError::Other(format!("Input prompt failed: {error}")))?;
        if !required || !value.trim().is_empty() {
            return Ok(value);
        }
    } else if let Some(default) = default {
        return Ok(default);
    }
    Err(NylError::config(format!("{prompt} is required")))
}

fn prompt_optional_string(value: Option<String>, prompt: &str, interactive: bool) -> Result<Option<String>> {
    if !interactive {
        return Ok(value.filter(|value| !value.trim().is_empty()));
    }
    let mut input = Input::<String>::new().with_prompt(prompt).allow_empty(true);
    if let Some(value) = value {
        input = input.default(value);
    }
    let value = input
        .interact_text()
        .map_err(|error| NylError::Other(format!("Input prompt failed: {error}")))?;
    Ok((!value.trim().is_empty()).then_some(value))
}

fn warn_for_missing_context(context: &str) {
    match kube::config::Kubeconfig::read() {
        Ok(kubeconfig) if !kubeconfig.contexts.iter().any(|candidate| candidate.name == context) => {
            eprintln!("⚠ Kubernetes context {context:?} was not found");
        }
        Err(error) => eprintln!("⚠ Could not inspect kubeconfig for context {context:?}: {error}"),
        _ => {}
    }
}

fn parse_resource_pattern(value: &str) -> Result<(String, String)> {
    let (group, kind) = value.split_once('/').ok_or_else(|| {
        NylError::config(format!(
            "Invalid cluster resource {value:?}; expected GROUP/KIND (use core/KIND for the core API group)"
        ))
    })?;
    let group = if group == "core" { "" } else { group };
    if kind.is_empty() {
        return Err(NylError::config(format!(
            "Invalid cluster resource {value:?}: KIND must not be empty"
        )));
    }
    Ok((group.to_owned(), kind.to_owned()))
}

fn validate_relative_path(field: &str, path: &Path) -> Result<()> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(NylError::config(format!(
            "{field} must be a non-empty relative path without '..'"
        )));
    }
    Ok(())
}

fn deduplicate(values: Vec<String>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    values.into_iter().filter(|value| seen.insert(value.clone())).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(root: &Path) -> GitOpsInitConfig {
        GitOpsInitConfig {
            project_root: root.to_path_buf(),
            output: root.join("gitops.yaml"),
            stdout: false,
            create_project_config: true,
            repository_name: "deploy".to_owned(),
            repo_url: "https://git.example.invalid/deploy.git".to_owned(),
            publish_url: Some("ssh://git@git.example.invalid/deploy.git".to_owned()),
            cluster_name: "production".to_owned(),
            context: Some("production-admin".to_owned()),
            destination_server: Some("https://kubernetes.default.svc".to_owned()),
            destination_name: None,
            target_name: "production".to_owned(),
            revision: "deploy/main".to_owned(),
            path_prefix: None,
            argocd_namespace: "argocd".to_owned(),
            project_name: "default".to_owned(),
            allowed_namespaces: vec!["default".to_owned()],
            allowed_cluster_resources: vec![(String::new(), "Namespace".to_owned())],
            applications_path: Some(PathBuf::from("applications")),
            applications_name: Some("applications".to_owned()),
            update_cluster: false,
        }
    }

    #[test]
    fn generated_simple_project_is_valid_and_uses_derived_target_defaults() {
        let temporary = tempfile::TempDir::new().unwrap();
        let documents = build_documents(&config(temporary.path())).unwrap();
        assert_eq!(documents.len(), 5);
        let target = documents
            .iter()
            .find(|value| value["kind"] == "DeploymentTarget")
            .unwrap();
        assert!(target["spec"].get("clusterRef").is_none());
        assert!(target["spec"]["publication"].get("pathPrefix").is_none());
        let project = documents
            .iter()
            .find(|value| value["kind"] == "AppProjectDefinition")
            .unwrap();
        assert_eq!(project["spec"]["sourceRepositoryRefs"][0]["name"], "deploy");
        assert_eq!(
            project["spec"]["manifest"]["spec"]["destinations"][0]["namespace"],
            "default"
        );
        assert_eq!(
            project["spec"]["manifest"]["spec"]["clusterResourceWhitelist"][0],
            json!({"group": "", "kind": "Namespace"})
        );
        let group = documents
            .iter()
            .find(|value| value["kind"] == "ApplicationGroup")
            .unwrap();
        assert!(group["spec"].get("destinationNamespace").is_none());
    }

    #[test]
    fn parses_cluster_resource_patterns() {
        assert_eq!(
            parse_resource_pattern("core/Namespace").unwrap(),
            (String::new(), "Namespace".to_owned())
        );
        assert_eq!(
            parse_resource_pattern("rbac.authorization.k8s.io/ClusterRole").unwrap(),
            ("rbac.authorization.k8s.io".to_owned(), "ClusterRole".to_owned())
        );
        assert!(parse_resource_pattern("Namespace").is_err());
    }
}
