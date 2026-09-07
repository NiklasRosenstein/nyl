use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use clap::Args;

use crate::cli::resource_file::{atomic_replace, replace_document};
use crate::gitops::discovery::{discover_gitops_inventory, DiscoveredGitOpsResource, GitOpsInventory};
use crate::kubernetes::{KubeClient, KubeRsClient};
use crate::resources::{Cluster, DeploymentTarget, GitOpsResource, GitOpsResourceKind};
use crate::{NylError, Result};

#[derive(Args, Debug)]
pub struct ClusterCaptureArgs {
    pub name: String,
    #[arg(long)]
    pub context: Option<String>,
    /// Check whether stored capabilities are current without modifying the file
    #[arg(long)]
    pub check: bool,
    /// Capture schemas for all served CRD versions.
    #[arg(long, conflicts_with = "no_crds")]
    pub crds: bool,
    /// Capture capabilities only, preserving existing schema files.
    #[arg(long)]
    pub no_crds: bool,
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

fn inventory(start_dir: &Path) -> Result<GitOpsInventory> {
    discover_gitops_inventory(start_dir, None)
}

pub fn resolve_target_cluster(start_dir: &Path, target_name: &str) -> Result<ResolvedTargetCluster> {
    let inventory = inventory(start_dir)?;
    resolve_target_cluster_from_inventory(&inventory, target_name)
}

pub fn resolve_target_cluster_from_inventory(
    inventory: &GitOpsInventory,
    target_name: &str,
) -> Result<ResolvedTargetCluster> {
    let target = get_target(inventory, target_name)?.clone();
    let cluster = crate::gitops::resolve_cluster_contract(inventory, target.cluster_name())?.cluster;
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

pub(crate) async fn capture(args: ClusterCaptureArgs) -> Result<()> {
    capture_from_dir(args, &std::env::current_dir()?).await
}

pub(crate) async fn capture_from_dir(args: ClusterCaptureArgs, start_dir: &Path) -> Result<()> {
    capture_with_client(args, start_dir, &LiveCapture).await
}

trait CaptureClient {
    async fn fetch(
        &self,
        cluster: &Cluster,
        context: Option<&str>,
        crds: bool,
    ) -> Result<(ClusterInfo, Option<Vec<serde_json::Value>>)>;
}

struct LiveCapture;

impl CaptureClient for LiveCapture {
    async fn fetch(
        &self,
        cluster: &Cluster,
        context: Option<&str>,
        crds: bool,
    ) -> Result<(ClusterInfo, Option<Vec<serde_json::Value>>)> {
        fetch_cluster_info(cluster, context, crds).await
    }
}

async fn capture_with_client(args: ClusterCaptureArgs, start_dir: &Path, client: &impl CaptureClient) -> Result<()> {
    use crate::validation::{schemas, store};
    let inventory = inventory(start_dir)?;
    let discovered = inventory
        .get(GitOpsResourceKind::Cluster, &args.name)
        .ok_or_else(|| NylError::config(format!("Cluster '{}' not found", args.name)))?;
    let cluster = discovered_cluster(discovered)?;
    if let Some(reference) = &cluster.spec.api_contract_from {
        return Err(NylError::config(format!(
            "Cluster {} borrows its API contract; capture source Cluster {} instead",
            args.name, reference.cluster_ref.name
        )));
    }
    let capture_crds = !args.no_crds && (args.crds || inventory.project_config.config.capture.cluster.crds);
    let (info, crds) = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        client.fetch(cluster, args.context.as_deref(), capture_crds),
    )
    .await
    .map_err(|_| NylError::validation("Cluster capture timed out after 60 seconds"))??;
    let capabilities = crate::resources::ClusterKubernetesCapabilities {
        kube_version: Some(info.kube_version.clone()),
        api_versions: info.api_versions.clone(),
    };
    let stored = cluster
        .spec
        .kubernetes
        .as_ref()
        .expect("local Cluster has capabilities");
    let differs = store::capabilities_fingerprint(stored)? != store::capabilities_fingerprint(&capabilities)?;
    let root = store::vendor_root(&inventory.project_root, &inventory.project_config)?;
    let prepared = crds
        .as_ref()
        .map(|crds| {
            let definitions = schemas::extract_crds(crds)?;
            store::prepare_capture(&args.name, &capabilities, &definitions)
        })
        .transpose()?;
    let schemas_differ = if let Some((index, _)) = &prepared {
        store::read_cluster_index(&root, &args.name).ok().flatten().as_ref() != Some(index)
            || index.crds.values().flat_map(|crd| crd.versions.values()).any(|refs| {
                store::read_blob(&root, &refs.strict).is_err() || store::read_blob(&root, &refs.permissive).is_err()
            })
    } else {
        false
    };
    if args.check {
        if differs || schemas_differ {
            return Err(NylError::validation(format!(
                "Cluster '{}' capture differs; run nyl capture cluster {}{}",
                args.name,
                args.name,
                if capture_crds { " --crds" } else { "" }
            )));
        }
        println!("Cluster '{}' capture is current", args.name);
        return Ok(());
    }
    if !differs && !schemas_differ {
        println!("Cluster '{}' capture is current", args.name);
        return Ok(());
    }
    // Validate the source edit before publishing a schema inventory.
    let (path, contents, updated) = prepare_cluster_document(&inventory.project_root, discovered, &info)?;
    if let Some((index, blobs)) = prepared {
        let _lock = store::lock(&root)?;
        for (hash, bytes) in blobs {
            debug_assert_eq!(hash, store::digest(&bytes));
            store::write_blob(&root, &bytes)?;
        }
        store::atomic_write(
            &store::cluster_index_path(&root, &args.name)?,
            &store::json_bytes(&index)?,
        )?;
        atomic_replace(&path, &contents, &updated)?;
    } else {
        atomic_replace(&path, &contents, &updated)?;
    }
    println!(
        "Captured Cluster '{}' into {}",
        args.name,
        crate::util::path_for_display(&path).display()
    );
    Ok(())
}

async fn fetch_cluster_info(
    cluster: &Cluster,
    context_override: Option<&str>,
    capture_crds: bool,
) -> Result<(ClusterInfo, Option<Vec<serde_json::Value>>)> {
    let config = load_cluster_kube_config(cluster, context_override).await?;
    let raw = kube::Client::try_from(config)?;
    let client = KubeRsClient::from_client(raw.clone()).await?;
    let kube_version = client.get_server_version().await?;
    let mut api_versions = client.get_api_versions().await?;
    api_versions.sort();
    api_versions.dedup();
    let crds = if capture_crds {
        let resource = kube::core::ApiResource::from_gvk(&kube::core::GroupVersionKind::gvk(
            "apiextensions.k8s.io",
            "v1",
            "CustomResourceDefinition",
        ));
        let api: kube::Api<kube::api::DynamicObject> = kube::Api::all_with(raw, &resource);
        let listed = api.list(&kube::api::ListParams::default()).await?;
        Some(
            listed
                .items
                .iter()
                .map(serde_json::to_value)
                .collect::<std::result::Result<Vec<_>, _>>()?,
        )
    } else {
        None
    };
    Ok((
        ClusterInfo {
            kube_version,
            api_versions,
        },
        crds,
    ))
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

fn discovered_cluster(discovered: &DiscoveredGitOpsResource) -> Result<&Cluster> {
    match discovered.resource.as_ref() {
        Some(GitOpsResource::Cluster(cluster)) => Ok(cluster),
        _ => Err(NylError::config(format!(
            "Cluster '{}' must be a static resource",
            discovered.identity.name
        ))),
    }
}

#[cfg(test)]
fn update_cluster_document(
    project_root: &Path,
    path: &Path,
    discovered: &DiscoveredGitOpsResource,
    info: &ClusterInfo,
) -> Result<()> {
    let (prepared_path, contents, updated) = prepare_cluster_document(project_root, discovered, info)?;
    if prepared_path != path {
        return Err(NylError::config("Cluster source path mismatch"));
    }
    atomic_replace(path, &contents, &updated)
}

fn prepare_cluster_document(
    project_root: &Path,
    discovered: &DiscoveredGitOpsResource,
    info: &ClusterInfo,
) -> Result<(std::path::PathBuf, String, String)> {
    let path = project_root.join(&discovered.source_path);
    if discovered.raw_document.contains("{{")
        || discovered.raw_document.contains("{%")
        || discovered.raw_document.contains("{#")
    {
        return Err(NylError::config(format!(
            "Cannot capture into templated Cluster source {}",
            path.display()
        )));
    }
    reject_symlink_path(project_root, &path)?;
    let contents = fs::read_to_string(&path)?;
    let replacement = replace_kubernetes_block(&discovered.raw_document, info)?;
    let updated = replace_document(
        &contents,
        discovered.document_index,
        &discovered.raw_document,
        &replacement,
    )?;
    Ok((path, contents, updated))
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
                            "spec.kubernetes must use a block mapping for cluster capture",
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
    use std::collections::BTreeMap;

    use crate::resources::GitOpsResourceIdentity;

    struct StubCapture {
        version: String,
        resources: Vec<serde_json::Value>,
        fail: bool,
    }

    impl CaptureClient for StubCapture {
        fn fetch(
            &self,
            _cluster: &Cluster,
            _context: Option<&str>,
            crds: bool,
        ) -> impl std::future::Future<Output = Result<(ClusterInfo, Option<Vec<serde_json::Value>>)>> {
            std::future::ready(if self.fail {
                Err(NylError::Kubernetes("CRD list forbidden".into()))
            } else {
                Ok((
                    ClusterInfo {
                        kube_version: self.version.clone(),
                        api_versions: vec!["v1".into(), "example.com/v1".into()],
                    },
                    crds.then(|| self.resources.clone()),
                ))
            })
        }
    }

    fn capture_fixture() -> tempfile::TempDir {
        let directory = tempfile::TempDir::new().unwrap();
        git2::Repository::init(directory.path()).unwrap();
        fs::write(directory.path().join("nyl.toml"), "[capture.cluster]\ncrds = true\n").unwrap();
        fs::write(directory.path().join("cluster.yaml"),
            "apiVersion: k8s.gitops.nyl/v1\nkind: Cluster\nmetadata:\n  name: staging\nspec:\n  destination:\n    name: staging\n  kubernetes:\n    kubeVersion: 1.31.4\n    apiVersions: [v1, example.com/v1]\n").unwrap();
        directory
    }

    fn capture_args(check: bool) -> ClusterCaptureArgs {
        ClusterCaptureArgs {
            name: "staging".into(),
            context: None,
            check,
            crds: false,
            no_crds: false,
        }
    }

    fn capture_stub(kind: &str) -> StubCapture {
        StubCapture {
            version: "1.31.4".into(),
            fail: false,
            resources: vec![serde_json::json!({
                "apiVersion":"apiextensions.k8s.io/v1","kind":"CustomResourceDefinition","metadata":{"name":"widgets.example.com"},
                "spec":{"group":"example.com","names":{"kind":"Widget"},"versions":[{"name":"v1","served":true,
                    "schema":{"openAPIV3Schema":{"type":"object","properties":{"spec":{"type":"object","properties":{"count":{"type":kind}}}}}}}]}
            })],
        }
    }

    #[tokio::test]
    async fn test_capture_detects_schema_changes_and_check_never_writes() {
        let directory = capture_fixture();
        let root = directory.path().join("vendor");
        assert!(
            capture_with_client(capture_args(true), directory.path(), &capture_stub("integer"))
                .await
                .is_err()
        );
        assert!(!root.exists());
        capture_with_client(capture_args(false), directory.path(), &capture_stub("integer"))
            .await
            .unwrap();
        let index_path = crate::validation::store::cluster_index_path(&root, "staging").unwrap();
        let original = fs::read(&index_path).unwrap();
        capture_with_client(capture_args(true), directory.path(), &capture_stub("integer"))
            .await
            .unwrap();
        assert!(
            capture_with_client(capture_args(true), directory.path(), &capture_stub("string"))
                .await
                .is_err()
        );
        assert_eq!(fs::read(&index_path).unwrap(), original);
        capture_with_client(capture_args(false), directory.path(), &capture_stub("string"))
            .await
            .unwrap();
        assert_ne!(fs::read(&index_path).unwrap(), original);
        let empty = StubCapture {
            resources: vec![],
            ..capture_stub("string")
        };
        capture_with_client(capture_args(false), directory.path(), &empty)
            .await
            .unwrap();
        assert!(crate::validation::store::read_cluster_index(&root, "staging")
            .unwrap()
            .unwrap()
            .crds
            .is_empty());
    }

    #[tokio::test]
    async fn test_capture_failures_preserve_committed_snapshot() {
        let directory = capture_fixture();
        capture_with_client(capture_args(false), directory.path(), &capture_stub("integer"))
            .await
            .unwrap();
        let source = directory.path().join("cluster.yaml");
        let index = directory.path().join("vendor/clusters/staging/schemas.json");
        let before = (fs::read(&source).unwrap(), fs::read(&index).unwrap());
        let failed = StubCapture {
            fail: true,
            version: "1.32.0".into(),
            ..capture_stub("integer")
        };
        assert!(capture_with_client(capture_args(false), directory.path(), &failed)
            .await
            .is_err());
        let mut invalid = capture_stub("integer");
        invalid.resources[0]["spec"]["versions"][0]["schema"] = serde_json::Value::Null;
        assert!(capture_with_client(capture_args(false), directory.path(), &invalid)
            .await
            .is_err());
        assert_eq!((fs::read(&source).unwrap(), fs::read(&index).unwrap()), before);
    }

    #[test]
    fn update_preserves_unrelated_document_content() {
        let input = "# cluster\napiVersion: k8s.gitops.nyl/v1\nkind: Cluster\nmetadata:\n  name: prod\nspec:\n  destination:\n    name: prod\n  kubernetes:\n    # generated\n    kubeVersion: old\n    apiVersions: [v1]\n  # cluster facts\n  values:\n    region: eu\n";
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
    fn update_edits_only_the_selected_document_in_a_shared_file() {
        let temporary = tempfile::TempDir::new().unwrap();
        let path = temporary.path().join("gitops.yaml");
        let first = "apiVersion: k8s.gitops.nyl/v1\nkind: Cluster\nmetadata:\n  name: first\nspec:\n  destination:\n    name: first\n  kubernetes:\n    kubeVersion: old\n    apiVersions: [v1]\n";
        let second = "apiVersion: k8s.gitops.nyl/v1\nkind: Cluster\nmetadata:\n  name: second\nspec:\n  destination:\n    name: second\n  kubernetes:\n    kubeVersion: old\n    apiVersions: [v1]\n";
        fs::write(&path, format!("{first}---\n{second}")).unwrap();
        let discovered = DiscoveredGitOpsResource {
            source_path: "gitops.yaml".into(),
            document_index: 2,
            raw_document: second.to_owned(),
            identity: GitOpsResourceIdentity {
                kind: GitOpsResourceKind::Cluster,
                name: "second".to_owned(),
            },
            static_labels: BTreeMap::new(),
            resource: None,
        };

        update_cluster_document(
            temporary.path(),
            &path,
            &discovered,
            &ClusterInfo {
                kube_version: "1.31.2".to_owned(),
                api_versions: vec!["v1".to_owned()],
            },
        )
        .unwrap();

        let updated = fs::read_to_string(path).unwrap();
        assert!(updated.starts_with(first));
        assert_eq!(updated.matches("kubeVersion: old").count(), 1);
        assert_eq!(updated.matches("kubeVersion: 1.31.2").count(), 1);
    }

    #[test]
    fn update_selects_only_the_direct_cluster_kubernetes_block() {
        let input = "apiVersion: k8s.gitops.nyl/v1\nkind: Cluster\nmetadata:\n  name: prod\nspec:\n  values:\n    kubernetes:\n      keep: true\n    spec:\n      kubernetes:\n        keep: true\n  destination:\n    name: prod\n  kubernetes:\n    kubeVersion: old\n    apiVersions: [v1]\n";
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
