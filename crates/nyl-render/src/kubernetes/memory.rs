//! An in-memory [`ReleaseStorage`], for tests of everything that records
//! release revisions without a cluster: the lifecycle of this module's
//! neighbours, the commands, and the publication driver.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::Utc;

use super::{ReleaseInfo, ReleaseState, ReleaseStatus, ReleaseStorage};
use crate::{NylError, Result};

/// Release revisions held in memory, keyed by namespace, name, and revision.
/// Like the Kubernetes storage, saving an existing revision fails.
#[derive(Clone, Default)]
pub struct InMemoryReleaseStorage {
    releases: Arc<Mutex<HashMap<(String, u32), ReleaseState>>>,
}

impl InMemoryReleaseStorage {
    pub fn new() -> Self {
        Self::default()
    }
}

/// A recorded revision with an empty resource set, for tests.
pub fn sample_release(name: &str, namespace: &str, revision: u32, status: ReleaseStatus) -> ReleaseState {
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
        meta: super::ReleaseMeta::default(),
    }
}

#[async_trait]
impl ReleaseStorage for InMemoryReleaseStorage {
    async fn save_release(&self, release: &ReleaseState) -> Result<()> {
        let mut store = self.releases.lock().unwrap();
        let key = (
            format!("{}/{}", release.release_namespace, release.release_name),
            release.revision,
        );
        if store.contains_key(&key) {
            return Err(NylError::Kubernetes(format!(
                "revision {} already exists",
                release.revision
            )));
        }
        store.insert(key, release.clone());
        Ok(())
    }

    async fn complete_release(&self, release: &ReleaseState) -> Result<()> {
        let mut store = self.releases.lock().unwrap();
        let key = (
            format!("{}/{}", release.release_namespace, release.release_name),
            release.revision,
        );
        if !store.contains_key(&key) {
            return Err(NylError::Config(format!("revision {} not found", release.revision)));
        }
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

    async fn get_release(&self, release_name: &str, namespace: &str, revision: u32) -> Result<Option<ReleaseState>> {
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
