use std::fs;
use std::path::Path;

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
    // Apply edits per document, in order, so several locks in one document
    // compose and every write checks the file it read.
    for resolution in resolutions.iter().filter(|resolution| resolution.is_stale()) {
        let lock = &resolution.lock;
        let resolved = resolution.resolved.as_deref().expect("stale locks have a resolution");
        let file = inventory.project_root.join(&lock.resource.source_path);
        let contents = fs::read_to_string(&file)?;
        let current = current_document(&contents, lock.resource.document_index)?;
        let mut current_lock = lock.clone();
        current_lock.resource.raw_document.clone_from(&current);
        let document = replace_lock_commit(&current_lock, resolved)?;
        let updated = replace_document(&contents, lock.resource.document_index, &current, &document)?;
        atomic_replace(&file, &contents, &updated)?;
        let note = resolution
            .note
            .as_deref()
            .map(|note| format!(" ({note})"))
            .unwrap_or_default();
        println!("✓ {}: updated {} lock to {resolved}{note}", lock.owner, lock.revision);
    }
    Ok(())
}

/// The document's current text, which earlier edits in this run may have changed.
fn current_document(contents: &str, document_index: usize) -> Result<String> {
    crate::cli::resource_file::document_text(contents, document_index)
        .map(ToOwned::to_owned)
        .ok_or_else(|| NylError::config(format!("Document {document_index} is missing")))
}
