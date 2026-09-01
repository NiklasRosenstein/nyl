use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use clap::{Args, ValueEnum};
use git2::Repository;
use similar::TextDiff;

use crate::git::GitManager;
use crate::gitops::{
    compile_target_tree, compile_target_tree_cached, discover_gitops_inventory, GitOpsCache, RenderIndex, TreeCacheArgs,
};
use crate::{NylError, Result};

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum DiffTreeBase {
    /// Compare with the currently published revision.
    Published,
    /// Render and compare with the source repository at --source-ref.
    Source,
}

/// Diff a target without modifying its publication tree.
#[derive(Args, Debug)]
pub struct DiffTreeArgs {
    #[command(flatten)]
    pub cache: TreeCacheArgs,

    /// Project directory or a path beneath it.
    #[arg(default_value = ".")]
    pub path: PathBuf,
    #[arg(long)]
    pub target: String,
    #[arg(long, value_enum, default_value = "published")]
    pub against: DiffTreeBase,
    /// Source revision used by --against source.
    #[arg(long, requires = "against")]
    pub source_ref: Option<String>,
    /// Source repository URL. Defaults to the current repository's origin.
    #[arg(long)]
    pub source_repository: Option<String>,
    /// Return an error when differences exist.
    #[arg(long)]
    pub fail_on_diff: bool,
}

#[derive(Debug)]
struct PublishedRenderedTree {
    files: BTreeMap<PathBuf, Vec<u8>>,
    index: Option<RenderIndex>,
}

pub async fn execute(args: DiffTreeArgs) -> Result<()> {
    let inventory = discover_gitops_inventory(&args.path, None)?;
    let cache = GitOpsCache::new(&inventory.project_root, args.cache.mode())?;
    let desired = compile_target_tree_cached(&inventory, &args.target, &cache, None).await?;
    let (mut base, source_baseline) = match args.against {
        DiffTreeBase::Published => (published_tree(&desired)?, None),
        DiffTreeBase::Source => {
            let source_ref = args
                .source_ref
                .as_deref()
                .ok_or_else(|| NylError::config("--source-ref is required with --against source"))?;
            let baseline = source_derived_tree(
                &inventory.project_root,
                args.source_repository.as_deref(),
                source_ref,
                &args.target,
            )
            .await?;
            (baseline.files.clone(), Some(baseline))
        }
    };
    let mut desired_files = desired.files.clone();
    if let Some(baseline) = source_baseline {
        let marker = PathBuf::from("_nyl/publication.json");
        base.insert(marker.clone(), publication_marker(&baseline)?);
        desired_files.insert(marker, publication_marker(&desired)?);
    }
    let diff = format_tree_diff(&base, &desired_files);
    if diff.is_empty() {
        println!("GitOps target {} has no rendered differences", args.target);
        return Ok(());
    }
    print!("{diff}");
    if args.fail_on_diff {
        Err(NylError::validation(format!(
            "GitOps target {:?} has rendered differences",
            args.target
        )))
    } else {
        Ok(())
    }
}

fn published_tree(compiled: &crate::gitops::CompiledTargetTree) -> Result<BTreeMap<PathBuf, Vec<u8>>> {
    let mut manager = GitManager::new().map_err(NylError::Git)?;
    let checkout = manager
        .resolve_ref_fresh(
            &compiled.repository.repo_url,
            Some(&compiled.target.spec.publication.revision),
            None,
        )
        .map_err(NylError::Git)?;
    let root = checked_published_root(&checkout, &compiled.target.spec.publication.path_prefix)?;
    let published = read_rendered_tree(&root)?;
    if let Some(index) = published.index {
        let repository = compiled
            .repository_name
            .as_deref()
            .unwrap_or(&compiled.repository.repo_url);
        if index.target != compiled.target.metadata.name
            || index.cluster != compiled.cluster.metadata.name
            || index.publication.repository != repository
            || index.publication.revision != compiled.target.spec.publication.revision
            || index.publication.path_prefix != compiled.target.spec.publication.path_prefix
        {
            return Err(NylError::config(format!(
                "Published ownership index at {} belongs to a different target, cluster, or publication",
                root.display()
            )));
        }
    }
    Ok(published.files)
}

fn checked_published_root(checkout: &Path, path_prefix: &str) -> Result<PathBuf> {
    crate::resources::validate_relative_path("GitOpsTarget publication.pathPrefix", path_prefix, true, false)?;
    let canonical_checkout = checkout.canonicalize().map_err(|error| {
        NylError::config(format!(
            "Failed to resolve published checkout {}: {error}",
            checkout.display()
        ))
    })?;
    let mut selected = checkout.to_path_buf();
    for component in Path::new(path_prefix).components() {
        selected.push(component.as_os_str());
        match std::fs::symlink_metadata(&selected) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(NylError::config(format!(
                    "Published rendered tree contains symbolic link {}",
                    selected.display()
                )))
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    if selected.exists() {
        let canonical_selected = selected.canonicalize()?;
        if !canonical_selected.starts_with(&canonical_checkout) {
            return Err(NylError::config(format!(
                "Published rendered root {} resolves outside checkout {}",
                selected.display(),
                checkout.display()
            )));
        }
    }
    Ok(selected)
}

async fn source_derived_tree(
    project_root: &Path,
    source_repository: Option<&str>,
    source_ref: &str,
    target: &str,
) -> Result<crate::gitops::CompiledTargetTree> {
    let repository_url = if let Some(url) = source_repository {
        url.to_string()
    } else {
        let repository = Repository::discover(project_root)
            .map_err(|error| NylError::config(format!("Failed to inspect source repository: {error}")))?;
        repository
            .find_remote("origin")
            .ok()
            .and_then(|remote| remote.url().map(ToOwned::to_owned))
            .ok_or_else(|| NylError::config("Source repository has no origin; pass --source-repository"))?
    };
    let mut manager = GitManager::new().map_err(NylError::Git)?;
    let checkout = manager
        .resolve_ref_fresh(&repository_url, Some(source_ref), None)
        .map_err(NylError::Git)?;
    let inventory = discover_gitops_inventory(&checkout, None)?;
    compile_target_tree(&inventory, target).await
}

fn publication_marker(compiled: &crate::gitops::CompiledTargetTree) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(&serde_json::json!({
        "cluster": compiled.cluster.metadata.name,
        "repoURL": compiled.repository.repo_url,
        "publishURL": compiled.repository.publish_url,
        "revision": compiled.target.spec.publication.revision,
        "pathPrefix": compiled.target.spec.publication.path_prefix,
    }))?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn read_rendered_tree(root: &Path) -> Result<PublishedRenderedTree> {
    if !root.exists() {
        return Ok(PublishedRenderedTree {
            files: BTreeMap::new(),
            index: None,
        });
    }
    let index_path = root.join(crate::gitops::reconcile::DEFAULT_INDEX_PATH);
    if !index_path.is_file() {
        return Err(NylError::config(format!(
            "Published rendered tree {} has no ownership index",
            root.display()
        )));
    }
    reject_published_symlink(root, &index_path)?;
    let index: RenderIndex = serde_json::from_slice(&std::fs::read(&index_path)?)?;
    if index.version != crate::gitops::reconcile::RENDER_INDEX_VERSION {
        return Err(NylError::config(format!(
            "Published ownership index {} uses unsupported version {}",
            index_path.display(),
            index.version
        )));
    }
    let mut files = BTreeMap::new();
    for (relative, expected_hash) in &index.files {
        crate::resources::validate_relative_path("published owned path", relative, false, false)?;
        let path = root.join(relative);
        reject_published_symlink(root, &path)?;
        let bytes = std::fs::read(&path).map_err(|error| {
            NylError::config(format!(
                "Published owned file {} is missing or unreadable: {error}",
                path.display()
            ))
        })?;
        if crate::gitops::reconcile::sha256(&bytes) != *expected_hash {
            return Err(NylError::config(format!(
                "Published owned file {} does not match its ownership index",
                path.display()
            )));
        }
        files.insert(PathBuf::from(relative), bytes);
    }
    Ok(PublishedRenderedTree {
        files,
        index: Some(index),
    })
}

fn reject_published_symlink(root: &Path, path: &Path) -> Result<()> {
    let relative = path
        .strip_prefix(root)
        .map_err(|error| NylError::config(format!("Published path escaped its root: {error}")))?;
    let mut current = root.to_path_buf();
    if std::fs::symlink_metadata(&current).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(NylError::config(format!(
            "Published rendered tree contains symbolic link {}",
            current.display()
        )));
    }
    for component in relative.components() {
        current.push(component.as_os_str());
        if std::fs::symlink_metadata(&current).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
            return Err(NylError::config(format!(
                "Published rendered tree contains symbolic link {}",
                current.display()
            )));
        }
    }
    Ok(())
}

fn format_tree_diff(base: &BTreeMap<PathBuf, Vec<u8>>, desired: &BTreeMap<PathBuf, Vec<u8>>) -> String {
    let paths = base.keys().chain(desired.keys()).cloned().collect::<BTreeSet<_>>();
    let mut output = String::new();
    for path in paths {
        let old = base
            .get(&path)
            .map_or("", |bytes| std::str::from_utf8(bytes).unwrap_or("<binary>\n"));
        let new = desired
            .get(&path)
            .map_or("", |bytes| std::str::from_utf8(bytes).unwrap_or("<binary>\n"));
        if old == new {
            continue;
        }
        let path = path.to_string_lossy().replace('\\', "/");
        output.push_str(
            &TextDiff::from_lines(old, new)
                .unified_diff()
                .context_radius(3)
                .header(&format!("a/{path}"), &format!("b/{path}"))
                .to_string(),
        );
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resources::{Cluster, GitOpsTarget, InlineGitRepository};

    #[test]
    fn formats_added_modified_and_removed_files() {
        let base = BTreeMap::from([
            (PathBuf::from("removed.yaml"), b"old\n".to_vec()),
            (PathBuf::from("same.yaml"), b"same\n".to_vec()),
            (PathBuf::from("changed.yaml"), b"old\n".to_vec()),
        ]);
        let desired = BTreeMap::from([
            (PathBuf::from("added.yaml"), b"new\n".to_vec()),
            (PathBuf::from("same.yaml"), b"same\n".to_vec()),
            (PathBuf::from("changed.yaml"), b"new\n".to_vec()),
        ]);
        let diff = format_tree_diff(&base, &desired);
        assert!(diff.contains("a/added.yaml"));
        assert!(diff.contains("a/changed.yaml"));
        assert!(diff.contains("a/removed.yaml"));
        assert!(!diff.contains("same.yaml"));
    }

    #[test]
    fn publication_marker_changes_when_ownership_coordinates_change() {
        let target: GitOpsTarget = serde_json::from_value(serde_json::json!({
            "apiVersion": crate::constants::API_VERSION_GITOPS,
            "kind": "GitOpsTarget",
            "metadata": {"name": "production"},
            "spec": {
                "clusterRef": {"name": "kasoku"},
                "publication": {
                    "repository": {"repoURL": "https://example.invalid/deploy.git"},
                    "revision": "deploy/production",
                    "pathPrefix": "production"
                }
            }
        }))
        .unwrap();
        let cluster: Cluster = serde_json::from_value(serde_json::json!({
            "apiVersion": crate::constants::API_VERSION_GITOPS,
            "kind": "Cluster",
            "metadata": {"name": "kasoku"},
            "spec": {
                "destination": {"server": "https://kubernetes.default.svc"},
                "kubernetes": {"kubeVersion": "1.31.4", "apiVersions": ["v1"]}
            }
        }))
        .unwrap();
        let baseline = crate::gitops::CompiledTargetTree {
            target: target.clone(),
            cluster,
            repository_name: None,
            repository: InlineGitRepository {
                repo_url: "https://example.invalid/deploy.git".to_string(),
                publish_url: None,
            },
            files: BTreeMap::new(),
            inputs: BTreeSet::new(),
        };
        let baseline_marker = publication_marker(&baseline).unwrap();
        let mut desired = baseline;
        desired.target.spec.publication.path_prefix = "new-prefix".to_string();
        assert_ne!(baseline_marker, publication_marker(&desired).unwrap());

        let changed_publication_marker = publication_marker(&desired).unwrap();
        desired.cluster.metadata.name = "magnolia".to_string();
        assert_ne!(changed_publication_marker, publication_marker(&desired).unwrap());
    }

    #[test]
    fn published_tree_requires_an_ownership_index() {
        let temp = tempfile::TempDir::new().unwrap();
        std::fs::write(temp.path().join("unrelated.yaml"), "kind: ConfigMap\n").unwrap();
        let error = read_rendered_tree(temp.path()).unwrap_err();
        assert!(error.to_string().contains("no ownership index"));
    }

    #[cfg(unix)]
    #[test]
    fn published_root_rejects_a_symlinked_prefix_ancestor() {
        use std::os::unix::fs::symlink;

        let checkout = tempfile::TempDir::new().unwrap();
        let outside = tempfile::TempDir::new().unwrap();
        symlink(outside.path(), checkout.path().join("production")).unwrap();

        let error = checked_published_root(checkout.path(), "production/apps").unwrap_err();
        assert!(error.to_string().contains("symbolic link"));
    }
}
