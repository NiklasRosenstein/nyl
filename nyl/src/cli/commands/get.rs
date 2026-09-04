use std::path::Path;

use clap::{Args, Subcommand};

use crate::cli::table::{render as render_table, Cell};
use crate::gitops::{discover_gitops_inventory, DiscoveredGitOpsResource};
use crate::resources::{GitOpsResource, GitOpsResourceKind};
use crate::{NylError, Result};

/// Inspect GitOps resources declared in the project.
#[derive(Args, Debug)]
pub struct GetArgs {
    #[command(subcommand)]
    command: GetCommand,
}

#[derive(Subcommand, Debug)]
enum GetCommand {
    /// Get Git repository declarations.
    #[command(alias = "repository")]
    Repositories(GetResourceArgs),
    /// Get cluster declarations.
    #[command(alias = "cluster")]
    Clusters(GetResourceArgs),
    /// Get Argo CD instance declarations.
    #[command(name = "argocd-instances", alias = "argocd-instance", alias = "argocd")]
    ArgoCDInstances(GetResourceArgs),
    /// Get deployment target declarations.
    #[command(alias = "target")]
    Targets(GetResourceArgs),
    /// Get AppProject definitions.
    #[command(name = "app-projects", alias = "app-project")]
    AppProjects(GetResourceArgs),
    /// Get application group declarations.
    #[command(name = "application-groups", alias = "application-group")]
    ApplicationGroups(GetResourceArgs),
}

#[derive(Args, Debug)]
struct GetResourceArgs {
    /// Return only the resource with this name.
    name: Option<String>,
}

pub fn execute(args: GetArgs) -> Result<()> {
    let (kind, selection) = match args.command {
        GetCommand::Repositories(args) => (GitOpsResourceKind::GitRepository, args),
        GetCommand::Clusters(args) => (GitOpsResourceKind::Cluster, args),
        GetCommand::ArgoCDInstances(args) => (GitOpsResourceKind::ArgoCDInstance, args),
        GetCommand::Targets(args) => (GitOpsResourceKind::DeploymentTarget, args),
        GetCommand::AppProjects(args) => (GitOpsResourceKind::AppProjectDefinition, args),
        GetCommand::ApplicationGroups(args) => (GitOpsResourceKind::ApplicationGroup, args),
    };
    let inventory = discover_gitops_inventory(Path::new("."), None)?;
    let resources = inventory
        .resources
        .values()
        .filter(|resource| resource.identity.kind == kind)
        .filter(|resource| {
            selection
                .name
                .as_deref()
                .is_none_or(|name| resource.identity.name == name)
        })
        .collect::<Vec<_>>();
    if let Some(name) = selection.name {
        if resources.is_empty() {
            return Err(NylError::config(format!("{} {name:?} was not found", kind.as_str())));
        }
    }
    print_resources(kind, &resources);
    Ok(())
}

fn print_resources(kind: GitOpsResourceKind, resources: &[&DiscoveredGitOpsResource]) {
    let headers = match kind {
        GitOpsResourceKind::GitRepository => vec!["NAME", "REPOSITORY", "PUBLISH", "FILE"],
        GitOpsResourceKind::Cluster => vec!["NAME", "DESTINATION", "CONTEXT", "VERSION", "FILE"],
        GitOpsResourceKind::ArgoCDInstance => vec!["NAME", "CLUSTER", "NAMESPACE", "FILE"],
        GitOpsResourceKind::DeploymentTarget => vec!["NAME", "CLUSTER", "PUBLICATION", "PATH-PREFIX", "FILE"],
        GitOpsResourceKind::AppProjectDefinition => vec!["NAME", "MANAGEMENT", "FILE"],
        GitOpsResourceKind::ApplicationGroup => vec!["NAME", "PROJECT", "SOURCE", "FILE"],
    };
    let rows = resources
        .iter()
        .map(|resource| resource_row(kind, resource).into_iter().map(Cell::plain).collect())
        .collect::<Vec<_>>();
    println!("{}", render_table(&headers, &rows));
}

fn resource_row(kind: GitOpsResourceKind, resource: &DiscoveredGitOpsResource) -> Vec<String> {
    let name = &resource.identity.name;
    let file = format!(
        "{}#document-{}",
        resource.source_path.display(),
        resource.document_index
    );
    match resource.resource.as_ref() {
        Some(GitOpsResource::GitRepository(repository)) => vec![
            name.clone(),
            repository.spec.repo_url.clone(),
            repository.spec.publish_url.clone().unwrap_or_else(|| "-".to_owned()),
            file,
        ],
        Some(GitOpsResource::Cluster(cluster)) => {
            let destination = cluster
                .spec
                .destination
                .server
                .as_deref()
                .or(cluster.spec.destination.name.as_deref())
                .unwrap_or("-");
            let context = cluster.spec.live.as_ref().map_or("-", |live| live.context.as_str());
            let version = cluster.spec.kubernetes.kube_version.as_deref().unwrap_or("-");
            vec![
                name.clone(),
                destination.to_owned(),
                context.to_owned(),
                version.to_owned(),
                file,
            ]
        }
        Some(GitOpsResource::ArgoCDInstance(instance)) => vec![
            name.clone(),
            instance.spec.cluster_ref.name.clone(),
            instance.spec.namespace.clone(),
            file,
        ],
        Some(GitOpsResource::DeploymentTarget(target)) => {
            let repository = target
                .spec
                .publication
                .repository_ref
                .as_ref()
                .map(|reference| reference.name.as_str())
                .or_else(|| {
                    target
                        .spec
                        .publication
                        .repository
                        .as_ref()
                        .map(|repository| repository.repo_url.as_str())
                })
                .unwrap_or("-");
            vec![
                name.clone(),
                target.cluster_name().to_owned(),
                format!("{}@{}", repository, target.spec.publication.revision),
                target.publication_path_prefix().to_owned(),
                file,
            ]
        }
        Some(GitOpsResource::AppProjectDefinition(project)) => {
            vec![name.clone(), format!("{:?}", project.spec.management), file]
        }
        Some(GitOpsResource::ApplicationGroup(group)) => {
            let project = group.spec.project_ref.as_deref().unwrap_or("<inline>");
            let source = group.spec.source.as_ref().map_or("-", |source| source.path.as_str());
            vec![name.clone(), project.to_owned(), source.to_owned(), file]
        }
        None => templated_resource_row(kind, name, file),
    }
}

fn templated_resource_row(kind: GitOpsResourceKind, name: &str, file: String) -> Vec<String> {
    match kind {
        GitOpsResourceKind::Cluster | GitOpsResourceKind::DeploymentTarget => {
            vec![
                name.to_owned(),
                "<templated>".to_owned(),
                "<templated>".to_owned(),
                "<templated>".to_owned(),
                file,
            ]
        }
        GitOpsResourceKind::GitRepository
        | GitOpsResourceKind::ArgoCDInstance
        | GitOpsResourceKind::ApplicationGroup => vec![
            name.to_owned(),
            "<templated>".to_owned(),
            "<templated>".to_owned(),
            file,
        ],
        GitOpsResourceKind::AppProjectDefinition => vec![name.to_owned(), "<templated>".to_owned(), file],
    }
}
