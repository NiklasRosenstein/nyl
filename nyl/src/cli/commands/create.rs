use clap::{Args, Subcommand};
use dialoguer::Confirm;
use serde_json::json;
use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use tracing::{debug, info};

use crate::cli::resource_file::{append_document, atomic_replace};
use crate::config::ProjectConfig;
use crate::constants::API_VERSION_K8S_GITOPS;
use crate::gitops::{
    central_group_source_root, derived_group_source_root, discover_gitops_inventory, DiscoveredGitOpsResource,
    GitOpsInventory, GitOpsInventoryKey, APPLICATION_GROUP_FILE_NAME,
};
use crate::resources::{
    ApplicationGroup, ApplicationGroupSource, GitOpsResource, GitOpsResourceKind, Release, KIND_RELEASE,
    RELEASE_SCHEMA_FILENAME,
};
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
    /// Create a Release manifest in a matching application group directory.
    Release(ReleaseScaffoldArgs),
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
struct ReleaseScaffoldArgs {
    /// Release name. It also names the created file.
    name: String,
    /// ApplicationGroup whose source directory receives the Release. Defaults to the only group.
    #[arg(long)]
    group: Option<String>,
    /// Create a missing ApplicationGroup without asking.
    #[arg(long, requires = "group")]
    create_group: bool,
    /// Release namespace. Defaults to the group destination namespace, then the Release name.
    #[arg(long)]
    namespace: Option<String>,
    /// Additional namespace the Release may target. Repeatable and comma-separated.
    #[arg(long = "additional-namespaces", value_name = "NAMESPACE", value_delimiter = ',')]
    additional_namespaces: Vec<String>,
    /// Exact output file path.
    #[arg(long)]
    output: Option<PathBuf>,
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
        CreateCommand::Release(args) => scaffold_release(args),
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

/// Source directory and defaults of the ApplicationGroup that owns a Release.
#[derive(Debug)]
struct ResolvedApplicationGroup {
    name: String,
    /// Absent when only the group's defaults are needed, such as with an explicit `--output`.
    root: Option<PathBuf>,
    destination_namespace: Option<String>,
    /// Statically known file selection of the group source, when the group parses without a target.
    source: Option<ApplicationGroupSource>,
    /// The Argo CD project name and the namespaces it admits, when the group
    /// narrows them. An implied permissive project states nothing.
    project: Option<(String, Vec<String>)>,
    /// The group must still be declared in project source.
    declare: bool,
}

fn scaffold_release(args: ReleaseScaffoldArgs) -> Result<()> {
    scaffold_release_in_dir(args, None).map(|_| ())
}

fn scaffold_release_in_dir(args: ReleaseScaffoldArgs, project_dir: Option<&Path>) -> Result<PathBuf> {
    validate_resource_name(&args.name)?;
    let start_dir = project_dir.unwrap_or_else(|| Path::new("."));
    let inventory = discover_gitops_inventory(start_dir, None)?;

    // An exact output path needs no source directory; the group still supplies the namespace default.
    let group = if args.output.is_some() {
        args.group
            .as_deref()
            .map(|name| resolve_application_group(&inventory, Some(name), args.create_group, false))
            .transpose()?
    } else {
        Some(resolve_application_group(
            &inventory,
            args.group.as_deref(),
            args.create_group,
            true,
        )?)
    };

    let namespace = args
        .namespace
        .or_else(|| group.as_ref().and_then(|group| group.destination_namespace.clone()))
        .unwrap_or_else(|| args.name.clone());
    // Validate the whole document before anything is created.
    let yaml = render_release_scaffold(&args.name, &namespace, &args.additional_namespaces)?;

    let output = match (args.output, group.as_ref().and_then(|group| group.root.as_ref())) {
        (Some(output), _) => output,
        (None, Some(root)) => root.join(format!("{}.yaml", args.name)),
        (None, None) => return Err(NylError::config("A Release needs an ApplicationGroup or --output")),
    };
    if output.exists() {
        return Err(NylError::config(format!(
            "Refusing to overwrite existing file: {}",
            display_path(&output)
        )));
    }

    if let Some(group) = &group {
        if group.declare {
            declare_application_group(&group.name, project_dir)?;
        }
    }
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&output, yaml)?;
    match &group {
        Some(group) => {
            warn_for_unselected_release(group, &output);
            warn_for_namespace_outside_project(group, &namespace, &args.additional_namespaces);
            println!(
                "✓ Created Release in ApplicationGroup {:?}: {}",
                group.name,
                display_path(&output)
            );
        }
        None => println!("✓ Created Release: {}", display_path(&output)),
    }
    Ok(output)
}

/// Select the ApplicationGroup that owns the new Release and locate its source directory.
///
/// `require_root` reports a group whose source directory cannot be determined
/// without a target context; a caller that supplies its own output path does
/// not need one.
fn resolve_application_group(
    inventory: &GitOpsInventory,
    requested: Option<&str>,
    create: bool,
    require_root: bool,
) -> Result<ResolvedApplicationGroup> {
    let name = if let Some(name) = requested {
        name.to_owned()
    } else {
        let names = inventory
            .resources
            .values()
            .filter(|resource| resource.identity.kind == GitOpsResourceKind::ApplicationGroup)
            .map(|resource| resource.identity.name.as_str())
            .collect::<Vec<_>>();
        match names.as_slice() {
            [] => {
                return Err(NylError::config(
                    "This project declares no ApplicationGroup; pass --group NAME to create one",
                ))
            }
            [name] => (*name).to_owned(),
            _ => {
                return Err(NylError::config(format!(
                    "--group is required because multiple ApplicationGroups are configured: {}",
                    names.join(", ")
                )))
            }
        }
    };

    let Some(discovered) = inventory.get(GitOpsResourceKind::ApplicationGroup, &name) else {
        return plan_application_group(inventory, name, create);
    };
    let group = match &discovered.resource {
        Some(GitOpsResource::ApplicationGroup(group)) => Some(group.as_ref()),
        // A templated spec is only parsed once a DeploymentTarget is selected.
        _ => None,
    };
    let root = match group_source_root(inventory, discovered, group, &name) {
        Ok(root) => Some(root),
        Err(_) if !require_root => None,
        Err(error) => return Err(error),
    };
    Ok(ResolvedApplicationGroup {
        name,
        root,
        destination_namespace: group.and_then(|group| group.spec.destination_namespace.clone()),
        source: group.and_then(|group| group.spec.source.clone()),
        project: resolved_project_scope(inventory, group),
        declare: false,
    })
}

/// Locate the project directory an ApplicationGroup reads Releases from.
fn group_source_root(
    inventory: &GitOpsInventory,
    discovered: &DiscoveredGitOpsResource,
    group: Option<&ApplicationGroup>,
    name: &str,
) -> Result<PathBuf> {
    let source = match group {
        Some(group) => group.spec.source.clone(),
        // The static envelope is known even when the spec needs a target; read
        // the source from the document itself, and refuse when it stays unknown.
        None => match crate::yaml::parse_yaml_value_k8s_compatible(&discovered.raw_document) {
            Ok(document) => match document.pointer("/spec/source") {
                None | Some(serde_json::Value::Null) => None,
                Some(source) => Some(
                    serde_json::from_value::<ApplicationGroupSource>(source.clone())
                        .map_err(|_| templated_source(name))?,
                ),
            },
            Err(_) if document_mentions_source(&discovered.raw_document) => return Err(templated_source(name)),
            Err(_) => None,
        },
    };
    match source {
        Some(source) if source.is_remote() => Err(NylError::config(format!(
            "ApplicationGroup {name:?} reads a remote source; create the Release in that repository"
        ))),
        Some(source) if contains_template(&source.path) => Err(templated_source(name)),
        Some(source) => Ok(inventory.project_root.join(&source.path)),
        None => Ok(derived_group_source_root(
            &inventory.project_root,
            &discovered.source_path,
            name,
        )),
    }
}

fn templated_source(name: &str) -> NylError {
    NylError::config(format!(
        "ApplicationGroup {name:?} has a templated spec.source; pass --output to select the Release file"
    ))
}

fn contains_template(value: &str) -> bool {
    value.contains("{{") || value.contains("{%")
}

/// Whether a document that no YAML parser accepts may still declare `spec.source`.
fn document_mentions_source(document: &str) -> bool {
    document.contains("source")
}

/// Plan the declaration of a missing ApplicationGroup so the Release has a discoverable home.
fn plan_application_group(inventory: &GitOpsInventory, name: String, create: bool) -> Result<ResolvedApplicationGroup> {
    validate_resource_name(&name)?;
    if !create && !confirm_application_group(&name)? {
        return Err(NylError::config(format!(
            "ApplicationGroup {name:?} does not exist; pass --create-group to declare it"
        )));
    }
    Ok(ResolvedApplicationGroup {
        // A new group is declared centrally and reads `applications/<name>`.
        root: Some(central_group_source_root(&inventory.project_root, &name)),
        destination_namespace: None,
        source: None,
        // A new group is scaffolded with the implied permissive project.
        project: None,
        declare: true,
        name,
    })
}

fn declare_application_group(name: &str, project_dir: Option<&Path>) -> Result<()> {
    scaffold_resource(
        ResourceScaffoldArgs {
            kind: GitOpsResourceKind::ApplicationGroup,
            name: name.to_owned(),
            output: None,
            source: None,
            colocate: false,
        },
        project_dir,
        None,
        None,
    )
    .map(|_| ())
}

fn confirm_application_group(name: &str) -> Result<bool> {
    if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        return Ok(false);
    }
    Confirm::new()
        .with_prompt(format!("ApplicationGroup {name:?} does not exist. Create it now?"))
        .default(true)
        .interact()
        .map_err(|error| NylError::Other(format!("Confirmation prompt failed: {error}")))
}

/// Warn when the group's own file selection skips the new Release.
fn warn_for_unselected_release(group: &ResolvedApplicationGroup, output: &Path) {
    let (Some(root), Some(source)) = (&group.root, &group.source) else {
        return;
    };
    if !crate::gitops::source_matches(root, output, source) {
        eprintln!(
            "⚠ ApplicationGroup {:?} does not select {}; adjust spec.source or use --output",
            group.name,
            display_path(output)
        );
    }
}

/// Warn when the group's Argo CD project does not admit the Release namespaces.
fn warn_for_namespace_outside_project(group: &ResolvedApplicationGroup, namespace: &str, additional: &[String]) {
    let Some((project, patterns)) = &group.project else {
        return;
    };
    for namespace in std::iter::once(namespace).chain(additional.iter().map(String::as_str)) {
        if !crate::gitops::namespace_matches_any(namespace, patterns) {
            eprintln!(
                "⚠ AppProject {project:?} does not allow namespace {namespace:?}; add it to the project destinations"
            );
        }
    }
}

/// The Argo CD project of a group and the namespaces it admits, when it narrows them.
fn resolved_project_scope(
    inventory: &GitOpsInventory,
    group: Option<&ApplicationGroup>,
) -> Option<(String, Vec<String>)> {
    let group = group?;
    if let Some(reference) = &group.spec.project_ref {
        return Some((reference.clone(), project_destination_namespaces(inventory, reference)?));
    }
    let template = group.spec.project_template.as_ref()?;
    let mut namespaces = template.destination_namespaces.clone();
    if let Some(namespace) = &group.spec.destination_namespace {
        namespaces.push(namespace.clone());
    }
    // An empty template list leaves the generated project's namespaces unrestricted.
    (!namespaces.is_empty()).then(|| {
        let name = template.name.clone().unwrap_or_else(|| group.metadata.name.clone());
        (name, namespaces)
    })
}

/// Literal destination namespaces of a statically declared AppProjectDefinition.
fn project_destination_namespaces(inventory: &GitOpsInventory, project: &str) -> Option<Vec<String>> {
    let discovered = inventory.get(GitOpsResourceKind::AppProjectDefinition, project)?;
    let Some(GitOpsResource::AppProjectDefinition(definition)) = &discovered.resource else {
        return None;
    };
    let namespaces = definition
        .spec
        .manifest
        .pointer("/spec/destinations")?
        .as_array()?
        .iter()
        .filter_map(|destination| destination.get("namespace")?.as_str())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    // An empty or unreadable destination list carries no statement about namespaces.
    (!namespaces.is_empty()).then_some(namespaces)
}

fn render_release_scaffold(name: &str, namespace: &str, additional_namespaces: &[String]) -> Result<String> {
    let mut document = json!({
        "apiVersion": API_VERSION_K8S_GITOPS,
        "kind": KIND_RELEASE,
        "metadata": {"name": name, "namespace": namespace},
    });
    if !additional_namespaces.is_empty() {
        document["spec"] = json!({"additionalNamespaces": additional_namespaces});
    }
    // Reject invalid namespaces before the file is written.
    Release::from_value(&document)?;
    let schema = format!(
        "https://niklasrosenstein.github.io/nyl/reference/schemas/{API_VERSION_K8S_GITOPS}/{RELEASE_SCHEMA_FILENAME}"
    );
    let body = crate::yaml::serialize_yaml_value(&document)?;
    Ok(format!(
        "# yaml-language-server: $schema={schema}\n{body}\n# Add this Release's workload manifests as further YAML documents below.\n"
    ))
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
            .join(APPLICATION_GROUP_FILE_NAME)
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
        "https://niklasrosenstein.github.io/nyl/reference/schemas/{}/{}",
        kind.api_version(),
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
                "apiVersion: k8s.gitops.nyl/v1\nkind: Cluster\nmetadata:\n  name: {name}\nspec:\n  destination:\n    server: https://kubernetes.default.svc\n  # Populate from the selected context with: nyl capture cluster {name}\n  kubernetes:\n    apiVersions: []\n  values: {{}}\n  live:\n    context: {context}\n"
            )
        }
        GitOpsResourceKind::ArgoCDInstance => format!(
            "apiVersion: k8s.gitops.nyl/v1\nkind: ArgoCDInstance\nmetadata:\n  name: {name}\nspec:\n  clusterRef:\n    name: {name}\n  namespace: argocd\n"
        ),
        GitOpsResourceKind::DeploymentTarget => format!(
            "apiVersion: k8s.gitops.nyl/v1\nkind: DeploymentTarget\nmetadata:\n  name: {name}\nspec:\n  publication:\n    repositoryRef:\n      name: deploy\n    revision: deploy/{name}\n"
        ),
        GitOpsResourceKind::AppProjectDefinition => format!(
            "apiVersion: k8s.gitops.nyl/v1\nkind: AppProjectDefinition\nmetadata:\n  name: {name}\nspec:\n  management: Rendered\n  manifest:\n    apiVersion: argoproj.io/v1alpha1\n    kind: AppProject\n    metadata:\n      name: {name}\n      namespace: argocd\n    spec:\n      sourceRepos: []\n      destinations: []\n"
        ),
        GitOpsResourceKind::ApplicationGroup => {
            let source = source.map_or_else(String::new, |source| format!("  source:\n    path: {source}\n"));
            // No project: the group owns its implied permissive AppProject.
            // No destinationNamespace: each Release keeps its own metadata.namespace.
            format!(
                "apiVersion: k8s.gitops.nyl/v1\nkind: ApplicationGroup\nmetadata:\n  name: {name}\nspec:\n  applicationNamespace: argocd\n{source}"
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

    const APPLICATION_GROUP: &str = "apiVersion: k8s.gitops.nyl/v1\nkind: ApplicationGroup\nmetadata:\n  name: platform\nspec:\n  projectRef: platform\n  applicationNamespace: argocd\n";

    fn release_args(name: &str) -> ReleaseScaffoldArgs {
        ReleaseScaffoldArgs {
            name: name.to_owned(),
            group: None,
            create_group: false,
            namespace: None,
            additional_namespaces: Vec::new(),
            output: None,
        }
    }

    fn project(gitops: Option<&str>) -> TempDir {
        let temp = TempDir::new().unwrap();
        git2::Repository::init(temp.path()).unwrap();
        fs::write(temp.path().join("nyl.toml"), "[project]\n").unwrap();
        if let Some(gitops) = gitops {
            fs::write(temp.path().join("gitops.yaml"), gitops).unwrap();
        }
        temp
    }

    #[test]
    fn test_scaffold_release_derives_directory_and_namespace_from_the_only_group() {
        let temp = project(Some(APPLICATION_GROUP));
        let path = scaffold_release_in_dir(release_args("api"), Some(temp.path())).unwrap();

        assert_eq!(path, temp.path().join("applications/platform/api.yaml"));
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("release.schema.json"));
        assert!(content.contains("kind: Release"));
        assert!(content.contains("name: api"));
        // Without a group destination namespace the Release owns its own namespace.
        assert!(content.contains("namespace: api"));
        assert!(scaffold_release_in_dir(release_args("api"), Some(temp.path())).is_err());
    }

    #[test]
    fn test_scaffold_release_follows_explicit_and_colocated_group_sources() {
        let temp = project(Some(
            "apiVersion: k8s.gitops.nyl/v1\nkind: ApplicationGroup\nmetadata:\n  name: platform\nspec:\n  projectRef: platform\n  applicationNamespace: argocd\n  destinationNamespace: platform-system\n  source:\n    path: workloads/platform\n",
        ));
        let path = scaffold_release_in_dir(release_args("api"), Some(temp.path())).unwrap();
        assert_eq!(path, temp.path().join("workloads/platform/api.yaml"));
        // A group destination namespace is the Release default.
        assert!(fs::read_to_string(&path)
            .unwrap()
            .contains("namespace: platform-system"));

        let colocated = temp.path().join("teams/search/_application-group.yaml");
        fs::create_dir_all(colocated.parent().unwrap()).unwrap();
        fs::write(
            &colocated,
            "apiVersion: k8s.gitops.nyl/v1\nkind: ApplicationGroup\nmetadata:\n  name: search\nspec:\n  projectRef: search\n  applicationNamespace: argocd\n",
        )
        .unwrap();
        let mut args = release_args("index");
        args.group = Some("search".to_owned());
        let path = scaffold_release_in_dir(args, Some(temp.path())).unwrap();
        assert_eq!(path, temp.path().join("teams/search/index.yaml"));
    }

    #[test]
    fn test_scaffold_release_creates_requested_group_on_demand() {
        let temp = project(None);
        let mut args = release_args("api");
        args.group = Some("platform".to_owned());
        args.create_group = true;
        let path = scaffold_release_in_dir(args, Some(temp.path())).unwrap();

        assert_eq!(path, temp.path().join("applications/platform/api.yaml"));
        let group = fs::read_to_string(temp.path().join("config/application-groups/platform.yaml")).unwrap();
        assert!(group.contains("kind: ApplicationGroup"));
        // The group must not pin a destination namespace over its Releases.
        assert!(!group.contains("destinationNamespace:"));
    }

    #[test]
    fn test_scaffold_release_requires_an_unambiguous_group() {
        let temp = project(None);
        let error = scaffold_release_in_dir(release_args("api"), Some(temp.path()))
            .unwrap_err()
            .to_string();
        assert!(error.contains("no ApplicationGroup"));

        let mut args = release_args("api");
        args.group = Some("platform".to_owned());
        let error = scaffold_release_in_dir(args, Some(temp.path()))
            .unwrap_err()
            .to_string();
        assert!(error.contains("--create-group"));

        fs::write(
            temp.path().join("gitops.yaml"),
            format!("{APPLICATION_GROUP}---\napiVersion: k8s.gitops.nyl/v1\nkind: ApplicationGroup\nmetadata:\n  name: search\nspec:\n  projectRef: search\n  applicationNamespace: argocd\n"),
        )
        .unwrap();
        let error = scaffold_release_in_dir(release_args("api"), Some(temp.path()))
            .unwrap_err()
            .to_string();
        assert!(error.contains("platform, search"));
    }

    #[test]
    fn test_scaffold_release_validates_namespaces_before_writing() {
        let temp = project(Some(APPLICATION_GROUP));
        let mut args = release_args("api");
        args.namespace = Some("Invalid".to_owned());
        assert!(scaffold_release_in_dir(args, Some(temp.path())).is_err());

        let mut args = release_args("api");
        args.additional_namespaces = vec!["observability".to_owned(), "observability".to_owned()];
        assert!(scaffold_release_in_dir(args, Some(temp.path())).is_err());
        assert!(!temp.path().join("applications/platform/api.yaml").exists());

        let mut args = release_args("api");
        args.additional_namespaces = vec!["observability".to_owned(), "ingress".to_owned()];
        let path = scaffold_release_in_dir(args, Some(temp.path())).unwrap();
        let content = fs::read_to_string(path).unwrap();
        assert!(content.contains("additionalNamespaces:"));
        assert!(content.contains("- observability"));
        assert!(content.contains("- ingress"));
    }

    /// A group whose spec only parses with a target context, keeping a static source.
    const TEMPLATED_GROUP: &str = "apiVersion: k8s.gitops.nyl/v1\nkind: ApplicationGroup\nmetadata:\n  name: platform\nspec:\n  enabled: '{{ target.metadata.name == \"production\" }}'\n  projectRef: platform\n  applicationNamespace: argocd\n  source:\n    path: workloads/platform\n";

    #[test]
    fn test_scaffold_release_reads_a_static_source_from_a_templated_group() {
        let temp = project(Some(TEMPLATED_GROUP));
        let path = scaffold_release_in_dir(release_args("api"), Some(temp.path())).unwrap();
        assert_eq!(path, temp.path().join("workloads/platform/api.yaml"));
    }

    #[test]
    fn test_scaffold_release_refuses_an_unreadable_source_and_accepts_an_explicit_output() {
        // A structural template hides whether the group declares a source.
        let temp = project(Some(
            "apiVersion: k8s.gitops.nyl/v1\nkind: ApplicationGroup\nmetadata:\n  name: platform\nspec:\n  projectRef: platform\n  applicationNamespace: argocd\n{% if target.metadata.name == 'production' %}\n  source:\n    path: workloads/platform\n{% endif %}\n",
        ));
        let error = scaffold_release_in_dir(release_args("api"), Some(temp.path()))
            .unwrap_err()
            .to_string();
        assert!(error.contains("--output"));

        // The escape hatch the error recommends must work with the same group.
        let mut args = release_args("api");
        args.group = Some("platform".to_owned());
        args.output = Some(temp.path().join("workloads/platform/api.yaml"));
        let path = scaffold_release_in_dir(args, Some(temp.path())).unwrap();
        assert!(path.is_file());
    }

    #[test]
    fn test_scaffold_release_leaves_no_group_behind_when_the_release_is_invalid() {
        let temp = project(None);
        let mut args = release_args("api");
        args.group = Some("platform".to_owned());
        args.create_group = true;
        args.namespace = Some("Invalid".to_owned());
        assert!(scaffold_release_in_dir(args, Some(temp.path())).is_err());
        assert!(!temp.path().join("config/application-groups/platform.yaml").exists());
        assert!(!temp.path().join("applications/platform").exists());
    }

    #[test]
    fn test_application_group_scaffold_declares_no_project() {
        let temp = project(None);
        scaffold_resource(
            ResourceScaffoldArgs {
                kind: GitOpsResourceKind::ApplicationGroup,
                name: "platform".to_string(),
                output: None,
                source: None,
                colocate: false,
            },
            Some(temp.path()),
            None,
            None,
        )
        .unwrap();
        // The implied permissive AppProject needs no declaration; narrowing is opt-in.
        let group = fs::read_to_string(temp.path().join("config/application-groups/platform.yaml")).unwrap();
        assert!(!group.contains("projectRef:"));
        assert!(!group.contains("projectTemplate:"));
        assert!(!group.contains("destinationNamespace:"));
    }

    #[test]
    fn test_resource_name_rejects_path_traversal() {
        assert!(validate_resource_name("../deploy").is_err());
        assert!(validate_resource_name("Production").is_err());
        assert!(validate_resource_name("production").is_ok());
    }
}
