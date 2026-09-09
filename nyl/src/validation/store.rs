//! Project-owned schema inventories and shared immutable blobs.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write as _;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::config::ProjectConfig;
use crate::resources::ClusterKubernetesCapabilities;
use crate::{NylError, Result};

use super::schemas::CrdSchemas;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SchemaDigests {
    pub strict: String,
    pub permissive: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedCrd {
    pub group: String,
    pub kind: String,
    pub versions: BTreeMap<String, SchemaDigests>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClusterSchemaIndex {
    pub version: u32,
    pub cluster: String,
    pub capabilities_fingerprint: String,
    pub crds: BTreeMap<String, CapturedCrd>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuiltinIndex {
    pub version: u32,
    /// Immutable schema URL to verified content digest.
    pub schemas: BTreeMap<String, String>,
}

pub fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

pub fn json_bytes(value: &impl Serialize) -> Result<Vec<u8>> {
    fn canonical(value: Value) -> Value {
        match value {
            Value::Object(fields) => Value::Object(
                fields
                    .into_iter()
                    .map(|(key, value)| (key, canonical(value)))
                    .collect::<BTreeMap<_, _>>()
                    .into_iter()
                    .collect(),
            ),
            Value::Array(values) => Value::Array(values.into_iter().map(canonical).collect()),
            other => other,
        }
    }
    let mut bytes = serde_json::to_vec_pretty(&canonical(serde_json::to_value(value)?))?;
    bytes.push(b'\n');
    Ok(bytes)
}

pub fn capabilities_fingerprint(capabilities: &ClusterKubernetesCapabilities) -> Result<String> {
    let mut normalized = capabilities.clone();
    normalized.api_versions.sort();
    normalized.api_versions.dedup();
    Ok(digest(&json_bytes(&normalized)?))
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
    if digest(&bytes) != hash {
        return Err(NylError::validation(format!(
            "Schema digest mismatch: {}",
            path.display()
        )));
    }
    serde_json::from_slice::<Value>(&bytes)?;
    Ok(bytes)
}

pub fn write_blob(root: &Path, bytes: &[u8]) -> Result<String> {
    let hash = digest(bytes);
    atomic_write(&blob_path(root, &hash)?, bytes)?;
    Ok(hash)
}

pub fn prepare_capture(
    name: &str,
    capabilities: &ClusterKubernetesCapabilities,
    definitions: &BTreeMap<String, CrdSchemas>,
) -> Result<(ClusterSchemaIndex, BTreeMap<String, Vec<u8>>)> {
    let mut blobs = BTreeMap::new();
    let mut crds = BTreeMap::new();
    for (name, definition) in definitions {
        let mut versions = BTreeMap::new();
        for (version, schemas) in &definition.versions {
            let strict = json_bytes(&schemas.strict)?;
            let permissive = json_bytes(&schemas.permissive)?;
            let refs = SchemaDigests {
                strict: digest(&strict),
                permissive: digest(&permissive),
            };
            blobs.insert(refs.strict.clone(), strict);
            blobs.insert(refs.permissive.clone(), permissive);
            versions.insert(version.clone(), refs);
        }
        crds.insert(
            name.clone(),
            CapturedCrd {
                group: definition.group.clone(),
                kind: definition.kind.clone(),
                versions,
            },
        );
    }
    Ok((
        ClusterSchemaIndex {
            version: 1,
            cluster: name.to_owned(),
            capabilities_fingerprint: capabilities_fingerprint(capabilities)?,
            crds,
        },
        blobs,
    ))
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
    let index: ClusterSchemaIndex = serde_json::from_slice(&bytes)?;
    if index.version != 1 || index.cluster != name {
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

/// Serialize capture, builtin inventory updates, and pruning across processes.
pub fn lock(root: &Path) -> Result<fs::File> {
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
    Ok(file)
}

/// Verify all source snapshots and collect their roots before deleting any blob.
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
                let index: ClusterSchemaIndex = serde_json::from_slice(&fs::read(entry.path())?)?;
                if index.version != 1 || cluster_index_path(root, &index.cluster)? != entry.path() {
                    return Err(NylError::config("Invalid cluster schema inventory identity"));
                }
                for crd in index.crds.values() {
                    for schema in crd.versions.values() {
                        referenced.extend([schema.strict.clone(), schema.permissive.clone()]);
                    }
                }
            }
        }
    }
    referenced.extend(read_builtins(root)?.schemas.into_values());
    for hash in &referenced {
        read_blob(root, hash)?;
    }
    let mut removed = 0;
    let directory = safe_path(root, Path::new("schemas/blobs"))?;
    if prune && directory.exists() {
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
                fs::remove_file(path)?;
                removed += 1;
            }
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn capabilities() -> ClusterKubernetesCapabilities {
        ClusterKubernetesCapabilities {
            kube_version: Some("1.31.4".into()),
            api_versions: vec!["v1".into(), "example.com/v1".into()],
        }
    }

    fn definitions() -> BTreeMap<String, CrdSchemas> {
        super::super::schemas::extract_crds(&[json!({
            "apiVersion":"apiextensions.k8s.io/v1","kind":"CustomResourceDefinition",
            "metadata":{"name":"widgets.example.com"},"spec":{"group":"example.com","names":{"kind":"Widget"},
            "versions":[{"name":"v1","served":true,"schema":{"openAPIV3Schema":{"type":"object","properties":{"spec":{"type":"object","properties":{"count":{"type":"integer"}}}}}}}]}
        })]).unwrap()
    }

    fn write_capture(root: &Path, name: &str) -> ClusterSchemaIndex {
        let (index, blobs) = prepare_capture(name, &capabilities(), &definitions()).unwrap();
        for bytes in blobs.values() {
            write_blob(root, bytes).unwrap();
        }
        atomic_write(&cluster_index_path(root, name).unwrap(), &json_bytes(&index).unwrap()).unwrap();
        index
    }

    #[test]
    fn test_capture_deduplicates_content_and_prune_retains_all_clusters() {
        let directory = TempDir::new().unwrap();
        let first = write_capture(directory.path(), "staging");
        let second = write_capture(directory.path(), "production");
        assert_eq!(first.crds, second.crds);
        let refs = &first.crds["widgets.example.com"].versions["v1"];
        assert_ne!(refs.strict, refs.permissive);
        write_blob(directory.path(), b"{\"unused\":true}").unwrap();
        assert_eq!(check_and_prune(directory.path(), true).unwrap(), 1);
        std::fs::remove_file(cluster_index_path(directory.path(), "staging").unwrap()).unwrap();
        assert_eq!(check_and_prune(directory.path(), true).unwrap(), 0);
        read_blob(directory.path(), &refs.strict).unwrap();
        read_blob(directory.path(), &refs.permissive).unwrap();
    }

    #[test]
    fn test_corruption_blocks_reads_and_pruning_without_removing_other_blobs() {
        let directory = TempDir::new().unwrap();
        let index = write_capture(directory.path(), "staging");
        let hash = &index.crds["widgets.example.com"].versions["v1"].strict;
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
