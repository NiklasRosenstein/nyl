use clap::Args;
use colored::Colorize;
use dialoguer::Confirm;

use crate::{
    cli::commands::apply::{apply_and_record_release, apply_sorted_manifests, print_apply_summary},
    cli::commands::cluster::load_target_kube_config,
    kubernetes::{KubeRsClient, KubernetesReleaseStorage, ReleaseState, ReleaseStorage, ResourceOrdering},
    NylError, Result,
};

/// Roll back a release to a previously stored revision
#[derive(Args, Debug)]
pub struct RollbackArgs {
    /// deployment target whose cluster stores the release
    #[arg(long)]
    pub target: String,

    /// Release name
    pub name: String,

    /// Release namespace
    #[arg(short, long)]
    pub namespace: String,

    /// Revision to roll back to (default: the revision before the latest)
    #[arg(short, long)]
    pub revision: Option<u32>,

    /// Skip confirmation prompt
    #[arg(short, long)]
    pub yes: bool,

    /// Kubernetes context to use
    #[arg(long)]
    pub context: Option<String>,
}

pub async fn execute(args: RollbackArgs) -> Result<()> {
    // Create Kubernetes clients
    let config = load_target_kube_config(&args.target, args.context.as_deref()).await?;
    let client = kube::Client::try_from(config)?;
    let kube_client = KubeRsClient::from_client(client.clone()).await?;
    let storage = KubernetesReleaseStorage::new(client);

    // Resolve the revision we are rolling back to.
    let target = resolve_rollback_target(&storage, &args.name, &args.namespace, args.revision).await?;

    // Parse the stored manifest back into individual documents.
    let mut manifests = crate::yaml::parse_yaml_documents_k8s_compatible(&target.manifest)
        .map_err(|e| NylError::Config(format!("Failed to parse stored manifest for rollback: {}", e)))?;
    if manifests.is_empty() {
        return Err(NylError::Config(format!(
            "Revision {} of release '{}' has no manifest to roll back to",
            target.revision, args.name
        )));
    }

    // Confirm unless --yes.
    if !args.yes {
        let prompt = format!(
            "{} Roll back release '{}' in namespace '{}' to revision {} (creates a new revision)?",
            "⚠".yellow(),
            args.name,
            args.namespace,
            target.revision,
        );
        let confirmed = Confirm::new()
            .with_prompt(prompt)
            .default(false)
            .interact()
            .map_err(|e| NylError::Other(format!("Confirmation prompt failed: {}", e)))?;
        if !confirmed {
            println!("Cancelled");
            return Ok(());
        }
    }

    println!(
        "Rolling back release '{}' to revision {}...\n",
        args.name, target.revision
    );

    // Sort resources by priority (Namespace → CRD → RBAC → Config → Workload) and apply.
    ResourceOrdering::sort_by_priority(&mut manifests)?;
    let apply_result = apply_sorted_manifests(&kube_client, &manifests).await?;

    // Record the rollback as a new revision (supersede previous + prune), reusing the apply path.
    let release = apply_and_record_release(
        &storage,
        &kube_client,
        &manifests,
        &apply_result,
        &args.name,
        &args.namespace,
        false,
    )
    .await?;

    print_apply_summary(
        &apply_result.outcomes,
        Some(&release),
        &std::collections::HashMap::new(),
        apply_result.failed_count,
    );

    if apply_result.failed_count > 0 {
        return Err(NylError::Other(format!(
            "Rollback completed with {} error(s)",
            apply_result.failed_count
        )));
    }

    Ok(())
}

/// Resolve which stored revision a rollback should target.
///
/// When `revision` is given, that exact revision is loaded. Otherwise the rollback
/// defaults to the revision immediately before the latest one (i.e. undo the most
/// recent deployment). Returns a clear error when the release or revision does not
/// exist, or when there is no previous revision to roll back to.
async fn resolve_rollback_target(
    storage: &dyn ReleaseStorage,
    name: &str,
    namespace: &str,
    revision: Option<u32>,
) -> Result<ReleaseState> {
    if let Some(rev) = revision {
        return storage.get_release(name, namespace, rev).await?.ok_or_else(|| {
            NylError::Config(format!(
                "Revision {} of release '{}' not found in namespace '{}'",
                rev, name, namespace
            ))
        });
    }

    let revisions = storage.list_revisions(name, namespace).await?;
    let Some(&latest) = revisions.iter().max() else {
        return Err(NylError::Config(format!(
            "Release '{}' not found in namespace '{}'",
            name, namespace
        )));
    };

    if latest <= 1 {
        return Err(NylError::Config(format!(
            "Release '{}' has no previous revision to roll back to (only revision {} exists). \
             Specify --revision to target a specific revision.",
            name, latest
        )));
    }

    let target_revision = latest - 1;
    storage
        .get_release(name, namespace, target_revision)
        .await?
        .ok_or_else(|| {
            NylError::Config(format!(
                "Cannot roll back release '{}' in namespace '{}': the previous revision ({}) no longer \
                 exists (it may have been deleted). Use --revision to choose a specific revision to roll back to.",
                name, namespace, target_revision
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kubernetes::{ReleaseInfo, ReleaseStatus};
    use chrono::Utc;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    /// In-memory release storage for testing (mirrors the mock in `state.rs`).
    struct MockReleaseStorage {
        releases: Arc<Mutex<HashMap<(String, u32), ReleaseState>>>,
    }

    impl MockReleaseStorage {
        fn new() -> Self {
            Self {
                releases: Arc::new(Mutex::new(HashMap::new())),
            }
        }
    }

    #[async_trait::async_trait]
    impl ReleaseStorage for MockReleaseStorage {
        async fn save_release(&self, release: &ReleaseState) -> Result<()> {
            let mut store = self.releases.lock().unwrap();
            let key = (
                format!("{}/{}", release.release_namespace, release.release_name),
                release.revision,
            );
            store.insert(key, release.clone());
            Ok(())
        }

        async fn get_latest_release(&self, release_name: &str, namespace: &str) -> Result<Option<ReleaseState>> {
            let revisions = self.list_revisions(release_name, namespace).await?;
            match revisions.iter().max() {
                Some(latest) => self.get_release(release_name, namespace, *latest).await,
                None => Ok(None),
            }
        }

        async fn get_release(
            &self,
            release_name: &str,
            namespace: &str,
            revision: u32,
        ) -> Result<Option<ReleaseState>> {
            let store = self.releases.lock().unwrap();
            let key = format!("{namespace}/{release_name}");
            Ok(store.get(&(key, revision)).cloned())
        }

        async fn list_revisions(&self, release_name: &str, namespace: &str) -> Result<Vec<u32>> {
            let store = self.releases.lock().unwrap();
            let key_prefix = format!("{namespace}/{release_name}");
            let mut revisions: Vec<u32> = store
                .keys()
                .filter(|(c, _)| c == &key_prefix)
                .map(|(_, r)| *r)
                .collect();
            revisions.sort_unstable();
            Ok(revisions)
        }

        async fn update_release_status(
            &self,
            release_name: &str,
            namespace: &str,
            revision: u32,
            status: ReleaseStatus,
            error: Option<String>,
        ) -> Result<()> {
            let mut store = self.releases.lock().unwrap();
            let key = format!("{namespace}/{release_name}");
            if let Some(release) = store.get_mut(&(key, revision)) {
                release.status = status;
                release.error = error;
            }
            Ok(())
        }

        async fn list_releases(&self, _namespace: Option<&str>) -> Result<Vec<ReleaseInfo>> {
            Ok(vec![])
        }

        async fn delete_release(&self, release_name: &str, namespace: &str, revision: u32) -> Result<()> {
            let mut store = self.releases.lock().unwrap();
            store.remove(&(format!("{namespace}/{release_name}"), revision));
            Ok(())
        }

        async fn delete_all_revisions(&self, release_name: &str, namespace: &str) -> Result<u32> {
            let revisions = self.list_revisions(release_name, namespace).await?;
            let count = u32::try_from(revisions.len())
                .map_err(|e| NylError::Other(format!("Too many revisions to count: {}", e)))?;
            for revision in revisions {
                self.delete_release(release_name, namespace, revision).await?;
            }
            Ok(count)
        }
    }

    fn make_release(name: &str, namespace: &str, revision: u32, status: ReleaseStatus) -> ReleaseState {
        ReleaseState {
            release_name: name.to_string(),
            release_namespace: namespace.to_string(),
            revision,
            resource_keys: vec![],
            manifest: format!("# manifest for revision {revision}"),
            status,
            rendered_at: Utc::now(),
            applied_at: Some(Utc::now()),
            error: None,
        }
    }

    async fn seed(storage: &MockReleaseStorage, name: &str, ns: &str, revisions: &[u32]) {
        for (i, rev) in revisions.iter().enumerate() {
            let status = if i + 1 == revisions.len() {
                ReleaseStatus::Deployed
            } else {
                ReleaseStatus::Superseded
            };
            storage
                .save_release(&make_release(name, ns, *rev, status))
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn test_resolve_explicit_revision() {
        let storage = MockReleaseStorage::new();
        seed(&storage, "app", "default", &[1, 2, 3, 4]).await;

        let target = resolve_rollback_target(&storage, "app", "default", Some(2))
            .await
            .unwrap();
        assert_eq!(target.revision, 2);
        assert_eq!(target.manifest, "# manifest for revision 2");
    }

    #[tokio::test]
    async fn test_resolve_default_is_latest_minus_one() {
        let storage = MockReleaseStorage::new();
        seed(&storage, "app", "default", &[1, 2, 3, 4]).await;

        // Latest is 4, so the default rollback target is revision 3.
        let target = resolve_rollback_target(&storage, "app", "default", None).await.unwrap();
        assert_eq!(target.revision, 3);
    }

    #[tokio::test]
    async fn test_resolve_missing_release() {
        let storage = MockReleaseStorage::new();
        let err = resolve_rollback_target(&storage, "ghost", "default", None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[tokio::test]
    async fn test_resolve_missing_explicit_revision() {
        let storage = MockReleaseStorage::new();
        seed(&storage, "app", "default", &[1, 2]).await;

        let err = resolve_rollback_target(&storage, "app", "default", Some(9))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Revision 9"));
    }

    #[tokio::test]
    async fn test_resolve_default_into_deleted_hole_errors() {
        let storage = MockReleaseStorage::new();
        // Revision 3 was deleted, leaving a hole immediately before the latest (4).
        seed(&storage, "app", "default", &[1, 2, 4]).await;

        let err = resolve_rollback_target(&storage, "app", "default", None)
            .await
            .unwrap_err();
        // Default targets latest-1 (3), which is gone: surface a clear error rather
        // than silently rolling back to a surprising revision.
        assert!(err.to_string().contains("no longer"));
    }

    #[tokio::test]
    async fn test_resolve_no_previous_revision() {
        let storage = MockReleaseStorage::new();
        seed(&storage, "app", "default", &[1]).await;

        let err = resolve_rollback_target(&storage, "app", "default", None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no previous revision"));
    }

    /// When the latest revision Failed, pruning must reconcile against the last
    /// Deployed revision's live resources (plus any partial resources the Failed
    /// revision applied), not just the numerically previous secret.
    #[tokio::test]
    async fn test_collect_live_state_spans_failed_revision_to_last_deployed() {
        use crate::cli::commands::apply::collect_live_state;
        use crate::kubernetes::{GroupVersionKind, ResourceKey};

        let key = |name: &str| ResourceKey {
            gvk: GroupVersionKind {
                group: String::new(),
                version: "v1".to_string(),
                kind: "ConfigMap".to_string(),
            },
            namespace: Some("ns".to_string()),
            name: name.to_string(),
        };

        let storage = MockReleaseStorage::new();

        // rev3 Deployed (live: a, x); rev4 Failed (partially applied: y).
        let mut rev3 = make_release("app", "ns", 3, ReleaseStatus::Deployed);
        rev3.resource_keys = vec![key("a"), key("x")];
        storage.save_release(&rev3).await.unwrap();

        let mut rev4 = make_release("app", "ns", 4, ReleaseStatus::Failed);
        rev4.resource_keys = vec![key("y")];
        storage.save_release(&rev4).await.unwrap();

        // Recording rev5 (e.g. a rollback): live state is rev3 + rev4's partials,
        // and the superseded revision is the last Deployed one (rev3).
        let (superseded, live) = collect_live_state(&storage, "app", "ns", 5).await.unwrap();
        assert_eq!(superseded, Some(3));
        let names: std::collections::HashSet<&str> = live.iter().map(|k| k.name.as_str()).collect();
        assert_eq!(names, ["a", "x", "y"].into_iter().collect());
    }
}
