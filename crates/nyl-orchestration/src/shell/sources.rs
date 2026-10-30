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

/// Capture one source for preflight and execution, reusing retained state evidence
/// when its tree is identical. No process environment or user index is modified.
pub fn snapshot_worktree(
    store: &dyn nyl_core::orchestration::ports::StateStore,
    sources: &dyn SourceResolver,
    location: &nyl_core::orchestration::state::StateLocation,
    base: &CommitId,
    identity: &nyl_core::orchestration::ports::CommitIdentity,
    clock: &dyn nyl_core::orchestration::ports::Clock,
) -> Result<nyl_core::orchestration::ports::WorktreeSnapshot, String> {
    let previous = store.snapshot(location).map_err(|error| error.to_string())?;
    sources
        .snapshot_worktree(
            base,
            identity,
            clock.now(),
            previous.environment.as_ref().map(|record| &record.source_commit),
        )
        .map_err(|error| error.to_string())
}

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

/// Check declared local paths before handing a checkout to a unit.
pub fn validate_local_inputs(
    sources: &dyn SourceResolver,
    driver: &dyn Driver,
    spec: &Value,
    commit: &CommitId,
) -> Result<(), String> {
    let fields = driver.source_fields(spec);
    let checkouts = source_checkouts(spec, &fields, commit)?;
    let (repository, input_commit) = primary_input_source(&fields, &checkouts, commit);
    for field in driver.local_path_fields(spec) {
        let Some(value) = spec.pointer(&field.pointer) else {
            continue;
        };
        let values: Vec<&str> = match value {
            Value::String(path) => vec![path],
            Value::Array(paths) => paths.iter().filter_map(Value::as_str).collect(),
            _ => Vec::new(),
        };
        for path in values {
            sources
                .validate_source_path(repository, input_commit, path, false)
                .map_err(|error| format!("cannot reconstruct {}: {error}", field.pointer))?;
        }
    }
    Ok(())
}

fn primary_input_source<'a>(
    fields: &[String],
    checkouts: &'a BTreeMap<String, (SourceRepository, CommitId)>,
    fallback: &'a CommitId,
) -> (&'a SourceRepository, &'a CommitId) {
    fields
        .first()
        .and_then(|field| checkouts.get(field))
        .map_or((&SourceRepository::This, fallback), |(repository, commit)| {
            (repository, commit)
        })
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
    prepare_source_roots_for_fields(
        sources,
        driver,
        spec,
        source_commit,
        project_dir,
        directory,
        &driver.source_fields(spec),
    )
}

/// Prepare only the selected source fields; omitted fields can use an invocation-owned worktree.
#[allow(clippy::too_many_arguments)] // The field selection shares the ordinary checkout inputs.
pub fn prepare_source_roots_for_fields(
    sources: &dyn SourceResolver,
    driver: &dyn Driver,
    spec: &Value,
    source_commit: &CommitId,
    project_dir: &str,
    directory: &Path,
    fields: &[String],
) -> Result<BTreeMap<String, SourceRoot>, String> {
    let mut roots = BTreeMap::new();
    for (field, (repository, commit)) in source_checkouts(spec, fields, source_commit)? {
        let suffix = nyl_core::digest::sha256_hex(field.as_bytes());
        let worktree = directory.join(&suffix[..16]);
        let mut paths = checkout_paths(driver, spec, project_dir, &repository);
        if let Some(source) = git_source(spec, &field)? {
            let source_path = source.path.unwrap_or_else(|| {
                if repository == SourceRepository::This {
                    project_dir.to_owned()
                } else {
                    "/".to_owned()
                }
            });
            sources
                .validate_source_path(&repository, &commit, &source_path, true)
                .map_err(|error| format!("cannot reconstruct source {field} at {commit}: {error}"))?;
            if let Some(paths) = &mut paths {
                paths.push(source_path);
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_local_inputs_use_primary_pinned_commit_in_same_repository() {
        let entry = CommitId::new("1111111111111111111111111111111111111111").unwrap();
        let pinned = CommitId::new("2222222222222222222222222222222222222222").unwrap();
        let checkouts = BTreeMap::from([("/source".to_owned(), (SourceRepository::This, pinned.clone()))]);
        assert_eq!(
            primary_input_source(&["/source".to_owned()], &checkouts, &entry),
            (&SourceRepository::This, &pinned)
        );
        assert_eq!(
            primary_input_source(&[], &checkouts, &entry),
            (&SourceRepository::This, &entry)
        );
    }
}
