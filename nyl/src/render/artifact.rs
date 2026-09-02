//! Shared exact-source cache and authoritative vendor resolution.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write as _;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::cache::{CacheMode, RenderCache, SourceOperation};
use crate::config::{ProjectConfig, VendorMode};
use crate::{NylError, Result};

const SOURCE_CACHE_VERSION: &str = "v1";
const VENDOR_LOCK_VERSION: u32 = 1;

/// The complete user-controlled selector for one remote renderer input.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ArtifactRequest {
    HelmChart {
        repository: String,
        name: String,
        version: String,
    },
    RemoteManifest {
        url: String,
    },
    GitSource {
        repository: String,
        revision: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        commit: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        subpath: Option<String>,
    },
}

impl ArtifactRequest {
    pub fn fingerprint(&self) -> Result<String> {
        Ok(sha256(&serde_json::to_vec(self)?))
    }

    pub fn display(&self) -> String {
        match self {
            Self::HelmChart {
                repository,
                name,
                version,
            } => format!("{}#{}@{}", crate::util::sanitize_url(repository), name, version),
            Self::RemoteManifest { url } => crate::util::sanitize_url(url),
            Self::GitSource {
                repository,
                revision,
                commit,
                subpath,
            } => {
                let mut value = format!("{}@{}", crate::util::sanitize_url(repository), revision);
                if let Some(commit) = commit {
                    value.push_str(" (commit ");
                    value.push_str(commit);
                    value.push(')');
                }
                if let Some(subpath) = subpath {
                    value.push('#');
                    value.push_str(subpath);
                }
                value
            }
        }
    }

    fn descriptor(&self) -> ArtifactDescriptor {
        ArtifactDescriptor {
            kind: match self {
                Self::HelmChart { .. } => "helm-chart",
                Self::RemoteManifest { .. } => "remote-manifest",
                Self::GitSource { .. } => "git-source",
            }
            .to_owned(),
            coordinate: self.display(),
        }
    }

    pub fn file_name(&self) -> Result<String> {
        let fingerprint = self.fingerprint()?;
        let suffix = &fingerprint[..12];
        let (base, extension) = match self {
            Self::HelmChart { name, version, .. } => (format!("{name}-{version}"), "tgz"),
            Self::RemoteManifest { url } => {
                let base = url
                    .split(['?', '#'])
                    .next()
                    .and_then(|url| url.trim_end_matches('/').rsplit('/').next())
                    .filter(|value| !value.is_empty())
                    .unwrap_or("manifest");
                let extension = Path::new(base)
                    .extension()
                    .and_then(|value| value.to_str())
                    .filter(|value| matches!(*value, "yaml" | "yml" | "json"))
                    .unwrap_or("manifest");
                (base.trim_end_matches(&format!(".{extension}")).to_owned(), extension)
            }
            Self::GitSource { revision, .. } => (format!("source-{revision}"), "tar.zst"),
        };
        Ok(format!("{}-{suffix}.{extension}", sanitize_segment(&base)))
    }

    pub fn vendor_relative_path(&self) -> Result<PathBuf> {
        let category = match self {
            Self::HelmChart { .. } => "helm",
            Self::RemoteManifest { .. } => "manifests",
            Self::GitSource { .. } => "git",
        };
        let coordinate = match self {
            Self::RemoteManifest { url } => url,
            Self::HelmChart { repository, .. } | Self::GitSource { repository, .. } => repository,
        };
        let host = coordinate_host(coordinate).unwrap_or("remote");
        Ok(PathBuf::from("artifacts")
            .join(category)
            .join(sanitize_segment(host))
            .join(self.file_name()?))
    }

    fn reuse_operation(&self) -> SourceOperation {
        match self {
            Self::HelmChart { .. } => SourceOperation::HelmChartReuse,
            Self::RemoteManifest { .. } => SourceOperation::RemoteManifestReuse,
            Self::GitSource { .. } => SourceOperation::GitSourceReuse,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ArtifactFormat {
    HelmArchive,
    Manifest,
    GitArchive,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArtifactOrigin {
    Vendor,
    Cache,
    Remote,
}

#[derive(Clone, Debug)]
pub struct ResolvedArtifact {
    pub path: PathBuf,
    pub digest: String,
    pub format: ArtifactFormat,
    pub resolved_ref: Option<String>,
    pub origin: ArtifactOrigin,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VendorLock {
    pub version: u32,
    pub artifacts: BTreeMap<String, VendorArtifact>,
}

impl Default for VendorLock {
    fn default() -> Self {
        Self {
            version: VENDOR_LOCK_VERSION,
            artifacts: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VendorArtifact {
    pub request: ArtifactDescriptor,
    pub path: String,
    pub format: ArtifactFormat,
    pub size: u64,
    pub digest: String,
    #[serde(rename = "resolvedRef", skip_serializing_if = "Option::is_none")]
    pub resolved_ref: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactDescriptor {
    pub kind: String,
    pub coordinate: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceCacheRecord {
    version: u32,
    path: PathBuf,
    format: ArtifactFormat,
    size: u64,
    digest: String,
    #[serde(rename = "resolvedRef", skip_serializing_if = "Option::is_none")]
    resolved_ref: Option<String>,
}

/// Minimal read contract that later Git or object-store backends can implement.
pub trait VendorStore: Send + Sync {
    fn lookup(&self, request: &ArtifactRequest) -> Result<Option<ResolvedArtifact>>;
}

/// Filesystem-backed, project-controlled vendor snapshot.
#[derive(Clone, Debug)]
pub struct DirectoryVendorStore {
    root: PathBuf,
    lock: VendorLock,
}

impl DirectoryVendorStore {
    pub fn load(root: PathBuf) -> Result<Self> {
        let lock_path = root.join("lock.yaml");
        let lock = match fs::read_to_string(&lock_path) {
            Ok(contents) => serde_norway::from_str::<VendorLock>(&contents).map_err(|error| {
                NylError::config(format!("Failed to parse vendor lock {}: {error}", lock_path.display()))
            })?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => VendorLock::default(),
            Err(error) => return Err(error.into()),
        };
        if lock.version != VENDOR_LOCK_VERSION {
            return Err(NylError::config(format!(
                "Unsupported vendor lock version {} in {}",
                lock.version,
                lock_path.display()
            )));
        }
        Ok(Self { root, lock })
    }
}

/// Mutating directory backend used only by explicit `nyl vendor` commands.
pub struct DirectoryVendorWriter {
    root: PathBuf,
    lfs_threshold_bytes: u64,
}

impl DirectoryVendorWriter {
    pub fn from_config(config: &ProjectConfig) -> Result<Self> {
        let settings = config.vendor().ok_or_else(|| {
            NylError::config("nyl.toml has no [vendor] section; configure vendor.mode before syncing")
        })?;
        Ok(Self {
            root: settings.path.clone(),
            lfs_threshold_bytes: settings.lfs_threshold_bytes,
        })
    }

    pub fn sync(
        &self,
        observed: &BTreeMap<ArtifactRequest, ResolvedArtifact>,
        preserve_existing: bool,
    ) -> Result<VendorSyncResult> {
        fs::create_dir_all(self.root.join("artifacts"))?;
        let existing = DirectoryVendorStore::load(self.root.clone())?;
        let mut lock = if preserve_existing {
            existing.lock
        } else {
            VendorLock::default()
        };
        let mut written = 0;
        let mut reused = 0;
        for (request, artifact) in observed {
            let fingerprint = request.fingerprint()?;
            let relative = request.vendor_relative_path()?;
            let portable_relative = portable_relative_path(&relative)?;
            let destination = checked_relative_path(&self.root, &relative, "generated vendor artifact path")?;
            let bytes = fs::read(&artifact.path)?;
            let digest = sha256(&bytes);
            if destination.is_file() && fs::read(&destination).is_ok_and(|current| sha256(&current) == digest) {
                reused += 1;
            } else {
                let parent = destination
                    .parent()
                    .ok_or_else(|| NylError::config("Generated vendor artifact has no parent"))?;
                fs::create_dir_all(parent)?;
                atomic_replace(&destination, &bytes)?;
                written += 1;
            }
            lock.artifacts.insert(
                fingerprint,
                VendorArtifact {
                    request: request.descriptor(),
                    path: portable_relative,
                    format: artifact.format,
                    size: bytes.len() as u64,
                    digest,
                    resolved_ref: artifact.resolved_ref.clone(),
                },
            );
        }
        self.write_attributes(&lock)?;
        if lock
            .artifacts
            .values()
            .any(|entry| entry.format != ArtifactFormat::Manifest || entry.size >= self.lfs_threshold_bytes)
            && !std::process::Command::new("git")
                .args(["lfs", "version"])
                .output()
                .is_ok_and(|output| output.status.success())
        {
            tracing::warn!(
                "Vendored binary or large artifacts are configured for Git LFS, but 'git lfs version' failed"
            );
        }
        let serialized = serde_norway::to_string(&lock)
            .map_err(|error| NylError::config(format!("Failed to serialize vendor lock: {error}")))?;
        atomic_replace(
            &self.root.join("lock.yaml"),
            format!("# Generated by nyl vendor; do not edit.\n{serialized}").as_bytes(),
        )?;
        Ok(VendorSyncResult {
            artifacts: lock.artifacts.len(),
            written,
            reused,
        })
    }

    pub fn check(
        &self,
        expected: &BTreeMap<ArtifactRequest, ResolvedArtifact>,
        report_unreferenced: bool,
    ) -> Result<()> {
        let store = DirectoryVendorStore::load(self.root.clone())?;
        let mut errors = Vec::new();
        for request in expected.keys() {
            match store.lookup(request) {
                Ok(Some(_)) => {}
                Ok(None) => errors.push(format!("missing {}", request.display())),
                Err(error) => errors.push(error.to_string()),
            }
        }
        if report_unreferenced {
            let expected_fingerprints = expected
                .keys()
                .map(ArtifactRequest::fingerprint)
                .collect::<Result<std::collections::BTreeSet<_>>>()?;
            for (fingerprint, entry) in &store.lock.artifacts {
                if !expected_fingerprints.contains(fingerprint) {
                    errors.push(format!("unreferenced {}", entry.request.coordinate));
                }
            }
        }
        let expected_attributes = self.attributes(&store.lock);
        match fs::read_to_string(self.root.join(".gitattributes")) {
            Ok(actual) if actual == expected_attributes => {}
            _ => errors.push("vendor/.gitattributes is missing or out of date".to_owned()),
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(NylError::validation(format!(
                "Vendor check found {} issue(s):\n- {}",
                errors.len(),
                errors.join("\n- ")
            )))
        }
    }

    pub fn prune(&self) -> Result<usize> {
        let store = DirectoryVendorStore::load(self.root.clone())?;
        let referenced = store
            .lock
            .artifacts
            .values()
            .map(|entry| self.root.join(&entry.path))
            .collect::<std::collections::BTreeSet<_>>();
        let artifacts = self.root.join("artifacts");
        if !artifacts.is_dir() {
            return Ok(0);
        }
        let mut removed = 0;
        for entry in walkdir::WalkDir::new(&artifacts).follow_links(false) {
            let entry = entry.map_err(|error| NylError::config(format!("Failed to inspect vendor tree: {error}")))?;
            if entry.file_type().is_file() && !referenced.contains(entry.path()) {
                fs::remove_file(entry.path())?;
                removed += 1;
            }
        }
        Ok(removed)
    }

    fn write_attributes(&self, lock: &VendorLock) -> Result<()> {
        atomic_replace(&self.root.join(".gitattributes"), self.attributes(lock).as_bytes())
    }

    fn attributes(&self, lock: &VendorLock) -> String {
        let mut rules = vec![
            "artifacts/helm/** filter=lfs diff=lfs merge=lfs -text".to_owned(),
            "artifacts/git/** filter=lfs diff=lfs merge=lfs -text".to_owned(),
        ];
        for entry in lock
            .artifacts
            .values()
            .filter(|entry| entry.format == ArtifactFormat::Manifest && entry.size >= self.lfs_threshold_bytes)
        {
            rules.push(format!(
                "{} filter=lfs diff=lfs merge=lfs -text",
                entry.path.replace(' ', "\\ ")
            ));
        }
        rules.sort();
        let mut lines = vec!["# Generated by nyl vendor; do not edit.".to_owned()];
        lines.extend(rules);
        lines.push(String::new());
        lines.join("\n")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VendorSyncResult {
    pub artifacts: usize,
    pub written: usize,
    pub reused: usize,
}

impl VendorStore for DirectoryVendorStore {
    fn lookup(&self, request: &ArtifactRequest) -> Result<Option<ResolvedArtifact>> {
        let fingerprint = request.fingerprint()?;
        let Some(entry) = self.lock.artifacts.get(&fingerprint) else {
            return Ok(None);
        };
        let path = checked_relative_path(&self.root, Path::new(&entry.path), "vendor lock artifact path")?;
        let bytes = fs::read(&path).map_err(|error| {
            NylError::config(format!(
                "Vendored artifact {} for {} is missing or unreadable: {error}",
                path.display(),
                request.display()
            ))
        })?;
        if bytes.starts_with(b"version https://git-lfs.github.com/spec/v1\n") {
            return Err(NylError::config(format!(
                "Vendored artifact {} is a Git LFS pointer; run 'git lfs pull'",
                path.display()
            )));
        }
        let digest = sha256(&bytes);
        if digest != entry.digest || bytes.len() as u64 != entry.size {
            return Err(NylError::config(format!(
                "Vendored artifact {} failed its lock integrity check for {}",
                path.display(),
                request.display()
            )));
        }
        Ok(Some(ResolvedArtifact {
            path,
            digest,
            format: entry.format,
            resolved_ref: entry.resolved_ref.clone(),
            origin: ArtifactOrigin::Vendor,
        }))
    }
}

/// Resolves authoritative vendor entries and disposable exact source-cache entries.
#[derive(Clone, Debug)]
pub struct ArtifactResolver {
    source_cache_root: PathBuf,
    cache_mode: CacheMode,
    vendor_mode: VendorMode,
    vendor: Option<DirectoryVendorStore>,
    render_cache: Option<RenderCache>,
    materialized_root: PathBuf,
    _owned_transient_root: Option<std::sync::Arc<tempfile::TempDir>>,
}

impl ArtifactResolver {
    pub fn new(project_root: &Path, config: &ProjectConfig, render_cache: Option<RenderCache>) -> Result<Self> {
        let cache_mode = render_cache.as_ref().map_or(CacheMode::Disabled, RenderCache::mode);
        let owned_transient_root = render_cache
            .is_none()
            .then(|| tempfile::TempDir::new().map(std::sync::Arc::new))
            .transpose()?;
        let transient_cache_root = render_cache
            .as_ref()
            .and_then(RenderCache::external_cache_root)
            .or_else(|| owned_transient_root.as_deref().map(tempfile::TempDir::path));
        let source_cache_base = transient_cache_root
            .map(Path::to_path_buf)
            .or_else(|| {
                render_cache
                    .as_ref()
                    .and_then(|cache| cache.root().parent().and_then(Path::parent))
                    .map(Path::to_path_buf)
            })
            .or_else(|| std::env::var_os("NYL_CACHE_DIR").map(PathBuf::from))
            .unwrap_or_else(|| project_root.join(".nyl/cache"));
        let source_cache_root = source_cache_base.join("sources").join(SOURCE_CACHE_VERSION);
        let materialized_root = source_cache_root.join("materialized");
        let manages_vendor = render_cache
            .as_ref()
            .is_some_and(|cache| cache.is_vendor_population() || cache.is_vendor_check());
        let (vendor_mode, vendor) = match config.vendor() {
            Some(settings) => (
                settings.mode,
                (settings.mode != VendorMode::Disabled || manages_vendor)
                    .then(|| DirectoryVendorStore::load(settings.path.clone()))
                    .transpose()?,
            ),
            None => (VendorMode::Disabled, None),
        };
        Ok(Self {
            source_cache_root,
            cache_mode,
            vendor_mode,
            vendor,
            render_cache,
            materialized_root,
            _owned_transient_root: owned_transient_root,
        })
    }

    pub fn lookup(&self, request: &ArtifactRequest) -> Result<Option<ResolvedArtifact>> {
        let population = self
            .render_cache
            .as_ref()
            .is_some_and(RenderCache::is_vendor_population);
        let population_refresh = self
            .render_cache
            .as_ref()
            .is_some_and(RenderCache::vendor_population_refresh);
        let vendor_check = self.render_cache.as_ref().is_some_and(RenderCache::is_vendor_check);
        if let Some(vendor) = self.vendor.as_ref().filter(|_| !population_refresh) {
            if let Some(artifact) = vendor.lookup(request)? {
                self.observe(SourceOperation::VendorArtifactReuse);
                self.observe_artifact(request, &artifact);
                return Ok(Some(artifact));
            }
            if (self.vendor_mode == VendorMode::Required || vendor_check) && !population {
                return Err(NylError::config(format!(
                    "Remote artifact {} is not present in the required vendor lock; run 'nyl vendor'",
                    request.display()
                )));
            }
        }
        if vendor_check {
            return Err(NylError::config(format!(
                "Remote artifact {} is not present in the vendor lock; run 'nyl vendor'",
                request.display()
            )));
        }
        if population_refresh {
            return Ok(None);
        }
        if !self.cache_mode.reads() {
            return Ok(None);
        }
        let fingerprint = request.fingerprint()?;
        let record_path = self
            .source_cache_root
            .join("records")
            .join(format!("{fingerprint}.json"));
        let record = match fs::read(&record_path) {
            Ok(bytes) => match serde_json::from_slice::<SourceCacheRecord>(&bytes) {
                Ok(record) => record,
                Err(error) => {
                    tracing::warn!(path = %record_path.display(), %error, "Ignoring invalid source cache record");
                    return Ok(None);
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if record.version != 1 {
            tracing::warn!(
                path = %record_path.display(),
                version = record.version,
                "Ignoring unsupported source cache record"
            );
            return Ok(None);
        }
        let path = checked_relative_path(&self.source_cache_root, &record.path, "source cache artifact path")?;
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if sha256(&bytes) != record.digest || bytes.len() as u64 != record.size {
            tracing::warn!(path = %path.display(), "Ignoring corrupt source cache artifact");
            return Ok(None);
        }
        self.observe(request.reuse_operation());
        let artifact = ResolvedArtifact {
            path,
            digest: record.digest,
            format: record.format,
            resolved_ref: record.resolved_ref,
            origin: ArtifactOrigin::Cache,
        };
        self.observe_artifact(request, &artifact);
        Ok(Some(artifact))
    }

    pub fn store(
        &self,
        request: &ArtifactRequest,
        source: &Path,
        format: ArtifactFormat,
        resolved_ref: Option<String>,
    ) -> Result<ResolvedArtifact> {
        let bytes = fs::read(source)?;
        let digest = sha256(&bytes);
        if !self.cache_mode.writes() {
            let extension = source.extension().and_then(|value| value.to_str()).unwrap_or("blob");
            let path = self
                .source_cache_root
                .join("transient")
                .join(format!("{digest}.{extension}"));
            atomic_write_once(&path, &bytes)?;
            let artifact = ResolvedArtifact {
                path,
                digest,
                format,
                resolved_ref,
                origin: ArtifactOrigin::Remote,
            };
            self.observe_artifact(request, &artifact);
            return Ok(artifact);
        }
        let extension = source.extension().and_then(|value| value.to_str()).unwrap_or("blob");
        let relative = PathBuf::from("blobs")
            .join(&digest[..2])
            .join(format!("{digest}.{extension}"));
        let destination = self.source_cache_root.join(&relative);
        atomic_write_once(&destination, &bytes)?;
        let record = SourceCacheRecord {
            version: 1,
            path: relative,
            format,
            size: bytes.len() as u64,
            digest: digest.clone(),
            resolved_ref: resolved_ref.clone(),
        };
        let fingerprint = request.fingerprint()?;
        atomic_replace(
            &self
                .source_cache_root
                .join("records")
                .join(format!("{fingerprint}.json")),
            &serde_json::to_vec(&record)?,
        )?;
        let artifact = ResolvedArtifact {
            path: destination,
            digest,
            format,
            resolved_ref,
            origin: ArtifactOrigin::Remote,
        };
        self.observe_artifact(request, &artifact);
        Ok(artifact)
    }

    pub fn vendor_mode(&self) -> VendorMode {
        self.vendor_mode
    }

    pub fn render_cache(&self) -> Option<&RenderCache> {
        self.render_cache.as_ref()
    }

    pub fn cache_base(&self) -> &Path {
        self.source_cache_root
            .parent()
            .and_then(Path::parent)
            .expect("source cache root always ends in sources/v1")
    }

    pub fn archive_git_tree(source: &Path, destination: &Path) -> Result<()> {
        let file = fs::File::create(destination)?;
        let encoder = zstd::Encoder::new(file, 9)
            .map_err(|error| NylError::Process(format!("Failed to initialize Git archive compression: {error}")))?;
        let mut archive = tar::Builder::new(encoder);
        archive.mode(tar::HeaderMode::Deterministic);
        archive.follow_symlinks(false);
        for entry in walkdir::WalkDir::new(source).follow_links(false).sort_by_file_name() {
            let entry = entry.map_err(|error| NylError::config(format!("Failed to inspect Git source: {error}")))?;
            let relative = entry
                .path()
                .strip_prefix(source)
                .expect("Git source entry is beneath its root");
            if relative.as_os_str().is_empty()
                || relative
                    .components()
                    .next()
                    .is_some_and(|component| component.as_os_str() == ".git")
            {
                continue;
            }
            if entry.file_type().is_symlink() {
                validate_internal_symlink(relative, entry.path())?;
                archive.append_path_with_name(entry.path(), relative)?;
            } else if entry.file_type().is_dir() {
                archive.append_dir(relative, entry.path())?;
            } else if entry.file_type().is_file() {
                archive.append_path_with_name(entry.path(), relative)?;
            }
        }
        let encoder = archive.into_inner()?;
        encoder
            .finish()
            .map_err(|error| NylError::Process(format!("Failed to finish Git archive: {error}")))?;
        Ok(())
    }

    pub fn materialize_git(&self, artifact: &ResolvedArtifact) -> Result<PathBuf> {
        if artifact.format != ArtifactFormat::GitArchive {
            return Err(NylError::config("Artifact is not a Git source archive"));
        }
        let destination = self.materialized_root.join(&artifact.digest);
        let marker = destination.join(".nyl-artifact-digest");
        if fs::read_to_string(&marker).is_ok_and(|value| value == artifact.digest) {
            return Ok(destination);
        }
        let staging = self.materialized_root.join(format!(".{}-staging", artifact.digest));
        if staging.exists() {
            fs::remove_dir_all(&staging)?;
        }
        fs::create_dir_all(&staging)?;
        let file = fs::File::open(&artifact.path)?;
        let decoder = zstd::Decoder::new(file)
            .map_err(|error| NylError::Process(format!("Failed to open Git source archive: {error}")))?;
        let mut archive = tar::Archive::new(decoder);
        archive.set_preserve_permissions(false);
        archive.unpack(&staging)?;
        fs::write(staging.join(".nyl-artifact-digest"), &artifact.digest)?;
        if destination.exists() {
            fs::remove_dir_all(&destination)?;
        }
        fs::rename(&staging, &destination)?;
        Ok(destination)
    }

    fn observe(&self, operation: SourceOperation) {
        if let Some(cache) = &self.render_cache {
            cache.observe_source(operation);
        }
    }

    fn observe_artifact(&self, request: &ArtifactRequest, artifact: &ResolvedArtifact) {
        if let Some(cache) = &self.render_cache {
            cache.observe_artifact(request.clone(), artifact.clone());
        }
    }
}

fn validate_internal_symlink(archive_path: &Path, source_path: &Path) -> Result<()> {
    let target = fs::read_link(source_path).map_err(|error| {
        NylError::config(format!(
            "Failed to read Git source symlink {}: {error}",
            source_path.display()
        ))
    })?;
    let mut depth = archive_path.parent().map_or(0, |parent| parent.components().count());
    for component in target.components() {
        match component {
            Component::Normal(_) => depth += 1,
            Component::CurDir => {}
            Component::ParentDir if depth > 0 => depth -= 1,
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(NylError::config(format!(
                    "Git source symlink {} points outside the archived tree: {}",
                    source_path.display(),
                    target.display()
                )));
            }
        }
    }
    Ok(())
}

fn checked_relative_path(root: &Path, relative: &Path, label: &str) -> Result<PathBuf> {
    if relative.is_absolute()
        || relative.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(NylError::config(format!("Invalid {label}: {}", relative.display())));
    }
    Ok(root.join(relative))
}

fn portable_relative_path(path: &Path) -> Result<String> {
    path.components()
        .map(|component| match component {
            Component::Normal(value) => value
                .to_str()
                .map(str::to_owned)
                .ok_or_else(|| NylError::config(format!("Generated vendor path is not UTF-8: {}", path.display()))),
            _ => Err(NylError::config(format!(
                "Generated vendor path is not a normalized relative path: {}",
                path.display()
            ))),
        })
        .collect::<Result<Vec<_>>>()
        .map(|components| components.join("/"))
}

fn coordinate_host(value: &str) -> Option<&str> {
    let value = value
        .strip_prefix("git+")
        .unwrap_or(value)
        .split("//")
        .nth(1)
        .unwrap_or(value);
    value.split(['/', ':']).next().filter(|value| !value.is_empty())
}

fn sanitize_segment(value: &str) -> String {
    let value = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_') {
                character
            } else {
                '-'
            }
        })
        .collect::<String>();
    let value = value.trim_matches(['.', '-', '_']);
    if value.is_empty() {
        "artifact".to_owned()
    } else {
        value.chars().take(96).collect()
    }
}

fn sha256(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

fn atomic_write_once(path: &Path, bytes: &[u8]) -> Result<()> {
    if path.is_file() {
        return Ok(());
    }
    let parent = path
        .parent()
        .ok_or_else(|| NylError::config("Artifact path has no parent"))?;
    fs::create_dir_all(parent)?;
    let temporary = tempfile::NamedTempFile::new_in(parent)?;
    fs::write(temporary.path(), bytes)?;
    match temporary.persist_noclobber(path) {
        Ok(_) => Ok(()),
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error.error.into()),
    }
}

fn atomic_replace(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| NylError::config("Artifact path has no parent"))?;
    fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(bytes)?;
    temporary.persist(path).map(|_| ()).map_err(|error| error.error.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProjectConfig;
    use crate::render::cache::RenderCache;
    use tempfile::TempDir;

    fn project_config(project: &Path, mode: &str) -> ProjectConfig {
        let config_path = project.join("nyl.toml");
        fs::write(
            &config_path,
            format!("[vendor]\nmode = \"{mode}\"\npath = \"vendor\"\nlfs_threshold_bytes = 8\n"),
        )
        .unwrap();
        ProjectConfig::load(Some(config_path)).unwrap()
    }

    fn manifest_request() -> ArtifactRequest {
        ArtifactRequest::RemoteManifest {
            url: "https://example.com/platform/resources.yaml?version=1".to_owned(),
        }
    }

    #[test]
    fn identical_coordinates_have_one_fingerprint() {
        let request = ArtifactRequest::HelmChart {
            repository: "https://cloudnative-pg.io/charts/".to_owned(),
            name: "cnpg".to_owned(),
            version: "0.29.0".to_owned(),
        };
        assert_eq!(request.fingerprint().unwrap(), request.clone().fingerprint().unwrap());
    }

    #[test]
    fn chart_name_is_part_of_the_fingerprint() {
        let request = |name: &str| ArtifactRequest::HelmChart {
            repository: "https://charts.example.com".to_owned(),
            name: name.to_owned(),
            version: "1.0.0".to_owned(),
        };
        assert_ne!(
            request("one").fingerprint().unwrap(),
            request("two").fingerprint().unwrap()
        );
    }

    #[test]
    fn exact_source_cache_survives_the_original_download() {
        let project = TempDir::new().unwrap();
        let config = project_config(project.path(), "disabled");
        let cache = RenderCache::with_root(project.path().join("cache"), CacheMode::Default).unwrap();
        let resolver = ArtifactResolver::new(project.path(), &config, Some(cache.clone())).unwrap();
        let download = project.path().join("download.yaml");
        fs::write(&download, "apiVersion: v1\nkind: ConfigMap\n").unwrap();
        let request = manifest_request();

        resolver
            .store(&request, &download, ArtifactFormat::Manifest, None)
            .unwrap();
        fs::remove_file(download).unwrap();

        let resolver = ArtifactResolver::new(project.path(), &config, Some(cache)).unwrap();
        let artifact = resolver.lookup(&request).unwrap().unwrap();
        assert_eq!(artifact.origin, ArtifactOrigin::Cache);
        assert_eq!(
            fs::read_to_string(artifact.path).unwrap(),
            "apiVersion: v1\nkind: ConfigMap\n"
        );
    }

    #[test]
    fn disabled_cache_keeps_sources_for_the_invocation_lifetime() {
        let project = TempDir::new().unwrap();
        let config = project_config(project.path(), "disabled");
        let cache = RenderCache::with_root(project.path().join("unused-cache"), CacheMode::Disabled).unwrap();
        let resolver = ArtifactResolver::new(project.path(), &config, Some(cache)).unwrap();
        let download = project.path().join("temporary-chart.tgz");
        fs::write(&download, "chart archive").unwrap();

        let artifact = resolver
            .store(
                &ArtifactRequest::HelmChart {
                    repository: "https://charts.example.com".to_owned(),
                    name: "example".to_owned(),
                    version: "1.0.0".to_owned(),
                },
                &download,
                ArtifactFormat::HelmArchive,
                Some("1.0.0".to_owned()),
            )
            .unwrap();
        fs::remove_file(download).unwrap();

        assert_eq!(fs::read_to_string(artifact.path).unwrap(), "chart archive");
        assert!(!project.path().join("unused-cache/sources").exists());
    }

    #[test]
    fn vendor_sync_deduplicates_by_request_and_writes_lfs_rules() {
        let project = TempDir::new().unwrap();
        let config = project_config(project.path(), "preferred");
        let source = project.path().join("manifest.yaml");
        fs::write(&source, "apiVersion: v1\nkind: ConfigMap\n").unwrap();
        let request = manifest_request();
        let artifact = ResolvedArtifact {
            path: source,
            digest: sha256(b"apiVersion: v1\nkind: ConfigMap\n"),
            format: ArtifactFormat::Manifest,
            resolved_ref: None,
            origin: ArtifactOrigin::Remote,
        };
        let observed = BTreeMap::from([(request.clone(), artifact)]);
        let writer = DirectoryVendorWriter::from_config(&config).unwrap();

        let first = writer.sync(&observed, false).unwrap();
        let second = writer.sync(&observed, false).unwrap();

        assert_eq!(first.artifacts, 1);
        assert_eq!(first.written, 1);
        assert_eq!(second.artifacts, 1);
        assert_eq!(second.reused, 1);
        let lock = DirectoryVendorStore::load(project.path().join("vendor")).unwrap();
        assert_eq!(lock.lock.artifacts.len(), 1);
        let attributes = fs::read_to_string(project.path().join("vendor/.gitattributes")).unwrap();
        assert!(attributes.contains("artifacts/helm/** filter=lfs"));
        assert!(attributes.contains("artifacts/git/** filter=lfs"));
        assert!(attributes.contains("artifacts/manifests/"));

        let resolver = ArtifactResolver::new(project.path(), &config, None).unwrap();
        let vendored = resolver.lookup(&request).unwrap().unwrap();
        assert_eq!(vendored.origin, ArtifactOrigin::Vendor);
    }

    #[test]
    fn matching_corrupt_vendor_artifact_is_an_error() {
        let project = TempDir::new().unwrap();
        let config = project_config(project.path(), "preferred");
        let source = project.path().join("manifest.yaml");
        fs::write(&source, "original").unwrap();
        let request = manifest_request();
        let artifact = ResolvedArtifact {
            path: source,
            digest: sha256(b"original"),
            format: ArtifactFormat::Manifest,
            resolved_ref: None,
            origin: ArtifactOrigin::Remote,
        };
        DirectoryVendorWriter::from_config(&config)
            .unwrap()
            .sync(&BTreeMap::from([(request.clone(), artifact)]), false)
            .unwrap();
        let relative = request.vendor_relative_path().unwrap();
        fs::write(project.path().join("vendor").join(relative), "changed").unwrap();

        let error = ArtifactResolver::new(project.path(), &config, None)
            .unwrap()
            .lookup(&request)
            .unwrap_err()
            .to_string();
        assert!(error.contains("failed its lock integrity check"));
    }

    #[test]
    fn required_mode_rejects_an_unvendored_request_without_fallback() {
        let project = TempDir::new().unwrap();
        let config = project_config(project.path(), "required");

        let error = ArtifactResolver::new(project.path(), &config, None)
            .unwrap()
            .lookup(&manifest_request())
            .unwrap_err()
            .to_string();

        assert!(error.contains("not present in the required vendor lock"));
    }

    #[cfg(unix)]
    #[test]
    fn git_archive_preserves_internal_relative_symlinks() {
        use std::os::unix::fs::symlink;

        let project = TempDir::new().unwrap();
        let source = TempDir::new().unwrap();
        let archive = tempfile::NamedTempFile::new().unwrap();
        fs::write(source.path().join("CLAUDE.md"), "shared instructions\n").unwrap();
        fs::create_dir(source.path().join(".github")).unwrap();
        symlink("../CLAUDE.md", source.path().join(".github/copilot-instructions.md")).unwrap();

        ArtifactResolver::archive_git_tree(source.path(), archive.path()).unwrap();
        let bytes = fs::read(archive.path()).unwrap();
        let artifact = ResolvedArtifact {
            path: archive.path().to_path_buf(),
            digest: sha256(&bytes),
            format: ArtifactFormat::GitArchive,
            resolved_ref: None,
            origin: ArtifactOrigin::Remote,
        };
        let config = project_config(project.path(), "disabled");
        let resolver = ArtifactResolver::new(project.path(), &config, None).unwrap();
        let materialized = resolver.materialize_git(&artifact).unwrap();

        assert_eq!(
            fs::read_link(materialized.join(".github/copilot-instructions.md")).unwrap(),
            Path::new("../CLAUDE.md")
        );
        assert_eq!(
            fs::read_to_string(materialized.join(".github/copilot-instructions.md")).unwrap(),
            "shared instructions\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn git_archive_rejects_symlinks_that_escape_the_source_tree() {
        use std::os::unix::fs::symlink;

        let source = TempDir::new().unwrap();
        let archive = tempfile::NamedTempFile::new().unwrap();
        fs::create_dir(source.path().join("nested")).unwrap();
        symlink("../../outside", source.path().join("nested/link")).unwrap();

        let error = ArtifactResolver::archive_git_tree(source.path(), archive.path())
            .unwrap_err()
            .to_string();

        assert!(error.contains("points outside the archived tree"));
        assert!(error.contains("../../outside"));
    }
}
