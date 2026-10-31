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
                paths.push(source.path.unwrap_or_else(|| "/".to_owned()));
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
    let checkouts = source_checkouts(spec, &driver.source_fields(spec), source_commit)?;
    let mut groups: BTreeMap<(SourceRepository, CommitId), Vec<String>> = BTreeMap::new();
    for (field, key) in checkouts {
        groups.entry(key).or_default().push(field);
    }
    let mut roots = BTreeMap::new();
    for ((repository, commit), fields) in groups {
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
                    paths.push(source.path.unwrap_or_else(|| "/".to_owned()));
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
