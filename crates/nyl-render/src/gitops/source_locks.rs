//! Source locks: ApplicationGroup remote sources and `fromGit` Release input
//! bindings, refreshed together by `nyl update source-locks`.
//!
//! Contract: [`fromGit`](../../../../design/release-inputs.md#binding-kinds).
//!
//! - Locks are grouped by repository and `revision`. Each group is resolved
//!   once, and every lock in it moves to the same new commit.
//! - A group whose `fromGit` files lie inside one DeploymentTarget's
//!   publication prefix on that branch moves to that target's newest
//!   publication commit, never to a later commit another tool made on the
//!   branch. A group with no such file moves to the branch head, and a group
//!   reading several targets' publications is an error.
//! - Locks are addressed by their position in the document, never by matching
//!   the commit text alone, so bindings that share a commit but name different
//!   revisions stay independent.

use std::collections::{BTreeMap, BTreeSet};

use crate::git::{normalize_git_url_for_equality, GitManager};
use crate::resources::{GitOpsResource, GitOpsResourceKind};
use crate::{NylError, Result};

use super::{DiscoveredGitOpsResource, GitOpsInventory};

/// Commit trailer naming the DeploymentTarget a publication commit belongs to.
pub const DEPLOYMENT_TARGET_TRAILER: &str = "Nyl-Deployment-Target";

/// The resource field that holds a lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockOwner {
    /// `ApplicationGroup.spec.source.commit`.
    ApplicationGroup { name: String },
    /// `DeploymentTarget.spec.releaseInputs.<key>.<input>.fromGit.commit`.
    ReleaseInput { target: String, key: String, input: String },
}

impl std::fmt::Display for LockOwner {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ApplicationGroup { name } => write!(formatter, "ApplicationGroup {name}"),
            Self::ReleaseInput { target, key, input } => write!(formatter, "DeploymentTarget {target} {key}/{input}"),
        }
    }
}

/// One commit lock in a project resource.
#[derive(Debug, Clone)]
pub struct SourceLock {
    pub owner: LockOwner,
    pub resource: DiscoveredGitOpsResource,
    pub repository_url: String,
    pub revision: String,
    pub commit: String,
    /// Repository-relative file a `fromGit` binding reads.
    pub path: Option<String>,
}

/// A lock and the commit it should name.
#[derive(Debug, Clone)]
pub struct LockResolution {
    pub lock: SourceLock,
    /// The commit the lock should name, or `None` when it cannot move yet.
    pub resolved: Option<String>,
    /// Why `resolved` was chosen, for reports.
    pub note: Option<String>,
}

impl LockResolution {
    /// Whether the lock names a different commit than it should.
    pub fn is_stale(&self) -> bool {
        self.resolved
            .as_ref()
            .is_some_and(|resolved| resolved != &self.lock.commit)
    }
}

/// Collect the locks selected by the ApplicationGroup and DeploymentTarget
/// filters. Without filters every lock is selected; with one or both, only
/// the named resources' locks are.
pub fn collect_source_locks(
    inventory: &GitOpsInventory,
    group: Option<&str>,
    target: Option<&str>,
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
                group_found = true;
                let repository = inventory
                    .resolve_git_repository(source.repository_ref.as_ref(), source.repository.as_ref())?
                    .0;
                locks.push(SourceLock {
                    owner: LockOwner::ApplicationGroup {
                        name: resource.metadata.name.clone(),
                    },
                    resource: discovered.clone(),
                    repository_url: repository.repo_url,
                    revision: source.revision.clone().expect("validated remote source has revision"),
                    commit: source.commit.clone().expect("validated remote source has commit"),
                    path: None,
                });
            }
            Some(GitOpsResource::DeploymentTarget(resource))
                if targets_selected && target.is_none_or(|requested| requested == resource.metadata.name) =>
            {
                target_found = true;
                for (key, inputs) in &resource.spec.release_inputs {
                    for (input, binding) in inputs {
                        let Some(source) = &binding.from_git else {
                            continue;
                        };
                        let repository = inventory
                            .resolve_git_repository(source.repository_ref.as_ref(), source.repository.as_ref())?
                            .0;
                        locks.push(SourceLock {
                            owner: LockOwner::ReleaseInput {
                                target: resource.metadata.name.clone(),
                                key: key.clone(),
                                input: input.clone(),
                            },
                            resource: discovered.clone(),
                            repository_url: repository.repo_url,
                            revision: source.revision.clone(),
                            commit: source.commit.clone(),
                            path: Some(source.path.clone()),
                        });
                    }
                }
            }
            _ => {}
        }
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

/// Resolve every lock, one repository and revision at a time.
///
/// Each repository's refs are fetched once. A group whose `fromGit` files lie
/// inside one DeploymentTarget's publication prefix moves to that target's
/// newest publication commit; a group with no such file moves to the branch
/// head. A group reading files of several targets' publications has no single
/// commit to move to and is an error.
pub fn resolve_source_locks(
    inventory: &GitOpsInventory,
    locks: Vec<SourceLock>,
    manager: &mut GitManager,
) -> Result<Vec<LockResolution>> {
    // Prefixes are needed only to place `fromGit` files, so an unrelated
    // target's publication never blocks an ApplicationGroup-only update.
    let publishers = if locks.iter().any(|lock| lock.path.is_some()) {
        publication_prefixes(inventory)?
    } else {
        Vec::new()
    };
    let mut groups = BTreeMap::<(String, String), Vec<SourceLock>>::new();
    for lock in locks {
        let group = (
            normalize_git_url_for_equality(&lock.repository_url),
            lock.revision.clone(),
        );
        groups.entry(group).or_default().push(lock);
    }
    let mut fetched = BTreeSet::new();
    let mut resolutions = Vec::new();
    for ((url, revision), locks) in groups {
        let repository_url = locks[0].repository_url.clone();
        if fetched.insert(url.clone()) {
            manager.fetch_refs(&repository_url).map_err(NylError::Git)?;
        }
        let head = manager
            .resolve_cached_ref(&repository_url, &revision)
            .map_err(NylError::Git)?;
        let owners = locks
            .iter()
            .filter_map(|lock| lock.path.as_deref())
            .filter_map(|path| {
                publishers
                    .iter()
                    .find(|(publisher_url, publisher_revision, prefix, _)| {
                        publisher_url == &url
                            && branch(publisher_revision) == branch(&revision)
                            && path_within(path, prefix)
                    })
                    .map(|(_, _, _, target)| target.clone())
            })
            .collect::<BTreeSet<_>>();
        let (resolved, note) = match owners.len() {
            0 => (Some(head.to_string()), None),
            1 => {
                let target = owners.first().expect("one owner");
                let trailer = format!("{DEPLOYMENT_TARGET_TRAILER}: {target}");
                match manager
                    .newest_commit_with_message_line(&repository_url, head, &trailer)
                    .map_err(NylError::Git)?
                {
                    Some(commit) => (
                        Some(commit.to_string()),
                        Some(format!("newest publication of DeploymentTarget {target}")),
                    ),
                    None => (
                        None,
                        Some(format!(
                            "{revision} has no publication commit of DeploymentTarget {target} yet"
                        )),
                    ),
                }
            }
            _ => {
                let owners = owners.into_iter().collect::<Vec<_>>().join(", ");
                let bindings = locks
                    .iter()
                    .map(|lock| lock.owner.to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(NylError::config(format!(
                    "Locks of {}@{revision} ({bindings}) read files published by DeploymentTargets {owners}; every lock of one repository and revision moves to one commit, so bind each target's published files from a revision that only that target publishes",
                    crate::util::sanitize_url(&repository_url)
                )));
            }
        };
        resolutions.extend(locks.into_iter().map(|lock| LockResolution {
            lock,
            resolved: resolved.clone(),
            note: note.clone(),
        }));
    }
    resolutions.sort_by_key(|resolution| resolution.lock.owner.to_string());
    Ok(resolutions)
}

/// Replace one lock's commit in its resource document.
pub fn replace_lock_commit(lock: &SourceLock, resolved: &str) -> Result<String> {
    let path = match &lock.owner {
        // A group has one source, so its only commit scalar is the lock, even
        // when the document is templated.
        LockOwner::ApplicationGroup { .. } => {
            return replace_single_commit_scalar(&lock.resource.raw_document, &lock.commit, resolved)
        }
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
    replace_block_scalar(&lock.resource.raw_document, &path, &lock.commit, resolved).ok_or_else(|| {
        NylError::config(format!(
            "Cannot locate the commit lock of {} in {}; write {} in block style, one key per line",
            lock.owner,
            lock.resource.source_path.display(),
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

/// Every target's publication as (normalized URL, revision, prefix, target).
/// Both the read and the publish URL identify the repository.
fn publication_prefixes(inventory: &GitOpsInventory) -> Result<Vec<(String, String, String, String)>> {
    let mut prefixes = Vec::new();
    for discovered in inventory.resources.values() {
        let Some(GitOpsResource::DeploymentTarget(target)) = &discovered.resource else {
            continue;
        };
        let publication = &target.spec.publication;
        let (repository, _) =
            inventory.resolve_git_repository(publication.repository_ref.as_ref(), publication.repository.as_ref())?;
        for url in std::iter::once(&repository.repo_url).chain(repository.publish_url.as_ref()) {
            prefixes.push((
                normalize_git_url_for_equality(url),
                publication.revision.clone(),
                target.publication_path_prefix().trim_matches('/').to_owned(),
                target.metadata.name.clone(),
            ));
        }
    }
    Ok(prefixes)
}

fn branch(revision: &str) -> &str {
    revision.strip_prefix("refs/heads/").unwrap_or(revision)
}

fn path_within(path: &str, prefix: &str) -> bool {
    prefix.is_empty() || path.strip_prefix(prefix).is_some_and(|rest| rest.starts_with('/'))
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
    fn test_group_lock_replaces_only_the_selected_commit_scalar() {
        let updated = replace_single_commit_scalar("revision: main\ncommit: aaaa\n", "aaaa", "bbbb").unwrap();
        assert_eq!(updated, "revision: main\ncommit: bbbb\n");
    }

    #[test]
    fn test_path_within_prefix() {
        assert!(path_within("dev/state/images.json", "dev"));
        assert!(!path_within("develop/state.json", "dev"));
        assert!(path_within("state.json", ""));
    }
}
