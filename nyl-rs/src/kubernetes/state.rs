use async_trait::async_trait;
use chrono::{DateTime, Utc};
use k8s_openapi::api::core::v1::Secret;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use k8s_openapi::ByteString;
use kube::{api::ListParams, Api, Client};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::{resources::HelmChart, NylError, Result};

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
    /// Component name
    pub component: String,
    /// Revision number (starts at 1)
    pub revision: u32,
    /// Full rendered YAML manifest
    pub manifest: String,
    /// Input values used for rendering
    pub values: serde_json::Value,
    /// Original HelmChart resource (if applicable)
    pub helmchart: Option<HelmChart>,
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

    /// Get the latest release for a component
    async fn get_latest_release(&self, component: &str) -> Result<Option<ReleaseState>>;

    /// Get a specific release revision
    async fn get_release(&self, component: &str, revision: u32) -> Result<Option<ReleaseState>>;

    /// List all revision numbers for a component
    async fn list_revisions(&self, component: &str) -> Result<Vec<u32>>;

    /// Update the status of a release
    async fn update_release_status(
        &self,
        component: &str,
        revision: u32,
        status: ReleaseStatus,
        error: Option<String>,
    ) -> Result<()>;
}

/// Kubernetes-based release storage using Secrets
pub struct KubernetesReleaseStorage {
    client: Client,
    namespace: String,
}

impl KubernetesReleaseStorage {
    /// Create a new Kubernetes release storage
    pub fn new(client: Client, namespace: String) -> Self {
        Self { client, namespace }
    }

    /// Generate secret name for a release
    fn secret_name(component: &str, revision: u32) -> String {
        format!("nyl.release.v1.{}.{}", component, revision)
    }

    /// Parse revision number from secret name
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
        data.insert(
            "manifest".to_string(),
            Self::encode_base64(&release.manifest),
        );
        data.insert(
            "values".to_string(),
            Self::encode_base64(&serde_json::to_string(&release.values)?),
        );
        if let Some(helmchart) = &release.helmchart {
            data.insert(
                "helmchart".to_string(),
                Self::encode_base64(&serde_json::to_string(helmchart)?),
            );
        }
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
        labels.insert("nyl.io/component".to_string(), release.component.clone());
        labels.insert("nyl.io/revision".to_string(), release.revision.to_string());

        Ok(Secret {
            metadata: ObjectMeta {
                name: Some(Self::secret_name(&release.component, release.revision)),
                namespace: Some(self.namespace.clone()),
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

        let component = secret
            .metadata
            .labels
            .as_ref()
            .and_then(|l| l.get("nyl.io/component"))
            .ok_or_else(|| NylError::Config("Secret missing component label".to_string()))?
            .clone();

        let revision: u32 = secret
            .metadata
            .labels
            .as_ref()
            .and_then(|l| l.get("nyl.io/revision"))
            .and_then(|r| r.parse().ok())
            .ok_or_else(|| NylError::Config("Secret missing or invalid revision label".to_string()))?;

        let manifest = Self::decode_base64(
            data.get("manifest")
                .ok_or_else(|| NylError::Config("Secret missing manifest field".to_string()))?,
        )?;

        let values_str = Self::decode_base64(
            data.get("values")
                .ok_or_else(|| NylError::Config("Secret missing values field".to_string()))?,
        )?;
        let values: serde_json::Value = serde_json::from_str(&values_str)?;

        let helmchart = if let Some(helmchart_data) = data.get("helmchart") {
            let helmchart_str = Self::decode_base64(helmchart_data)?;
            Some(serde_json::from_str(&helmchart_str)?)
        } else {
            None
        };

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
            component,
            revision,
            manifest,
            values,
            helmchart,
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
        let api: Api<Secret> = Api::namespaced(self.client.clone(), &self.namespace);
        let secret = self.to_secret(release)?;
        let name = Self::secret_name(&release.component, release.revision);

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

    async fn get_latest_release(&self, component: &str) -> Result<Option<ReleaseState>> {
        let revisions = self.list_revisions(component).await?;
        if revisions.is_empty() {
            return Ok(None);
        }

        let latest_revision = revisions.iter().max().unwrap();
        self.get_release(component, *latest_revision).await
    }

    async fn get_release(&self, component: &str, revision: u32) -> Result<Option<ReleaseState>> {
        let api: Api<Secret> = Api::namespaced(self.client.clone(), &self.namespace);
        let name = Self::secret_name(component, revision);

        match api.get(&name).await {
            Ok(secret) => Ok(Some(self.from_secret(&secret)?)),
            Err(kube::Error::Api(err)) if err.code == 404 => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn list_revisions(&self, component: &str) -> Result<Vec<u32>> {
        let api: Api<Secret> = Api::namespaced(self.client.clone(), &self.namespace);
        let label_selector = format!("nyl.io/component={}", component);
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
        component: &str,
        revision: u32,
        status: ReleaseStatus,
        error: Option<String>,
    ) -> Result<()> {
        // Get existing release
        let mut release = self
            .get_release(component, revision)
            .await?
            .ok_or_else(|| {
                NylError::Config(format!(
                    "Release {} revision {} not found",
                    component, revision
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
            store.insert((release.component.clone(), release.revision), release.clone());
            Ok(())
        }

        async fn get_latest_release(&self, component: &str) -> Result<Option<ReleaseState>> {
            let revisions = self.list_revisions(component).await?;
            if revisions.is_empty() {
                return Ok(None);
            }

            let latest = revisions.iter().max().unwrap();
            self.get_release(component, *latest).await
        }

        async fn get_release(&self, component: &str, revision: u32) -> Result<Option<ReleaseState>> {
            let store = self.releases.lock().unwrap();
            Ok(store.get(&(component.to_string(), revision)).cloned())
        }

        async fn list_revisions(&self, component: &str) -> Result<Vec<u32>> {
            let store = self.releases.lock().unwrap();
            let mut revisions: Vec<u32> = store
                .keys()
                .filter(|(c, _)| c == component)
                .map(|(_, r)| *r)
                .collect();
            revisions.sort();
            Ok(revisions)
        }

        async fn update_release_status(
            &self,
            component: &str,
            revision: u32,
            status: ReleaseStatus,
            error: Option<String>,
        ) -> Result<()> {
            let mut store = self.releases.lock().unwrap();
            if let Some(release) = store.get_mut(&(component.to_string(), revision)) {
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
            component: "myapp".to_string(),
            revision: 1,
            manifest: "apiVersion: v1\nkind: ConfigMap".to_string(),
            values: serde_json::json!({"key": "value"}),
            helmchart: None,
            status: ReleaseStatus::Rendered,
            rendered_at: Utc::now(),
            applied_at: None,
            error: None,
        };

        storage.save_release(&release).await.unwrap();

        let retrieved = storage.get_release("myapp", 1).await.unwrap();
        assert!(retrieved.is_some());
        let retrieved = retrieved.unwrap();
        assert_eq!(retrieved.component, "myapp");
        assert_eq!(retrieved.revision, 1);
    }

    #[tokio::test]
    async fn test_get_latest_release() {
        let storage = MockReleaseStorage::new();

        // Save multiple revisions
        for i in 1..=3 {
            let release = ReleaseState {
                component: "myapp".to_string(),
                revision: i,
                manifest: format!("revision {}", i),
                values: serde_json::json!({}),
                helmchart: None,
                status: ReleaseStatus::Deployed,
                rendered_at: Utc::now(),
                applied_at: Some(Utc::now()),
                error: None,
            };
            storage.save_release(&release).await.unwrap();
        }

        let latest = storage.get_latest_release("myapp").await.unwrap();
        assert!(latest.is_some());
        assert_eq!(latest.unwrap().revision, 3);
    }

    #[tokio::test]
    async fn test_list_revisions() {
        let storage = MockReleaseStorage::new();

        // Save revisions out of order
        for i in [3, 1, 2] {
            let release = ReleaseState {
                component: "myapp".to_string(),
                revision: i,
                manifest: format!("revision {}", i),
                values: serde_json::json!({}),
                helmchart: None,
                status: ReleaseStatus::Deployed,
                rendered_at: Utc::now(),
                applied_at: Some(Utc::now()),
                error: None,
            };
            storage.save_release(&release).await.unwrap();
        }

        let revisions = storage.list_revisions("myapp").await.unwrap();
        assert_eq!(revisions, vec![1, 2, 3]);
    }

    #[tokio::test]
    async fn test_update_release_status() {
        let storage = MockReleaseStorage::new();
        let release = ReleaseState {
            component: "myapp".to_string(),
            revision: 1,
            manifest: "test".to_string(),
            values: serde_json::json!({}),
            helmchart: None,
            status: ReleaseStatus::Rendered,
            rendered_at: Utc::now(),
            applied_at: None,
            error: None,
        };

        storage.save_release(&release).await.unwrap();

        storage
            .update_release_status("myapp", 1, ReleaseStatus::Deployed, None)
            .await
            .unwrap();

        let updated = storage.get_release("myapp", 1).await.unwrap().unwrap();
        assert_eq!(updated.status, ReleaseStatus::Deployed);
        assert!(updated.applied_at.is_some());
    }

    #[tokio::test]
    async fn test_get_missing_release() {
        let storage = MockReleaseStorage::new();
        let result = storage.get_release("missing", 1).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_get_latest_no_releases() {
        let storage = MockReleaseStorage::new();
        let result = storage.get_latest_release("missing").await.unwrap();
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
