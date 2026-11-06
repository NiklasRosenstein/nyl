//! Ownership-indexed reconciliation of rendered files.

use nyl_core::digest::sha256_hex;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{NylError, Result};

/// Index version without compilation metadata, written by `render-tree`.
pub const RENDER_INDEX_VERSION: u32 = 2;
/// Index version carrying per-Release compilation metadata, written by publications.
pub const COMPILED_RENDER_INDEX_VERSION: u32 = 3;
/// Index version 3 plus the owning orchestration unit, written by orchestrated
/// publications (`KubernetesPublication`). Versions 2 and 3 name no owner and
/// stay readable: the first orchestrated publication over one records its
/// owner and upgrades it.
pub const OWNED_RENDER_INDEX_VERSION: u32 = 4;
pub const DEFAULT_INDEX_PATH: &str = "_nyl/index.json";
const TRANSACTION_PATH: &str = "_nyl/transaction.json";

/// Whether `path` lies in a managed rendered output tree: beneath a directory
/// holding the ownership index. Nyl owns those files, so other commands must
/// not write there.
pub fn in_rendered_tree(path: &Path) -> bool {
    path.ancestors()
        .skip(1)
        .any(|directory| directory.join(DEFAULT_INDEX_PATH).is_file())
}
const TRANSACTION_TEMP_PATH: &str = "_nyl/transaction.json.tmp";

/// Controls how rendered files recorded in the ownership index are reconciled.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReconcileOptions {
    /// Recreate missing owned files and replace owned files modified outside Nyl.
    pub force_owned: bool,
    /// Unowned existing paths the target may take over, such as a state file
    /// that a `fromPublication` binding newly declares with `carryFileFromWorktree`.
    pub adopt: BTreeSet<PathBuf>,
    /// Previously owned paths that leave ownership without being deleted, such
    /// as a state file whose `fromPublication` binding no longer declares
    /// `carryFileFromWorktree` and which other tools now commit.
    pub release: BTreeSet<PathBuf>,
    /// The owner of an existing version 4 index that the caller established
    /// is gone, so the new owner takes the prefix over. Any other owner
    /// named by the index refuses the reconciliation.
    pub take_over_from: Option<RenderIndexOwner>,
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

/// The orchestration unit incarnation that owns a published prefix, as index
/// version 4 records it: the environment, the unit, and the incarnation's uid.
///
/// Contract: [Kubernetes publication unit](../../../../design/orchestration-core.md#kubernetes-publication-unit),
/// ownership index. Owner fencing: [`ensure_same_owner`] refuses to
/// overwrite or remove a prefix whose index names another owner, unless the
/// caller established that the other incarnation is gone
/// ([`ReconcileOptions::take_over_from`]).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct RenderIndexOwner {
    pub environment: String,
    pub unit: String,
    pub uid: String,
}

impl std::fmt::Display for RenderIndexOwner {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "unit {}/{} (uid {})", self.environment, self.unit, self.uid)
    }
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
    /// Version 3 compilation metadata; absent in legacy version 2 indexes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compilation: Option<serde_json::Value>,
    /// The owning orchestration unit; present exactly in version 4 indexes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<RenderIndexOwner>,
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
            compilation: None,
            owner: None,
        }
    }

    /// Record the orchestration unit that owns this index: version 4.
    #[must_use]
    pub fn owned_by(mut self, owner: RenderIndexOwner) -> Self {
        self.version = OWNED_RENDER_INDEX_VERSION;
        self.owner = Some(owner);
        self
    }

    fn same_owner(&self, other: &Self) -> bool {
        self.target == other.target && self.cluster == other.cluster && self.publication == other.publication
    }

    fn same_compilation_policy(&self, other: &Self) -> bool {
        use super::tree::partial::PublicationSnapshot;
        match (&self.compilation, &other.compilation) {
            (None, None) => true,
            (Some(left), Some(right)) => {
                match (
                    PublicationSnapshot::deserialize(left),
                    PublicationSnapshot::deserialize(right),
                ) {
                    (Ok(left), Ok(right)) => left.same_policy(&right),
                    _ => false,
                }
            }
            _ => false,
        }
    }
}

/// The files a target owns: output-relative path to bytes, borrowed from the
/// compiled tree.
pub type DesiredFiles<'a> = BTreeMap<&'a Path, &'a [u8]>;

/// Reconcile a complete desired target tree while preserving unowned files.
///
/// All bytes and collisions are validated in a sibling staging directory first.
/// Each file is renamed atomically and the ownership index is installed last.
pub fn reconcile_rendered_tree(
    output_root: &Path,
    desired: &DesiredFiles<'_>,
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
        ensure_same_owner(&index_path, &previous, expected, &ReconcileOptions::default())?;
    }
    Ok(())
}

/// The owner the ownership index beneath `output_root` records, if the
/// prefix has an index with one (version 4).
pub fn read_index_owner(output_root: &Path) -> Result<Option<RenderIndexOwner>> {
    let index_path = output_root.join(DEFAULT_INDEX_PATH);
    reject_symlink_components(output_root, output_root)?;
    reject_symlink_components(output_root, &index_path)?;
    Ok(load_index(&index_path)?.and_then(|index| index.owner))
}

/// Reconcile a complete desired target tree with explicit recovery options.
pub fn reconcile_rendered_tree_with_options(
    output_root: &Path,
    desired: &DesiredFiles<'_>,
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
        .map(|(path, bytes)| Ok((path_text(path)?, sha256_hex(bytes))))
        .collect::<Result<_>>()?;
    if let Some(previous) = &previous {
        if previous.same_owner(&next_index)
            && previous.inputs == next_index.inputs
            && previous.files == next_index.files
            && previous.same_compilation_policy(&next_index)
        {
            next_index.source_commit.clone_from(&previous.source_commit);
            next_index.dirty = previous.dirty;
            next_index.compilation.clone_from(&previous.compilation);
        }
    }
    let resumes_transaction = transaction
        .as_ref()
        .is_some_and(|transaction| transaction.index == next_index);

    if let Some(previous) = &previous {
        ensure_same_owner(&index_path, previous, &next_index, &options)?;
        verify_owned_files(output_root, previous, desired, resumes_transaction, &options)?;
    }

    let previous_files = previous.as_ref().map(|index| &index.files).cloned().unwrap_or_default();
    for relative in desired.keys() {
        reject_symlink_components(output_root, &output_root.join(relative))?;
        let destination = output_root.join(relative);
        let Some(existing) = existing_entry(output_root, relative)? else {
            continue;
        };
        if !previous_files.contains_key(&path_text(&existing)?) && !options.adopt.contains(*relative) {
            let expected = desired.get(relative).expect("iterated desired key exists");
            let actual = fs::read(&destination)?;
            if !resumes_transaction || actual.as_slice() != *expected {
                return Err(NylError::config(format!(
                    "Refusing to overwrite unowned rendered path {}",
                    destination.display()
                )));
            }
        }
    }

    if is_unchanged(previous.as_ref(), transaction.as_ref(), &next_index, &options) {
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
    // Remove stale files, and the directories they leave empty, before
    // installing new ones. On a case-insensitive filesystem a stale
    // `Admin.yaml` and a desired `admin.yaml` are one file, and removing the
    // stale name afterwards would delete the new file.
    for stale in previous_files
        .keys()
        .filter(|path| !next_index.files.contains_key(*path) && !options.release.contains(Path::new(path)))
    {
        let stale_path = output_root.join(stale);
        reject_symlink_components(output_root, &stale_path)?;
        if stale_path.exists() {
            fs::remove_file(stale_path)?;
        }
        remove_empty_parents(output_root, Path::new(stale))?;
    }
    for relative in desired.keys() {
        let source = staged.path().join(relative);
        let destination = output_root.join(relative);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::rename(source, destination)?;
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

/// Return the existing entry that `relative` resolves to beneath `output_root`.
///
/// On a case-insensitive filesystem, any component of the entry may differ from
/// `relative` in case; the returned path carries the names found on disk, so
/// ownership is checked against the file that would be replaced.
fn existing_entry(output_root: &Path, relative: &Path) -> Result<Option<PathBuf>> {
    if !output_root.join(relative).exists() {
        return Ok(None);
    }
    let mut resolved = PathBuf::new();
    for component in relative.components() {
        let name = component.as_os_str();
        let mut on_disk = None;
        for entry in fs::read_dir(output_root.join(&resolved))? {
            let entry_name = entry?.file_name();
            if entry_name == name {
                on_disk = Some(entry_name);
                break;
            }
            if on_disk.is_none() && entry_name.eq_ignore_ascii_case(name) {
                on_disk = Some(entry_name);
            }
        }
        resolved.push(on_disk.as_deref().unwrap_or(name));
    }
    Ok(Some(resolved))
}

/// Remove the directories between `relative` and `output_root` that became empty.
///
/// A directory renamed only by case is then recreated under its new name on a
/// case-insensitive filesystem instead of receiving the new files.
fn remove_empty_parents(output_root: &Path, relative: &Path) -> Result<()> {
    let mut directory = relative.parent();
    while let Some(current) = directory.filter(|current| !current.as_os_str().is_empty()) {
        let path = output_root.join(current);
        match fs::read_dir(&path) {
            Ok(mut entries) => {
                let empty = entries.next().is_none();
                // Close the listing first; Windows cannot remove an open directory.
                drop(entries);
                if !empty {
                    break;
                }
                fs::remove_dir(&path)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        directory = current.parent();
    }
    Ok(())
}

/// Refuse an index that belongs to another target, publication, or (from
/// version 4) another orchestration unit incarnation.
///
/// An index without an owner (versions 2 and 3) is taken over by an owned
/// publication, which then records itself. An index with an owner is
/// reconciled only by that owner, by a caller that established the owner is
/// gone (`options.take_over_from`), and never by a publication that records
/// no owner, such as `publish-tree`, which cannot tell whether the unit still
/// exists.
fn ensure_same_owner(
    path: &Path,
    actual: &RenderIndex,
    expected: &RenderIndex,
    options: &ReconcileOptions,
) -> Result<()> {
    if !actual.same_owner(expected) {
        return Err(NylError::config(format!(
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
        )));
    }
    let Some(owner) = &actual.owner else {
        return Ok(());
    };
    if expected.owner.as_ref() == Some(owner) {
        return Ok(());
    }
    if expected.owner.is_some() && options.take_over_from.as_ref() == Some(owner) {
        return Ok(());
    }
    Err(NylError::OwnerFenced(match &expected.owner {
        Some(next) => format!(
            "NYL-KUBERNETES-OWNER-FENCED: rendered ownership index {} is owned by {owner}, not by {next}; \
             it stays fenced while that incarnation exists in its environment's state",
            path.display()
        ),
        None => format!(
            "{OWNED_PREFIX_CODE}: rendered ownership index {} is owned by {owner}, and a publication outside \
             orchestration cannot overwrite or remove it; {OWNED_PREFIX_FIX}",
            path.display()
        ),
    }))
}

/// The stable code of a refusal by a publication that records no owner, such
/// as `publish-tree` and `render-tree`, of a prefix an orchestrated unit owns.
pub const OWNED_PREFIX_CODE: &str = "NYL-TREE-OWNED-BY-UNIT";

/// How to release a prefix deliberately: the way out when its owner can no
/// longer release it, such as an environment that was renamed or removed, or
/// a unit released with `nyl state forget`.
pub const OWNED_PREFIX_FIX: &str =
    "publish it through the environment that owns it, or release it deliberately: in a commit to the \
     publication branch, remove the `owner` member from `<prefix>/_nyl/index.json` and set its `version` to 3, \
     which makes the prefix unowned, so the next publication adopts it";

fn is_unchanged(
    previous: Option<&RenderIndex>,
    transaction: Option<&RenderTransaction>,
    next: &RenderIndex,
    options: &ReconcileOptions,
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
    parse_index(&fs::read(path)?, &path.display().to_string()).map(Some)
}

/// Read and verify index-owned files, excluding state paths released from ownership.
pub fn read_indexed_files(
    output_root: &Path,
    index: &RenderIndex,
    released: &BTreeSet<PathBuf>,
) -> Result<BTreeMap<PathBuf, Vec<u8>>> {
    let mut files = BTreeMap::new();
    for (relative, digest) in &index.files {
        if released.contains(Path::new(relative)) {
            continue;
        }
        nyl_core::resources::validate_relative_path("published owned path", relative, false, false)?;
        let path = output_root.join(relative);
        reject_symlink_components(output_root, &path)?;
        let bytes = fs::read(&path).map_err(|error| {
            NylError::config(format!(
                "Published owned file {} is missing or unreadable: {error}",
                path.display()
            ))
        })?;
        if sha256_hex(&bytes) != *digest {
            return Err(NylError::config(format!(
                "Published owned file {} does not match its ownership index",
                path.display()
            )));
        }
        files.insert(PathBuf::from(relative), bytes);
    }
    Ok(files)
}

/// Parse and version-check ownership index bytes read from `origin`.
pub fn parse_index(bytes: &[u8], origin: &str) -> Result<RenderIndex> {
    let index: RenderIndex = serde_json::from_slice(bytes)
        .map_err(|error| NylError::config(format!("Invalid rendered ownership index {origin}: {error}")))?;
    if ![
        RENDER_INDEX_VERSION,
        COMPILED_RENDER_INDEX_VERSION,
        OWNED_RENDER_INDEX_VERSION,
    ]
    .contains(&index.version)
    {
        return Err(NylError::config(format!(
            "Unsupported rendered ownership index version {} in {origin}",
            index.version
        )));
    }
    if (index.version != RENDER_INDEX_VERSION) != index.compilation.is_some() {
        return Err(NylError::config(
            "Rendered index compilation metadata does not match its version",
        ));
    }
    if (index.version == OWNED_RENDER_INDEX_VERSION) != index.owner.is_some() {
        return Err(NylError::config(
            "Rendered index owner does not match its version; only version 4 names an owner",
        ));
    }
    Ok(index)
}

fn verify_owned_files(
    output_root: &Path,
    index: &RenderIndex,
    desired: &DesiredFiles<'_>,
    resumes_transaction: bool,
    options: &ReconcileOptions,
) -> Result<()> {
    for (relative, expected) in &index.files {
        nyl_core::resources::validate_relative_path("owned rendered path", relative, false, false)?;
        if options.release.contains(Path::new(relative)) && !desired.contains_key(Path::new(relative)) {
            continue;
        }
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
        let actual_hash = sha256_hex(&actual);
        let desired_hash = desired.get(Path::new(relative)).map(|bytes| sha256_hex(bytes));
        if actual_hash != *expected && (!resumes_transaction || desired_hash.as_deref() != Some(&actual_hash)) {
            if options.force_owned {
                tracing::warn!("Replacing modified owned rendered file {}", path.display());
                continue;
            }
            return Err(NylError::OwnedFileModified(format!(
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

fn validate_desired_paths(desired: &DesiredFiles<'_>) -> Result<()> {
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
    Ok(nyl_core::resources::relative_path_to_posix(
        "rendered output path",
        path,
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index() -> RenderIndex {
        RenderIndex::new(
            "production".to_string(),
            "prod-eu-1".to_string(),
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
            (Path::new("apps/a.yaml"), &b"a\n"[..]),
            (Path::new("apps/stale.yaml"), &b"stale\n"[..]),
        ]);
        reconcile_rendered_tree(&root, &first, index()).unwrap();
        fs::write(root.join("unowned.txt"), "keep\n").unwrap();

        let second = BTreeMap::from([(Path::new("apps/a.yaml"), &b"b\n"[..])]);
        reconcile_rendered_tree(&root, &second, index()).unwrap();

        assert_eq!(fs::read(root.join("apps/a.yaml")).unwrap(), b"b\n");
        assert!(!root.join("apps/stale.yaml").exists());
        assert_eq!(fs::read_to_string(root.join("unowned.txt")).unwrap(), "keep\n");
    }

    #[test]
    fn case_only_rename_installs_the_new_file_name() {
        // Meaningful on case-insensitive filesystems (macOS and Windows CI),
        // where both names refer to one file.
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().join("production");
        let first = BTreeMap::from([(Path::new("apps/Admin.yaml"), &b"role\n"[..])]);
        reconcile_rendered_tree(&root, &first, index()).unwrap();

        let second = BTreeMap::from([(Path::new("apps/admin.yaml"), &b"role\n"[..])]);
        reconcile_rendered_tree(&root, &second, index()).unwrap();
        reconcile_rendered_tree(&root, &second, index()).unwrap();

        let names = fs::read_dir(root.join("apps"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(names, ["admin.yaml"]);
        assert_eq!(fs::read(root.join("apps/admin.yaml")).unwrap(), b"role\n");
    }

    #[test]
    fn case_only_directory_rename_installs_the_new_directory_name() {
        // Meaningful on case-insensitive filesystems (macOS and Windows CI),
        // where both names refer to one directory.
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().join("production");
        let first = BTreeMap::from([(Path::new("Web/api/a.yaml"), &b"a\n"[..])]);
        reconcile_rendered_tree(&root, &first, index()).unwrap();

        let second = BTreeMap::from([(Path::new("web/api/a.yaml"), &b"a\n"[..])]);
        reconcile_rendered_tree(&root, &second, index()).unwrap();
        reconcile_rendered_tree(&root, &second, index()).unwrap();

        let names = fs::read_dir(&root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name != "_nyl")
            .collect::<Vec<_>>();
        assert_eq!(names, ["web"]);
        assert_eq!(fs::read(root.join("web/api/a.yaml")).unwrap(), b"a\n");
    }

    #[test]
    fn removing_owned_files_prunes_only_directories_left_empty() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().join("production");
        let first = BTreeMap::from([
            (Path::new("api/configmap/api/a.yaml"), &b"a\n"[..]),
            (Path::new("api/secret/api/b.yaml"), &b"b\n"[..]),
            (Path::new("api/service/api/c.yaml"), &b"c\n"[..]),
        ]);
        reconcile_rendered_tree(&root, &first, index()).unwrap();
        fs::write(root.join("api/secret/api/unowned.txt"), "keep\n").unwrap();

        let second = BTreeMap::from([(Path::new("api/service/api/c.yaml"), &b"c\n"[..])]);
        reconcile_rendered_tree(&root, &second, index()).unwrap();

        assert!(!root.join("api/configmap").exists());
        assert!(root.join("api/secret/api/unowned.txt").is_file());
        assert!(root.join("api/service/api/c.yaml").is_file());
    }

    #[test]
    fn preserves_provenance_when_semantic_inputs_and_outputs_are_unchanged() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().join("production");
        let desired = BTreeMap::from([(Path::new("apps/a.yaml"), &b"a\n"[..])]);
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
        let desired = BTreeMap::from([(Path::new("apps/a.yaml"), &b"a\n"[..])]);
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
        let desired = BTreeMap::from([(Path::new("apps/a.yaml"), &b"a\n"[..])]);
        reconcile_rendered_tree(&root, &desired, index()).unwrap();
        fs::write(root.join("apps/a.yaml"), "manual\n").unwrap();
        let error = reconcile_rendered_tree(&root, &desired, index()).unwrap_err();
        assert!(error.to_string().contains("modified outside Nyl"));
    }

    #[test]
    fn force_recreates_missing_and_modified_owned_files() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().join("production");
        let desired = BTreeMap::from([(Path::new("apps/a.yaml"), &b"a\n"[..])]);
        reconcile_rendered_tree(&root, &desired, index()).unwrap();

        fs::remove_file(root.join("apps/a.yaml")).unwrap();
        reconcile_rendered_tree_with_options(
            &root,
            &desired,
            index(),
            ReconcileOptions {
                force_owned: true,
                ..ReconcileOptions::default()
            },
        )
        .unwrap();
        assert_eq!(fs::read(root.join("apps/a.yaml")).unwrap(), b"a\n");

        fs::write(root.join("apps/a.yaml"), "manual\n").unwrap();
        reconcile_rendered_tree_with_options(
            &root,
            &desired,
            index(),
            ReconcileOptions {
                force_owned: true,
                ..ReconcileOptions::default()
            },
        )
        .unwrap();
        assert_eq!(fs::read(root.join("apps/a.yaml")).unwrap(), b"a\n");
    }

    #[test]
    fn rejects_owned_file_preemptively_changed_to_next_output_without_transaction() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().join("production");
        let first = BTreeMap::from([(Path::new("apps/a.yaml"), &b"old\n"[..])]);
        reconcile_rendered_tree(&root, &first, index()).unwrap();
        fs::write(root.join("apps/a.yaml"), "new\n").unwrap();
        let desired = BTreeMap::from([(Path::new("apps/a.yaml"), &b"new\n"[..])]);

        let error = reconcile_rendered_tree(&root, &desired, index()).unwrap_err();
        assert!(error.to_string().contains("modified outside Nyl"));
    }

    #[test]
    fn rejects_unowned_collision() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().join("production");
        fs::create_dir_all(root.join("apps")).unwrap();
        fs::write(root.join("apps/a.yaml"), "manual\n").unwrap();
        let desired = BTreeMap::from([(Path::new("apps/a.yaml"), &b"a\n"[..])]);
        let error = reconcile_rendered_tree(&root, &desired, index()).unwrap_err();
        assert!(error.to_string().contains("unowned"));
    }

    #[test]
    fn resumes_an_interrupted_owned_file_installation() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().join("production");
        let first = BTreeMap::from([
            (Path::new("apps/a.yaml"), &b"old\n"[..]),
            (Path::new("apps/stale.yaml"), &b"stale\n"[..]),
        ]);
        reconcile_rendered_tree(&root, &first, index()).unwrap();

        let desired = BTreeMap::from([
            (Path::new("apps/a.yaml"), &b"new\n"[..]),
            (Path::new("apps/new.yaml"), &b"added\n"[..]),
        ]);
        let mut intended = index();
        intended.files = desired
            .iter()
            .map(|(path, bytes)| (path_text(path).unwrap(), sha256_hex(bytes)))
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
        let desired = BTreeMap::from([(Path::new("apps/a.yaml"), &b"a\n"[..])]);

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

        let desired = BTreeMap::from([(Path::new("apps/a.yaml"), &b"a\n"[..])]);
        let error = reconcile_rendered_tree(&root, &desired, index()).unwrap_err();
        assert!(error.to_string().contains("symbolic link"));
        assert!(!outside.join("a.yaml").exists());
    }

    fn owner(unit: &str, uid: &str) -> RenderIndexOwner {
        RenderIndexOwner {
            environment: "dev".to_owned(),
            unit: unit.to_owned(),
            uid: uid.to_owned(),
        }
    }

    /// An index as an orchestrated publication writes it: version 3
    /// compilation metadata plus `owner`, which makes version 4.
    fn owned(owner: RenderIndexOwner) -> RenderIndex {
        let mut index = index();
        index.version = COMPILED_RENDER_INDEX_VERSION;
        index.compilation = Some(serde_json::json!({
            "version": 1, "controlDigest": "sha256-example", "groups": {}, "releases": {}, "bindings": {}
        }));
        index.owned_by(owner)
    }

    /// A version 3 index, as `publish-tree` writes it.
    fn compiled() -> RenderIndex {
        let mut index = owned(owner("kubernetes", "u1"));
        index.version = COMPILED_RENDER_INDEX_VERSION;
        index.owner = None;
        index
    }

    fn reconcile_with(root: &Path, index: RenderIndex, options: ReconcileOptions) -> Result<RenderIndex> {
        let desired = BTreeMap::from([(Path::new("apps/a.yaml"), &b"a\n"[..])]);
        reconcile_rendered_tree_with_options(root, &desired, index, options)
    }

    /// Catches an index naming an owner under a version that cannot carry
    /// one, and the reverse: version 4 is exactly the version with an owner.
    #[test]
    fn test_index_versions_two_to_four_are_checked_against_their_fields() {
        let bytes = |index: &RenderIndex| serde_json::to_vec(index).unwrap();
        assert!(parse_index(&bytes(&index()), "v2").is_ok());
        assert!(parse_index(&bytes(&compiled()), "v3").is_ok());
        let version4 = owned(owner("kubernetes", "u1"));
        assert_eq!(version4.version, OWNED_RENDER_INDEX_VERSION);
        assert!(parse_index(&bytes(&version4), "v4").is_ok());

        let mut ownerless = version4.clone();
        ownerless.owner = None;
        assert!(parse_index(&bytes(&ownerless), "v4").is_err());
        let mut fenced_v3 = compiled();
        fenced_v3.owner = Some(owner("kubernetes", "u1"));
        assert!(parse_index(&bytes(&fenced_v3), "v3").is_err());
        let mut v5 = version4;
        v5.version = 5;
        assert!(parse_index(&bytes(&v5), "v5").is_err());
    }

    /// Catches an owner being lost, or a publication outside orchestration
    /// overwriting an owned prefix: the owner reconciles it again, an index
    /// without an owner (versions 2 and 3) is adopted and upgraded, and
    /// nothing else may change or remove it.
    #[test]
    fn test_owner_fencing_refuses_every_owner_but_the_recorded_one() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().join("production");
        let me = owner("kubernetes", "u1");
        let other = owner("kubernetes", "u2");

        // Version 3 (no owner) is adopted by the first orchestrated publication.
        reconcile_with(&root, compiled(), ReconcileOptions::default()).unwrap();
        let adopted = reconcile_with(&root, owned(me.clone()), ReconcileOptions::default()).unwrap();
        assert_eq!((adopted.version, adopted.owner.as_ref()), (4, Some(&me)));

        // The owner again, and no change at all.
        reconcile_with(&root, owned(me.clone()), ReconcileOptions::default()).unwrap();

        // Another incarnation, and publication outside orchestration, are refused.
        let error = reconcile_with(&root, owned(other.clone()), ReconcileOptions::default()).unwrap_err();
        assert!(error.is_owner_fenced(), "{error}");
        assert!(error.to_string().contains("dev/kubernetes (uid u1)"), "{error}");
        let error = reconcile_with(&root, compiled(), ReconcileOptions::default()).unwrap_err();
        assert!(error.is_owner_fenced(), "{error}");
        assert!(error.to_string().contains("outside orchestration"), "{error}");
        // The refusal carries its stable code and the way out when the owner is gone for good.
        assert!(error.to_string().starts_with("NYL-TREE-OWNED-BY-UNIT:"), "{error}");
        assert!(error.to_string().contains("remove the `owner` member"), "{error}");
        assert!(validate_rendered_tree_owner(&root, &compiled()).is_err());
        assert_eq!(read_index_owner(&root).unwrap(), Some(me.clone()));

        // A takeover decided for the recorded owner, and only for it.
        let wrong = ReconcileOptions {
            take_over_from: Some(other.clone()),
            ..ReconcileOptions::default()
        };
        assert!(reconcile_with(&root, owned(other.clone()), wrong)
            .unwrap_err()
            .is_owner_fenced());
        let takeover = ReconcileOptions {
            take_over_from: Some(me),
            ..ReconcileOptions::default()
        };
        let taken = reconcile_with(&root, owned(other.clone()), takeover).unwrap();
        assert_eq!(taken.owner, Some(other));
    }

    /// Catches a publication of another target over an owned prefix being
    /// reported as an owner problem, or passing: target identity is checked
    /// before the owner, with its own message.
    #[test]
    fn test_another_target_is_refused_before_the_owner_is_considered() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().join("production");
        reconcile_with(&root, owned(owner("kubernetes", "u1")), ReconcileOptions::default()).unwrap();
        let mut elsewhere = owned(owner("kubernetes", "u1"));
        elsewhere.target = "staging".to_owned();
        let error = reconcile_with(&root, elsewhere, ReconcileOptions::default()).unwrap_err();
        assert!(!error.is_owner_fenced(), "{error}");
        assert!(error.to_string().contains("belongs to target"), "{error}");
    }
}
