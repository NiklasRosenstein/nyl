use std::path::Path;

use clap::{Args, Subcommand};

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
    match kind {
        GitOpsResourceKind::GitRepository => println!("NAME\tREPOSITORY\tPUBLISH\tFILE"),
        GitOpsResourceKind::Cluster => println!("NAME\tDESTINATION\tCONTEXT\tVERSION\tFILE"),
        GitOpsResourceKind::ArgoCDInstance => println!("NAME\tCLUSTER\tNAMESPACE\tFILE"),
        GitOpsResourceKind::DeploymentTarget => println!("NAME\tCLUSTER\tPUBLICATION\tPATH-PREFIX\tFILE"),
        GitOpsResourceKind::AppProjectDefinition => println!("NAME\tMANAGEMENT\tFILE"),
        GitOpsResourceKind::ApplicationGroup => println!("NAME\tPROJECT\tSOURCE\tFILE"),
    }
    for resource in resources {
        let name = &resource.identity.name;
        let file = format!(
            "{}#document-{}",
            resource.source_path.display(),
            resource.document_index
        );
        match resource.resource.as_ref() {
            Some(GitOpsResource::GitRepository(repository)) => println!(
                "{name}\t{}\t{}\t{file}",
                repository.spec.repo_url,
                repository.spec.publish_url.as_deref().unwrap_or("-")
            ),
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
                println!("{name}\t{destination}\t{context}\t{version}\t{file}");
            }
            Some(GitOpsResource::ArgoCDInstance(instance)) => println!(
                "{name}\t{}\t{}\t{file}",
                instance.spec.cluster_ref.name, instance.spec.namespace
            ),
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
                println!(
                    "{name}\t{}\t{}@{}\t{}\t{file}",
                    target.cluster_name(),
                    repository,
                    target.spec.publication.revision,
                    target.publication_path_prefix()
                );
            }
            Some(GitOpsResource::AppProjectDefinition(project)) => {
                println!("{name}\t{:?}\t{file}", project.spec.management);
            }
            Some(GitOpsResource::ApplicationGroup(group)) => {
                let project = group.spec.project_ref.as_deref().unwrap_or("<inline>");
                let source = group.spec.source.as_ref().map_or("-", |source| source.path.as_str());
                println!("{name}\t{project}\t{source}\t{file}");
            }
            None => println_templated(kind, name, &file),
        }
    }
}

fn println_templated(kind: GitOpsResourceKind, name: &str, file: &str) {
    match kind {
        GitOpsResourceKind::GitRepository => println!("{name}\t<templated>\t<templated>\t{file}"),
        GitOpsResourceKind::Cluster | GitOpsResourceKind::DeploymentTarget => {
            println!("{name}\t<templated>\t<templated>\t<templated>\t{file}");
        }
        GitOpsResourceKind::ArgoCDInstance | GitOpsResourceKind::ApplicationGroup => {
            println!("{name}\t<templated>\t<templated>\t{file}");
        }
        GitOpsResourceKind::AppProjectDefinition => println!("{name}\t<templated>\t{file}"),
    }
}
