use async_trait::async_trait;
use chrono::{DateTime, Utc};
use k8s_openapi::api::core::v1::Secret;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use k8s_openapi::ByteString;
use kube::{api::ListParams, Api, Client};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::{kubernetes::ResourceKey, NylError, Result};

/// Status of a release
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ReleaseStatus {
    /// Generated but not applied
    Rendered,
    /// Successfully applied
    Deployed,
    /// Apply failed
    Failed,
    /// Newer revision deployed
    Superseded,
}

/// State of a release revision
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseState {
    /// Release name
    pub release_name: String,
    /// Release namespace (target namespace for resources)
    pub release_namespace: String,
    /// Revision number (starts at 1)
    pub revision: u32,
    /// Resource keys for tracking applied resources
    pub resource_keys: Vec<ResourceKey>,
    /// Full rendered YAML manifest (for rollback)
    pub manifest: String,
    /// Current status
    pub status: ReleaseStatus,
    /// When the manifest was rendered
    pub rendered_at: DateTime<Utc>,
    /// When the release was applied (if applicable)
    pub applied_at: Option<DateTime<Utc>>,
    /// Error message (if status is Failed)
    pub error: Option<String>,
}

/// Trait for storing and retrieving release state
#[async_trait]
pub trait ReleaseStorage: Send + Sync {
    /// Save a release state
    async fn save_release(&self, release: &ReleaseState) -> Result<()>;

    /// Get the latest release for a release name and namespace
    async fn get_latest_release(
        &self,
        release_name: &str,
        namespace: &str,
    ) -> Result<Option<ReleaseState>>;

    /// Get a specific release revision
    async fn get_release(
        &self,
        release_name: &str,
        namespace: &str,
        revision: u32,
    ) -> Result<Option<ReleaseState>>;

    /// List all revision numbers for a release
    async fn list_revisions(&self, release_name: &str, namespace: &str) -> Result<Vec<u32>>;

    /// Update the status of a release
    async fn update_release_status(
        &self,
        release_name: &str,
        namespace: &str,
        revision: u32,
        status: ReleaseStatus,
        error: Option<String>,
    ) -> Result<()>;
}

/// Kubernetes-based release storage using Secrets
pub struct KubernetesReleaseStorage {
    client: Client,
}

impl KubernetesReleaseStorage {
    /// Create a new Kubernetes release storage
    pub fn new(client: Client) -> Self {
        Self { client }
    }

    /// Generate secret name for a release
    fn secret_name(release_name: &str, revision: u32) -> String {
        format!("nyl.release.v1.{}.{}", release_name, revision)
    }

    /// Parse revision number from secret name
    #[allow(dead_code)]
    fn parse_revision(name: &str) -> Option<u32> {
        // Format: nyl.release.v1.<component>.<revision>
        name.split('.').last()?.parse().ok()
    }

    /// Encode string to ByteString
    fn encode_base64(data: &str) -> ByteString {
        ByteString(data.as_bytes().to_vec())
    }

    /// Decode ByteString to string
    fn decode_base64(encoded: &ByteString) -> Result<String> {
        String::from_utf8(encoded.0.clone())
            .map_err(|e| NylError::Config(format!("Invalid UTF-8 in data: {}", e)))
    }

    /// Convert ReleaseState to Secret
    fn to_secret(&self, release: &ReleaseState) -> Result<Secret> {
        let mut data: BTreeMap<String, ByteString> = BTreeMap::new();

        // Serialize resource keys
        data.insert(
            "resource_keys".to_string(),
            Self::encode_base64(&serde_json::to_string(&release.resource_keys)?),
        );

        data.insert(
            "manifest".to_string(),
            Self::encode_base64(&release.manifest),
        );

        data.insert(
            "status".to_string(),
            Self::encode_base64(&serde_json::to_string(&release.status)?),
        );
        data.insert(
            "rendered_at".to_string(),
            Self::encode_base64(&release.rendered_at.to_rfc3339()),
        );
        if let Some(applied_at) = &release.applied_at {
            data.insert(
                "applied_at".to_string(),
                Self::encode_base64(&applied_at.to_rfc3339()),
            );
        }
        if let Some(error) = &release.error {
            data.insert("error".to_string(), Self::encode_base64(error));
        }

        let mut labels = BTreeMap::new();
        labels.insert("nyl.io/release".to_string(), release.release_name.clone());
        labels.insert("nyl.io/revision".to_string(), release.revision.to_string());

        Ok(Secret {
            metadata: ObjectMeta {
                name: Some(Self::secret_name(&release.release_name, release.revision)),
                namespace: Some(release.release_namespace.clone()),
                labels: Some(labels),
                ..Default::default()
            },
            type_: Some("nyl.io/release.v1".to_string()),
            data: Some(data),
            ..Default::default()
        })
    }

    /// Convert Secret to ReleaseState
    fn from_secret(&self, secret: &Secret) -> Result<ReleaseState> {
        let data = secret
            .data
            .as_ref()
            .ok_or_else(|| NylError::Config("Secret missing data field".to_string()))?;

        let release_name = secret
            .metadata
            .labels
            .as_ref()
            .and_then(|l| l.get("nyl.io/release"))
            .ok_or_else(|| NylError::Config("Secret missing release label".to_string()))?
            .clone();

        let release_namespace = secret
            .metadata
            .namespace
            .as_ref()
            .ok_or_else(|| NylError::Config("Secret missing namespace".to_string()))?
            .clone();

        let revision: u32 = secret
            .metadata
            .labels
            .as_ref()
            .and_then(|l| l.get("nyl.io/revision"))
            .and_then(|r| r.parse().ok())
            .ok_or_else(|| NylError::Config("Secret missing or invalid revision label".to_string()))?;

        // Deserialize resource keys
        let resource_keys_str = Self::decode_base64(
            data.get("resource_keys")
                .ok_or_else(|| NylError::Config("Secret missing resource_keys field".to_string()))?,
        )?;
        let resource_keys: Vec<ResourceKey> = serde_json::from_str(&resource_keys_str)?;

        let manifest = Self::decode_base64(
            data.get("manifest")
                .ok_or_else(|| NylError::Config("Secret missing manifest field".to_string()))?,
        )?;

        let status_str = Self::decode_base64(
            data.get("status")
                .ok_or_else(|| NylError::Config("Secret missing status field".to_string()))?,
        )?;
        let status: ReleaseStatus = serde_json::from_str(&status_str)?;

        let rendered_at_str = Self::decode_base64(
            data.get("rendered_at")
                .ok_or_else(|| NylError::Config("Secret missing rendered_at field".to_string()))?,
        )?;
        let rendered_at = DateTime::parse_from_rfc3339(&rendered_at_str)
            .map_err(|e| NylError::Config(format!("Invalid rendered_at timestamp: {}", e)))?
            .with_timezone(&Utc);

        let applied_at = if let Some(applied_at_data) = data.get("applied_at") {
            let applied_at_str = Self::decode_base64(applied_at_data)?;
            Some(
                DateTime::parse_from_rfc3339(&applied_at_str)
                    .map_err(|e| NylError::Config(format!("Invalid applied_at timestamp: {}", e)))?
                    .with_timezone(&Utc),
            )
        } else {
            None
        };

        let error = if let Some(error_data) = data.get("error") {
            Some(Self::decode_base64(error_data)?)
        } else {
            None
        };

        Ok(ReleaseState {
            release_name,
            release_namespace,
            revision,
            resource_keys,
            manifest,
            status,
            rendered_at,
            applied_at,
            error,
        })
    }
}

#[async_trait]
impl ReleaseStorage for KubernetesReleaseStorage {
    async fn save_release(&self, release: &ReleaseState) -> Result<()> {
        let api: Api<Secret> = Api::namespaced(self.client.clone(), &release.release_namespace);
        let secret = self.to_secret(release)?;
        let name = Self::secret_name(&release.release_name, release.revision);

        // Try to get existing secret
        match api.get(&name).await {
            Ok(_) => {
                // Update existing secret
                api.replace(&name, &Default::default(), &secret).await?;
            }
            Err(kube::Error::Api(err)) if err.code == 404 => {
                // Create new secret
                api.create(&Default::default(), &secret).await?;
            }
            Err(e) => return Err(e.into()),
        }

        Ok(())
    }

    async fn get_latest_release(
        &self,
        release_name: &str,
        namespace: &str,
    ) -> Result<Option<ReleaseState>> {
        let revisions = self.list_revisions(release_name, namespace).await?;
        if revisions.is_empty() {
            return Ok(None);
        }

        let latest_revision = revisions.iter().max().unwrap();
        self.get_release(release_name, namespace, *latest_revision)
            .await
    }

    async fn get_release(
        &self,
        release_name: &str,
        namespace: &str,
        revision: u32,
    ) -> Result<Option<ReleaseState>> {
        let api: Api<Secret> = Api::namespaced(self.client.clone(), namespace);
        let name = Self::secret_name(release_name, revision);

        match api.get(&name).await {
            Ok(secret) => Ok(Some(self.from_secret(&secret)?)),
            Err(kube::Error::Api(err)) if err.code == 404 => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn list_revisions(&self, release_name: &str, namespace: &str) -> Result<Vec<u32>> {
        let api: Api<Secret> = Api::namespaced(self.client.clone(), namespace);
        let label_selector = format!("nyl.io/release={}", release_name);
        let lp = ListParams::default().labels(&label_selector);

        let secrets = api.list(&lp).await?;
        let mut revisions: Vec<u32> = secrets
            .items
            .iter()
            .filter_map(|s| {
                s.metadata
                    .labels
                    .as_ref()
                    .and_then(|l| l.get("nyl.io/revision"))
                    .and_then(|r| r.parse().ok())
            })
            .collect();

        revisions.sort();
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
        // Get existing release
        let mut release = self
            .get_release(release_name, namespace, revision)
            .await?
            .ok_or_else(|| {
                NylError::Config(format!(
                    "Release {} revision {} not found",
                    release_name, revision
                ))
            })?;

        // Update status
        release.status = status;
        release.error = error;

        // If status is Deployed, set applied_at
        if release.status == ReleaseStatus::Deployed && release.applied_at.is_none() {
            release.applied_at = Some(Utc::now());
        }

        // Save updated release
        self.save_release(&release).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    /// Mock release storage for testing
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

    #[async_trait]
    impl ReleaseStorage for MockReleaseStorage {
        async fn save_release(&self, release: &ReleaseState) -> Result<()> {
            let mut store = self.releases.lock().unwrap();
            // Use compound key for storage
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
        ) -> Result<Option<ReleaseState>> {
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
        ) -> Result<Option<ReleaseState>> {
            let store = self.releases.lock().unwrap();
            let key = format!("{}/{}", namespace, release_name);
            Ok(store.get(&(key, revision)).cloned())
        }

        async fn list_revisions(&self, release_name: &str, namespace: &str) -> Result<Vec<u32>> {
            let store = self.releases.lock().unwrap();
            let key_prefix = format!("{}/{}", namespace, release_name);
            let mut revisions: Vec<u32> = store
                .keys()
                .filter(|(c, _)| c == &key_prefix)
                .map(|(_, r)| *r)
                .collect();
            revisions.sort();
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
    }

    #[tokio::test]
    async fn test_save_and_get_release() {
        let storage = MockReleaseStorage::new();
        let release = ReleaseState {
            release_name: "myapp".to_string(),
            release_namespace: "default".to_string(),
            revision: 1,
            resource_keys: vec![],
            manifest: "apiVersion: v1\nkind: ConfigMap".to_string(),
            status: ReleaseStatus::Rendered,
            rendered_at: Utc::now(),
            applied_at: None,
            error: None,
        };

        storage.save_release(&release).await.unwrap();

        let retrieved = storage.get_release("myapp", "default", 1).await.unwrap();
        assert!(retrieved.is_some());
        let retrieved = retrieved.unwrap();
        assert_eq!(retrieved.release_name, "myapp");
        assert_eq!(retrieved.release_namespace, "default");
        assert_eq!(retrieved.revision, 1);
    }

    #[tokio::test]
    async fn test_get_latest_release() {
        let storage = MockReleaseStorage::new();

        // Save multiple revisions
        for i in 1..=3 {
            let release = ReleaseState {
                release_name: "myapp".to_string(),
                release_namespace: "default".to_string(),
                revision: i,
                resource_keys: vec![],
                manifest: format!("revision {}", i),
                status: ReleaseStatus::Deployed,
                rendered_at: Utc::now(),
                applied_at: Some(Utc::now()),
                error: None,
            };
            storage.save_release(&release).await.unwrap();
        }

        let latest = storage.get_latest_release("myapp", "default").await.unwrap();
        assert!(latest.is_some());
        assert_eq!(latest.unwrap().revision, 3);
    }

    #[tokio::test]
    async fn test_list_revisions() {
        let storage = MockReleaseStorage::new();

        // Save revisions out of order
        for i in [3, 1, 2] {
            let release = ReleaseState {
                release_name: "myapp".to_string(),
                release_namespace: "default".to_string(),
                revision: i,
                resource_keys: vec![],
                manifest: format!("revision {}", i),
                status: ReleaseStatus::Deployed,
                rendered_at: Utc::now(),
                applied_at: Some(Utc::now()),
                error: None,
            };
            storage.save_release(&release).await.unwrap();
        }

        let revisions = storage.list_revisions("myapp", "default").await.unwrap();
        assert_eq!(revisions, vec![1, 2, 3]);
    }

    #[tokio::test]
    async fn test_update_release_status() {
        let storage = MockReleaseStorage::new();
        let release = ReleaseState {
            release_name: "myapp".to_string(),
            release_namespace: "default".to_string(),
            revision: 1,
            resource_keys: vec![],
            manifest: "test".to_string(),
            status: ReleaseStatus::Rendered,
            rendered_at: Utc::now(),
            applied_at: None,
            error: None,
        };

        storage.save_release(&release).await.unwrap();

        storage
            .update_release_status("myapp", "default", 1, ReleaseStatus::Deployed, None)
            .await
            .unwrap();

        let updated = storage
            .get_release("myapp", "default", 1)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated.status, ReleaseStatus::Deployed);
        assert!(updated.applied_at.is_some());
    }

    #[tokio::test]
    async fn test_get_missing_release() {
        let storage = MockReleaseStorage::new();
        let result = storage.get_release("missing", "default", 1).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_get_latest_no_releases() {
        let storage = MockReleaseStorage::new();
        let result = storage
            .get_latest_release("missing", "default")
            .await
            .unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_secret_name_generation() {
        assert_eq!(
            KubernetesReleaseStorage::secret_name("myapp", 1),
            "nyl.release.v1.myapp.1"
        );
        assert_eq!(
            KubernetesReleaseStorage::secret_name("my-component", 42),
            "nyl.release.v1.my-component.42"
        );
    }

    #[test]
    fn test_parse_revision() {
        assert_eq!(
            KubernetesReleaseStorage::parse_revision("nyl.release.v1.myapp.1"),
            Some(1)
        );
        assert_eq!(
            KubernetesReleaseStorage::parse_revision("nyl.release.v1.myapp.42"),
            Some(42)
        );
        assert_eq!(KubernetesReleaseStorage::parse_revision("invalid"), None);
    }

    #[test]
    fn test_bytestring_roundtrip() {
        let original = "test data with special chars: 你好";
        let encoded = KubernetesReleaseStorage::encode_base64(original);
        let decoded = KubernetesReleaseStorage::decode_base64(&encoded).unwrap();
        assert_eq!(original, decoded);
    }
}
