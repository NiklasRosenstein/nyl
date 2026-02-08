use chrono::Utc;
use nyl::kubernetes::{ReleaseInfo, ReleaseState, ReleaseStatus, ReleaseStorage, ResourceKey};
use nyl::kubernetes::GroupVersionKind;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Mock release storage for testing (copied from state.rs tests)
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
    async fn save_release(&self, release: &ReleaseState) -> nyl::Result<()> {
        let mut store = self.releases.lock().unwrap();
        let key = (
            format!("{}/{}", release.release_namespace, release.release_name),
            release.revision,
        );
        store.insert(key, release.clone());
        Ok(())
    }

    async fn get_latest_release(
        &self,
        release_name: &str,
        namespace: &str,
    ) -> nyl::Result<Option<ReleaseState>> {
        let revisions = self.list_revisions(release_name, namespace).await?;
        if revisions.is_empty() {
            return Ok(None);
        }

        let latest = revisions.iter().max().unwrap();
        self.get_release(release_name, namespace, *latest).await
    }

    async fn get_release(
        &self,
        release_name: &str,
        namespace: &str,
        revision: u32,
    ) -> nyl::Result<Option<ReleaseState>> {
        let store = self.releases.lock().unwrap();
        let key = format!("{}/{}", namespace, release_name);
        Ok(store.get(&(key, revision)).cloned())
    }

    async fn list_revisions(&self, release_name: &str, namespace: &str) -> nyl::Result<Vec<u32>> {
        let store = self.releases.lock().unwrap();
        let key_prefix = format!("{}/{}", namespace, release_name);
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
    ) -> nyl::Result<()> {
        let mut store = self.releases.lock().unwrap();
        let key = format!("{}/{}", namespace, release_name);
        if let Some(release) = store.get_mut(&(key, revision)) {
            release.status = status;
            release.error = error;
            if release.status == ReleaseStatus::Deployed && release.applied_at.is_none() {
                release.applied_at = Some(Utc::now());
            }
        }
        Ok(())
    }

    async fn list_releases(
        &self,
        namespace: Option<&str>,
    ) -> nyl::Result<Vec<ReleaseInfo>> {
        use std::collections::HashMap;

        let store = self.releases.lock().unwrap();
        let mut releases: HashMap<(String, String), ReleaseState> = HashMap::new();

        for ((key, revision), state) in store.iter() {
            // Parse namespace/name from key
            if let Some((ns, name)) = key.split_once('/') {
                // Filter by namespace if specified
                if let Some(filter_ns) = namespace {
                    if ns != filter_ns {
                        continue;
                    }
                }

                let release_key = (name.to_string(), ns.to_string());
                releases
                    .entry(release_key)
                    .and_modify(|existing| {
                        if revision > &existing.revision {
                            *existing = state.clone();
                        }
                    })
                    .or_insert_with(|| state.clone());
            }
        }

        let mut result: Vec<ReleaseInfo> = releases
            .into_values()
            .map(|state| ReleaseInfo {
                release_name: state.release_name,
                release_namespace: state.release_namespace,
                latest_revision: state.revision,
                status: state.status,
                rendered_at: state.rendered_at,
                applied_at: state.applied_at,
                resource_count: state.resource_keys.len(),
            })
            .collect();

        result.sort_by(|a, b| {
            a.release_namespace
                .cmp(&b.release_namespace)
                .then_with(|| a.release_name.cmp(&b.release_name))
        });

        Ok(result)
    }

    async fn delete_release(
        &self,
        release_name: &str,
        namespace: &str,
        revision: u32,
    ) -> nyl::Result<()> {
        let mut store = self.releases.lock().unwrap();
        let key = (format!("{}/{}", namespace, release_name), revision);
        store.remove(&key);
        Ok(())
    }

    async fn delete_all_revisions(
        &self,
        release_name: &str,
        namespace: &str,
    ) -> nyl::Result<u32> {
        let revisions = self.list_revisions(release_name, namespace).await?;
        let count = revisions.len() as u32;

        for revision in revisions {
            self.delete_release(release_name, namespace, revision).await?;
        }

        Ok(count)
    }
}

/// Helper function to create a test release
fn create_test_release(
    name: &str,
    namespace: &str,
    revision: u32,
    status: ReleaseStatus,
    resource_count: usize,
) -> ReleaseState {
    let resource_keys: Vec<ResourceKey> = (0..resource_count)
        .map(|i| ResourceKey {
            gvk: GroupVersionKind {
                group: String::new(),
                version: "v1".to_string(),
                kind: "ConfigMap".to_string(),
            },
            namespace: Some(namespace.to_string()),
            name: format!("resource-{}", i),
        })
        .collect();

    let is_deployed = status == ReleaseStatus::Deployed;

    ReleaseState {
        release_name: name.to_string(),
        release_namespace: namespace.to_string(),
        revision,
        resource_keys,
        manifest: format!("# Test manifest for {} revision {}", name, revision),
        status,
        rendered_at: Utc::now(),
        applied_at: if is_deployed {
            Some(Utc::now())
        } else {
            None
        },
        error: None,
    }
}

#[tokio::test]
async fn test_list_releases_empty() {
    let storage = MockReleaseStorage::new();
    let releases = storage.list_releases(None).await.unwrap();
    assert_eq!(releases.len(), 0);
}

#[tokio::test]
async fn test_list_releases_single_namespace() {
    let storage = MockReleaseStorage::new();

    // Create releases in different namespaces
    storage
        .save_release(&create_test_release(
            "app1",
            "default",
            1,
            ReleaseStatus::Deployed,
            5,
        ))
        .await
        .unwrap();
    storage
        .save_release(&create_test_release(
            "app2",
            "default",
            1,
            ReleaseStatus::Deployed,
            3,
        ))
        .await
        .unwrap();
    storage
        .save_release(&create_test_release(
            "app3",
            "prod",
            1,
            ReleaseStatus::Deployed,
            10,
        ))
        .await
        .unwrap();

    // List all releases
    let all = storage.list_releases(None).await.unwrap();
    assert_eq!(all.len(), 3);

    // List only default namespace
    let default_ns = storage.list_releases(Some("default")).await.unwrap();
    assert_eq!(default_ns.len(), 2);
    assert!(default_ns.iter().all(|r| r.release_namespace == "default"));

    // List only prod namespace
    let prod_ns = storage.list_releases(Some("prod")).await.unwrap();
    assert_eq!(prod_ns.len(), 1);
    assert_eq!(prod_ns[0].release_name, "app3");
}

#[tokio::test]
async fn test_list_releases_shows_latest_revision() {
    let storage = MockReleaseStorage::new();

    // Create multiple revisions of the same release
    for rev in 1..=3 {
        storage
            .save_release(&create_test_release(
                "myapp",
                "default",
                rev,
                ReleaseStatus::Deployed,
                5 + rev as usize, // Different resource counts
            ))
            .await
            .unwrap();
    }

    let releases = storage.list_releases(None).await.unwrap();
    assert_eq!(releases.len(), 1); // Only one release
    assert_eq!(releases[0].release_name, "myapp");
    assert_eq!(releases[0].latest_revision, 3); // Latest revision
    assert_eq!(releases[0].resource_count, 8); // Resource count from revision 3
}

#[tokio::test]
async fn test_list_releases_sorted_by_namespace_then_name() {
    let storage = MockReleaseStorage::new();

    // Create releases in non-alphabetical order
    storage
        .save_release(&create_test_release(
            "zebra",
            "prod",
            1,
            ReleaseStatus::Deployed,
            1,
        ))
        .await
        .unwrap();
    storage
        .save_release(&create_test_release(
            "alpha",
            "default",
            1,
            ReleaseStatus::Deployed,
            1,
        ))
        .await
        .unwrap();
    storage
        .save_release(&create_test_release(
            "beta",
            "default",
            1,
            ReleaseStatus::Deployed,
            1,
        ))
        .await
        .unwrap();

    let releases = storage.list_releases(None).await.unwrap();
    assert_eq!(releases.len(), 3);

    // Should be sorted by namespace first (default < prod)
    assert_eq!(releases[0].release_namespace, "default");
    assert_eq!(releases[0].release_name, "alpha");
    assert_eq!(releases[1].release_namespace, "default");
    assert_eq!(releases[1].release_name, "beta");
    assert_eq!(releases[2].release_namespace, "prod");
    assert_eq!(releases[2].release_name, "zebra");
}

#[tokio::test]
async fn test_delete_release_single_revision() {
    let storage = MockReleaseStorage::new();

    storage
        .save_release(&create_test_release(
            "myapp",
            "default",
            1,
            ReleaseStatus::Deployed,
            5,
        ))
        .await
        .unwrap();

    // Verify it exists
    let release = storage.get_release("myapp", "default", 1).await.unwrap();
    assert!(release.is_some());

    // Delete it
    storage
        .delete_release("myapp", "default", 1)
        .await
        .unwrap();

    // Verify it's gone
    let release = storage.get_release("myapp", "default", 1).await.unwrap();
    assert!(release.is_none());
}

#[tokio::test]
async fn test_delete_all_revisions() {
    let storage = MockReleaseStorage::new();

    // Create multiple revisions
    for rev in 1..=5 {
        storage
            .save_release(&create_test_release(
                "myapp",
                "default",
                rev,
                ReleaseStatus::Deployed,
                5,
            ))
            .await
            .unwrap();
    }

    // Verify they exist
    let revisions = storage.list_revisions("myapp", "default").await.unwrap();
    assert_eq!(revisions.len(), 5);

    // Delete all
    let count = storage
        .delete_all_revisions("myapp", "default")
        .await
        .unwrap();
    assert_eq!(count, 5);

    // Verify they're gone
    let revisions = storage.list_revisions("myapp", "default").await.unwrap();
    assert_eq!(revisions.len(), 0);
}

#[tokio::test]
async fn test_delete_specific_revision_keeps_others() {
    let storage = MockReleaseStorage::new();

    // Create multiple revisions
    for rev in 1..=3 {
        storage
            .save_release(&create_test_release(
                "myapp",
                "default",
                rev,
                ReleaseStatus::Deployed,
                5,
            ))
            .await
            .unwrap();
    }

    // Delete only revision 2
    storage
        .delete_release("myapp", "default", 2)
        .await
        .unwrap();

    // Verify revisions 1 and 3 still exist
    let revisions = storage.list_revisions("myapp", "default").await.unwrap();
    assert_eq!(revisions, vec![1, 3]);
}

#[tokio::test]
async fn test_get_release_with_different_statuses() {
    let storage = MockReleaseStorage::new();

    // Create releases with different statuses
    let statuses = vec![
        ReleaseStatus::Rendered,
        ReleaseStatus::Deployed,
        ReleaseStatus::Failed,
        ReleaseStatus::Superseded,
    ];

    for (i, status) in statuses.iter().enumerate() {
        let mut release = create_test_release(
            &format!("app{}", i),
            "default",
            1,
            status.clone(),
            5,
        );
        if status == &ReleaseStatus::Failed {
            release.error = Some("Test error message".to_string());
        }
        storage.save_release(&release).await.unwrap();
    }

    // Verify each release has correct status
    for i in 0..statuses.len() {
        let release = storage
            .get_release(&format!("app{}", i), "default", 1)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(release.status, statuses[i]);

        if statuses[i] == ReleaseStatus::Failed {
            assert!(release.error.is_some());
        }

        if statuses[i] == ReleaseStatus::Deployed {
            assert!(release.applied_at.is_some());
        }
    }
}

#[tokio::test]
async fn test_list_revisions_returns_sorted() {
    let storage = MockReleaseStorage::new();

    // Save revisions out of order
    for rev in [3, 1, 5, 2, 4] {
        storage
            .save_release(&create_test_release(
                "myapp",
                "default",
                rev,
                ReleaseStatus::Deployed,
                5,
            ))
            .await
            .unwrap();
    }

    let revisions = storage.list_revisions("myapp", "default").await.unwrap();
    assert_eq!(revisions, vec![1, 2, 3, 4, 5]);
}

#[tokio::test]
async fn test_release_info_structure() {
    let storage = MockReleaseStorage::new();

    storage
        .save_release(&create_test_release(
            "myapp",
            "default",
            3,
            ReleaseStatus::Deployed,
            10,
        ))
        .await
        .unwrap();

    let releases = storage.list_releases(None).await.unwrap();
    assert_eq!(releases.len(), 1);

    let info = &releases[0];
    assert_eq!(info.release_name, "myapp");
    assert_eq!(info.release_namespace, "default");
    assert_eq!(info.latest_revision, 3);
    assert_eq!(info.status, ReleaseStatus::Deployed);
    assert_eq!(info.resource_count, 10);
    assert!(info.applied_at.is_some());
}

#[tokio::test]
async fn test_multiple_releases_different_namespaces() {
    let storage = MockReleaseStorage::new();

    // Create same release name in different namespaces
    storage
        .save_release(&create_test_release(
            "webapp",
            "dev",
            1,
            ReleaseStatus::Deployed,
            5,
        ))
        .await
        .unwrap();
    storage
        .save_release(&create_test_release(
            "webapp",
            "staging",
            1,
            ReleaseStatus::Deployed,
            5,
        ))
        .await
        .unwrap();
    storage
        .save_release(&create_test_release(
            "webapp",
            "prod",
            1,
            ReleaseStatus::Deployed,
            5,
        ))
        .await
        .unwrap();

    // List all
    let all = storage.list_releases(None).await.unwrap();
    assert_eq!(all.len(), 3);

    // Each namespace should show the release
    for ns in ["dev", "staging", "prod"] {
        let releases = storage.list_releases(Some(ns)).await.unwrap();
        assert_eq!(releases.len(), 1);
        assert_eq!(releases[0].release_name, "webapp");
        assert_eq!(releases[0].release_namespace, ns);
    }
}

#[tokio::test]
async fn test_delete_nonexistent_release_succeeds() {
    let storage = MockReleaseStorage::new();

    // Deleting non-existent release should not error
    let result = storage.delete_release("nonexistent", "default", 1).await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_delete_all_revisions_of_nonexistent_release() {
    let storage = MockReleaseStorage::new();

    let count = storage
        .delete_all_revisions("nonexistent", "default")
        .await
        .unwrap();
    assert_eq!(count, 0);
}
