//! Source locks: groups, Release inputs, unit sources and named contexts,
//! and Environment sources, refreshed together by `nyl update source-locks`.
//!
//! Contract: [`fromGit`](../../../../design/release-inputs.md#binding-kinds).
//!
//! - Every lock moves to the head of its revision. Moving a lock to what
//!   another target runs is promotion, not a lock update.
//! - The `--group`/`--target` filter selects which locks move, never where
//!   they move, so a filtered update agrees with an unfiltered `--check`.
//! - Each repository is fetched once per run, and each branch head is looked
//!   up once.
//! - Locks are addressed by their position in the document, never by matching
//!   the commit text alone, so bindings that share a commit but name different
//!   revisions stay independent.

use std::collections::{BTreeMap, BTreeSet};

use crate::git::GitManager;
use crate::resources::{GitOpsResource, GitOpsResourceKind};
use crate::{NylError, Result};

use super::GitOpsInventory;

/// The resource field that holds a lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockOwner {
    /// `ApplicationGroup.spec.source.commit`.
    ApplicationGroup { name: String },
    /// `DeploymentTarget.spec.releaseInputs.<key>.<input>.fromGit.commit`.
    ReleaseInput { target: String, key: String, input: String },
    /// A unit or Environment Git source field, addressed by its document path.
    Source {
        kind: String,
        name: String,
        path: Vec<String>,
    },
}

impl std::fmt::Display for LockOwner {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ApplicationGroup { name } => write!(formatter, "ApplicationGroup {name}"),
            Self::ReleaseInput { target, key, input } => write!(formatter, "DeploymentTarget {target} {key}/{input}"),
            Self::Source { kind, name, path } => write!(formatter, "{kind} {name} {}", path.join(".")),
        }
    }
}

/// One commit lock in a project resource.
#[derive(Debug, Clone)]
pub struct SourceLock {
    pub owner: LockOwner,
    pub source_path: std::path::PathBuf,
    pub document_index: usize,
    pub raw_document: String,
    pub repository_url: String,
    /// This project uses the clone's local and origin-tracking refs without a second clone.
    pub local: bool,
    pub revision: String,
    pub commit: String,
}

/// A lock and the commit it should name.
#[derive(Debug, Clone)]
pub struct LockResolution {
    pub lock: SourceLock,
    /// The commit the lock should name.
    pub resolved: String,
}

impl LockResolution {
    /// Whether the lock names a different commit than it should.
    pub fn is_stale(&self) -> bool {
        self.resolved != self.lock.commit
    }
}

/// Collect the locks selected by the ApplicationGroup and DeploymentTarget
/// filters. Without filters every lock is selected; with one or both, only
/// the named resources' locks are.
pub fn collect_source_locks(
    inventory: &GitOpsInventory,
    group: Option<&str>,
    target: Option<&str>,
    source_fields: &dyn Fn(&str, &str, &serde_json::Value) -> Vec<String>,
) -> Result<Vec<SourceLock>> {
    let groups_selected = group.is_some() || target.is_none();
    let targets_selected = target.is_some() || group.is_none();
    if let Some(resource) = inventory.resources.values().find(|resource| {
        resource.identity.kind == GitOpsResourceKind::ApplicationGroup
            && resource.resource.is_none()
            && groups_selected
            && group.is_none_or(|requested| requested == resource.identity.name)
    }) {
        return Err(NylError::config(format!(
            "ApplicationGroup {:?} must render to a complete static resource for source-lock update",
            resource.identity.name
        )));
    }

    let mut locks = Vec::new();
    let mut group_found = false;
    let mut target_found = false;
    for discovered in inventory.resources.values() {
        match &discovered.resource {
            Some(GitOpsResource::ApplicationGroup(resource))
                if groups_selected && group.is_none_or(|requested| requested == resource.metadata.name) =>
            {
                let Some(source) = resource.spec.source.as_ref().filter(|source| source.is_remote()) else {
                    continue;
                };
                let source = &source.git;
                group_found = true;
                let repository = inventory.resolve_git_source(source)?.0;
                locks.push(SourceLock {
                    owner: LockOwner::ApplicationGroup {
                        name: resource.metadata.name.clone(),
                    },
                    source_path: discovered.source_path.clone(),
                    document_index: discovered.document_index,
                    raw_document: discovered.raw_document.clone(),
                    repository_url: repository.repo_url,
                    local: false,
                    revision: source.revision.clone().expect("validated remote source has revision"),
                    commit: source.commit.clone().expect("validated remote source has commit"),
                });
            }
            Some(GitOpsResource::DeploymentTarget(resource))
                if targets_selected && target.is_none_or(|requested| requested == resource.metadata.name) =>
            {
                target_found = true;
                for (key, inputs) in &resource.spec.release_inputs {
                    for (input, binding) in inputs {
                        let Some(source) = binding.from_git.as_ref().map(|source| &source.git) else {
                            continue;
                        };
                        let repository = inventory.resolve_git_source(source)?.0;
                        locks.push(SourceLock {
                            owner: LockOwner::ReleaseInput {
                                target: resource.metadata.name.clone(),
                                key: key.clone(),
                                input: input.clone(),
                            },
                            source_path: discovered.source_path.clone(),
                            document_index: discovered.document_index,
                            raw_document: discovered.raw_document.clone(),
                            repository_url: repository.repo_url,
                            local: false,
                            revision: source.revision.clone().expect("validated fromGit source has revision"),
                            commit: source.commit.clone().expect("validated fromGit source has commit"),
                        });
                    }
                }
            }
            _ => {}
        }
    }
    if group.is_none() && target.is_none() {
        collect_unit_locks(inventory, source_fields, &mut locks)?;
        collect_environment_locks(inventory, &mut locks)?;
    }
    if let Some(requested) = group.filter(|_| !group_found) {
        return Err(NylError::config(format!(
            "Remote ApplicationGroup {requested:?} was not found"
        )));
    }
    if let Some(requested) = target.filter(|_| !target_found) {
        return Err(NylError::config(format!(
            "DeploymentTarget {requested:?} was not found"
        )));
    }
    locks.sort_by_key(|lock| lock.owner.to_string());
    Ok(locks)
}

fn collect_unit_locks(
    inventory: &GitOpsInventory,
    source_fields: &dyn Fn(&str, &str, &serde_json::Value) -> Vec<String>,
    locks: &mut Vec<SourceLock>,
) -> Result<()> {
    for (name, unit) in super::units::discover_units(inventory)? {
        let spec = match super::units::render_direct_unit(inventory, &name, None) {
            Ok((_, spec)) => spec,
            Err(error) => {
                tracing::warn!(
                    "{}: cannot inspect source locks for {} {}: {error}; make the unit render without environment values or refresh its locks manually",
                    nyl_core::orchestration::codes::Code::UnitInvalid,
                    unit.envelope.kind,
                    name
                );
                continue;
            }
        };
        for pointer in source_fields(&unit.envelope.api_version, &unit.envelope.kind, &spec) {
            let Some(value) = spec.pointer(&pointer).filter(|value| value.get("commit").is_some()) else {
                continue;
            };
            let source: crate::resources::GitSource = serde_json::from_value(value.clone())
                .map_err(|error| NylError::config(format!("{} {} {pointer}: {error}", unit.envelope.kind, name)))?;
            let mut path = vec!["spec".to_owned()];
            path.extend(
                pointer
                    .trim_start_matches('/')
                    .split('/')
                    .map(|segment| segment.replace("~1", "/").replace("~0", "~")),
            );
            let owner = LockOwner::Source {
                kind: unit.envelope.kind.clone(),
                name: name.to_string(),
                path,
            };
            if let Some(lock) = collect_git_source(
                inventory,
                owner,
                (&unit.source_path, unit.document_index, &unit.raw_document),
                source,
            )? {
                locks.push(lock);
            }
        }
    }
    Ok(())
}

fn collect_environment_locks(inventory: &GitOpsInventory, locks: &mut Vec<SourceLock>) -> Result<()> {
    for resource in inventory.resources.values() {
        let Some(GitOpsResource::Environment(environment)) = &resource.resource else {
            continue;
        };
        let Some(source) = &environment.spec.source else {
            continue;
        };
        let owner = LockOwner::Source {
            kind: "Environment".to_owned(),
            name: environment.metadata.name.clone(),
            path: vec!["spec".to_owned(), "source".to_owned()],
        };
        if let Some(lock) = collect_git_source(
            inventory,
            owner,
            (&resource.source_path, resource.document_index, &resource.raw_document),
            source.git.clone(),
        )? {
            locks.push(lock);
        }
    }
    Ok(())
}

fn collect_git_source(
    inventory: &GitOpsInventory,
    owner: LockOwner,
    (source_path, document_index, raw_document): (&std::path::Path, usize, &str),
    source: crate::resources::GitSource,
) -> Result<Option<SourceLock>> {
    let (Some(revision), Some(commit)) = (&source.revision, &source.commit) else {
        return Ok(None);
    };
    if nyl_core::template_syntax::has_template_syntax(revision)
        || nyl_core::template_syntax::has_template_syntax(commit)
    {
        return Err(NylError::config(format!(
            "{owner}: revision and commit must be literal for source-lock update"
        )));
    }
    let local = !source.is_remote();
    let repository_url = if source.is_remote() {
        inventory.resolve_git_source(&source)?.0.repo_url
    } else {
        inventory.worktree_root.to_string_lossy().into_owned()
    };
    Ok(Some(SourceLock {
        owner,
        source_path: source_path.to_owned(),
        document_index,
        raw_document: raw_document.to_owned(),
        repository_url,
        local,
        revision: revision.clone(),
        commit: commit.clone(),
    }))
}

/// Resolve every lock to the commit it should name: the head of its revision.
///
/// Contract: [`fromGit`](../../../../design/release-inputs.md#binding-kinds),
/// source locks. Refs are fetched once per repository URL and each revision
/// is resolved once, so locks of one repository and revision move together.
pub fn resolve_source_locks(locks: Vec<SourceLock>, manager: &mut GitManager) -> Result<Vec<LockResolution>> {
    // Key fetches and lookups by the exact URL handed to the Git manager:
    // equal repositories may be spelled differently, and each spelling can
    // map to its own cached copy.
    let mut fetched = BTreeSet::new();
    let mut heads = BTreeMap::<(String, String, bool), git2::Oid>::new();
    let mut resolutions = Vec::new();
    for lock in locks {
        let url = lock.repository_url.clone();
        if !lock.local && fetched.insert(url.clone()) {
            manager.fetch_refs(&url).map_err(NylError::Git)?;
        }
        let head_key = (url.clone(), lock.revision.clone(), lock.local);
        let head = if let Some(head) = heads.get(&head_key) {
            *head
        } else {
            let head = if lock.local {
                resolve_project_ref(&url, &lock.revision)?
            } else {
                manager
                    .resolve_cached_ref(&url, &lock.revision)
                    .map_err(NylError::Git)?
            };
            heads.insert(head_key, head);
            head
        };
        resolutions.push(LockResolution {
            lock,
            resolved: head.to_string(),
        });
    }
    resolutions.sort_by_key(|resolution| resolution.lock.owner.to_string());
    Ok(resolutions)
}

/// Match SourceResolver::This: local branches, then origin branches, then tags.
fn resolve_project_ref(path: &str, revision: &str) -> Result<git2::Oid> {
    let repository = git2::Repository::open(path).map_err(|error| NylError::config(error.to_string()))?;
    if revision.len() == 40 && revision.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        if let Ok(commit) = git2::Oid::from_str(revision).and_then(|oid| repository.find_commit(oid)) {
            return Ok(commit.id());
        }
    }
    let candidates = if revision.starts_with("refs/") {
        vec![revision.to_owned()]
    } else {
        vec![
            format!("refs/heads/{revision}"),
            format!("refs/remotes/origin/{revision}"),
            format!("refs/tags/{revision}"),
        ]
    };
    for candidate in candidates {
        if let Ok(reference) = repository.find_reference(&candidate) {
            return reference
                .peel_to_commit()
                .map(|commit| commit.id())
                .map_err(|error| NylError::config(error.to_string()));
        }
    }
    Err(NylError::config(format!(
        "revision {revision:?} does not exist in this repository"
    )))
}

/// Replace one lock's commit in its resource document.
pub fn replace_lock_commit(lock: &SourceLock, resolved: &str) -> Result<String> {
    let path: Vec<&str> = match &lock.owner {
        // A group has one source, so its only commit scalar is the lock, even
        // when the document is templated.
        LockOwner::ApplicationGroup { .. } => {
            return replace_single_commit_scalar(&lock.raw_document, &lock.commit, resolved)
        }
        LockOwner::Source { path, .. } => path
            .iter()
            .map(String::as_str)
            .chain(std::iter::once("commit"))
            .collect(),
        LockOwner::ReleaseInput { key, input, .. } => {
            vec![
                "spec",
                "releaseInputs",
                key.as_str(),
                input.as_str(),
                "fromGit",
                "commit",
            ]
        }
    };
    replace_block_scalar(&lock.raw_document, &path, &lock.commit, resolved).ok_or_else(|| {
        NylError::config(format!(
            "Cannot locate the commit lock of {} in {}; write {} in block style, one key per line",
            lock.owner,
            lock.source_path.display(),
            path.join(".")
        ))
    })
}

fn replace_single_commit_scalar(document: &str, current: &str, resolved: &str) -> Result<String> {
    let pattern = regex::Regex::new(&format!(
        r#"(?m)^([ \t]*commit:[ \t]*)["']?{}["']?([ \t]*(?:#.*)?)$"#,
        regex::escape(current)
    ))
    .expect("escaped commit produces a valid regex");
    let matches = pattern.find_iter(document).count();
    if matches != 1 {
        return Err(NylError::config(format!(
            "Expected exactly one commit lock {current:?} in the selected document, found {matches}"
        )));
    }
    let replacement = format!("${{1}}{resolved}${{2}}");
    Ok(pattern.replacen(document, 1, replacement).into_owned())
}

/// Replace the scalar at a block-style mapping `path` when it equals `current`,
/// preserving quoting style, trailing comments, and every other line.
fn replace_block_scalar(document: &str, path: &[&str], current: &str, resolved: &str) -> Option<String> {
    let lines = document.split_inclusive('\n').collect::<Vec<_>>();
    let mut start = 0;
    let mut parent_indent: Option<usize> = None;
    let mut found = None;
    for (depth, segment) in path.iter().enumerate() {
        let mut index = start;
        let mut hit = None;
        // The first key line of the block sets the indentation of its direct
        // children; deeper lines belong to nested values or block scalars.
        let mut child_indent = None;
        while index < lines.len() {
            let line = lines[index];
            let trimmed = line.trim_start();
            let indent = line.len() - trimmed.len();
            let blank = trimmed.trim().is_empty() || trimmed.starts_with('#');
            if !blank && parent_indent.is_some_and(|parent| indent <= parent) {
                break;
            }
            if !blank && *child_indent.get_or_insert(indent) == indent && key_matches(trimmed, segment) {
                hit = Some((index, indent));
                break;
            }
            index += 1;
        }
        let (line, indent) = hit?;
        if depth + 1 == path.len() {
            found = Some(line);
        }
        start = line + 1;
        parent_indent = Some(indent);
    }
    let line = found?;
    let original = lines[line];
    let colon = original.find(':')?;
    let (head, rest) = original.split_at(colon + 1);
    let value_start = rest.len() - rest.trim_start().len();
    let value = &rest[value_start..];
    let (quote, unquoted) = match value.chars().next() {
        Some(quote @ ('"' | '\'')) => (Some(quote), &value[1..]),
        _ => (None, value),
    };
    let scalar_end = match quote {
        Some(quote) => unquoted.find(quote)?,
        None => unquoted.find([' ', '\t', '\r', '\n', '#']).unwrap_or(unquoted.len()),
    };
    if &unquoted[..scalar_end] != current {
        return None;
    }
    let tail = &unquoted[scalar_end..];
    let replaced = match quote {
        Some(quote) => format!("{head}{}{quote}{resolved}{tail}", &rest[..value_start]),
        None => format!("{head}{}{resolved}{tail}", &rest[..value_start]),
    };
    let mut output = String::with_capacity(document.len());
    for (index, text) in lines.iter().enumerate() {
        output.push_str(if index == line { &replaced } else { text });
    }
    Some(output)
}

fn key_matches(line: &str, key: &str) -> bool {
    [format!("{key}:"), format!("\"{key}\":"), format!("'{key}':")]
        .iter()
        .any(|prefix| line.starts_with(prefix.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TARGET: &str = r#"apiVersion: k8s.gitops.nyl/v1
kind: DeploymentTarget
metadata:
  name: production
spec:
  releaseInputs:
    platform/web:
      image:
        fromGit:
          repository: {repoURL: https://git.example.com/deploy.git}
          revision: deploy/dev
          commit: aaaa   # moved by source-locks
          path: dev/state/images.json
    "platform/api":
      image:
        fromGit:
          revision: deploy/staging
          commit: "aaaa"
          path: staging/state/images.json
"#;

    #[test]
    fn test_replace_block_scalar_addresses_locks_by_position() {
        let web = replace_block_scalar(
            TARGET,
            &["spec", "releaseInputs", "platform/web", "image", "fromGit", "commit"],
            "aaaa",
            "bbbb",
        )
        .unwrap();
        assert!(web.contains("commit: bbbb   # moved by source-locks"));
        assert!(web.contains("commit: \"aaaa\""));

        let api = replace_block_scalar(
            TARGET,
            &["spec", "releaseInputs", "platform/api", "image", "fromGit", "commit"],
            "aaaa",
            "cccc",
        )
        .unwrap();
        assert!(api.contains("commit: aaaa   # moved"));
        assert!(api.contains("commit: \"cccc\""));
        assert_eq!(api.len(), TARGET.len());
    }

    #[test]
    fn test_replace_block_scalar_refuses_missing_or_mismatched_locks() {
        let path = ["spec", "releaseInputs", "platform/web", "image", "fromGit", "commit"];
        assert!(replace_block_scalar(TARGET, &path, "ffff", "bbbb").is_none());
        let path = ["spec", "releaseInputs", "platform/web", "other", "fromGit", "commit"];
        assert!(replace_block_scalar(TARGET, &path, "aaaa", "bbbb").is_none());
        let flow = "spec:\n  releaseInputs:\n    g/r: {image: {fromGit: {commit: aaaa}}}\n";
        let path = ["spec", "releaseInputs", "g/r", "image", "fromGit", "commit"];
        assert!(replace_block_scalar(flow, &path, "aaaa", "bbbb").is_none());
    }

    #[test]
    fn test_replace_block_scalar_matches_only_direct_children() {
        let document = r"spec:
  releaseInputs:
    workloads/api:
      config:
        value:
          image: {fromGit: none}
          notes: |
            path:
              fromGit:
                commit: aaaa
      image:
        fromGit:
          commit: aaaa
          path: x.json
      path:
        fromGit:
          commit: aaaa
          path: y.json
";
        let image = replace_block_scalar(
            document,
            &["spec", "releaseInputs", "workloads/api", "image", "fromGit", "commit"],
            "aaaa",
            "bbbb",
        )
        .unwrap();
        assert_eq!(
            image,
            document.replacen(
                "commit: aaaa\n          path: x.json",
                "commit: bbbb\n          path: x.json",
                1
            )
        );
        let path = replace_block_scalar(
            document,
            &["spec", "releaseInputs", "workloads/api", "path", "fromGit", "commit"],
            "aaaa",
            "bbbb",
        )
        .unwrap();
        assert_eq!(
            path,
            document.replacen(
                "commit: aaaa\n          path: y.json",
                "commit: bbbb\n          path: y.json",
                1
            )
        );
    }

    #[test]
    fn test_unit_context_and_configuration_locks_are_addressed_independently() {
        let directory = tempfile::TempDir::new().unwrap();
        std::fs::write(directory.path().join("nyl.toml"), "[project]\n").unwrap();
        let old = "a".repeat(40);
        let document = format!("apiVersion: units.gitops.nyl/v1\nkind: OciImage\nmetadata: {{name: web}}\nspec:\n  context:\n    path: services/web\n    revision: main\n    commit: {old}\n  contexts:\n    tools:\n      path: tools\n      revision: tooling\n      commit: {old}\n  repository: registry.invalid/web\n");
        std::fs::write(directory.path().join("units.yaml"), &document).unwrap();
        std::fs::write(directory.path().join("unrenderable.yaml"), "apiVersion: units.gitops.nyl/v1\nkind: OciImage\nmetadata: {name: dynamic}\nspec:\n  repository: '{{ values.registry }}'\n").unwrap();
        let repository = git2::Repository::init(directory.path()).unwrap();
        let mut index = repository.index().unwrap();
        index.add_all(["*"], git2::IndexAddOption::DEFAULT, None).unwrap();
        index.write().unwrap();
        let tree = repository.find_tree(index.write_tree().unwrap()).unwrap();
        let signature = git2::Signature::now("Test", "test@example.invalid").unwrap();
        repository
            .commit(Some("HEAD"), &signature, &signature, "fixture", &tree, &[])
            .unwrap();
        let head = repository.head().unwrap().target().unwrap();
        repository.reference("refs/heads/main", head, true, "main").unwrap();
        repository
            .reference("refs/remotes/origin/tooling", head, true, "tooling")
            .unwrap();
        let inventory = super::super::discover_gitops_inventory(directory.path(), None).unwrap();
        let locks = collect_source_locks(&inventory, None, None, &|_, _, _| {
            vec!["/context".to_owned(), "/contexts/tools".to_owned()]
        })
        .unwrap();
        assert_eq!(locks.len(), 2);
        let tools = locks
            .iter()
            .find(
                |lock| matches!(&lock.owner, LockOwner::Source { path, .. } if path == &["spec", "contexts", "tools"]),
            )
            .unwrap();
        let updated = replace_lock_commit(tools, &"b".repeat(40)).unwrap();
        assert_eq!(
            updated,
            document.replacen(
                &format!("      commit: {old}"),
                &format!("      commit: {}", "b".repeat(40)),
                1
            )
        );
        assert_eq!(tools.repository_url, inventory.worktree_root.to_string_lossy());
        let cache = tempfile::TempDir::new().unwrap();
        let resolved = resolve_source_locks(locks, &mut GitManager::with_cache_dir(cache.path())).unwrap();
        assert!(resolved
            .iter()
            .all(|resolution| resolution.resolved == head.to_string()));
    }

    #[test]
    fn test_group_lock_replaces_only_the_selected_commit_scalar() {
        let updated = replace_single_commit_scalar("revision: main\ncommit: aaaa\n", "aaaa", "bbbb").unwrap();
        assert_eq!(updated, "revision: main\ncommit: bbbb\n");
    }
}
