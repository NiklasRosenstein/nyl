use std::fmt::Write as _;
use std::fs;
use std::io::Write as _;
use std::path::Path;

use clap::{Args, Subcommand};

use crate::gitops::discovery::{discover_gitops_inventory, DiscoveredGitOpsResource, GitOpsInventory};
use crate::kubernetes::{KubeClient, KubeRsClient};
use crate::resources::{Cluster, DeploymentTarget, GitOpsResource, GitOpsResourceKind};
use crate::{NylError, Result};

#[derive(Args, Debug)]
pub struct ClusterArgs {
    #[command(subcommand)]
    pub command: ClusterSubcommand,
}

#[derive(Subcommand, Debug)]
pub enum ClusterSubcommand {
    /// List configured clusters without connecting to Kubernetes
    List,
    /// Refresh the stored capabilities of a configured cluster
    Update(ClusterUpdateArgs),
}

#[derive(Args, Debug)]
pub struct ClusterUpdateArgs {
    pub name: String,
    #[arg(long)]
    pub context: Option<String>,
    /// Check whether stored capabilities are current without modifying the file
    #[arg(long)]
    pub check: bool,
}

#[derive(Debug, PartialEq, Eq)]
struct ClusterInfo {
    kube_version: String,
    api_versions: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ResolvedTargetCluster {
    pub target: DeploymentTarget,
    pub cluster: Cluster,
}

pub async fn execute(args: ClusterArgs) -> Result<()> {
    match args.command {
        ClusterSubcommand::List => list_clusters(),
        ClusterSubcommand::Update(args) => update(args).await,
    }
}

fn inventory(start_dir: &Path) -> Result<GitOpsInventory> {
    discover_gitops_inventory(start_dir, None)
}

pub fn resolve_target_cluster(start_dir: &Path, target_name: &str) -> Result<ResolvedTargetCluster> {
    let inventory = inventory(start_dir)?;
    let target = get_target(&inventory, target_name)?.clone();
    let cluster = get_cluster(&inventory, target.cluster_name())?.clone();
    Ok(ResolvedTargetCluster { target, cluster })
}

pub fn resolved_cluster_context<'a>(cluster: &'a Cluster, override_context: Option<&'a str>) -> Option<&'a str> {
    override_context.or_else(|| cluster.spec.live.as_ref().map(|live| live.context.as_str()))
}

pub async fn load_target_kube_config(target_name: &str, context_override: Option<&str>) -> Result<kube::Config> {
    let cwd = std::env::current_dir()?;
    let resolved = resolve_target_cluster(&cwd, target_name)?;
    load_cluster_kube_config(&resolved.cluster, context_override).await
}

pub async fn load_cluster_kube_config(cluster: &Cluster, context_override: Option<&str>) -> Result<kube::Config> {
    let context = resolved_cluster_context(cluster, context_override);
    let config = KubeRsClient::load_kube_config(None, context).await?;
    verify_cluster_server(cluster, &config)?;
    Ok(config)
}

fn verify_cluster_server(cluster: &Cluster, config: &kube::Config) -> Result<()> {
    let Some(expected) = cluster.spec.destination.server.as_deref() else {
        return Ok(());
    };
    let actual = config.cluster_url.to_string();
    if normalize_cluster_url(expected) == "https://kubernetes.default.svc" {
        tracing::debug!(
            cluster = %cluster.metadata.name,
            "Cannot verify an in-cluster Argo CD destination against a local kubeconfig endpoint"
        );
    } else if normalize_cluster_url(expected) != normalize_cluster_url(&actual) {
        return Err(NylError::config(format!(
            "Selected kube context points to {actual}, but Cluster '{}' expects {}",
            cluster.metadata.name, expected
        )));
    }
    Ok(())
}

fn normalize_cluster_url(value: &str) -> String {
    let value = value.trim().trim_end_matches('/');
    let Some((scheme, remainder)) = value.split_once("://") else {
        return value.to_owned();
    };
    let boundary = remainder.find(['/', '?', '#']).unwrap_or(remainder.len());
    let (authority, suffix) = remainder.split_at(boundary);
    format!(
        "{}://{}{suffix}",
        scheme.to_ascii_lowercase(),
        authority.to_ascii_lowercase()
    )
}

fn list_clusters() -> Result<()> {
    let inventory = inventory(&std::env::current_dir()?)?;
    for discovered in inventory.resources.values() {
        if discovered.identity.kind == GitOpsResourceKind::Cluster {
            println!("{}", discovered.identity.name);
        }
    }
    Ok(())
}

pub(crate) async fn update(args: ClusterUpdateArgs) -> Result<()> {
    update_from_dir(args, &std::env::current_dir()?).await
}

pub(crate) async fn update_from_dir(args: ClusterUpdateArgs, start_dir: &Path) -> Result<()> {
    let inventory = inventory(start_dir)?;
    let discovered = inventory
        .get(GitOpsResourceKind::Cluster, &args.name)
        .ok_or_else(|| NylError::config(format!("Cluster '{}' not found", args.name)))?;
    let cluster = discovered_cluster(discovered)?;
    let info = fetch_cluster_info(cluster, args.context.as_deref()).await?;
    let stored_version = cluster.spec.kubernetes.kube_version.as_deref().unwrap_or_default();
    let mut stored_apis = cluster.spec.kubernetes.api_versions.clone();
    stored_apis.sort();
    stored_apis.dedup();
    let differs = stored_version != info.kube_version || stored_apis != info.api_versions;

    if !differs {
        println!("Cluster '{}' capabilities are current", args.name);
        return Ok(());
    }
    if args.check {
        return Err(NylError::config(format!(
            "Cluster '{}' capabilities differ from the live cluster; run `nyl cluster update {}`",
            args.name, args.name
        )));
    }

    let path = inventory.project_root.join(&discovered.source_path);
    update_cluster_document(&inventory.project_root, &path, discovered, &info)?;
    println!("Updated {}", crate::util::path_for_display(&path).display());
    Ok(())
}

async fn fetch_cluster_info(cluster: &Cluster, context_override: Option<&str>) -> Result<ClusterInfo> {
    let config = load_cluster_kube_config(cluster, context_override).await?;
    let client = KubeRsClient::from_client(kube::Client::try_from(config)?).await?;
    let kube_version = client.get_server_version().await?;
    let mut api_versions = client.get_api_versions().await?;
    api_versions.sort();
    api_versions.dedup();
    Ok(ClusterInfo {
        kube_version,
        api_versions,
    })
}

fn get_target<'a>(inventory: &'a GitOpsInventory, name: &str) -> Result<&'a DeploymentTarget> {
    let discovered = inventory
        .get(GitOpsResourceKind::DeploymentTarget, name)
        .ok_or_else(|| NylError::config(format!("DeploymentTarget '{name}' not found")))?;
    match discovered.resource.as_ref() {
        Some(GitOpsResource::DeploymentTarget(target)) => Ok(target),
        _ => Err(NylError::config(format!(
            "DeploymentTarget '{name}' is not a static resource"
        ))),
    }
}

fn get_cluster<'a>(inventory: &'a GitOpsInventory, name: &str) -> Result<&'a Cluster> {
    let discovered = inventory
        .get(GitOpsResourceKind::Cluster, name)
        .ok_or_else(|| NylError::config(format!("Cluster '{name}' not found")))?;
    discovered_cluster(discovered)
}

fn discovered_cluster(discovered: &DiscoveredGitOpsResource) -> Result<&Cluster> {
    match discovered.resource.as_ref() {
        Some(GitOpsResource::Cluster(cluster)) => Ok(cluster),
        _ => Err(NylError::config(format!(
            "Cluster '{}' must be a static resource",
            discovered.identity.name
        ))),
    }
}

fn update_cluster_document(
    project_root: &Path,
    path: &Path,
    discovered: &DiscoveredGitOpsResource,
    info: &ClusterInfo,
) -> Result<()> {
    if discovered.raw_document.contains("{{")
        || discovered.raw_document.contains("{%")
        || discovered.raw_document.contains("{#")
    {
        return Err(NylError::config(format!(
            "Cannot update templated Cluster source {}",
            path.display()
        )));
    }
    let contents = fs::read_to_string(path)?;
    let replacement = replace_kubernetes_block(&discovered.raw_document, info)?;
    let occurrences = contents.match_indices(&discovered.raw_document).count();
    if occurrences != 1 {
        return Err(NylError::config(format!(
            "Cannot safely locate Cluster '{}' document in {}",
            discovered.identity.name,
            path.display()
        )));
    }
    let updated = contents.replacen(&discovered.raw_document, &replacement, 1);
    reject_symlink_path(project_root, path)?;
    if fs::read_to_string(path)? != contents {
        return Err(NylError::config(format!(
            "Cluster source {} changed while capabilities were being fetched; refusing to overwrite it",
            path.display()
        )));
    }
    let parent = path
        .parent()
        .ok_or_else(|| NylError::config(format!("Cluster source {} has no parent directory", path.display())))?;
    let permissions = fs::metadata(path)?.permissions();
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.as_file().set_permissions(permissions)?;
    temporary.write_all(updated.as_bytes())?;
    temporary.as_file().sync_all()?;
    reject_symlink_path(project_root, path)?;
    let backup_path = tempfile::Builder::new()
        .prefix(".nyl-cluster-backup-")
        .tempfile_in(parent)?
        .into_temp_path();
    fs::remove_file(&backup_path)?;
    fs::rename(path, &backup_path)?;
    if fs::symlink_metadata(&backup_path)?.file_type().is_symlink() || fs::read_to_string(&backup_path)? != contents {
        fs::hard_link(&backup_path, path)?;
        return Err(NylError::config(format!(
            "Cluster source {} changed while capabilities were being fetched; refusing to overwrite it",
            path.display()
        )));
    }
    if let Err(error) = temporary.persist_noclobber(path) {
        if !path.exists() {
            fs::hard_link(&backup_path, path)?;
        }
        return Err(NylError::config(format!(
            "Cluster source {} changed while capabilities were being installed: {}",
            path.display(),
            error.error
        )));
    }
    Ok(())
}

fn reject_symlink_path(project_root: &Path, path: &Path) -> Result<()> {
    let relative = path.strip_prefix(project_root).map_err(|_| {
        NylError::config(format!(
            "Cluster source {} is outside project root {}",
            path.display(),
            project_root.display()
        ))
    })?;
    let mut current = project_root.to_path_buf();
    for component in relative.components() {
        current.push(component.as_os_str());
        if fs::symlink_metadata(&current)?.file_type().is_symlink() {
            return Err(NylError::config(format!(
                "Refusing to update Cluster source through symbolic link {}",
                current.display()
            )));
        }
    }
    Ok(())
}

fn replace_kubernetes_block(document: &str, info: &ClusterInfo) -> Result<String> {
    let lines: Vec<&str> = document.split_inclusive('\n').collect();
    let mut spec_indent = None;
    let mut spec_child_indent = None;
    let mut start = None;
    let mut child_indent = None;
    for (index, line) in lines.iter().enumerate() {
        let text = line.trim_end_matches(['\r', '\n']);
        let trimmed = text.trim_start();
        let indent = text.len() - trimmed.len();
        if indent == 0 && trimmed == "spec:" {
            spec_indent = Some(indent);
            continue;
        }
        if let Some(parent) = spec_indent {
            if !trimmed.is_empty() && !trimmed.starts_with('#') && indent <= parent {
                spec_indent = None;
                spec_child_indent = None;
            } else if !trimmed.is_empty() && !trimmed.starts_with('#') && indent > parent {
                let direct_child = *spec_child_indent.get_or_insert(indent);
                if indent == direct_child && trimmed.starts_with("kubernetes:") {
                    if trimmed != "kubernetes:" {
                        return Err(NylError::config(
                            "spec.kubernetes must use a block mapping for cluster update",
                        ));
                    }
                    start = Some(index);
                    child_indent = Some(indent);
                    break;
                }
            }
        }
    }
    let start = start.ok_or_else(|| NylError::config("Cluster source is missing a block spec.kubernetes field"))?;
    let indent = child_indent.expect("indent exists with block start");
    let mut end = start + 1;
    while end < lines.len() {
        let text = lines[end].trim_end_matches(['\r', '\n']);
        let trimmed = text.trim_start();
        let line_indent = text.len() - trimmed.len();
        if !trimmed.is_empty() && line_indent <= indent {
            break;
        }
        end += 1;
    }

    let indentation = " ".repeat(indent);
    let nested = " ".repeat(indent + 2);
    let item = " ".repeat(indent + 4);
    let mut block = format!(
        "{indentation}kubernetes:\n{nested}kubeVersion: {}\n{nested}apiVersions:\n",
        info.kube_version
    );
    for api_version in &info.api_versions {
        writeln!(block, "{item}- {api_version}").expect("writing to String cannot fail");
    }
    let mut result = String::with_capacity(document.len() + block.len());
    result.push_str(&lines[..start].concat());
    result.push_str(&block);
    result.push_str(&lines[end..].concat());
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_preserves_unrelated_document_content() {
        let input = "# cluster\napiVersion: gitops.nyl/v1\nkind: Cluster\nmetadata:\n  name: prod\nspec:\n  destination:\n    name: prod\n  kubernetes:\n    # generated\n    kubeVersion: old\n    apiVersions: [v1]\n  # cluster facts\n  values:\n    region: eu\n";
        let output = replace_kubernetes_block(
            input,
            &ClusterInfo {
                kube_version: "1.31.2".to_string(),
                api_versions: vec!["apps/v1".to_string(), "v1".to_string()],
            },
        )
        .unwrap();
        assert!(output.starts_with("# cluster\napiVersion:"));
        assert!(output.contains("  # cluster facts\n  values:\n    region: eu\n"));
        assert!(output.contains("    kubeVersion: 1.31.2\n"));
        assert!(output.contains("      - apps/v1\n      - v1\n"));
    }

    #[test]
    fn update_selects_only_the_direct_cluster_kubernetes_block() {
        let input = "apiVersion: gitops.nyl/v1\nkind: Cluster\nmetadata:\n  name: prod\nspec:\n  values:\n    kubernetes:\n      keep: true\n    spec:\n      kubernetes:\n        keep: true\n  destination:\n    name: prod\n  kubernetes:\n    kubeVersion: old\n    apiVersions: [v1]\n";
        let output = replace_kubernetes_block(
            input,
            &ClusterInfo {
                kube_version: "1.31.2".to_string(),
                api_versions: vec!["v1".to_string()],
            },
        )
        .unwrap();
        assert!(output.contains("    kubernetes:\n      keep: true"));
        assert!(output.contains("      kubernetes:\n        keep: true"));
        assert!(output.contains("  kubernetes:\n    kubeVersion: 1.31.2"));
    }

    #[test]
    fn cluster_url_normalization_preserves_case_sensitive_paths() {
        assert_eq!(
            normalize_cluster_url("HTTPS://EXAMPLE.invalid/clusters/Prod/"),
            "https://example.invalid/clusters/Prod"
        );
        assert_ne!(
            normalize_cluster_url("https://example.invalid/clusters/Prod"),
            normalize_cluster_url("https://example.invalid/clusters/prod")
        );
    }
}
