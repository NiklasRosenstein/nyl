use std::fs;
use std::path::Path;

use git2::Repository;

use crate::cli::resource_file::{atomic_replace, replace_document};
use crate::git::GitManager;
use crate::gitops::{discover_gitops_inventory, DiscoveredGitOpsResource};
use crate::resources::{ApplicationGroupSource, GitOpsResource, GitOpsResourceKind};
use crate::{NylError, Result};

pub(crate) fn update_locks(path: &Path, requested_group: Option<&str>, check: bool) -> Result<()> {
    let inventory = discover_gitops_inventory(path, None)?;
    if let Some(resource) = inventory.resources.values().find(|resource| {
        resource.identity.kind == GitOpsResourceKind::ApplicationGroup
            && resource.resource.is_none()
            && requested_group.is_none_or(|requested| requested == resource.identity.name)
    }) {
        return Err(NylError::config(format!(
            "ApplicationGroup {:?} must render to a complete static resource for source-lock update",
            resource.identity.name
        )));
    }
    let mut groups = inventory
        .resources
        .values()
        .filter_map(|discovered| match &discovered.resource {
            Some(GitOpsResource::ApplicationGroup(group))
                if requested_group.is_none_or(|requested| requested == group.metadata.name) =>
            {
                group
                    .spec
                    .source
                    .as_ref()
                    .filter(|source| source.is_remote())
                    .map(|source| (group.metadata.name.as_str(), discovered, source))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    groups.sort_by_key(|(name, _, _)| *name);
    if let Some(requested_group) = requested_group.filter(|_| groups.is_empty()) {
        return Err(NylError::config(format!(
            "Remote ApplicationGroup {:?} was not found",
            requested_group
        )));
    }

    let mut manager = GitManager::new().map_err(NylError::Git)?;
    let mut stale = 0;
    for (name, discovered, source) in groups {
        let repository_url = source_repository_url(&inventory, source)?;
        let revision = source
            .revision
            .as_deref()
            .expect("validated remote source has revision");
        let checkout = manager
            .resolve_ref_fresh(repository_url, Some(revision), None)
            .map_err(NylError::Git)?;
        let repository = Repository::discover(&checkout)
            .map_err(|error| NylError::config(format!("Failed to inspect resolved source: {error}")))?;
        let resolved = repository
            .head()
            .ok()
            .and_then(|head| head.target())
            .ok_or_else(|| NylError::config(format!("Resolved source {repository_url}@{revision} has no HEAD")))?
            .to_string();
        let current = source.commit.as_deref().expect("validated remote source has commit");
        if resolved == current {
            println!("✓ {name}: {revision} is locked to {resolved}");
            continue;
        }
        stale += 1;
        if check {
            println!("✗ {name}: {revision} resolves to {resolved}, lock is {current}");
        } else {
            let path = inventory.project_root.join(&discovered.source_path);
            replace_commit_lock(&path, discovered, current, &resolved)?;
            println!("✓ {name}: updated {revision} lock to {resolved}");
        }
    }
    if check && stale > 0 {
        return Err(NylError::validation(format!(
            "{stale} remote ApplicationGroup source lock(s) are stale"
        )));
    }
    Ok(())
}

fn source_repository_url<'a>(
    inventory: &'a crate::gitops::GitOpsInventory,
    source: &'a ApplicationGroupSource,
) -> Result<&'a str> {
    if let Some(repository) = &source.repository {
        return Ok(&repository.repo_url);
    }
    let reference = source
        .repository_ref
        .as_ref()
        .expect("validated remote source has repositoryRef or repository");
    let discovered = inventory
        .get(GitOpsResourceKind::GitRepository, &reference.name)
        .ok_or_else(|| NylError::config(format!("GitRepository {:?} was not found", reference.name)))?;
    let Some(GitOpsResource::GitRepository(repository)) = &discovered.resource else {
        unreachable!("inventory key and resource variant must agree");
    };
    Ok(&repository.spec.repo_url)
}

fn replace_commit_lock(
    path: &Path,
    discovered: &DiscoveredGitOpsResource,
    current: &str,
    resolved: &str,
) -> Result<()> {
    let contents = fs::read_to_string(path)?;
    let document = replace_commit_lock_document(&discovered.raw_document, current, resolved)?;
    let updated = replace_document(
        &contents,
        discovered.document_index,
        &discovered.raw_document,
        &document,
    )?;
    atomic_replace(path, &contents, &updated)
}

fn replace_commit_lock_document(document: &str, current: &str, resolved: &str) -> Result<String> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use crate::resources::GitOpsResourceIdentity;

    #[test]
    fn replaces_only_the_selected_commit_scalar() {
        let updated = replace_commit_lock_document("revision: main\ncommit: aaaa\n", "aaaa", "bbbb").unwrap();
        assert_eq!(updated, "revision: main\ncommit: bbbb\n");
    }

    #[test]
    fn updates_only_the_selected_document_in_a_shared_file() {
        let temporary = tempfile::TempDir::new().unwrap();
        let path = temporary.path().join("gitops.yaml");
        let first = "apiVersion: gitops.nyl/v1\nkind: ApplicationGroup\nmetadata:\n  name: first\nspec:\n  source:\n    revision: main\n    commit: aaaa\n    path: apps\n";
        let second = "apiVersion: gitops.nyl/v1\nkind: ApplicationGroup\nmetadata:\n  name: second\nspec:\n  source:\n    revision: main\n    commit: aaaa\n    path: apps\n";
        fs::write(&path, format!("{first}---\n{second}")).unwrap();
        let discovered = DiscoveredGitOpsResource {
            source_path: "gitops.yaml".into(),
            document_index: 2,
            raw_document: second.to_owned(),
            identity: GitOpsResourceIdentity {
                kind: GitOpsResourceKind::ApplicationGroup,
                name: "second".to_owned(),
            },
            static_labels: BTreeMap::new(),
            resource: None,
        };

        replace_commit_lock(&path, &discovered, "aaaa", "bbbb").unwrap();
        assert_eq!(
            fs::read_to_string(path).unwrap(),
            format!("{first}---\n{}", second.replace("commit: aaaa", "commit: bbbb"))
        );
    }
}
