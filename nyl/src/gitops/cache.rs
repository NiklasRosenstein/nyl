//! Versioned, content-addressed storage for rendered GitOps operations.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use clap::Args;
use getrandom::fill as fill_random;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{NylError, Result};

const CACHE_LAYOUT_VERSION: &str = "v1";
const SECRET_KEY_FILE: &str = "secret-fingerprint-key";

/// Cache controls shared by all rendered-tree commands.
#[derive(Args, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TreeCacheArgs {
    /// Retrieve external inputs again and rebuild cached render artifacts.
    #[arg(long, conflicts_with = "no_cache")]
    pub refresh: bool,

    /// Use no persistent cache reads or writes.
    #[arg(long)]
    pub no_cache: bool,
}

impl TreeCacheArgs {
    pub fn mode(self) -> CacheMode {
        if self.no_cache {
            CacheMode::Disabled
        } else if self.refresh {
            CacheMode::Refresh
        } else {
            CacheMode::Default
        }
    }
}

/// Persistent cache behavior for one command invocation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CacheMode {
    #[default]
    Default,
    Refresh,
    Disabled,
}

impl CacheMode {
    pub fn reads(self) -> bool {
        self == Self::Default
    }

    pub fn writes(self) -> bool {
        self != Self::Disabled
    }
}

/// One observed renderer dependency. Only a digest is persisted.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RecordedDependency {
    pub kind: String,
    pub digest: String,
}

/// Dependencies and the artifact produced from them by one successful action.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DependencyRecord {
    pub version: u32,
    pub action: String,
    pub dependencies: BTreeMap<String, RecordedDependency>,
    #[serde(rename = "artifactDigest")]
    pub artifact_digest: String,
}

/// Collects every input that can affect a cached renderer action.
#[derive(Clone, Debug)]
pub struct DependencyRecorder {
    dependencies: BTreeMap<String, RecordedDependency>,
    secret_key: [u8; 32],
    cacheable: bool,
}

impl DependencyRecorder {
    fn new(secret_key: [u8; 32]) -> Self {
        Self {
            dependencies: BTreeMap::new(),
            secret_key,
            cacheable: true,
        }
    }

    pub fn record_bytes(&mut self, name: impl Into<String>, kind: impl Into<String>, bytes: &[u8]) {
        self.dependencies.insert(
            name.into(),
            RecordedDependency {
                kind: kind.into(),
                digest: sha256(bytes),
            },
        );
    }

    pub fn record_value<T: Serialize>(&mut self, name: impl Into<String>, value: &T) -> Result<()> {
        self.record_bytes(name, "value", &serde_json::to_vec(value)?);
        Ok(())
    }

    pub fn record_file(&mut self, name: impl Into<String>, path: &Path) -> Result<()> {
        self.record_bytes(name, "file", &fs::read(path)?);
        Ok(())
    }

    /// Records a sensitive value without persisting it or a guessable plain digest.
    pub fn record_secret(&mut self, name: impl Into<String>, value: &[u8]) {
        self.dependencies.insert(
            name.into(),
            RecordedDependency {
                kind: "secret".to_string(),
                digest: hmac_sha256(&self.secret_key, value),
            },
        );
    }

    /// Prevents lookup or publication when a renderer dependency cannot be observed completely.
    pub fn mark_uncacheable(&mut self) {
        self.cacheable = false;
    }

    pub fn is_cacheable(&self) -> bool {
        self.cacheable
    }

    pub fn finish(self, action: impl Into<String>, artifact_digest: String) -> DependencyRecord {
        DependencyRecord {
            version: 1,
            action: action.into(),
            dependencies: self.dependencies,
            artifact_digest,
        }
    }
}

/// Cache storage scoped to the stable GitOps cache layout.
#[derive(Debug)]
pub struct GitOpsCache {
    root: PathBuf,
    mode: CacheMode,
    secret_key: [u8; 32],
}

impl GitOpsCache {
    pub fn new(project_root: &Path, mode: CacheMode) -> Result<Self> {
        let cache_root =
            std::env::var_os("NYL_CACHE_DIR").map_or_else(|| project_root.join(".nyl/cache"), PathBuf::from);
        Self::with_root(cache_root, mode)
    }

    pub fn with_root(cache_root: impl Into<PathBuf>, mode: CacheMode) -> Result<Self> {
        let root = cache_root.into().join("gitops").join(CACHE_LAYOUT_VERSION);
        let secret_key = if mode == CacheMode::Disabled {
            random_key()?
        } else {
            load_or_create_secret_key(&root)?
        };
        Ok(Self { root, mode, secret_key })
    }

    pub fn mode(&self) -> CacheMode {
        self.mode
    }

    pub fn recorder(&self) -> DependencyRecorder {
        DependencyRecorder::new(self.secret_key)
    }

    pub fn load_artifact<T: DeserializeOwned>(&self, kind: &str, digest: &str) -> Result<Option<T>> {
        if !self.mode.reads() {
            return Ok(None);
        }
        let path = self.artifact_path(kind, digest)?;
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if sha256(&bytes) != digest {
            tracing::warn!(path = %path.display(), "Ignoring corrupt GitOps cache artifact");
            return Ok(None);
        }
        match serde_json::from_slice(&bytes) {
            Ok(value) => Ok(Some(value)),
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "Ignoring invalid GitOps cache artifact");
                Ok(None)
            }
        }
    }

    pub fn store_artifact<T: Serialize>(&self, kind: &str, value: &T) -> Result<Option<String>> {
        if !self.mode.writes() {
            return Ok(None);
        }
        let bytes = serde_json::to_vec(value)?;
        let digest = sha256(&bytes);
        let path = self.artifact_path(kind, &digest)?;
        atomic_write_once(&path, &bytes)?;
        Ok(Some(digest))
    }

    pub fn load_record(&self, action: &str, key: &str) -> Result<Option<DependencyRecord>> {
        if !self.mode.reads() {
            return Ok(None);
        }
        let path = self.record_path(action, key)?;
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        match serde_json::from_slice(&bytes) {
            Ok(record) => Ok(Some(record)),
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "Ignoring invalid GitOps cache dependency record");
                Ok(None)
            }
        }
    }

    pub fn store_record(&self, action: &str, key: &str, record: &DependencyRecord) -> Result<()> {
        if !self.mode.writes() {
            return Ok(());
        }
        let path = self.record_path(action, key)?;
        atomic_replace(&path, &serde_json::to_vec(record)?)
    }

    fn artifact_path(&self, kind: &str, digest: &str) -> Result<PathBuf> {
        validate_segment("artifact kind", kind)?;
        validate_digest(digest)?;
        Ok(self
            .root
            .join("artifacts")
            .join(kind)
            .join(&digest[..2])
            .join(format!("{digest}.json")))
    }

    fn record_path(&self, action: &str, key: &str) -> Result<PathBuf> {
        validate_segment("cache action", action)?;
        Ok(self
            .root
            .join("records")
            .join(action)
            .join(format!("{}.json", sha256(key.as_bytes()))))
    }
}

fn validate_segment(label: &str, value: &str) -> Result<()> {
    if value.is_empty()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(NylError::config(format!("Invalid {label} {value:?}")));
    }
    Ok(())
}

fn validate_digest(value: &str) -> Result<()> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(NylError::config(format!("Invalid cache artifact digest {value:?}")));
    }
    Ok(())
}

fn load_or_create_secret_key(root: &Path) -> Result<[u8; 32]> {
    let path = root.join(SECRET_KEY_FILE);
    match fs::read(&path) {
        Ok(bytes) => {
            return bytes
                .try_into()
                .map_err(|_| NylError::config(format!("Invalid cache key {}", path.display())))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let key = random_key()?;
    atomic_write_once(&path, &key)?;
    let bytes = fs::read(&path)?;
    bytes
        .try_into()
        .map_err(|_| NylError::config(format!("Invalid cache key {}", path.display())))
}

fn random_key() -> Result<[u8; 32]> {
    let mut key = [0_u8; 32];
    fill_random(&mut key).map_err(|error| NylError::config(format!("Failed to generate cache key: {error}")))?;
    Ok(key)
}

fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn hmac_sha256(key: &[u8; 32], value: &[u8]) -> String {
    const BLOCK_SIZE: usize = 64;
    let mut inner_key = [0x36_u8; BLOCK_SIZE];
    let mut outer_key = [0x5c_u8; BLOCK_SIZE];
    for (index, byte) in key.iter().enumerate() {
        inner_key[index] ^= byte;
        outer_key[index] ^= byte;
    }
    let mut inner = Sha256::new();
    inner.update(inner_key);
    inner.update(value);
    let mut outer = Sha256::new();
    outer.update(outer_key);
    outer.update(inner.finalize());
    hex::encode(outer.finalize())
}

fn atomic_write_once(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().expect("cache file has parent");
    create_private_dir_all(parent)?;
    if path.is_file() {
        if fs::read(path)? == bytes {
            return Ok(());
        }
        return atomic_replace(path, bytes);
    }
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    std::io::Write::write_all(&mut temporary, bytes)?;
    set_private_file_permissions(temporary.path())?;
    match temporary.persist_noclobber(path) {
        Ok(_) => Ok(()),
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error.error.into()),
    }
}

fn atomic_replace(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().expect("cache file has parent");
    create_private_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    std::io::Write::write_all(&mut temporary, bytes)?;
    set_private_file_permissions(temporary.path())?;
    temporary.persist(path).map(|_| ()).map_err(|error| error.error.into())
}

fn create_private_dir_all(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn set_private_file_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artifact_corruption_is_a_safe_cache_miss() {
        let temp = tempfile::TempDir::new().unwrap();
        let cache = GitOpsCache::with_root(temp.path(), CacheMode::Default).unwrap();
        let digest = cache.store_artifact("target", &vec!["manifest"]).unwrap().unwrap();
        fs::write(cache.artifact_path("target", &digest).unwrap(), b"corrupt").unwrap();

        assert_eq!(cache.load_artifact::<Vec<String>>("target", &digest).unwrap(), None);
    }

    #[test]
    fn disabled_cache_neither_reads_nor_writes() {
        let temp = tempfile::TempDir::new().unwrap();
        let persistent = GitOpsCache::with_root(temp.path(), CacheMode::Default).unwrap();
        let digest = persistent.store_artifact("target", &vec!["manifest"]).unwrap().unwrap();
        let disabled = GitOpsCache::with_root(temp.path(), CacheMode::Disabled).unwrap();

        assert_eq!(disabled.load_artifact::<Vec<String>>("target", &digest).unwrap(), None);
        assert_eq!(disabled.store_artifact("target", &vec!["other"]).unwrap(), None);
    }

    #[test]
    fn secret_dependencies_use_a_cache_local_key() {
        let first_root = tempfile::TempDir::new().unwrap();
        let second_root = tempfile::TempDir::new().unwrap();
        let first = GitOpsCache::with_root(first_root.path(), CacheMode::Default).unwrap();
        let second = GitOpsCache::with_root(second_root.path(), CacheMode::Default).unwrap();
        let mut first_recording = first.recorder();
        let mut second_recording = second.recorder();
        first_recording.record_secret("token", b"shared secret");
        second_recording.record_secret("token", b"shared secret");

        assert_ne!(
            first_recording.dependencies["token"].digest,
            second_recording.dependencies["token"].digest
        );
    }
}
