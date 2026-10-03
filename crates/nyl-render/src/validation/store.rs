//! Project-owned schema inventories and shared immutable blobs.

use nyl_core::digest::sha256_hex;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write as _;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::ProjectConfig;
use crate::resources::{group_kind_key, ClusterKubernetesCapabilities, ResourceScope};
use crate::{NylError, Result};

use super::schemas::{extract_crds, CrdSchemas};

/// Converted schema digests of one served version in a version 1 snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SchemaDigests {
    pub strict: String,
    pub permissive: String,
}

/// Where a captured CRD's schemas come from.
#[derive(Debug, Clone, PartialEq)]
pub enum CrdSource {
    /// Digest of the vendored CustomResourceDefinition; schemas are derived when read.
    Definition(String),
    /// Converted schemas per served version, as stored by snapshot version 1.
    Legacy(BTreeMap<String, SchemaDigests>),
}

/// One CustomResourceDefinition recorded in a Cluster schema snapshot.
#[derive(Debug, Clone, PartialEq)]
pub struct CapturedCrd {
    pub group: String,
    pub kind: String,
    /// Declared scope; unknown for version 1 snapshots.
    pub scope: Option<ResourceScope>,
    /// Served versions.
    pub versions: BTreeSet<String>,
    pub source: CrdSource,
}

/// A Cluster's schema snapshot, read from either supported format version.
#[derive(Debug, Clone, PartialEq)]
pub struct ClusterSchemaIndex {
    /// Format version the snapshot was read from; capture always writes the current one.
    pub version: u32,
    pub cluster: String,
    /// Fingerprint of the Cluster's complete capabilities, including those the snapshot supplies.
    pub capabilities_fingerprint: String,
    pub crds: BTreeMap<String, CapturedCrd>,
}

/// The snapshot format capture writes.
pub const CLUSTER_SCHEMA_INDEX_VERSION: u32 = 2;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct IndexFile<C> {
    version: u32,
    cluster: String,
    capabilities_fingerprint: String,
    crds: BTreeMap<String, C>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CrdFileV1 {
    group: String,
    kind: String,
    versions: BTreeMap<String, SchemaDigests>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CrdFileV2 {
    group: String,
    kind: String,
    scope: ResourceScope,
    versions: BTreeSet<String>,
    definition: String,
}

impl ClusterSchemaIndex {
    /// Parse either format version; version 1 snapshots lack scopes and vendored definitions.
    pub fn from_slice(bytes: &[u8]) -> Result<Self> {
        let version = serde_json::from_slice::<Value>(bytes)?
            .get("version")
            .and_then(Value::as_u64)
            .ok_or_else(|| NylError::validation("Cluster schema snapshot has no version"))?;
        match version {
            1 => {
                let file: IndexFile<CrdFileV1> = serde_json::from_slice(bytes)?;
                Ok(Self {
                    version: 1,
                    cluster: file.cluster,
                    capabilities_fingerprint: file.capabilities_fingerprint,
                    crds: file
                        .crds
                        .into_iter()
                        .map(|(name, crd)| {
                            let captured = CapturedCrd {
                                group: crd.group,
                                kind: crd.kind,
                                scope: None,
                                versions: crd.versions.keys().cloned().collect(),
                                source: CrdSource::Legacy(crd.versions),
                            };
                            (name, captured)
                        })
                        .collect(),
                })
            }
            2 => {
                let file: IndexFile<CrdFileV2> = serde_json::from_slice(bytes)?;
                for crd in file.crds.values() {
                    validate_digest(&crd.definition)?;
                }
                Ok(Self {
                    version: 2,
                    cluster: file.cluster,
                    capabilities_fingerprint: file.capabilities_fingerprint,
                    crds: file
                        .crds
                        .into_iter()
                        .map(|(name, crd)| {
                            let captured = CapturedCrd {
                                group: crd.group,
                                kind: crd.kind,
                                scope: Some(crd.scope),
                                versions: crd.versions,
                                source: CrdSource::Definition(crd.definition),
                            };
                            (name, captured)
                        })
                        .collect(),
                })
            }
            other => Err(NylError::validation(format!(
                "Unsupported Cluster schema snapshot version {other}; upgrade Nyl"
            ))),
        }
    }

    /// Serialize in the current format. Only snapshots with vendored definitions can be written.
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        let crds = self
            .crds
            .iter()
            .map(|(name, crd)| match (&crd.source, crd.scope) {
                (CrdSource::Definition(definition), Some(scope)) => Ok((
                    name.clone(),
                    CrdFileV2 {
                        group: crd.group.clone(),
                        kind: crd.kind.clone(),
                        scope,
                        versions: crd.versions.clone(),
                        definition: definition.clone(),
                    },
                )),
                _ => Err(NylError::validation(format!(
                    "Cannot write version 1 schema snapshot entry {name}; recapture the Cluster"
                ))),
            })
            .collect::<Result<BTreeMap<_, _>>>()?;
        json_bytes(&IndexFile {
            version: CLUSTER_SCHEMA_INDEX_VERSION,
            cluster: self.cluster.clone(),
            capabilities_fingerprint: self.capabilities_fingerprint.clone(),
            crds,
        })
    }

    /// Blob digests the snapshot references.
    fn referenced_blobs(&self) -> impl Iterator<Item = String> + '_ {
        self.crds.values().flat_map(|crd| match &crd.source {
            CrdSource::Definition(digest) => vec![digest.clone()],
            CrdSource::Legacy(versions) => versions
                .values()
                .flat_map(|schema| [schema.strict.clone(), schema.permissive.clone()])
                .collect(),
        })
    }

    /// `apiVersions` entries the vendored CRDs serve: `group/version` and `group/version/Kind`.
    pub fn api_versions(&self) -> BTreeSet<String> {
        self.crds
            .values()
            .flat_map(|crd| {
                crd.versions.iter().flat_map(move |version| {
                    [
                        format!("{}/{version}", crd.group),
                        format!("{}/{version}/{}", crd.group, crd.kind),
                    ]
                })
            })
            .collect()
    }

    /// `clusterScopedKinds` entries of the vendored cluster-scoped CRDs.
    pub fn cluster_scoped_kinds(&self) -> BTreeSet<String> {
        self.crds
            .values()
            .filter(|crd| crd.scope == Some(ResourceScope::Cluster))
            .map(|crd| group_kind_key(&crd.group, &crd.kind))
            .collect()
    }
}

/// Recorded capabilities with the entries the vendored CRDs supply removed.
pub fn without_vendored(
    complete: &ClusterKubernetesCapabilities,
    index: &ClusterSchemaIndex,
) -> ClusterKubernetesCapabilities {
    let api_versions = index.api_versions();
    let cluster_scoped_kinds = index.cluster_scoped_kinds();
    ClusterKubernetesCapabilities {
        kube_version: complete.kube_version.clone(),
        api_versions: complete
            .api_versions
            .iter()
            .filter(|entry| !api_versions.contains(*entry))
            .cloned()
            .collect(),
        cluster_scoped_kinds: complete
            .cluster_scoped_kinds
            .iter()
            .filter(|entry| !cluster_scoped_kinds.contains(*entry))
            .cloned()
            .collect(),
        vendored_crds: true,
    }
}

/// Recorded capabilities completed with the entries the vendored CRDs supply.
pub fn with_vendored(
    recorded: &ClusterKubernetesCapabilities,
    index: &ClusterSchemaIndex,
) -> ClusterKubernetesCapabilities {
    let mut api_versions: BTreeSet<String> = recorded.api_versions.iter().cloned().collect();
    api_versions.extend(index.api_versions());
    let mut cluster_scoped_kinds: BTreeSet<String> = recorded.cluster_scoped_kinds.iter().cloned().collect();
    cluster_scoped_kinds.extend(index.cluster_scoped_kinds());
    ClusterKubernetesCapabilities {
        kube_version: recorded.kube_version.clone(),
        api_versions: api_versions.into_iter().collect(),
        cluster_scoped_kinds: cluster_scoped_kinds.into_iter().collect(),
        vendored_crds: false,
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuiltinIndex {
    pub version: u32,
    /// Immutable schema URL to verified content digest.
    pub schemas: BTreeMap<String, String>,
    /// Pinned version-directory URL to the digest of its complete file inventory.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub collections: BTreeMap<String, String>,
}

pub fn json_bytes(value: &impl Serialize) -> Result<Vec<u8>> {
    Ok(nyl_core::digest::canonical_json_bytes(value)?)
}

/// Fingerprint of complete capabilities, independent of order and of which entries were vendored.
pub fn capabilities_fingerprint(capabilities: &ClusterKubernetesCapabilities) -> Result<String> {
    let mut normalized = capabilities.clone();
    normalized.api_versions.sort();
    normalized.api_versions.dedup();
    normalized.cluster_scoped_kinds.sort();
    normalized.cluster_scoped_kinds.dedup();
    normalized.vendored_crds = false;
    Ok(sha256_hex(&json_bytes(&normalized)?))
}

pub fn vendor_root(project: &Path, config: &ProjectConfig) -> Result<PathBuf> {
    // Config paths retain the source filename's spelling; discovery may canonicalize the root.
    let relative = match config.vendor() {
        Some(settings) => settings
            .path
            .strip_prefix(config.file.as_deref().and_then(Path::parent).unwrap_or(project))
            .map_err(|_| NylError::config("Schema vendor directory must be beneath the project root"))?,
        None => Path::new("vendor"),
    };
    safe_path(project, relative)
}

pub fn safe_path(root: &Path, relative: &Path) -> Result<PathBuf> {
    if relative.is_absolute()
        || relative
            .components()
            .any(|part| !matches!(part, Component::Normal(_) | Component::CurDir))
    {
        return Err(NylError::config(format!("Unsafe schema path: {}", relative.display())));
    }
    let mut path = root.to_path_buf();
    for part in std::iter::once(None).chain(relative.components().map(Some)) {
        if let Some(part) = part {
            path.push(part.as_os_str());
        }
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(NylError::config(format!("Refusing schema symlink: {}", path.display())))
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(path)
}

pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    if fs::read(path).is_ok_and(|existing| existing == bytes) {
        return Ok(());
    }
    let parent = path
        .parent()
        .ok_or_else(|| NylError::config("Schema path has no parent"))?;
    fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| NylError::Io(error.error))?;
    Ok(())
}

fn validate_digest(value: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(NylError::config("Invalid schema content digest"));
    }
    Ok(())
}

fn blob_path(root: &Path, hash: &str) -> Result<PathBuf> {
    validate_digest(hash)?;
    safe_path(root, &PathBuf::from("schemas/blobs").join(format!("{hash}.json")))
}

pub fn read_blob(root: &Path, hash: &str) -> Result<Vec<u8>> {
    let path = blob_path(root, hash)?;
    let bytes = fs::read(&path).map_err(|error| {
        NylError::validation(format!(
            "Cannot read schema {}: {error}; recapture the source Cluster or run nyl vendor",
            path.display()
        ))
    })?;
    if sha256_hex(&bytes) != hash {
        return Err(NylError::validation(format!(
            "Schema digest mismatch: {}",
            path.display()
        )));
    }
    serde_json::from_slice::<Value>(&bytes)?;
    Ok(bytes)
}

pub fn write_blob(root: &Path, bytes: &[u8]) -> Result<String> {
    let hash = sha256_hex(bytes);
    atomic_write(&blob_path(root, &hash)?, bytes)?;
    Ok(hash)
}

/// Strip server-managed and rotating fields so a vendored CRD changes only with its contract.
pub fn clean_crd(resource: &Value) -> Value {
    let mut spec = resource.get("spec").cloned().unwrap_or(Value::Null);
    // Webhook CA bundles rotate with certificates and do not describe the API.
    if let Some(client_config) = spec
        .pointer_mut("/conversion/webhook/clientConfig")
        .and_then(Value::as_object_mut)
    {
        client_config.remove("caBundle");
    }
    serde_json::json!({
        "apiVersion": resource.get("apiVersion").cloned().unwrap_or(Value::Null),
        "kind": resource.get("kind").cloned().unwrap_or(Value::Null),
        "metadata": {"name": resource.pointer("/metadata/name").cloned().unwrap_or(Value::Null)},
        "spec": spec,
    })
}

/// Build a snapshot from listed CRDs. `complete` holds every capability the cluster
/// reported, CRD-served entries included.
pub fn prepare_capture(
    name: &str,
    complete: &ClusterKubernetesCapabilities,
    resources: &[Value],
) -> Result<(ClusterSchemaIndex, BTreeMap<String, Vec<u8>>)> {
    // Reject CRDs whose schemas cannot be converted before vendoring them.
    let definitions = extract_crds(resources)?;
    let mut blobs = BTreeMap::new();
    let mut crds = BTreeMap::new();
    for resource in resources {
        let Some(crd_name) = resource.pointer("/metadata/name").and_then(Value::as_str) else {
            continue;
        };
        let Some(definition) = definitions.get(crd_name) else {
            continue;
        };
        let scope = match resource.pointer("/spec/scope").and_then(Value::as_str) {
            Some("Cluster") => ResourceScope::Cluster,
            Some("Namespaced") => ResourceScope::Namespaced,
            other => {
                return Err(NylError::validation(format!(
                    "CRD {crd_name} has unsupported spec.scope {other:?}"
                )))
            }
        };
        let bytes = json_bytes(&clean_crd(resource))?;
        let digest = sha256_hex(&bytes);
        blobs.insert(digest.clone(), bytes);
        crds.insert(
            crd_name.to_owned(),
            CapturedCrd {
                group: definition.group.clone(),
                kind: definition.kind.clone(),
                scope: Some(scope),
                versions: definition.versions.keys().cloned().collect(),
                source: CrdSource::Definition(digest),
            },
        );
    }
    let mut index = ClusterSchemaIndex {
        version: CLUSTER_SCHEMA_INDEX_VERSION,
        cluster: name.to_owned(),
        capabilities_fingerprint: String::new(),
        crds,
    };
    // Fingerprint what rendering reconstructs, so a CRD discovery has not yet
    // reported still yields a consistent snapshot.
    index.capabilities_fingerprint = capabilities_fingerprint(&with_vendored(complete, &index))?;
    Ok((index, blobs))
}

/// Read a vendored CRD and derive its schemas.
pub fn read_definition(root: &Path, name: &str, digest: &str) -> Result<CrdSchemas> {
    let resource: Value = serde_json::from_slice(&read_blob(root, digest)?)?;
    extract_crds(&[resource])?
        .remove(name)
        .ok_or_else(|| NylError::validation(format!("Vendored CRD {name} does not match its snapshot entry")))
}

pub fn cluster_index_path(root: &Path, name: &str) -> Result<PathBuf> {
    if name.is_empty() || name.contains(['/', '\\']) || matches!(name, "." | "..") {
        return Err(NylError::config("Unsafe Cluster schema snapshot name"));
    }
    safe_path(root, &PathBuf::from("clusters").join(name).join("schemas.json"))
}

pub fn read_cluster_index(root: &Path, name: &str) -> Result<Option<ClusterSchemaIndex>> {
    let path = cluster_index_path(root, name)?;
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let index = ClusterSchemaIndex::from_slice(&bytes)?;
    if index.cluster != name {
        return Err(NylError::validation(format!(
            "Invalid schema snapshot for Cluster {name}"
        )));
    }
    Ok(Some(index))
}

pub fn read_builtins(root: &Path) -> Result<BuiltinIndex> {
    let path = safe_path(root, Path::new("schemas/builtins.json"))?;
    match fs::read(path) {
        Ok(bytes) => {
            let index: BuiltinIndex = serde_json::from_slice(&bytes)?;
            if index.version != 1 {
                return Err(NylError::config("Unsupported built-in schema inventory version"));
            }
            Ok(index)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(BuiltinIndex {
            version: 1,
            ..BuiltinIndex::default()
        }),
        Err(error) => Err(error.into()),
    }
}

/// Exclusive schema-store lock released when its guard is dropped.
pub struct SchemaStoreLock {
    file: fs::File,
}

impl Drop for SchemaStoreLock {
    fn drop(&mut self) {
        // Closing alone can retain the lock while a spawned child holds an inherited descriptor.
        if let Err(error) = self.file.unlock() {
            tracing::warn!("Cannot unlock schema store: {error}");
        }
    }
}

/// Serialize capture, builtin inventory updates, and pruning across processes.
pub fn lock(root: &Path) -> Result<SchemaStoreLock> {
    let path = safe_path(root, Path::new("schemas/.lock"))?;
    fs::create_dir_all(path.parent().expect("schema lock has parent"))?;
    let ignore = safe_path(root, Path::new("schemas/.gitignore"))?;
    if !ignore.exists() {
        atomic_write(&ignore, b".lock\n")?;
    }
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    file.try_lock()
        .map_err(|error| NylError::config(format!("Schema store is in use; retry: {error}")))?;
    let lock = SchemaStoreLock { file };
    let attributes = safe_path(root, Path::new("schemas/.gitattributes"))?;
    let rules = b"# Generated by Nyl. Blob hashes require exact bytes.\nblobs/*.json binary\n";
    if fs::read(&attributes).ok().as_deref() != Some(rules) {
        atomic_write(&attributes, rules)?;
    }
    Ok(lock)
}

/// Verify all source snapshots and collect their roots before deleting any blob.
/// Return the number of unreferenced blobs, deleting them only when `prune` is true.
pub fn check_and_prune(root: &Path, prune: bool) -> Result<usize> {
    let _lock = if prune { Some(lock(root)?) } else { None };
    let mut referenced = BTreeSet::new();
    let clusters = safe_path(root, Path::new("clusters"))?;
    if clusters.exists() {
        for entry in walkdir::WalkDir::new(&clusters).follow_links(false) {
            let entry = entry.map_err(|error| NylError::config(error.to_string()))?;
            if entry.file_type().is_symlink() {
                return Err(NylError::config("Symlink in cluster schema inventory"));
            }
            if entry.file_type().is_file() && entry.file_name() == "schemas.json" {
                let index = ClusterSchemaIndex::from_slice(&fs::read(entry.path())?)?;
                if cluster_index_path(root, &index.cluster)? != entry.path() {
                    return Err(NylError::config("Invalid cluster schema inventory identity"));
                }
                referenced.extend(index.referenced_blobs());
            }
        }
    }
    let builtins = read_builtins(root)?;
    referenced.extend(builtins.schemas.into_values());
    referenced.extend(builtins.collections.into_values());
    for hash in &referenced {
        read_blob(root, hash)?;
    }
    let mut unreferenced = 0;
    let directory = safe_path(root, Path::new("schemas/blobs"))?;
    if directory.exists() {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            if entry.file_type()?.is_symlink() {
                return Err(NylError::config("Symlink in schema blob store"));
            }
            let path = entry.path();
            let name = path.file_stem().and_then(|name| name.to_str()).unwrap_or("");
            if entry.file_type()?.is_file()
                && path.extension().is_some_and(|ext| ext == "json")
                && !referenced.contains(name)
            {
                validate_digest(name)?;
                if prune {
                    fs::remove_file(path)?;
                }
                unreferenced += 1;
            }
        }
    }
    Ok(unreferenced)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn capabilities() -> ClusterKubernetesCapabilities {
        ClusterKubernetesCapabilities {
            kube_version: Some("1.31.4".into()),
            api_versions: vec![
                "v1".into(),
                "v1/Namespace".into(),
                "example.com/v1".into(),
                "example.com/v1/Widget".into(),
            ],
            cluster_scoped_kinds: vec!["core/Namespace".into(), "example.com/Widget".into()],
            vendored_crds: false,
        }
    }

    /// A listed CRD as the API server returns it, with server-managed fields.
    fn crd() -> Value {
        json!({
            "apiVersion":"apiextensions.k8s.io/v1","kind":"CustomResourceDefinition",
            "metadata":{"name":"widgets.example.com","uid":"1234","resourceVersion":"99",
                "managedFields":[{"manager":"kubectl"}],"annotations":{"kubectl.kubernetes.io/last-applied-configuration":"{}"}},
            "spec":{"group":"example.com","scope":"Cluster","names":{"kind":"Widget","plural":"widgets"},
                "conversion":{"strategy":"Webhook","webhook":{"conversionReviewVersions":["v1"],
                    "clientConfig":{"caBundle":"Y2VydA==","service":{"name":"widgets","namespace":"system"}}}},
                "versions":[
                    {"name":"v1","served":true,"storage":true,"schema":{"openAPIV3Schema":{"type":"object","properties":{"spec":{"type":"object","properties":{"count":{"type":"integer"}}}}}}},
                    {"name":"v0","served":false,"storage":false,"schema":{"openAPIV3Schema":{"type":"object"}}}
                ]},
            "status":{"acceptedNames":{"kind":"Widget"}}
        })
    }

    fn write_capture(root: &Path, name: &str) -> ClusterSchemaIndex {
        let (index, blobs) = prepare_capture(name, &capabilities(), &[crd()]).unwrap();
        for bytes in blobs.values() {
            write_blob(root, bytes).unwrap();
        }
        atomic_write(&cluster_index_path(root, name).unwrap(), &index.to_bytes().unwrap()).unwrap();
        index
    }

    fn definition_digest(index: &ClusterSchemaIndex) -> String {
        match &index.crds["widgets.example.com"].source {
            CrdSource::Definition(digest) => digest.clone(),
            CrdSource::Legacy(_) => panic!("capture writes vendored definitions"),
        }
    }

    #[test]
    fn test_capture_deduplicates_content_and_prune_retains_all_clusters() {
        let directory = TempDir::new().unwrap();
        let first = write_capture(directory.path(), "staging");
        let second = write_capture(directory.path(), "production");
        assert_eq!(first.crds, second.crds);
        let digest = definition_digest(&first);
        let unused = write_blob(directory.path(), b"{\"unused\":true}").unwrap();
        assert_eq!(check_and_prune(directory.path(), false).unwrap(), 1);
        read_blob(directory.path(), &unused).unwrap();
        assert_eq!(check_and_prune(directory.path(), true).unwrap(), 1);
        std::fs::remove_file(cluster_index_path(directory.path(), "staging").unwrap()).unwrap();
        assert_eq!(check_and_prune(directory.path(), true).unwrap(), 0);
        read_blob(directory.path(), &digest).unwrap();
    }

    #[test]
    fn test_capture_vendors_the_crd_contract_without_server_state() {
        let directory = TempDir::new().unwrap();
        let index = write_capture(directory.path(), "staging");
        let captured = &index.crds["widgets.example.com"];
        assert_eq!(captured.scope, Some(ResourceScope::Cluster));
        // Only served versions are part of the API contract.
        assert_eq!(captured.versions, BTreeSet::from(["v1".to_owned()]));
        let vendored: Value =
            serde_json::from_slice(&read_blob(directory.path(), &definition_digest(&index)).unwrap()).unwrap();
        assert_eq!(vendored["metadata"], json!({"name": "widgets.example.com"}));
        assert_eq!(vendored.get("status"), None);
        assert_eq!(
            vendored["spec"]["conversion"]["webhook"]["clientConfig"],
            json!({"service": {"name": "widgets", "namespace": "system"}})
        );
        assert_eq!(vendored["spec"]["names"]["plural"], "widgets");
        // Validation derives schemas from the vendored definition.
        let schemas = read_definition(directory.path(), "widgets.example.com", &definition_digest(&index)).unwrap();
        assert_eq!(
            schemas.versions["v1"].strict["properties"]["spec"]["properties"]["count"]["type"],
            "integer"
        );
    }

    #[test]
    fn test_vendored_entries_are_recorded_once_and_restored_for_rendering() {
        let (index, _) = prepare_capture("staging", &capabilities(), &[crd()]).unwrap();
        let recorded = without_vendored(&capabilities(), &index);
        assert_eq!(recorded.api_versions, ["v1", "v1/Namespace"]);
        assert_eq!(recorded.cluster_scoped_kinds, ["core/Namespace"]);
        assert!(recorded.vendored_crds);
        let restored = with_vendored(&recorded, &index);
        assert_eq!(
            restored.api_versions,
            ["example.com/v1", "example.com/v1/Widget", "v1", "v1/Namespace"]
        );
        assert_eq!(restored.cluster_scoped_kinds, ["core/Namespace", "example.com/Widget"]);
        assert_eq!(
            capabilities_fingerprint(&restored).unwrap(),
            index.capabilities_fingerprint
        );
    }

    #[test]
    fn test_snapshot_format_matches_golden_files() {
        // Version 2 is what capture writes; version 1 snapshots remain readable.
        let (index, _) = prepare_capture("staging", &capabilities(), &[crd()]).unwrap();
        let golden = include_str!("testdata/cluster-schemas-v2.json");
        assert_eq!(String::from_utf8(index.to_bytes().unwrap()).unwrap(), golden);
        assert_eq!(ClusterSchemaIndex::from_slice(golden.as_bytes()).unwrap(), index);

        let legacy = ClusterSchemaIndex::from_slice(include_bytes!("testdata/cluster-schemas-v1.json")).unwrap();
        assert_eq!(legacy.version, 1);
        let widget = &legacy.crds["widgets.example.com"];
        assert_eq!(widget.scope, None);
        assert_eq!(widget.versions, BTreeSet::from(["v1".to_owned()]));
        assert!(matches!(&widget.source, CrdSource::Legacy(versions) if versions.contains_key("v1")));
        // Version 1 entries carry no vendored definition to write back.
        assert!(legacy.to_bytes().is_err());
    }

    #[cfg(unix)]
    #[test]
    fn test_lock_releases_while_a_duplicated_descriptor_remains_open() {
        let directory = TempDir::new().unwrap();
        let held = lock(directory.path()).unwrap();
        // A duplicate shares the lock just like a descriptor inherited during process spawn.
        let duplicate = held.file.try_clone().unwrap();
        assert!(lock(directory.path()).is_err());
        drop(held);
        let reacquired = lock(directory.path()).unwrap();
        drop(duplicate);
        assert!(lock(directory.path()).is_err());
        drop(reacquired);
        lock(directory.path()).unwrap();
    }

    #[test]
    fn test_corruption_blocks_reads_and_pruning_without_removing_other_blobs() {
        let directory = TempDir::new().unwrap();
        let index = write_capture(directory.path(), "staging");
        let hash = &definition_digest(&index);
        let unused = write_blob(directory.path(), b"{}").unwrap();
        fs::write(blob_path(directory.path(), hash).unwrap(), b"{}").unwrap();
        assert!(read_blob(directory.path(), hash).is_err());
        assert!(check_and_prune(directory.path(), true).is_err());
        assert!(blob_path(directory.path(), &unused).unwrap().exists());
    }

    #[test]
    fn test_capabilities_fingerprint_ignores_api_order_and_detects_changes() {
        let original = capabilities();
        let mut reordered = original.clone();
        reordered.api_versions.reverse();
        reordered.api_versions.push("v1".into());
        assert_eq!(
            capabilities_fingerprint(&original).unwrap(),
            capabilities_fingerprint(&reordered).unwrap()
        );
        // Which entries a snapshot supplies does not change the complete contract.
        reordered.vendored_crds = true;
        reordered.cluster_scoped_kinds.reverse();
        assert_eq!(
            capabilities_fingerprint(&original).unwrap(),
            capabilities_fingerprint(&reordered).unwrap()
        );
        reordered.kube_version = Some("1.32.0".into());
        assert_ne!(
            capabilities_fingerprint(&original).unwrap(),
            capabilities_fingerprint(&reordered).unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_schema_paths_reject_symlink_traversal() {
        let directory = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        std::os::unix::fs::symlink(outside.path(), directory.path().join("schemas")).unwrap();
        assert!(write_blob(directory.path(), b"{}").is_err());
        assert!(safe_path(directory.path(), Path::new("../outside")).is_err());
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
    }
}
