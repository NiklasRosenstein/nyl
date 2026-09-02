use std::fs;
use std::path::{Path, PathBuf};

use clap::{Args, Subcommand};
use git2::Repository;

use crate::git::GitManager;
use crate::gitops::discover_gitops_inventory;
use crate::resources::{ApplicationGroupSource, GitOpsResource, GitOpsResourceKind};
use crate::{NylError, Result};

/// Manage immutable remote ApplicationGroup source locks.
#[derive(Args, Debug)]
pub struct SourceArgs {
    #[command(subcommand)]
    pub command: SourceSubcommand,
}

#[derive(Subcommand, Debug)]
pub enum SourceSubcommand {
    /// Resolve mutable revisions and update their authoritative commit locks.
    Update {
        /// ApplicationGroup name. All remote groups are updated when omitted.
        group: Option<String>,
        /// Project directory or a path beneath it.
        #[arg(long, default_value = ".")]
        path: PathBuf,
        /// Report stale locks without modifying files.
        #[arg(long)]
        check: bool,
    },
}

pub fn execute(args: SourceArgs) -> Result<()> {
    match args.command {
        SourceSubcommand::Update { group, path, check } => update(&path, group.as_deref(), check),
    }
}

fn update(path: &Path, requested_group: Option<&str>, check: bool) -> Result<()> {
    let inventory = discover_gitops_inventory(path, None)?;
    if let Some(resource) = inventory.resources.values().find(|resource| {
        resource.identity.kind == GitOpsResourceKind::ApplicationGroup
            && resource.resource.is_none()
            && requested_group.is_none_or(|requested| requested == resource.identity.name)
    }) {
        return Err(NylError::config(format!(
            "ApplicationGroup {:?} must render to a complete static resource for source update",
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
                    .map(|source| (group.metadata.name.as_str(), &discovered.source_path, source))
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
    for (name, source_path, source) in groups {
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
            let path = inventory.project_root.join(source_path);
            replace_commit_lock(&path, current, &resolved)?;
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

fn replace_commit_lock(path: &Path, current: &str, resolved: &str) -> Result<()> {
    let contents = fs::read_to_string(path)?;
    let pattern = regex::Regex::new(&format!(
        r#"(?m)^([ \t]*commit:[ \t]*)["']?{}["']?([ \t]*(?:#.*)?)$"#,
        regex::escape(current)
    ))
    .expect("escaped commit produces a valid regex");
    let matches = pattern.find_iter(&contents).count();
    if matches != 1 {
        return Err(NylError::config(format!(
            "Expected exactly one commit lock {current:?} in {}, found {matches}",
            path.display()
        )));
    }
    let replacement = format!("${{1}}{resolved}${{2}}");
    fs::write(path, pattern.replacen(&contents, 1, replacement).as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_only_the_selected_commit_scalar() {
        let temp = tempfile::NamedTempFile::new().unwrap();
        fs::write(temp.path(), "revision: main\ncommit: aaaa\n").unwrap();
        replace_commit_lock(temp.path(), "aaaa", "bbbb").unwrap();
        assert_eq!(
            fs::read_to_string(temp.path()).unwrap(),
            "revision: main\ncommit: bbbb\n"
        );
    }
}
