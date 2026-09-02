//! Ownership-indexed reconciliation of rendered files.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{NylError, Result};

pub const RENDER_INDEX_VERSION: u32 = 2;
pub const DEFAULT_INDEX_PATH: &str = "_nyl/index.json";
const TRANSACTION_PATH: &str = "_nyl/transaction.json";
const TRANSACTION_TEMP_PATH: &str = "_nyl/transaction.json.tmp";

/// Controls how rendered files recorded in the ownership index are reconciled.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReconcileOptions {
    /// Recreate missing owned files and replace owned files modified outside Nyl.
    pub force_owned: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct RenderTransaction {
    index: RenderIndex,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RenderIndexPublication {
    pub repository: String,
    pub revision: String,
    #[serde(rename = "pathPrefix")]
    pub path_prefix: String,
}

/// Provenance and ownership record for one rendered target prefix.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RenderIndex {
    pub version: u32,
    pub target: String,
    pub cluster: String,
    pub publication: RenderIndexPublication,
    #[serde(rename = "sourceCommit", skip_serializing_if = "Option::is_none")]
    pub source_commit: Option<String>,
    pub dirty: bool,
    pub inputs: BTreeMap<String, String>,
    pub files: BTreeMap<String, String>,
}

impl RenderIndex {
    pub fn new(
        target: String,
        cluster: String,
        publication: RenderIndexPublication,
        source_commit: Option<String>,
        dirty: bool,
        inputs: BTreeMap<String, String>,
    ) -> Self {
        Self {
            version: RENDER_INDEX_VERSION,
            target,
            cluster,
            publication,
            source_commit,
            dirty,
            inputs,
            files: BTreeMap::new(),
        }
    }

    fn same_owner(&self, other: &Self) -> bool {
        self.version == other.version
            && self.target == other.target
            && self.cluster == other.cluster
            && self.publication == other.publication
    }
}

/// SHA-256 digest used for source provenance and owned output verification.
pub fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// Reconcile a complete desired target tree while preserving unowned files.
///
/// All bytes and collisions are validated in a sibling staging directory first.
/// Each file is renamed atomically and the ownership index is installed last.
pub fn reconcile_rendered_tree(
    output_root: &Path,
    desired: &BTreeMap<PathBuf, Vec<u8>>,
    next_index: RenderIndex,
) -> Result<RenderIndex> {
    reconcile_rendered_tree_with_options(output_root, desired, next_index, ReconcileOptions::default())
}

/// Reject an existing rendered tree owned by another target or publication.
///
/// This preflight uses only control-resource data, so callers can run it before
/// rendering any Releases or Helm charts.
pub fn validate_rendered_tree_owner(output_root: &Path, expected: &RenderIndex) -> Result<()> {
    let index_path = output_root.join(DEFAULT_INDEX_PATH);
    reject_symlink_components(output_root, output_root)?;
    reject_symlink_components(output_root, &index_path)?;
    if let Some(previous) = load_index(&index_path)? {
        ensure_same_owner(&index_path, &previous, expected)?;
    }
    Ok(())
}

/// Reconcile a complete desired target tree with explicit recovery options.
pub fn reconcile_rendered_tree_with_options(
    output_root: &Path,
    desired: &BTreeMap<PathBuf, Vec<u8>>,
    mut next_index: RenderIndex,
    options: ReconcileOptions,
) -> Result<RenderIndex> {
    validate_desired_paths(desired)?;
    let index_relative = Path::new(DEFAULT_INDEX_PATH);
    let index_path = output_root.join(index_relative);
    let transaction_path = output_root.join(TRANSACTION_PATH);

    reject_symlink_components(output_root, output_root)?;
    reject_symlink_components(output_root, &index_path)?;
    reject_symlink_components(output_root, &transaction_path)?;
    let previous = load_index(&index_path)?;
    let transaction = load_transaction(&transaction_path)?;

    next_index.files = desired
        .iter()
        .map(|(path, bytes)| Ok((path_text(path)?, sha256(bytes))))
        .collect::<Result<_>>()?;
    if let Some(previous) = &previous {
        if previous.same_owner(&next_index)
            && previous.inputs == next_index.inputs
            && previous.files == next_index.files
        {
            next_index.source_commit.clone_from(&previous.source_commit);
            next_index.dirty = previous.dirty;
        }
    }
    let resumes_transaction = transaction
        .as_ref()
        .is_some_and(|transaction| transaction.index == next_index);

    if let Some(previous) = &previous {
        ensure_same_owner(&index_path, previous, &next_index)?;
        verify_owned_files(output_root, previous, desired, resumes_transaction, options)?;
    }

    let previous_files = previous.as_ref().map(|index| &index.files).cloned().unwrap_or_default();
    for relative in desired.keys() {
        reject_symlink_components(output_root, &output_root.join(relative))?;
        let relative_text = path_text(relative)?;
        let destination = output_root.join(relative);
        if destination.exists() && !previous_files.contains_key(&relative_text) {
            let expected = desired.get(relative).expect("iterated desired key exists");
            let actual = fs::read(&destination)?;
            if !resumes_transaction || actual.as_slice() != expected.as_slice() {
                return Err(NylError::config(format!(
                    "Refusing to overwrite unowned rendered path {}",
                    destination.display()
                )));
            }
        }
    }

    if is_unchanged(previous.as_ref(), transaction.as_ref(), &next_index, options) {
        return unchanged_result(output_root, next_index);
    }

    let parent = output_root.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let staged = tempfile::Builder::new().prefix(".nyl-render-").tempdir_in(parent)?;
    for (relative, bytes) in desired {
        let path = staged.path().join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, bytes)?;
    }
    let staged_index = staged.path().join(index_relative);
    if let Some(parent) = staged_index.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut index_bytes = serde_json::to_vec_pretty(&next_index)?;
    index_bytes.push(b'\n');
    fs::write(&staged_index, index_bytes)?;

    fs::create_dir_all(output_root)?;
    install_transaction(output_root, &next_index)?;
    for relative in desired.keys() {
        let source = staged.path().join(relative);
        let destination = output_root.join(relative);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::rename(source, destination)?;
    }
    for stale in previous_files
        .keys()
        .filter(|path| !next_index.files.contains_key(*path))
    {
        let stale_path = output_root.join(stale);
        reject_symlink_components(output_root, &stale_path)?;
        if stale_path.exists() {
            fs::remove_file(stale_path)?;
        }
    }
    if let Some(parent) = index_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::rename(staged_index, index_path)?;
    match fs::remove_file(&transaction_path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }

    Ok(next_index)
}

fn ensure_same_owner(path: &Path, actual: &RenderIndex, expected: &RenderIndex) -> Result<()> {
    if actual.same_owner(expected) {
        return Ok(());
    }
    Err(NylError::config(format!(
        "Rendered ownership index {} belongs to target {:?}, cluster {:?}, publication {:?}@{} path {:?}; expected target {:?}, cluster {:?}, publication {:?}@{} path {:?}",
        path.display(),
        actual.target,
        actual.cluster,
        actual.publication.repository,
        actual.publication.revision,
        actual.publication.path_prefix,
        expected.target,
        expected.cluster,
        expected.publication.repository,
        expected.publication.revision,
        expected.publication.path_prefix,
    )))
}

fn is_unchanged(
    previous: Option<&RenderIndex>,
    transaction: Option<&RenderTransaction>,
    next: &RenderIndex,
    options: ReconcileOptions,
) -> bool {
    !options.force_owned && transaction.is_none() && previous == Some(next)
}

fn unchanged_result(output_root: &Path, index: RenderIndex) -> Result<RenderIndex> {
    tracing::debug!(root = %output_root.display(), "Rendered tree is unchanged; skipping reconciliation writes");
    Ok(index)
}

fn load_transaction(path: &Path) -> Result<Option<RenderTransaction>> {
    if !path.exists() {
        return Ok(None);
    }
    serde_json::from_slice(&fs::read(path)?)
        .map(Some)
        .map_err(|error| NylError::config(format!("Invalid rendered transaction {}: {error}", path.display())))
}

fn install_transaction(output_root: &Path, index: &RenderIndex) -> Result<()> {
    let path = output_root.join(TRANSACTION_PATH);
    let temporary = output_root.join(TRANSACTION_TEMP_PATH);
    reject_symlink_components(output_root, &path)?;
    reject_symlink_components(output_root, &temporary)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut bytes = serde_json::to_vec_pretty(&RenderTransaction { index: index.clone() })?;
    bytes.push(b'\n');
    let mut file = fs::File::create(&temporary)?;
    std::io::Write::write_all(&mut file, &bytes)?;
    file.sync_all()?;
    fs::rename(&temporary, &path)?;
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

fn load_index(path: &Path) -> Result<Option<RenderIndex>> {
    if !path.exists() {
        return Ok(None);
    }
    let index: RenderIndex = serde_json::from_slice(&fs::read(path)?)
        .map_err(|error| NylError::config(format!("Invalid rendered ownership index {}: {error}", path.display())))?;
    if index.version != RENDER_INDEX_VERSION {
        return Err(NylError::config(format!(
            "Unsupported rendered ownership index version {} in {}",
            index.version,
            path.display()
        )));
    }
    Ok(Some(index))
}

fn verify_owned_files(
    output_root: &Path,
    index: &RenderIndex,
    desired: &BTreeMap<PathBuf, Vec<u8>>,
    resumes_transaction: bool,
    options: ReconcileOptions,
) -> Result<()> {
    for (relative, expected) in &index.files {
        crate::resources::validate_relative_path("owned rendered path", relative, false, false)?;
        let path = output_root.join(relative);
        reject_symlink_components(output_root, &path)?;
        let actual = match fs::read(&path) {
            Ok(actual) => actual,
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound && !desired.contains_key(Path::new(relative)) =>
            {
                continue;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && options.force_owned => {
                tracing::warn!("Recreating missing owned rendered file {}", path.display());
                continue;
            }
            Err(error) => {
                return Err(NylError::config(format!(
                    "Owned rendered file {} is missing or unreadable: {error}",
                    path.display()
                )))
            }
        };
        let actual_hash = sha256(&actual);
        let desired_hash = desired.get(Path::new(relative)).map(|bytes| sha256(bytes));
        if actual_hash != *expected && (!resumes_transaction || desired_hash.as_deref() != Some(&actual_hash)) {
            if options.force_owned {
                tracing::warn!("Replacing modified owned rendered file {}", path.display());
                continue;
            }
            return Err(NylError::config(format!(
                "Owned rendered file {} was modified outside Nyl",
                path.display()
            )));
        }
    }
    Ok(())
}

fn reject_symlink_components(output_root: &Path, path: &Path) -> Result<()> {
    let relative = path.strip_prefix(output_root).map_err(|error| {
        NylError::config(format!(
            "Rendered output path {} is outside {}: {error}",
            path.display(),
            output_root.display()
        ))
    })?;
    let mut current = output_root.to_path_buf();
    for component in std::iter::once(std::path::Component::CurDir).chain(relative.components()) {
        if component != std::path::Component::CurDir {
            current.push(component.as_os_str());
        }
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(NylError::config(format!(
                    "Refusing to traverse symbolic link in rendered output path {}",
                    current.display()
                )))
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn validate_desired_paths(desired: &BTreeMap<PathBuf, Vec<u8>>) -> Result<()> {
    for path in desired.keys() {
        let text = path_text(path)?;
        if matches!(
            text.as_str(),
            DEFAULT_INDEX_PATH | TRANSACTION_PATH | TRANSACTION_TEMP_PATH
        ) {
            return Err(NylError::config(format!(
                "Rendered output path {text} is reserved for ownership reconciliation"
            )));
        }
    }
    Ok(())
}

fn path_text(path: &Path) -> Result<String> {
    crate::resources::relative_path_to_posix("rendered output path", path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index() -> RenderIndex {
        RenderIndex::new(
            "production".to_string(),
            "kasoku".to_string(),
            RenderIndexPublication {
                repository: "deploy".to_string(),
                revision: "deploy/production".to_string(),
                path_prefix: "production".to_string(),
            },
            None,
            true,
            BTreeMap::new(),
        )
    }

    #[test]
    fn reconciles_owned_files_and_preserves_unowned_files() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().join("production");
        let first = BTreeMap::from([
            (PathBuf::from("apps/a.yaml"), b"a\n".to_vec()),
            (PathBuf::from("apps/stale.yaml"), b"stale\n".to_vec()),
        ]);
        reconcile_rendered_tree(&root, &first, index()).unwrap();
        fs::write(root.join("unowned.txt"), "keep\n").unwrap();

        let second = BTreeMap::from([(PathBuf::from("apps/a.yaml"), b"b\n".to_vec())]);
        reconcile_rendered_tree(&root, &second, index()).unwrap();

        assert_eq!(fs::read(root.join("apps/a.yaml")).unwrap(), b"b\n");
        assert!(!root.join("apps/stale.yaml").exists());
        assert_eq!(fs::read_to_string(root.join("unowned.txt")).unwrap(), "keep\n");
    }

    #[test]
    fn preserves_provenance_when_semantic_inputs_and_outputs_are_unchanged() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().join("production");
        let desired = BTreeMap::from([(PathBuf::from("apps/a.yaml"), b"a\n".to_vec())]);
        let mut first = index();
        first.source_commit = Some("first".to_string());
        first.dirty = false;
        reconcile_rendered_tree(&root, &desired, first).unwrap();

        let mut second = index();
        second.source_commit = Some("second".to_string());
        second.dirty = true;
        let reconciled = reconcile_rendered_tree(&root, &desired, second).unwrap();
        assert_eq!(reconciled.source_commit.as_deref(), Some("first"));
        assert!(!reconciled.dirty);
    }

    #[test]
    fn unchanged_reconciliation_preserves_owned_file_mtime() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().join("production");
        let desired = BTreeMap::from([(PathBuf::from("apps/a.yaml"), b"a\n".to_vec())]);
        reconcile_rendered_tree(&root, &desired, index()).unwrap();
        let before = fs::metadata(root.join("apps/a.yaml")).unwrap().modified().unwrap();

        std::thread::sleep(std::time::Duration::from_millis(20));
        reconcile_rendered_tree(&root, &desired, index()).unwrap();

        assert_eq!(
            fs::metadata(root.join("apps/a.yaml")).unwrap().modified().unwrap(),
            before
        );
    }

    #[test]
    fn rejects_modified_owned_file() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().join("production");
        let desired = BTreeMap::from([(PathBuf::from("apps/a.yaml"), b"a\n".to_vec())]);
        reconcile_rendered_tree(&root, &desired, index()).unwrap();
        fs::write(root.join("apps/a.yaml"), "manual\n").unwrap();
        let error = reconcile_rendered_tree(&root, &desired, index()).unwrap_err();
        assert!(error.to_string().contains("modified outside Nyl"));
    }

    #[test]
    fn force_recreates_missing_and_modified_owned_files() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().join("production");
        let desired = BTreeMap::from([(PathBuf::from("apps/a.yaml"), b"a\n".to_vec())]);
        reconcile_rendered_tree(&root, &desired, index()).unwrap();

        fs::remove_file(root.join("apps/a.yaml")).unwrap();
        reconcile_rendered_tree_with_options(&root, &desired, index(), ReconcileOptions { force_owned: true }).unwrap();
        assert_eq!(fs::read(root.join("apps/a.yaml")).unwrap(), b"a\n");

        fs::write(root.join("apps/a.yaml"), "manual\n").unwrap();
        reconcile_rendered_tree_with_options(&root, &desired, index(), ReconcileOptions { force_owned: true }).unwrap();
        assert_eq!(fs::read(root.join("apps/a.yaml")).unwrap(), b"a\n");
    }

    #[test]
    fn rejects_owned_file_preemptively_changed_to_next_output_without_transaction() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().join("production");
        let first = BTreeMap::from([(PathBuf::from("apps/a.yaml"), b"old\n".to_vec())]);
        reconcile_rendered_tree(&root, &first, index()).unwrap();
        fs::write(root.join("apps/a.yaml"), "new\n").unwrap();
        let desired = BTreeMap::from([(PathBuf::from("apps/a.yaml"), b"new\n".to_vec())]);

        let error = reconcile_rendered_tree(&root, &desired, index()).unwrap_err();
        assert!(error.to_string().contains("modified outside Nyl"));
    }

    #[test]
    fn rejects_unowned_collision() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().join("production");
        fs::create_dir_all(root.join("apps")).unwrap();
        fs::write(root.join("apps/a.yaml"), "manual\n").unwrap();
        let desired = BTreeMap::from([(PathBuf::from("apps/a.yaml"), b"a\n".to_vec())]);
        let error = reconcile_rendered_tree(&root, &desired, index()).unwrap_err();
        assert!(error.to_string().contains("unowned"));
    }

    #[test]
    fn resumes_an_interrupted_owned_file_installation() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().join("production");
        let first = BTreeMap::from([
            (PathBuf::from("apps/a.yaml"), b"old\n".to_vec()),
            (PathBuf::from("apps/stale.yaml"), b"stale\n".to_vec()),
        ]);
        reconcile_rendered_tree(&root, &first, index()).unwrap();

        let desired = BTreeMap::from([
            (PathBuf::from("apps/a.yaml"), b"new\n".to_vec()),
            (PathBuf::from("apps/new.yaml"), b"added\n".to_vec()),
        ]);
        let mut intended = index();
        intended.files = desired
            .iter()
            .map(|(path, bytes)| (path_text(path).unwrap(), sha256(bytes)))
            .collect();
        install_transaction(&root, &intended).unwrap();
        fs::write(root.join("apps/a.yaml"), "new\n").unwrap();
        fs::write(root.join("apps/new.yaml"), "added\n").unwrap();
        fs::remove_file(root.join("apps/stale.yaml")).unwrap();

        reconcile_rendered_tree(&root, &desired, index()).unwrap();
        assert_eq!(fs::read(root.join("apps/a.yaml")).unwrap(), b"new\n");
        assert_eq!(fs::read(root.join("apps/new.yaml")).unwrap(), b"added\n");
    }

    #[test]
    fn rejects_identical_unowned_file_without_transaction() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().join("production");
        fs::create_dir_all(root.join("apps")).unwrap();
        fs::write(root.join("apps/a.yaml"), "a\n").unwrap();
        let desired = BTreeMap::from([(PathBuf::from("apps/a.yaml"), b"a\n".to_vec())]);

        let error = reconcile_rendered_tree(&root, &desired, index()).unwrap_err();
        assert!(error.to_string().contains("unowned"));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_ancestors_in_the_output_tree() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().join("production");
        let outside = temp.path().join("outside");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        symlink(&outside, root.join("apps")).unwrap();

        let desired = BTreeMap::from([(PathBuf::from("apps/a.yaml"), b"a\n".to_vec())]);
        let error = reconcile_rendered_tree(&root, &desired, index()).unwrap_err();
        assert!(error.to_string().contains("symbolic link"));
        assert!(!outside.join("a.yaml").exists());
    }
}
