//! An environment of the project, resolved for a command: its Environment,
//! the location of its state, and the Git state store that reaches it.

use std::path::PathBuf;

use nyl_core::orchestration::codes::{Code, Finding};
use nyl_core::orchestration::ids::EnvironmentName;
use nyl_core::orchestration::ports::CommitIdentity;
use nyl_core::orchestration::state::StateLocation;
use nyl_core::resources::environment::{Environment, EnvironmentGroup};
use nyl_orchestration::rules::environment_groups::sharing_group;
use nyl_orchestration::rules::state_location::{resolve, LocationInputs};
use nyl_state::git::{CommitSigner, Endpoint, GitStateStore};

use crate::gitops::GitOpsInventory;
use crate::resources::{GitOpsResource, InlineGitRepository};
use crate::{NylError, Result};

/// An environment ready for state operations.
pub struct ResolvedEnvironment {
    pub name: EnvironmentName,
    pub environment: Environment,
    pub location: StateLocation,
    pub warnings: Vec<Finding>,
    /// The git directory of the clone the project lives in.
    pub git_dir: PathBuf,
    /// How to reach the location's repository, when it is not the clone.
    pub endpoint: Option<Endpoint>,
}

/// The Environments and EnvironmentGroups the project declares.
pub fn declared(inventory: &GitOpsInventory) -> (Vec<Environment>, Vec<EnvironmentGroup>) {
    let mut environments = Vec::new();
    let mut groups = Vec::new();
    for discovered in inventory.resources.values() {
        match &discovered.resource {
            Some(GitOpsResource::Environment(environment)) => environments.push((**environment).clone()),
            Some(GitOpsResource::EnvironmentGroup(group)) => groups.push(group.clone()),
            _ => {}
        }
    }
    (environments, groups)
}

/// Resolve `name` against the project: its Environment, its group defaults,
/// and where its state lives.
pub fn resolve_environment(
    inventory: &GitOpsInventory,
    name: &str,
) -> std::result::Result<ResolvedEnvironment, Finding> {
    let (environments, groups) = declared(inventory);
    let Some(environment) = environments
        .into_iter()
        .find(|environment| environment.metadata.name == name)
    else {
        return Err(Finding::new(
            Code::EnvironmentUnknown,
            format!("the project declares no Environment {name:?}"),
            "list the declared environments with `nyl get environments`",
        ));
    };
    let environment_name = environment
        .name()
        .map_err(|error| Finding::new(Code::EnvironmentUnknown, error.to_string(), "rename the Environment"))?;
    let group = sharing_group(&environment, &groups)?;
    let repository = git2::Repository::discover(&inventory.project_root).map_err(|error| {
        Finding::new(
            Code::StateUnavailable,
            format!("the project is not in a Git repository: {error}"),
            "run Nyl in a clone of the project's repository",
        )
    })?;
    let origin = repository
        .find_remote("origin")
        .ok()
        .and_then(|remote| remote.url().ok().map(str::to_owned));
    let origin_identity = origin.as_deref().map(crate::util::credential_free_url);
    let repositories =
        |reference: &crate::resources::LocalReference| -> std::result::Result<InlineGitRepository, String> {
            inventory
                .resolve_git_repository(Some(reference), None)
                .map(|(repository, _)| repository)
                .map_err(|error| error.to_string())
        };
    let resolved = resolve(&LocationInputs {
        environment: &environment_name,
        state: &environment.spec.state,
        group: group.map(|group| &group.spec.state),
        repositories: &repositories,
        origin: origin_identity.as_deref(),
    })
    .map_err(|message| {
        Finding::new(
            Code::StateUnavailable,
            format!("cannot resolve the state location of {name}: {message}"),
            "fix spec.state of the Environment or its group",
        )
    })?;
    let location = resolved.location;
    let endpoint = if location.is_local() {
        None
    } else if Some(&location.repository) == origin_identity.as_ref() && resolved.publish_url.is_none() {
        // The checkout's own remote, reached as `git` reaches it.
        Some(Endpoint {
            read: origin.clone().unwrap_or_else(|| location.repository.clone()),
            write: None,
        })
    } else {
        Some(Endpoint {
            read: location.repository.clone(),
            write: resolved.publish_url.clone(),
        })
    };
    Ok(ResolvedEnvironment {
        name: environment_name,
        environment,
        location,
        warnings: resolved.warnings,
        git_dir: repository.path().to_path_buf(),
        endpoint,
    })
}

/// The Git state store for a resolved environment, with the project's
/// optional dedicated credentials.
/// Its commits are signed by `signer`; without one the store is read-only,
/// for commands that never write (`plan`, `status`, `get`), which therefore
/// need no commit identity.
pub fn open_store(
    inventory: &GitOpsInventory,
    resolved: &ResolvedEnvironment,
    signer: Option<CommitSigner>,
) -> Result<GitStateStore> {
    let auth = crate::cli::git_auth::project_git_auth(&inventory.project_config.config)?;
    let store = match signer {
        Some(signer) => GitStateStore::new(&resolved.git_dir, auth, signer),
        None => GitStateStore::read_only(&resolved.git_dir, auth),
    };
    Ok(match &resolved.endpoint {
        Some(endpoint) => store.with_endpoint(resolved.location.repository.clone(), endpoint.clone()),
        None => store,
    })
}

/// The fix of a missing or invalid commit identity.
const IDENTITY_FIX: &str =
    "pass --author \"Name <email>\", or set git config user.name and git config user.email in the clone";

/// Who writes state commits: `author` (`--author "Name <email>"`) when
/// given, else the clone's Git `user.name` and `user.email`, else a
/// [`Code::CommitIdentityMissing`] finding that names both fixes.
///
/// Contract: [Transition commits](../../../design/orchestration-core.md#transition-commits);
/// decided for M3 part 3: the command line resolves the identity once, only
/// for commands that write, and passes it with the injected clock to
/// `nyl-state`, which never reads Git configuration itself.
pub fn commit_identity(
    git_dir: &std::path::Path,
    author: Option<&str>,
) -> std::result::Result<CommitIdentity, Finding> {
    if let Some(author) = author {
        return author
            .parse()
            .map_err(|error: String| Finding::new(Code::CommitIdentityMissing, error, IDENTITY_FIX));
    }
    let configured = git2::Repository::open(git_dir)
        .ok()
        .and_then(|repository| repository.config().ok())
        .and_then(|config| {
            let name = config
                .get_string("user.name")
                .ok()
                .filter(|name| !name.trim().is_empty())?;
            let email = config
                .get_string("user.email")
                .ok()
                .filter(|email| !email.trim().is_empty())?;
            Some(CommitIdentity { name, email })
        });
    configured.ok_or_else(|| {
        Finding::new(
            Code::CommitIdentityMissing,
            "no identity to write state commits with: no --author was given and the clone's Git \
             configuration names no user.name and user.email",
            IDENTITY_FIX,
        )
    })
}

/// Who runs Nyl: the CI job's URL when a CI system says so, else `user@host`.
pub fn runner() -> String {
    let variable = |name: &str| std::env::var(name).ok().filter(|value| !value.is_empty());
    if let (Some(server), Some(repository), Some(run)) = (
        variable("GITHUB_SERVER_URL"),
        variable("GITHUB_REPOSITORY"),
        variable("GITHUB_RUN_ID"),
    ) {
        return format!("{server}/{repository}/actions/runs/{run}");
    }
    if let Some(url) = variable("CI_JOB_URL") {
        return url;
    }
    let user = variable("USER")
        .or_else(|| variable("USERNAME"))
        .unwrap_or_else(|| "unknown".to_owned());
    let host = variable("HOSTNAME").unwrap_or_else(|| "localhost".to_owned());
    format!("{user}@{host}")
}

/// Who requested an operator action: the Git user of the clone, else `runner`.
pub fn requested_by(git_dir: &std::path::Path, runner: &str) -> String {
    git2::Repository::open(git_dir)
        .ok()
        .and_then(|repository| repository.config().ok())
        .and_then(|config| {
            let name = config.get_string("user.name").ok()?;
            let email = config.get_string("user.email").ok();
            Some(email.map_or_else(|| name.clone(), |email| format!("{name} <{email}>")))
        })
        .unwrap_or_else(|| runner.to_owned())
}

/// The state tips as of `revision`, a state commit (`--state-revision`):
/// that commit on the state ref whose history holds it, and on the other
/// ref its newest commit no later than that commit, so a transition that
/// wrote both refs at once is read whole. `current` are the tips of a
/// snapshot just read, which made the history available in the clone.
pub fn state_tips_at(
    git_dir: &std::path::Path,
    current: &nyl_core::orchestration::ports::StateTips,
    revision: &str,
) -> std::result::Result<nyl_core::orchestration::ports::StateTips, Finding> {
    use nyl_core::orchestration::ids::CommitId;
    use nyl_core::orchestration::ports::StateTips;

    let unavailable = |message: String| {
        Finding::new(
            Code::StateUnavailable,
            message,
            "name a commit of the environment's desired or observed state ref, as `git log` of the ref lists them",
        )
    };
    let repository = git2::Repository::open(git_dir)
        .map_err(|error| unavailable(format!("cannot open the project's clone: {}", error.message())))?;
    let target = repository
        .revparse_single(revision)
        .and_then(|object| object.peel_to_commit())
        .map_err(|error| {
            unavailable(format!(
                "--state-revision {revision:?} names no commit: {}",
                error.message()
            ))
        })?;
    let oid = |tip: &Option<CommitId>| tip.as_ref().and_then(|tip| git2::Oid::from_str(tip.as_str()).ok());
    let holds = |tip: Option<git2::Oid>| {
        tip.is_some_and(|tip| tip == target.id() || repository.graph_descendant_of(tip, target.id()).unwrap_or(false))
    };
    let at_or_before = |tip: Option<git2::Oid>| -> Option<CommitId> {
        let mut walk = repository.revwalk().ok()?;
        walk.set_sorting(git2::Sort::TIME).ok()?;
        walk.push(tip?).ok()?;
        walk.filter_map(std::result::Result::ok)
            .find(|candidate| {
                repository
                    .find_commit(*candidate)
                    .is_ok_and(|commit| commit.time().seconds() <= target.time().seconds())
            })
            .and_then(|found| CommitId::new(found.to_string()).ok())
    };
    let (desired, observed) = (oid(&current.desired), oid(&current.observed));
    let (in_desired, in_observed) = (holds(desired), holds(observed));
    if !in_desired && !in_observed {
        return Err(unavailable(format!(
            "--state-revision {revision:?} is not a commit of the environment's state refs"
        )));
    }
    let named = CommitId::new(target.id().to_string()).map_err(|error| unavailable(error.to_string()))?;
    Ok(StateTips {
        desired: if in_desired {
            Some(named.clone())
        } else {
            at_or_before(desired)
        },
        observed: if in_observed {
            Some(named)
        } else {
            at_or_before(observed)
        },
    })
}

/// Map a finding to the CLI's error type, keeping its code and fix.
pub fn finding_error(finding: &Finding) -> NylError {
    NylError::config(format!("{}: {}\nHint: {}", finding.code, finding.message, finding.fix))
}

/// Check every declared Environment's state configuration: group overlaps
/// and state refs that Git cannot hold together are errors; refs forges
/// cannot protect are returned as warnings.
pub fn validate_environments(inventory: &GitOpsInventory) -> Result<Vec<Finding>> {
    let (environments, _) = declared(inventory);
    let mut locations = Vec::new();
    let mut warnings = Vec::new();
    for environment in &environments {
        let resolved =
            resolve_environment(inventory, &environment.metadata.name).map_err(|finding| finding_error(&finding))?;
        warnings.extend(resolved.warnings);
        locations.push((resolved.name, resolved.location));
    }
    let conflicts = nyl_orchestration::rules::state_location::ref_conflicts(&locations);
    if let Some(conflict) = conflicts.first() {
        return Err(finding_error(conflict));
    }
    Ok(warnings)
}
