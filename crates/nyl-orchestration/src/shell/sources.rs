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
    let snapshot = sources
        .snapshot_worktree(
            base,
            identity,
            clock.now(),
            previous.environment.as_ref().map(|record| &record.source_commit),
        )
        .map_err(|error| error.to_string())?;
    store
        .prepare_worktree_snapshot(location, &snapshot.commit)
        .map_err(|error| error.to_string())?;
    Ok(snapshot)
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

/// Include all local source paths sharing the main checkout in its sparse set.
pub fn main_checkout_paths(
    driver: &dyn Driver,
    spec: &Value,
    source_commit: &CommitId,
    main_commit: &CommitId,
    project_dir: &str,
) -> Result<Option<Vec<String>>, String> {
    let mut paths = checkout_paths(driver, spec, project_dir, &SourceRepository::This);
    for (field, (repository, commit)) in source_checkouts(spec, &driver.source_fields(spec), source_commit)? {
        if repository == SourceRepository::This && &commit == main_commit {
            if let (Some(paths), Some(source)) = (&mut paths, git_source(spec, &field)?) {
                paths.push(source.path.unwrap_or_else(|| {
                    if repository == SourceRepository::This {
                        project_dir.to_owned()
                    } else {
                        "/".to_owned()
                    }
                }));
            }
        }
    }
    if let Some(paths) = &mut paths {
        paths.sort();
        paths.dedup();
    }
    Ok(paths)
}

/// Prepare resolved source fields, sharing a checkout when repository and commit match.
pub fn prepare_source_roots(
    sources: &dyn SourceResolver,
    driver: &dyn Driver,
    spec: &Value,
    source_commit: &CommitId,
    project_dir: &str,
    directory: &Path,
    main: Option<(&Path, &CommitId)>,
) -> Result<BTreeMap<String, SourceRoot>, String> {
    prepare_source_roots_for_fields(
        sources,
        driver,
        spec,
        source_commit,
        project_dir,
        directory,
        main,
        &driver.source_fields(spec),
    )
}

/// Prepare selected source fields, sharing checkouts by repository and commit.
#[allow(clippy::too_many_arguments)] // The field selection shares the ordinary checkout inputs.
pub fn prepare_source_roots_for_fields(
    sources: &dyn SourceResolver,
    driver: &dyn Driver,
    spec: &Value,
    source_commit: &CommitId,
    project_dir: &str,
    directory: &Path,
    main: Option<(&Path, &CommitId)>,
    selected_fields: &[String],
) -> Result<BTreeMap<String, SourceRoot>, String> {
    let checkouts = source_checkouts(spec, selected_fields, source_commit)?;
    let mut groups: BTreeMap<(SourceRepository, CommitId), Vec<String>> = BTreeMap::new();
    for (field, key) in checkouts {
        groups.entry(key).or_default().push(field);
    }
    let mut roots = BTreeMap::new();
    for ((repository, commit), fields) in groups {
        for field in &fields {
            if let Some(source) = git_source(spec, field)? {
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
            }
        }
        let worktree = if let Some((worktree, _)) =
            main.filter(|(_, main_commit)| repository == SourceRepository::This && *main_commit == &commit)
        {
            worktree.to_path_buf()
        } else {
            let suffix = nyl_core::digest::sha256_hex(fields[0].as_bytes());
            let worktree = directory.join(&suffix[..16]);
            let mut paths = checkout_paths(driver, spec, project_dir, &repository);
            for field in &fields {
                if let (Some(paths), Some(source)) = (&mut paths, git_source(spec, field)?) {
                    paths.push(source.path.unwrap_or_else(|| {
                        if repository == SourceRepository::This {
                            project_dir.to_owned()
                        } else {
                            "/".to_owned()
                        }
                    }));
                }
            }
            if let Some(paths) = &mut paths {
                paths.sort();
                paths.dedup();
            }
            sources
                .reset_worktree_with_paths(&repository, &commit, &worktree, paths.as_deref())
                .map_err(|error| format!("cannot check out source {} at {commit}: {error}", fields[0]))?;
            worktree
        };
        let project_dir = if repository == SourceRepository::This {
            project_path(&worktree, project_dir)
        } else {
            worktree.clone()
        };
        for field in fields {
            roots.insert(
                field,
                SourceRoot {
                    worktree: worktree.clone(),
                    project_dir: project_dir.clone(),
                },
            );
        }
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
