//! Independent source-field checkouts shared by resolution and execution.
//!
//! Contract: [Units](../../../../design/orchestration-core.md#units), Git source shape.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use nyl_core::orchestration::driver::{Driver, SourceRoot};
use nyl_core::orchestration::ids::CommitId;
use nyl_core::orchestration::ports::{SourceRepository, SourceResolver};
use serde_json::Value;

use crate::rules::sources::{git_source, source_checkouts};

/// Declared inputs plus the local configuration directory. Remote source
/// checkouts have no local configuration directory to include.
pub fn checkout_paths(
    driver: &dyn Driver,
    spec: &Value,
    project_dir: &str,
    repository: &SourceRepository,
) -> Option<Vec<String>> {
    let mut paths = driver.checkout_paths(spec)?;
    if *repository == SourceRepository::This {
        paths.push(if project_dir.trim_matches('/').is_empty() {
            "/nyl.toml".to_owned()
        } else {
            project_dir.to_owned()
        });
    }
    paths.sort();
    paths.dedup();
    Some(paths)
}

/// Prepare every resolved Git source field at its own repository and commit.
/// Callers own the directory; existing private checkouts are reset between calls.
pub fn prepare_source_roots(
    sources: &dyn SourceResolver,
    driver: &dyn Driver,
    spec: &Value,
    source_commit: &CommitId,
    project_dir: &str,
    directory: &Path,
) -> Result<BTreeMap<String, SourceRoot>, String> {
    let fields = driver.source_fields(spec);
    let mut roots = BTreeMap::new();
    for (field, (repository, commit)) in source_checkouts(spec, &fields, source_commit)? {
        let suffix = nyl_core::digest::sha256_hex(field.as_bytes());
        let worktree = directory.join(&suffix[..16]);
        let mut paths = checkout_paths(driver, spec, project_dir, &repository);
        if let (Some(paths), Some(source)) = (&mut paths, git_source(spec, &field)?) {
            paths.push(source.path.unwrap_or_else(|| "/".to_owned()));
        }
        sources
            .reset_worktree_with_paths(&repository, &commit, &worktree, paths.as_deref())
            .map_err(|error| format!("cannot check out source {field} at {commit}: {error}"))?;
        let project_dir = if repository == SourceRepository::This {
            project_path(&worktree, project_dir)
        } else {
            worktree.clone()
        };
        roots.insert(field, SourceRoot { worktree, project_dir });
    }
    Ok(roots)
}

/// Resolve a recorded worktree-rooted directory beneath a private checkout.
pub fn project_path(worktree: &Path, directory: &str) -> PathBuf {
    worktree.join(directory.trim_start_matches('/'))
}
