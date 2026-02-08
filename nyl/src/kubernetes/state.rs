use async_trait::async_trait;
use chrono::{DateTime, Utc};
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use k8s_openapi::api::core::v1::Secret;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use k8s_openapi::ByteString;
use kube::{api::ListParams, Api, Client};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{Read, Write};

use crate::{
    constants::{LABEL_RELEASE, LABEL_REVISION, SECRET_TYPE_RELEASE},
    kubernetes::ResourceKey,
    NylError, Result,
};

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
    async fn get_latest_release(&self, release_name: &str, namespace: &str) -> Result<Option<ReleaseState>>;

    /// Get a specific release revision
    async fn get_release(&self, release_name: &str, namespace: &str, revision: u32) -> Result<Option<ReleaseState>>;

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
        name.split('.').next_back()?.parse().ok()
    }

    /// Encode string to ByteString
    fn encode_base64(data: &str) -> ByteString {
        ByteString(data.as_bytes().to_vec())
    }

    /// Decode ByteString to string
    fn decode_base64(encoded: &ByteString) -> Result<String> {
        String::from_utf8(encoded.0.clone()).map_err(|e| NylError::Config(format!("Invalid UTF-8 in data: {}", e)))
    }

    /// Compress and encode string to ByteString (for large manifest field)
    fn compress_and_encode(data: &str) -> Result<ByteString> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::new(6));
        encoder
            .write_all(data.as_bytes())
            .map_err(|e| NylError::Config(format!("Compression failed: {}", e)))?;
        let compressed = encoder
            .finish()
            .map_err(|e| NylError::Config(format!("Compression finish failed: {}", e)))?;

        Ok(ByteString(compressed))
    }

    /// Decode and decompress ByteString to string
    fn decode_and_decompress(encoded: &ByteString) -> Result<String> {
        let mut decoder = GzDecoder::new(&encoded.0[..]);
        let mut decompressed = String::new();
        decoder.read_to_string(&mut decompressed).map_err(|e| {
            NylError::Config(format!(
                "Decompression failed: {}.\nHint: The release Secret may be corrupted.",
                e
            ))
        })?;

        tracing::debug!("Decompressed manifest: {} bytes", decompressed.len());
        Ok(decompressed)
    }

    /// Convert ReleaseState to Secret
    fn to_secret(release: &ReleaseState) -> Result<Secret> {
        let mut data: BTreeMap<String, ByteString> = BTreeMap::new();

        // Serialize resource keys
        data.insert(
            "resource_keys".to_string(),
            Self::encode_base64(&serde_json::to_string(&release.resource_keys)?),
        );

        let compressed_manifest = Self::compress_and_encode(&release.manifest)?;

        // Log compression ratio for visibility
        let original_size = release.manifest.len();
        let compressed_size = compressed_manifest.0.len();
        #[allow(clippy::cast_precision_loss)]
        let ratio = original_size as f64 / compressed_size as f64;
        tracing::debug!(
            "Compressed manifest: {} bytes → {} bytes ({:.1}x reduction)",
            original_size,
            compressed_size,
            ratio
        );

        data.insert("manifest".to_string(), compressed_manifest);

        data.insert(
            "status".to_string(),
            Self::encode_base64(&serde_json::to_string(&release.status)?),
        );
        data.insert(
            "rendered_at".to_string(),
            Self::encode_base64(&release.rendered_at.to_rfc3339()),
        );
        if let Some(applied_at) = &release.applied_at {
            data.insert("applied_at".to_string(), Self::encode_base64(&applied_at.to_rfc3339()));
        }
        if let Some(error) = &release.error {
            data.insert("error".to_string(), Self::encode_base64(error));
        }

        // Validate total Secret size doesn't exceed 1MB
        let total_size: usize = data.values().map(|v| v.0.len()).sum();
        if total_size > 1_000_000 {
            #[allow(clippy::cast_precision_loss)]
            let size_mb = total_size as f64 / 1_000_000.0;
            return Err(NylError::Kubernetes(format!(
                "Release Secret exceeds 1MB limit even after compression ({:.2}MB).\n\
                 Hint: Consider splitting your manifests into multiple releases or components.",
                size_mb
            )));
        }

        let mut labels = BTreeMap::new();
        labels.insert(LABEL_RELEASE.to_string(), release.release_name.clone());
        labels.insert(LABEL_REVISION.to_string(), release.revision.to_string());

        Ok(Secret {
            metadata: ObjectMeta {
                name: Some(Self::secret_name(&release.release_name, release.revision)),
                namespace: Some(release.release_namespace.clone()),
                labels: Some(labels),
                ..Default::default()
            },
            type_: Some(SECRET_TYPE_RELEASE.to_string()),
            data: Some(data),
            ..Default::default()
        })
    }

    /// Convert Secret to ReleaseState
    fn from_secret(secret: &Secret) -> Result<ReleaseState> {
        let data = secret
            .data
            .as_ref()
            .ok_or_else(|| NylError::Config("Secret missing data field".to_string()))?;

        let release_name = secret
            .metadata
            .labels
            .as_ref()
            .and_then(|l| l.get(LABEL_RELEASE))
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
            .and_then(|l| l.get(LABEL_REVISION))
            .and_then(|r| r.parse().ok())
            .ok_or_else(|| NylError::Config("Secret missing or invalid revision label".to_string()))?;

        // Deserialize resource keys
        let resource_keys_str = Self::decode_base64(
            data.get("resource_keys")
                .ok_or_else(|| NylError::Config("Secret missing resource_keys field".to_string()))?,
        )?;
        let resource_keys: Vec<ResourceKey> = serde_json::from_str(&resource_keys_str)?;

        let manifest = Self::decode_and_decompress(
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
        let secret = Self::to_secret(release)?;
        let name = Self::secret_name(&release.release_name, release.revision);

        // Try to get existing secret
        match api.get(&name).await {
            Ok(_) => {
                // Update existing secret
                api.replace(&name, &kube::api::PostParams::default(), &secret).await?;
            }
            Err(kube::Error::Api(err)) if err.code == 404 => {
                // Create new secret
                api.create(&kube::api::PostParams::default(), &secret).await?;
            }
            Err(e) => return Err(e.into()),
        }

        Ok(())
    }

    async fn get_latest_release(&self, release_name: &str, namespace: &str) -> Result<Option<ReleaseState>> {
        let revisions = self.list_revisions(release_name, namespace).await?;
        if revisions.is_empty() {
            return Ok(None);
        }

        let latest_revision = revisions.iter().max().unwrap();
        self.get_release(release_name, namespace, *latest_revision).await
    }

    async fn get_release(&self, release_name: &str, namespace: &str, revision: u32) -> Result<Option<ReleaseState>> {
        let api: Api<Secret> = Api::namespaced(self.client.clone(), namespace);
        let name = Self::secret_name(release_name, revision);

        match api.get(&name).await {
            Ok(secret) => Ok(Some(Self::from_secret(&secret)?)),
            Err(kube::Error::Api(err)) if err.code == 404 => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn list_revisions(&self, release_name: &str, namespace: &str) -> Result<Vec<u32>> {
        let api: Api<Secret> = Api::namespaced(self.client.clone(), namespace);
        let label_selector = format!("{}={}", LABEL_RELEASE, release_name);
        let lp = ListParams::default().labels(&label_selector);

        let secrets = api.list(&lp).await?;
        let mut revisions: Vec<u32> = secrets
            .items
            .iter()
            .filter_map(|s| {
                s.metadata
                    .labels
                    .as_ref()
                    .and_then(|l| l.get(LABEL_REVISION))
                    .and_then(|r| r.parse().ok())
            })
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
        // Get existing release
        let mut release = self
            .get_release(release_name, namespace, revision)
            .await?
            .ok_or_else(|| NylError::Config(format!("Release {} revision {} not found", release_name, revision)))?;

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

        async fn get_latest_release(&self, release_name: &str, namespace: &str) -> Result<Option<ReleaseState>> {
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

        let updated = storage.get_release("myapp", "default", 1).await.unwrap().unwrap();
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
        let result = storage.get_latest_release("missing", "default").await.unwrap();
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

    #[test]
    fn test_compression_roundtrip() {
        let original = "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: test\n".repeat(100);
        let compressed = KubernetesReleaseStorage::compress_and_encode(&original).unwrap();
        let decompressed = KubernetesReleaseStorage::decode_and_decompress(&compressed).unwrap();
        assert_eq!(original, decompressed);
    }

    #[test]
    fn test_compression_reduces_size() {
        let large_manifest =
            "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: test\ndata:\n  key: value\n".repeat(1000);
        let original_size = large_manifest.len();
        let compressed = KubernetesReleaseStorage::compress_and_encode(&large_manifest).unwrap();
        let compressed_size = compressed.0.len();

        // Expect at least 5x compression for repetitive YAML
        assert!(compressed_size < original_size / 5);
    }

    #[test]
    fn test_unicode_compression_roundtrip() {
        let unicode_data = "Hello 世界 🚀 café\n".repeat(50);
        let compressed = KubernetesReleaseStorage::compress_and_encode(&unicode_data).unwrap();
        let decompressed = KubernetesReleaseStorage::decode_and_decompress(&compressed).unwrap();
        assert_eq!(unicode_data, decompressed);
    }

    #[test]
    fn test_gzip_magic_header_detection() {
        let data = "test data for compression";
        let compressed = KubernetesReleaseStorage::compress_and_encode(data).unwrap();

        // Verify gzip magic header is present
        assert_eq!(compressed.0[0], 0x1f);
        assert_eq!(compressed.0[1], 0x8b);
    }

    #[test]
    fn test_corrupted_compressed_data_error() {
        // Create corrupted data with gzip header but invalid payload
        let mut corrupted = vec![0x1f, 0x8b, 0x08, 0x00];
        corrupted.extend_from_slice(&[0xFF; 100]);
        let corrupted_bytes = ByteString(corrupted);

        let result = KubernetesReleaseStorage::decode_and_decompress(&corrupted_bytes);
        assert!(result.is_err());
        // Assert on the stable wrapper error message rather than backend-specific wording
        assert!(result.unwrap_err().to_string().contains("Decompression failed"));
    }

    #[test]
    fn test_empty_manifest_compression() {
        let empty = "";
        let compressed = KubernetesReleaseStorage::compress_and_encode(empty).unwrap();
        let decompressed = KubernetesReleaseStorage::decode_and_decompress(&compressed).unwrap();
        assert_eq!(empty, decompressed);
    }

    #[tokio::test]
    async fn test_release_state_compression_roundtrip() {
        let large_manifest = "apiVersion: v1\nkind: ConfigMap\ndata:\n  key: value\n".repeat(5000);
        let release = ReleaseState {
            release_name: "test-release".to_string(),
            release_namespace: "default".to_string(),
            revision: 1,
            resource_keys: vec![],
            manifest: large_manifest.clone(),
            status: ReleaseStatus::Rendered,
            rendered_at: Utc::now(),
            applied_at: None,
            error: None,
        };

        let secret = KubernetesReleaseStorage::to_secret(&release).unwrap();
        let restored = KubernetesReleaseStorage::from_secret(&secret).unwrap();

        assert_eq!(release.manifest, restored.manifest);
        assert_eq!(release.release_name, restored.release_name);
    }
}
