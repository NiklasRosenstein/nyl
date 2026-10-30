//! Read-only preparation of a unit from the current worktree or a source revision.
//! Shared by direct artifact builds and operator tools using a worktree spec.
use crate::gitops::units::{render_direct_unit, resolve_effective_environment};
use crate::gitops::{discover_gitops_inventory, GitOpsInventory};
use crate::orchestration::project::{open_store, resolve_environment};
use crate::orchestration::run::Wiring;
use crate::{NylError, Result};
use nyl_core::orchestration::driver::{ResolutionContext, SourceRoot};
use nyl_core::orchestration::ids::{CommitId, EnvironmentName, UnitName};
use nyl_core::orchestration::ports::{EnvironmentSnapshot, SourceRepository, SourceResolver, StateStore};
use nyl_core::orchestration::state::{DesiredUnit, DesiredUnitKind, Lifecycle, Readiness, StateV1, UnitIdentity};
use nyl_orchestration::rules::{
    execution_key::{execution_key, KeyMaterial},
    references::{resolve_unit, ReferenceView},
    sources,
};
use nyl_state::source::GitSourceResolver;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// A rendered, reference-resolved worktree unit and its read-only state snapshot.
pub struct DirectUnit {
    pub unit: DesiredUnit,
    pub inventory: GitOpsInventory,
    pub worktree: PathBuf,
    pub project_dir: PathBuf,
    pub source_roots: BTreeMap<String, SourceRoot>,
    pub snapshot: EnvironmentSnapshot,
    /// Owns pinned checkouts and per-command tool state until the caller finishes.
    pub workspace: tempfile::TempDir,
    pub dirty: bool,
    pub changed_paths: Vec<String>,
    pub snapshot_digest: String,
    pub audit: serde_json::Value,
}
fn config(error: impl std::fmt::Display) -> NylError {
    NylError::config(error.to_string())
}

/// Render one explicitly named unit without taking a lease or writing state.
/// No source override preserves uncommitted files in local unpinned contexts.
#[allow(clippy::too_many_lines)] // Shared rendering, reference/source resolution, snapshotting, and key computation run in this order.
pub fn prepare(
    path: &Path,
    name: &str,
    environment: Option<&str>,
    source: Option<&str>,
    wiring: &Wiring<'_>,
) -> Result<DirectUnit> {
    let entry = discover_gitops_inventory(path, None)?;
    let repository = git2::Repository::discover(&entry.project_root).map_err(config)?;
    let head = repository
        .head()
        .and_then(|head| head.peel_to_commit())
        .map_err(config)?;
    let head = CommitId::new(head.id().to_string()).map_err(config)?;
    let auth = crate::cli::git_auth::project_git_auth(&entry.project_config.config)?;
    let resolver = GitSourceResolver::new(repository.path(), auth);
    let workspace = tempfile::TempDir::new()?;
    let commit = source
        .map(|source| {
            resolver
                .resolve_revision(&SourceRepository::This, source)
                .map_err(config)
        })
        .transpose()?
        .unwrap_or(head);
    let inventory = if source.is_some() {
        let tree = workspace.path().join("source");
        resolver
            .worktree(&SourceRepository::This, &commit, &tree)
            .map_err(config)?;
        let project = entry.project_root.strip_prefix(&entry.worktree_root).map_err(config)?;
        let located = crate::util::project_path::locate_checkout_project(
            &tree,
            project,
            &[],
            &entry.project_config.config.project.previous_paths,
        )?;
        discover_gitops_inventory(&located.directory, None)?
    } else {
        entry.clone()
    };
    let name = UnitName::new(name).map_err(config)?;
    let selected = environment.map(EnvironmentName::new).transpose().map_err(config)?;
    let effective = selected
        .as_ref()
        .map(|name| resolve_effective_environment(&inventory, name))
        .transpose()?;
    let (envelope, spec) = render_direct_unit(&inventory, &name, effective.as_ref())?;
    let driver = wiring
        .drivers
        .get(&envelope.api_version, &envelope.kind)
        .ok_or_else(|| config(format!("no driver serves {} {}", envelope.api_version, envelope.kind)))?;
    driver.validate_spec(&spec).map_err(config)?;
    let mut snapshot = EnvironmentSnapshot::default();
    let mut environments = BTreeMap::new();
    if let Some(environment) = selected.as_ref() {
        let resolved = resolve_environment(&entry, environment.as_str()).map_err(config)?;
        snapshot = open_store(&entry, &resolved, None)?
            .snapshot(&resolved.location)
            .map_err(config)?;
        for name in nyl_orchestration::rules::multi_environment::dependencies(&spec) {
            if name == *environment {
                continue;
            }
            let resolved = resolve_environment(&entry, name.as_str()).map_err(config)?;
            let state = open_store(&entry, &resolved, None)?
                .snapshot(&resolved.location)
                .map_err(config)?;
            environments.insert(name, state);
        }
    } else if !nyl_core::bindings::unit_references("", &spec)
        .map_err(|errors| config(errors.join("; ")))?
        .is_empty()
        || spec
            .get("dependsOn")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|entries| !entries.is_empty())
    {
        return Err(config(
            "unit references require -e <environment> so a direct operation can read current receipts",
        ));
    }
    let environment = selected.unwrap_or(EnvironmentName::new("local").expect("local name"));
    let mut unit = DesiredUnit {
        api_version: StateV1,
        kind: DesiredUnitKind,
        unit: UnitIdentity {
            environment: environment.clone(),
            name: name.clone(),
            uid: Some(wiring.ids.uid()),
            api_version: envelope.api_version,
            kind: envelope.kind,
        },
        lifecycle: Lifecycle::Active,
        source_commit: Some(commit.clone()),
        source_revision: Some(commit.clone()),
        spec: spec.clone(),
        resolved_spec: None,
        provenance: BTreeMap::new(),
        execution_key: None,
        fingerprint: None,
        attestations: driver.attestations(&spec),
        readiness: Readiness::Ready,
    };
    let resolved = resolve_unit(
        &ReferenceView {
            environments: &environments,
            environment: &environment,
            desired: &snapshot.desired,
            observed: &snapshot.observed,
            artifacts: &snapshot.artifacts,
            observed_commit: snapshot.tips.observed.as_ref(),
            run: None,
            produced_in_run: &BTreeSet::new(),
            previous: &snapshot.desired,
        },
        &unit,
    );
    if !resolved.errors.is_empty() {
        return Err(config(resolved.errors.join("; ")));
    }
    let mut spec = resolved
        .resolved_spec
        .ok_or_else(|| config(format!("unit {name} has unresolved inputs: {:?}", resolved.readiness)))?;
    unit.provenance = resolved.provenance;
    let fields = driver.source_fields(&spec);
    let repositories = inventory
        .resources
        .values()
        .filter_map(|resource| match &resource.resource {
            Some(crate::resources::GitOpsResource::GitRepository(repository)) => {
                Some((repository.metadata.name.clone(), repository.spec.repo_url.clone()))
            }
            _ => None,
        })
        .collect();
    sources::resolve_repository_refs(&mut spec, &fields, &repositories).map_err(config)?;
    let unpinned = fields
        .iter()
        .filter_map(|pointer| {
            sources::git_source(&spec, pointer)
                .ok()
                .flatten()
                .filter(|source| !source.is_remote() && source.revision.is_none() && source.commit.is_none())
                .map(|_| pointer.clone())
        })
        .collect::<BTreeSet<_>>();
    let mut revisions = BTreeMap::new();
    for lock in sources::source_revisions(&spec, &fields).map_err(config)? {
        if let Some(locked) = &lock.commit {
            let default = resolver.default_branch(&lock.repository).map_err(config)?;
            let refs = effective.as_ref().map_or_else(
                || vec![default.clone(), lock.revision.clone()],
                |environment| sources::pin_refs(environment, &default, Some(&lock.revision)),
            );
            if resolver
                .reachable_from(&lock.repository, locked, &refs)
                .map_err(config)?
                .is_none()
            {
                return Err(config(format!("locked source {} at {locked} is not reachable from {}; push it to a protected or declared source ref", lock.field, refs.join(", "))));
            }
        } else {
            let commit = resolver
                .resolve_revision(&lock.repository, &lock.revision)
                .map_err(config)?;
            revisions.insert((lock.repository, lock.revision), commit);
        }
    }
    let project_relative = inventory
        .project_root
        .strip_prefix(&inventory.worktree_root)
        .map_err(config)?
        .to_str()
        .ok_or_else(|| config("project directory must be UTF-8"))?;
    let project_record = if project_relative.is_empty() {
        "/".to_owned()
    } else {
        format!("/{project_relative}")
    };
    sources::pin_source_fields(&mut spec, &fields, &project_record, &commit, &revisions).map_err(config)?;
    let checkout_fields = checkout_source_fields(&fields, &unpinned, source.is_some());
    let mut roots = nyl_orchestration::shell::sources::prepare_source_roots_for_fields(
        &resolver,
        driver,
        &spec,
        &commit,
        &project_record,
        &workspace.path().join("contexts"),
        &checkout_fields,
    )
    .map_err(config)?;
    if source.is_none() {
        for pointer in &unpinned {
            roots.insert(
                pointer.clone(),
                SourceRoot {
                    worktree: inventory.worktree_root.clone(),
                    project_dir: inventory.project_root.clone(),
                },
            );
        }
    }
    unit.resolved_spec = Some(spec.clone());
    let mut prepared = DirectUnit {
        worktree: inventory.worktree_root.clone(),
        project_dir: inventory.project_root.clone(),
        inventory,
        unit,
        source_roots: roots,
        snapshot,
        workspace,
        dirty: false,
        changed_paths: Vec::new(),
        snapshot_digest: String::new(),
        audit: serde_json::Value::Null,
    };
    let primary_remote = fields
        .first()
        .and_then(|field| sources::git_source(&spec, field).ok().flatten())
        .is_some_and(|source| source.is_remote());
    let mut validation_spec = spec.clone();
    sources::root_local_paths(
        &mut validation_spec,
        &driver.local_path_fields(&spec),
        if primary_remote { "/" } else { &project_record },
    )
    .map_err(config)?;
    let local_inputs_in_worktree = source.is_none() && fields.first().is_none_or(|field| unpinned.contains(field));
    if !local_inputs_in_worktree {
        nyl_orchestration::shell::sources::validate_local_inputs(&resolver, driver, &validation_spec, &commit)
            .map_err(config)?;
    }
    snapshot_entry(&mut prepared, driver, &validation_spec, local_inputs_in_worktree)?;
    let now = wiring.clock.now();
    let mut key_spec = spec.clone();
    if let Some(env) = key_spec.get_mut("env").and_then(serde_json::Value::as_object_mut) {
        env.remove("secrets");
    }
    let env =
        nyl_orchestration::shell::run::execution_environment(&wiring.process_env, None, &key_spec).map_err(config)?;
    let deadline = nyl_orchestration::shell::run::unit_timeout(wiring.drivers, &prepared.unit).after(now);
    let tools = nyl_orchestration::shell::tools::DeadlineRunner {
        tools: wiring.tools,
        clock: wiring.clock.as_ref(),
        deadline,
    };
    let context = ResolutionContext {
        environment,
        worktree: prepared.worktree.clone(),
        project_dir: prepared.project_dir.clone(),
        source_roots: prepared.source_roots.clone(),
        env,
        deadline,
        now,
        tools: &tools,
        cache_dir: prepared.workspace.path().join("cache"),
    };
    let inputs = driver.execution_key_inputs(&context, &spec).map_err(config)?;
    prepared.unit.execution_key = Some(
        execution_key(&KeyMaterial {
            api_version: &prepared.unit.unit.api_version,
            kind: &prepared.unit.unit.kind,
            behavior_version: driver.behavior_version(),
            resolved_spec: &spec,
            source_fields: &fields,
            inputs: &inputs,
        })
        .map_err(config)?,
    );
    Ok(prepared)
}

fn checkout_source_fields(fields: &[String], unpinned: &BTreeSet<String>, explicit_source: bool) -> Vec<String> {
    fields
        .iter()
        .filter(|field| explicit_source || !unpinned.contains(*field))
        .cloned()
        .collect()
}

fn validate_worktree_input(required: &[(&str, bool)], path: &str, is_submodule: bool, contents: &[u8]) -> Result<()> {
    let lfs = nyl_state::source::worktree::is_lfs_pointer(contents);
    if !is_submodule && !lfs {
        return Ok(());
    }
    for (input, recursive) in required {
        if nyl_state::source::worktree::source_path_matches(input, path, *recursive, is_submodule).map_err(config)? {
            let reason = if is_submodule {
                format!("submodule {path}; vendor its contents into the source repository")
            } else {
                format!("Git LFS file {path}; hydrate it")
            };
            return Err(config(format!(
                "source path {input} requires {reason} before running Nyl"
            )));
        }
    }
    Ok(())
}

/// Copy Git-visible worktree files into invocation-owned storage before a native
/// tool can write. Recorded metadata lets held execution explain spec differences.
#[allow(clippy::too_many_lines)] // One snapshot owns input validation, copying, and provenance.
pub fn snapshot_entry(
    prepared: &mut DirectUnit,
    driver: &dyn nyl_core::orchestration::driver::Driver,
    validation_spec: &serde_json::Value,
    local_inputs_in_worktree: bool,
) -> Result<()> {
    let original = prepared.worktree.clone();
    let repository = git2::Repository::discover(&original).map_err(config)?;
    let (files, changed) = nyl_state::source::worktree::visible_files(&repository).map_err(config)?;
    let destination = prepared.workspace.path().join("worktree-snapshot");
    std::fs::create_dir_all(&destination)?;
    let mut digests = BTreeMap::new();
    let index = repository.index().map_err(config)?;
    let spec = prepared
        .unit
        .resolved_spec
        .as_ref()
        .ok_or_else(|| config("unit has no resolved specification"))?;
    let mut required = Vec::new();
    for field in driver.local_path_fields(validation_spec) {
        if !local_inputs_in_worktree {
            break;
        }
        if let Some(value) = validation_spec.pointer(&field.pointer) {
            if let Some(path) = value.as_str() {
                required.push((path, false));
            }
            if let Some(paths) = value.as_array() {
                required.extend(
                    paths
                        .iter()
                        .filter_map(serde_json::Value::as_str)
                        .map(|path| (path, false)),
                );
            }
        }
    }
    let default_source_path = format!(
        "/{}",
        prepared
            .project_dir
            .strip_prefix(&original)
            .map_err(config)?
            .to_str()
            .ok_or_else(|| config("project directory must be UTF-8"))?
    );
    for (field, root) in &prepared.source_roots {
        if root.worktree == original {
            let path = spec
                .pointer(field)
                .and_then(|source| source.get("path"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or(&default_source_path);
            required.push((path, true));
        }
    }
    for entry in index.iter().filter(|entry| entry.mode == 0o160_000) {
        let link = std::str::from_utf8(&entry.path).map_err(config)?;
        validate_worktree_input(&required, link, true, &[])?;
    }
    for relative in files {
        if index
            .get_path(std::path::Path::new(&relative), 0)
            .is_some_and(|entry| entry.mode == 0o160_000)
        {
            continue;
        }
        let Some(metadata) =
            nyl_state::source::worktree::snapshot_file_metadata(&original, &relative).map_err(config)?
        else {
            continue;
        };
        let contents =
            nyl_state::source::worktree::snapshot_file_contents(&original, &relative, &metadata).map_err(config)?;
        if !metadata.file_type().is_symlink() {
            validate_worktree_input(&required, &relative, false, &contents)?;
        }
        let mut bytes = contents.clone();
        bytes.push(u8::from(metadata.file_type().is_symlink()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            bytes.extend_from_slice(&(metadata.permissions().mode() & 0o7777).to_le_bytes());
        }
        digests.insert(relative.clone(), nyl_core::digest::sha256_prefixed(&bytes));
        let target = destination.join(relative);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if metadata.file_type().is_symlink() {
            let link = std::str::from_utf8(&contents).map_err(config)?;
            #[cfg(unix)]
            std::os::unix::fs::symlink(link, &target)?;
            #[cfg(windows)]
            std::os::windows::fs::symlink_file(link, &target)?;
        } else {
            std::fs::write(&target, contents)?;
            std::fs::set_permissions(target, metadata.permissions())?;
        }
    }
    prepared.snapshot_digest =
        nyl_core::digest::sha256_prefixed(&nyl_core::digest::canonical_json_bytes(&digests).map_err(config)?);
    prepared.dirty = !changed.is_empty();
    prepared.changed_paths = changed;
    prepared.audit = serde_json::json!({"sourceCommit":prepared.unit.source_commit,"dirty":prepared.dirty,"snapshotDigest":prepared.snapshot_digest,"changedPaths":prepared.changed_paths});
    let project = prepared.project_dir.strip_prefix(&original).map_err(config)?;
    prepared.project_dir = destination.join(project);
    for root in prepared.source_roots.values_mut() {
        if root.worktree == original {
            let project = root.project_dir.strip_prefix(&original).map_err(config)?;
            root.project_dir = destination.join(project);
            root.worktree.clone_from(&destination);
        }
    }
    prepared.worktree = destination;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use nyl_core::orchestration::driver::LocalPathField;

    #[test]
    fn test_nested_project_inputs_reject_required_worktree_submodules_and_lfs_pointers() {
        let mut spec = serde_json::json!({"files": ["*/entry.sh", "asset.bin"]});
        let fields = vec![LocalPathField {
            pointer: "/files".to_owned(),
            defaults_to_project_dir: false,
        }];
        sources::root_local_paths(&mut spec, &fields, "/nyl").unwrap();
        let required: Vec<_> = spec["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|path| (path.as_str().unwrap(), false))
            .collect();
        assert!(validate_worktree_input(&required, "other/module", true, &[]).is_ok());
        assert!(validate_worktree_input(&required, "nyl/module", true, &[])
            .unwrap_err()
            .to_string()
            .contains("requires submodule nyl/module"));
        let pointer = b"version https://git-lfs.github.com/spec/v1\noid sha256:123\nsize 1\n";
        assert!(validate_worktree_input(&required, "asset.bin", false, pointer).is_ok());
        assert!(validate_worktree_input(&required, "nyl/asset.bin", false, pointer)
            .unwrap_err()
            .to_string()
            .contains("requires Git LFS file nyl/asset.bin"));
        assert!(validate_worktree_input(&required, "nyl/asset.bin", false, b"hydrated").is_ok());
        let fields = vec!["/context".to_owned(), "/locked".to_owned()];
        let unpinned = BTreeSet::from(["/context".to_owned()]);
        assert_eq!(checkout_source_fields(&fields, &unpinned, false), ["/locked"]);
        assert_eq!(checkout_source_fields(&fields, &unpinned, true), fields);
        let default_source = [("/nyl", true)];
        assert!(validate_worktree_input(&default_source, "nyl/untracked.bin", false, pointer).is_err());
        assert!(validate_worktree_input(&default_source, "outside.bin", false, pointer).is_ok());
        assert!(validate_worktree_input(&default_source, "nyl/untracked.bin", false, b"hydrated").is_ok());
    }
}
