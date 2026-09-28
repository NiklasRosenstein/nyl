use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::cli::resource_file::{atomic_replace, replace_document};
use crate::git::GitManager;
use crate::gitops::discover_gitops_inventory;
use crate::gitops::source_locks::{collect_source_locks, replace_lock_commit, resolve_source_locks, LockResolution};
use crate::{NylError, Result};

pub(crate) fn update_locks(
    path: &Path,
    requested_group: Option<&str>,
    requested_target: Option<&str>,
    check: bool,
) -> Result<()> {
    let inventory = discover_gitops_inventory(path, None)?;
    let locks = collect_source_locks(&inventory, requested_group, requested_target)?;
    let mut manager = GitManager::new().map_err(NylError::Git)?;
    let resolutions = resolve_source_locks(&inventory, locks, &mut manager)?;
    let mut stale = 0;
    for resolution in &resolutions {
        let LockResolution { lock, resolved, note } = resolution;
        let note = note.as_deref().map(|note| format!(" ({note})")).unwrap_or_default();
        let Some(resolved) = resolved else {
            println!(
                "- {}: {} lock stays at {}{note}",
                lock.owner, lock.revision, lock.commit
            );
            continue;
        };
        if !resolution.is_stale() {
            println!("✓ {}: {} is locked to {resolved}{note}", lock.owner, lock.revision);
            continue;
        }
        stale += 1;
        if check {
            println!(
                "✗ {}: {} resolves to {resolved}, lock is {}{note}",
                lock.owner, lock.revision, lock.commit
            );
        }
    }
    if check {
        if stale > 0 {
            return Err(NylError::validation(format!("{stale} source lock(s) are stale")));
        }
        return Ok(());
    }
    let edits = plan_lock_edits(&inventory.project_root, &resolutions)?;
    for (file, (original, contents)) in &edits {
        atomic_replace(file, original, contents)?;
    }
    for resolution in resolutions.iter().filter(|resolution| resolution.is_stale()) {
        let note = resolution
            .note
            .as_deref()
            .map(|note| format!(" ({note})"))
            .unwrap_or_default();
        println!(
            "✓ {}: updated {} lock to {}{note}",
            resolution.lock.owner,
            resolution.lock.revision,
            resolution.resolved.as_deref().unwrap_or_default()
        );
    }
    Ok(())
}

/// The original and updated contents of every file a stale lock lives in.
///
/// Every edit is computed before any file is written, so a lock that cannot
/// be located leaves the project unchanged. Each document is checked against
/// the text discovery parsed, so a concurrent edit of the resource or a
/// shifted document index refuses the update.
fn plan_lock_edits(project_root: &Path, resolutions: &[LockResolution]) -> Result<BTreeMap<PathBuf, (String, String)>> {
    let mut files = BTreeMap::<PathBuf, (String, String, BTreeMap<usize, String>)>::new();
    for resolution in resolutions.iter().filter(|resolution| resolution.is_stale()) {
        let lock = &resolution.lock;
        let resolved = resolution.resolved.as_deref().expect("stale locks have a resolution");
        let file = project_root.join(&lock.resource.source_path);
        if !files.contains_key(&file) {
            let contents = fs::read_to_string(&file)?;
            files.insert(file.clone(), (contents.clone(), contents, BTreeMap::new()));
        }
        let (_, contents, documents) = files.get_mut(&file).expect("inserted above");
        let index = lock.resource.document_index;
        let expected = documents
            .get(&index)
            .cloned()
            .unwrap_or_else(|| lock.resource.raw_document.clone());
        let mut current_lock = lock.clone();
        current_lock.resource.raw_document.clone_from(&expected);
        let document = replace_lock_commit(&current_lock, resolved)?;
        *contents = replace_document(contents, index, &expected, &document)?;
        documents.insert(index, document);
    }
    Ok(files
        .into_iter()
        .map(|(file, (original, contents, _))| (file, (original, contents)))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gitops::source_locks::{LockOwner, SourceLock};
    use crate::gitops::DiscoveredGitOpsResource;
    use crate::resources::{GitOpsResourceIdentity, GitOpsResourceKind};

    fn group(name: &str) -> String {
        format!("apiVersion: k8s.gitops.nyl/v1\nkind: ApplicationGroup\nmetadata:\n  name: {name}\nspec:\n  source:\n    revision: main\n    commit: aaaa\n")
    }

    fn stale(file: &str, document_index: usize, raw_document: &str, owner: LockOwner) -> LockResolution {
        LockResolution {
            lock: SourceLock {
                owner,
                resource: DiscoveredGitOpsResource {
                    source_path: file.into(),
                    document_index,
                    raw_document: raw_document.to_owned(),
                    identity: GitOpsResourceIdentity {
                        kind: GitOpsResourceKind::ApplicationGroup,
                        name: "second".to_owned(),
                    },
                    static_labels: BTreeMap::default(),
                    resource: None,
                },
                repository_url: "https://git.example.com/apps.git".to_owned(),
                revision: "main".to_owned(),
                commit: "aaaa".to_owned(),
                path: None,
                selected: true,
            },
            resolved: Some("bbbb".to_owned()),
            note: None,
        }
    }

    #[test]
    fn test_plan_lock_edits_edits_only_the_discovered_document() {
        let temp = tempfile::TempDir::new().unwrap();
        let (first, second) = (group("first"), group("second"));
        let owner = LockOwner::ApplicationGroup {
            name: "second".to_owned(),
        };
        fs::write(temp.path().join("gitops.yaml"), format!("{first}---\n{second}")).unwrap();
        let resolutions = [stale("gitops.yaml", 2, &second, owner.clone())];
        let edits = plan_lock_edits(temp.path(), &resolutions).unwrap();
        assert_eq!(
            edits[&temp.path().join("gitops.yaml")].1,
            format!("{first}---\n{}", second.replace("aaaa", "bbbb"))
        );

        // A document inserted before the resource while the update resolved
        // shifts its index; the update refuses instead of editing `first`.
        fs::write(
            temp.path().join("gitops.yaml"),
            format!("{first}---\n{first}---\n{second}"),
        )
        .unwrap();
        assert!(plan_lock_edits(temp.path(), &resolutions).is_err());
    }

    #[test]
    fn test_plan_lock_edits_fails_when_any_lock_cannot_be_located() {
        let temp = tempfile::TempDir::new().unwrap();
        let first = group("first");
        fs::write(temp.path().join("a.yaml"), &first).unwrap();
        let unlocatable = "apiVersion: k8s.gitops.nyl/v1\nkind: DeploymentTarget\nmetadata:\n  name: b\nspec:\n  releaseInputs:\n    g/r: {image: {fromGit: {commit: aaaa}}}\n";
        fs::write(temp.path().join("b.yaml"), unlocatable).unwrap();
        let resolutions = [
            stale(
                "a.yaml",
                1,
                &first,
                LockOwner::ApplicationGroup {
                    name: "first".to_owned(),
                },
            ),
            stale(
                "b.yaml",
                1,
                unlocatable,
                LockOwner::ReleaseInput {
                    target: "b".to_owned(),
                    key: "g/r".to_owned(),
                    input: "image".to_owned(),
                },
            ),
        ];
        let error = plan_lock_edits(temp.path(), &resolutions).unwrap_err().to_string();
        assert!(error.contains("Cannot locate the commit lock"), "{error}");
    }
}
