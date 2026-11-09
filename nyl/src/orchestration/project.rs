//! An environment of the project, resolved for a command: its Environment,
//! the location of its state, and the Git state store that reaches it.

use std::collections::BTreeMap;
use std::path::PathBuf;

use nyl_core::orchestration::codes::{Code, Finding};
use nyl_core::orchestration::ids::EnvironmentName;
use nyl_core::orchestration::instance::{InstanceDescriptor, InstanceTerms};
use nyl_core::orchestration::ports::{CommitIdentity, StateStore};
use nyl_core::orchestration::state::{StateLocation, StateRecord};
use nyl_core::resources::environment::{sharing_group, Environment, EnvironmentGroup, EnvironmentTemplate};
use nyl_orchestration::rules::state_location::{resolve, LocationInputs};
use nyl_state::git::{CommitAuthor, Endpoint, GitStateStore};
use serde_json::Value;

use nyl_core::resources::InlineGitRepository;
use nyl_render::gitops::GitOpsInventory;
use nyl_render::resources::GitOpsResource;
use nyl_render::{NylError, Result};

/// An environment ready for state operations.
pub struct ResolvedEnvironment {
    pub name: EnvironmentName,
    pub environment: Environment,
    pub location: StateLocation,
    pub warnings: Vec<Finding>,
    /// The git directory of the clone the project lives in.
    pub git_dir: PathBuf,
    /// Credential-free source origin, or the local checkout path without an origin.
    pub source_repository: String,
    /// How to reach the location's repository, when it is not the clone.
    pub endpoint: Option<Endpoint>,
    /// Set when the environment is an instance of an EnvironmentTemplate.
    pub instance: Option<ResolvedInstance>,
}

/// An instance of an EnvironmentTemplate, as a command resolved it: the
/// template, the parameters recorded in `state.yaml` (with `--param` laid
/// over them), and the record itself.
pub struct ResolvedInstance {
    pub template: String,
    /// The parameters the instance renders with, bound against the template.
    pub params: BTreeMap<String, Value>,
    /// The template's terms for the instance.
    pub terms: InstanceTerms,
    /// The `--source-ref` this command was given, if any.
    pub source_ref: Option<String>,
    /// The instance's `state.yaml`; `None` before `state init --template` created it.
    pub record: Option<StateRecord>,
}

impl ResolvedInstance {
    /// The descriptor every environment-wide function takes.
    pub fn descriptor(&self) -> InstanceDescriptor {
        InstanceDescriptor {
            template: self.template.clone(),
            params: self.params.clone(),
        }
    }
}

/// What a command says about the instance it names: `--template` and
/// `--param`, which create or update it, and whether state is read offline.
/// Without a template, the environment must be a declared Environment or an
/// existing instance, found by its record.
#[derive(Debug, Clone, Default)]
pub struct InstanceRequest {
    pub template: Option<String>,
    pub params: BTreeMap<String, Value>,
    /// `--source-ref`: a full ref name of `origin` that carries the commit the
    /// run records, validated by [`parse_source_ref`] and recorded only by a
    /// command that records the instance's commit.
    pub source_ref: Option<String>,
    /// `--local`: read the record without network access.
    pub offline: bool,
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
/// and where its state lives. An instance of an EnvironmentTemplate resolves
/// the same way once its record names it ([`resolve_environment_with`]).
pub fn resolve_environment(
    inventory: &GitOpsInventory,
    name: &str,
) -> std::result::Result<ResolvedEnvironment, Finding> {
    resolve_environment_with(inventory, name, &InstanceRequest::default())
}

/// [`resolve_environment`] with what the command says about an instance.
///
/// Contract: [Instances](../../../design/orchestration-core.md#instances).
/// - A declared Environment resolves as before; `--template` for its name is
///   `NYL-INSTANCE-NAME-TAKEN`.
/// - Otherwise every EnvironmentTemplate's state location for the name is
///   read, and the one whose `state.yaml` names this environment and
///   template is the instance. Without `--template`, no such record means no
///   such environment (`NYL-ENVIRONMENT-UNKNOWN`); with it, the instance is
///   to be created, and a record of another template refuses
///   (`NYL-INSTANCE-NAME-TAKEN`).
/// - The instance's parameters are its record's, with `--param` laid over
///   them, bound against the template at the entry worktree.
#[allow(clippy::too_many_lines)] // One resolution: declared, else scan the templates' records, then bind the parameters.
pub fn resolve_environment_with(
    inventory: &GitOpsInventory,
    name: &str,
    request: &InstanceRequest,
) -> std::result::Result<ResolvedEnvironment, Finding> {
    let (environments, groups) = declared(inventory);
    if let Some(environment) = environments
        .into_iter()
        .find(|environment| environment.metadata.name == name)
    {
        if let Some(template) = &request.template {
            return Err(name_taken(
                name,
                &format!("is a declared Environment, so it cannot be an instance of {template}"),
            ));
        }
        return locate(inventory, &environment, &groups);
    }
    if request.template.is_none() && !request.params.is_empty() {
        return Err(Finding::new(
            Code::InstanceTemplateRequired,
            "--param is recorded when an instance is created or updated, so it needs --template",
            "name the EnvironmentTemplate with --template <name>",
        ));
    }
    let unknown = || {
        Finding::new(
            Code::EnvironmentUnknown,
            format!("the project declares no Environment {name:?}, and no instance of an EnvironmentTemplate has that name"),
            "list the declared environments with `nyl get environments`, or create an instance with `nyl state init -e <name> --template <template>`",
        )
    };
    let environment_name = EnvironmentName::new(name).map_err(|_| unknown())?;
    let templates = nyl_render::gitops::instances::templates(inventory);
    if let Some(template) = &request.template {
        if !templates.iter().any(|candidate| candidate.metadata.name == *template) {
            return Err(Finding::new(
                Code::InstanceTemplateUnknown,
                format!("the project declares no EnvironmentTemplate {template:?}"),
                "list the templates with `nyl get environment-templates`",
            ));
        }
    }
    let found = find_records(inventory, &environment_name, &groups, templates, request)?;
    // A record belongs to the template it names. Templates that share a state
    // location see each other's records there, which are taken names, not
    // instances of themselves.
    let (mut owned, foreign): (Vec<_>, Vec<_>) = found
        .into_iter()
        .partition(|(template, _, record)| record.template.as_deref() == Some(template.metadata.name.as_str()));
    let (template, mut located, record) = match owned.len() {
        0 => match (&request.template, foreign.first()) {
            (Some(_), Some((_, _, other))) => {
                return Err(name_taken(
                    name,
                    &format!(
                        "is an instance of EnvironmentTemplate {}",
                        other.template.as_deref().unwrap_or_default()
                    ),
                ));
            }
            (None, Some((_, _, other))) => {
                let recorded = other.template.as_deref().unwrap_or_default();
                return Err(Finding::new(
                    Code::InstanceTemplateUnknown,
                    format!("{name} is an instance of EnvironmentTemplate {recorded}, which this checkout does not declare"),
                    "run Nyl from a checkout that declares the template; renaming or removing a template needs `nyl state move --template`",
                ));
            }
            (None, None) => return Err(unknown()),
            (Some(wanted), None) => {
                let template = nyl_render::gitops::instances::find_template(inventory, wanted)
                    .expect("the template was checked above");
                let state = nyl_render::gitops::instances::instance_state(template, &environment_name)?;
                let mut skeleton = template.environment_skeleton(name);
                skeleton.spec.state = state;
                (template, locate(inventory, &skeleton, &groups)?, None)
            }
        },
        1 => {
            let (template, located, record) = owned.remove(0);
            if let Some(wanted) = &request.template {
                if *wanted != template.metadata.name {
                    return Err(name_taken(
                        name,
                        &format!(
                            "is an instance of EnvironmentTemplate {}, not {wanted}",
                            template.metadata.name
                        ),
                    ));
                }
            }
            (template, located, Some(record))
        }
        _ => {
            return Err(Finding::new(
                Code::InstanceAmbiguous,
                format!(
                    "{name} has state as an instance of several EnvironmentTemplates: {}",
                    owned
                        .iter()
                        .map(|(template, _, _)| template.metadata.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                "give the templates distinct state locations, or delete one of the instances' state",
            ))
        }
    };
    let mut given = record.as_ref().map(|record| record.params.clone()).unwrap_or_default();
    given.extend(request.params.clone());
    let (environment, terms) = nyl_render::gitops::instances::instantiate(template, &environment_name, &given)?;
    located.environment = environment;
    located.instance = Some(ResolvedInstance {
        template: template.metadata.name.clone(),
        params: terms.params.clone(),
        terms,
        source_ref: request.source_ref.clone(),
        record,
    });
    Ok(located)
}

/// The templates whose state location for `name` holds a `state.yaml` of
/// that environment, with the location and the record.
fn find_records<'a>(
    inventory: &GitOpsInventory,
    environment_name: &EnvironmentName,
    groups: &[EnvironmentGroup],
    templates: Vec<&'a EnvironmentTemplate>,
    request: &InstanceRequest,
) -> std::result::Result<Vec<(&'a EnvironmentTemplate, ResolvedEnvironment, StateRecord)>, Finding> {
    let name = environment_name.as_str();
    let mut found = Vec::new();
    for template in templates {
        let state = nyl_render::gitops::instances::instance_state(template, environment_name)?;
        let mut skeleton = template.environment_skeleton(name);
        skeleton.spec.state = state;
        let located = locate(inventory, &skeleton, groups)?;
        let snapshot = open_store_mode(inventory, &located, None, request.offline)
            .map_err(|error| unavailable(&error.to_string()))?
            .snapshot(&located.location)
            .map_err(|error| unavailable(&error.to_string()))?;
        if let Some(record) = snapshot.record {
            if record.environment == *environment_name && record.template.is_some() {
                found.push((template, located, record));
            }
        }
    }
    Ok(found)
}

/// Where the instance `name` of `template` keeps its state, from the
/// template alone: nothing is read, so every listed instance can be placed.
pub fn instance_location(
    inventory: &GitOpsInventory,
    name: &EnvironmentName,
    template: &str,
) -> std::result::Result<StateLocation, Finding> {
    let template = nyl_render::gitops::instances::find_template(inventory, template).ok_or_else(|| {
        Finding::new(
            Code::InstanceTemplateUnknown,
            format!("{name} is an instance of EnvironmentTemplate {template}, which this checkout does not declare"),
            "run Nyl from a checkout that declares the template; renaming or removing a template needs `nyl state move --template`",
        )
    })?;
    let mut skeleton = template.environment_skeleton(name.as_str());
    skeleton.spec.state = nyl_render::gitops::instances::instance_state(template, name)?;
    let (_, groups) = declared(inventory);
    Ok(locate(inventory, &skeleton, &groups)?.location)
}

fn name_taken(name: &str, why: &str) -> Finding {
    Finding::new(
        Code::InstanceNameTaken,
        format!("environment {name} {why}"),
        "environment names are unique across declared Environments and instances, because leases, run refs, and keep refs are named after them; pick another name",
    )
}

fn unavailable(message: &str) -> Finding {
    Finding::new(
        Code::StateUnavailable,
        format!("cannot read the state of the project's environments: {message}"),
        "check access to the state repository, or run without network access with --local",
    )
}

/// An Environment's state location and the endpoint that reaches it.
fn locate(
    inventory: &GitOpsInventory,
    environment: &Environment,
    groups: &[EnvironmentGroup],
) -> std::result::Result<ResolvedEnvironment, Finding> {
    let name = environment.metadata.name.as_str();
    let environment_name = environment
        .name()
        .map_err(|error| Finding::new(Code::EnvironmentUnknown, error.to_string(), "rename the Environment"))?;
    let group = sharing_group(environment, groups)?;
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
    let origin_identity = origin.as_deref().map(nyl_render::util::credential_free_url);
    let source_repository = origin_identity.clone().unwrap_or_else(|| {
        repository
            .workdir()
            .unwrap_or(&inventory.worktree_root)
            .display()
            .to_string()
    });
    let repositories =
        |reference: &nyl_core::resources::LocalReference| -> std::result::Result<InlineGitRepository, String> {
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
            "fix spec.state of the Environment, its template, or its group",
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
    if let Some(endpoint) = &endpoint {
        local_repository_path(&inventory.project_root, endpoint).map_err(|message| {
            Finding::new(
                Code::StateUnavailable,
                format!("cannot resolve the state location of {name}: {message}"),
                "fix spec.state.repository of the Environment, its template, or its group: a file:// URL names a local path such as file:///srv/state.git",
            )
        })?;
    }
    Ok(ResolvedEnvironment {
        name: environment_name,
        environment: environment.clone(),
        location,
        warnings: resolved.warnings,
        git_dir: repository.path().to_path_buf(),
        source_repository,
        endpoint,
        instance: None,
    })
}

/// The Git state store for a resolved environment, with the project's
/// optional dedicated credentials.
/// Its commits are written by `author`; without one the store is read-only,
/// for commands that never write (`plan`, `status`, `get`), which therefore
/// need no commit identity.
pub fn open_store(
    inventory: &GitOpsInventory,
    resolved: &ResolvedEnvironment,
    author: Option<CommitAuthor>,
) -> Result<GitStateStore> {
    open_store_mode(inventory, resolved, author, false)
}

/// Open the configured state repositories, optionally without network access.
pub fn open_store_mode(
    inventory: &GitOpsInventory,
    resolved: &ResolvedEnvironment,
    author: Option<CommitAuthor>,
    offline: bool,
) -> Result<GitStateStore> {
    let auth = crate::cli::git_auth::project_git_auth(&inventory.project_config.config)?;
    let mut store = match author {
        Some(author) => GitStateStore::new(&resolved.git_dir, auth, author),
        None => GitStateStore::read_only(&resolved.git_dir, auth),
    };
    store = store.strict_routing();
    let mut routed = std::collections::BTreeSet::new();
    let mut route = |store: GitStateStore, other: &ResolvedEnvironment| -> Result<GitStateStore> {
        match &other.endpoint {
            Some(endpoint) if routed.insert(other.location.repository.clone()) => configure_repository(
                inventory,
                &resolved.git_dir,
                store,
                &other.location.repository,
                endpoint.clone(),
            ),
            _ => Ok(store),
        }
    };
    for environment in declared(inventory).0 {
        let Ok(other) = resolve_environment(inventory, &environment.metadata.name) else {
            continue;
        };
        store = route(store, &other)?;
    }
    // Instances of a template live in its repository, wherever its state is.
    store = route(store, resolved)?;
    for other in template_locations(inventory) {
        store = route(store, &other)?;
    }
    Ok(if offline { store.offline() } else { store })
}

/// The local path an endpoint's read URL names, or `None` for a remote
/// repository. A `file://` URL that does not name a local path is an error
/// the state location reports, so that `nyl validate` and every command find
/// it where the environment is resolved, not where its store is opened.
fn local_repository_path(
    project_root: &std::path::Path,
    endpoint: &Endpoint,
) -> std::result::Result<Option<PathBuf>, String> {
    if endpoint.read.starts_with("file://") {
        return reqwest::Url::parse(&endpoint.read)
            .ok()
            .and_then(|url| url.to_file_path().ok())
            .map(Some)
            .ok_or_else(|| format!("invalid local state repository URL: {}", endpoint.read));
    }
    let path = std::path::Path::new(&endpoint.read);
    Ok(
        (path.is_absolute() || (!endpoint.read.contains("://") && !endpoint.read.contains(':')))
            .then(|| project_root.join(path)),
    )
}

/// Route a repository identity through its own local object database.
pub fn configure_repository(
    inventory: &GitOpsInventory,
    git_dir: &std::path::Path,
    mut store: GitStateStore,
    identity: &str,
    endpoint: Endpoint,
) -> Result<GitStateStore> {
    let cache = nyl_render::git::CacheLayout::for_project(&inventory.project_root);
    let file_path = local_repository_path(&inventory.project_root, &endpoint).map_err(NylError::config)?;
    let direct = file_path.is_some() && endpoint.write.as_ref().is_none_or(|write| write == &endpoint.read);
    let source = git2::Repository::open(git_dir)
        .ok()
        .and_then(|repo| {
            repo.find_remote("origin")
                .ok()
                .and_then(|remote| remote.url().ok().map(nyl_render::util::credential_free_url))
        })
        .is_some_and(|origin| origin == identity);
    let path = if let Some(path) = file_path {
        path
    } else if source {
        git_dir.to_path_buf()
    } else {
        let path = cache.state_repo_path(identity);
        store = store.with_managed_repository(&path);
        path
    };
    if direct {
        store = store.with_direct_repository(identity);
    }
    Ok(store.with_repository(identity, path).with_endpoint(identity, endpoint))
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
    let configured = clone_config(git_dir).and_then(|config| {
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

/// The Git configuration the clone at `git_dir` sees.
fn clone_config(git_dir: &std::path::Path) -> Option<git2::Config> {
    git2::Repository::open(git_dir).ok()?.config().ok()
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
    clone_config(git_dir)
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
fn finding_error(finding: &Finding) -> NylError {
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
    // Instances of a template get one state location each, wherever the
    // template puts it: two sample instances must not share one, or a second
    // instance would find the first's `state.yaml` at its own location.
    let (_, groups) = declared(inventory);
    for template in nyl_render::gitops::instances::templates(inventory) {
        // A limit counts the instances, which only a listable location lets Nyl do.
        if template.spec.max_instances.is_some() && listing_location(inventory, template).is_none() {
            return Err(finding_error(&Finding::new(
                Code::TemplateInvalid,
                format!(
                    "EnvironmentTemplate {} sets maxInstances, but its instances cannot be listed: its state location depends on the instance name",
                    template.metadata.name
                ),
                "end spec.state.path with `{{ environment.name }}` and keep the repository and refs independent of the name, or drop maxInstances",
            )));
        }
        let mut sampled = Vec::new();
        for (name, _) in nyl_render::gitops::instances::sample_instances(template) {
            let state = nyl_render::gitops::instances::instance_state(template, &name)
                .map_err(|finding| finding_error(&finding))?;
            let mut skeleton = template.environment_skeleton(name.as_str());
            skeleton.spec.state = state;
            let located = locate(inventory, &skeleton, &groups).map_err(|finding| finding_error(&finding))?;
            warnings.extend(located.warnings.clone());
            sampled.push((located.name, located.location));
        }
        if let [(first, left), (second, right)] = sampled.as_slice() {
            if left.repository == right.repository
                && left.path == right.path
                && (left.desired_ref == right.desired_ref || left.observed_ref == right.observed_ref)
            {
                return Err(finding_error(&Finding::new(
                    Code::TemplateInvalid,
                    format!(
                        "EnvironmentTemplate {} gives instances {first} and {second} the same state location {}",
                        template.metadata.name,
                        nyl_orchestration::rules::state_location::describe(left)
                    ),
                    "set spec.state.path to `{{ environment.name }}`, or refs that read environment.name",
                )));
            }
        }
        locations.extend(sampled);
    }
    let conflicts = nyl_orchestration::rules::state_location::ref_conflicts(&locations);
    if let Some(conflict) = conflicts.first() {
        return Err(finding_error(conflict));
    }
    Ok(warnings)
}

/// Sample instances of every template the project declares, which
/// `nyl validate` checks as if they existed ([`nyl_render::gitops::instances::sample_instances`]).
pub fn sample_instances(
    inventory: &GitOpsInventory,
) -> std::collections::BTreeMap<EnvironmentName, nyl_core::orchestration::instance::InstanceDescriptor> {
    nyl_render::gitops::instances::templates(inventory)
        .into_iter()
        .flat_map(nyl_render::gitops::instances::sample_instances)
        .collect()
}

/// Where each EnvironmentTemplate keeps the state of an instance, for
/// routing its repository: the location of a sample instance.
fn template_locations(inventory: &GitOpsInventory) -> Vec<ResolvedEnvironment> {
    let (_, groups) = declared(inventory);
    nyl_render::gitops::instances::templates(inventory)
        .into_iter()
        .filter_map(|template| {
            let (name, _) = nyl_render::gitops::instances::sample_instances(template)
                .into_iter()
                .next()?;
            let mut skeleton = template.environment_skeleton(name.as_str());
            skeleton.spec.state = nyl_render::gitops::instances::instance_state(template, &name).ok()?;
            locate(inventory, &skeleton, &groups).ok()
        })
        .collect()
}

/// `--param name=value` options as parameters: each value is parsed as YAML,
/// so `123` is an integer and `pr-123` a string.
pub fn parse_params(params: &[String]) -> std::result::Result<BTreeMap<String, Value>, Finding> {
    let invalid = |message: String| {
        Finding::new(
            Code::InstanceParameterInvalid,
            message,
            "write each parameter as --param <name>=<value>, with the value as YAML",
        )
    };
    let mut parsed = BTreeMap::new();
    for param in params {
        let Some((name, value)) = param.split_once('=') else {
            return Err(invalid(format!("--param {param:?} is not <name>=<value>")));
        };
        let value = nyl_render::yaml::parse_yaml_value_k8s_compatible(value)
            .map_err(|error| invalid(format!("--param {name}: the value {value:?} is not YAML: {error}")))?;
        if parsed.insert(name.to_owned(), value).is_some() {
            return Err(invalid(format!("--param {name} is given more than once")));
        }
    }
    Ok(parsed)
}

/// `--source-ref <ref>`: a full ref name under `refs/`, checked by its syntax
/// alone (it is only a hint, so no remote is asked). Contract:
/// [Instances](../../../design/orchestration-core.md#instances).
pub fn parse_source_ref(reference: Option<&str>) -> std::result::Result<Option<String>, Finding> {
    reference
        .map(|reference| {
            nyl_orchestration::rules::source_ref::validate(reference)
                .map(|()| reference.to_owned())
                .map_err(|why| {
                    Finding::new(
                        Code::InstanceOptionInvalid,
                        format!("--source-ref {why}"),
                        "pass the full name of a ref of the source repository's origin, such as refs/pull/123/head",
                    )
                })
        })
        .transpose()
}

/// An instance found by listing its template's state.
pub struct ListedInstance {
    pub name: EnvironmentName,
    pub descriptor: InstanceDescriptor,
    pub record: StateRecord,
}

/// The name no environment has, standing for an instance's name while a
/// template's state location is examined.
const SENTINEL_NAME: &str = "nyl-instance-sentinel";

/// Every live instance of every EnvironmentTemplate, found by listing each
/// template's state location. Contract: [Instances](../../../design/orchestration-core.md#instances):
/// "Instances are discovered by listing the template's state location, so no
/// central index exists."
///
/// A template whose refs or repository depend on the environment name has no
/// one location to list, and a `state.path` must end in the environment's
/// name to be a directory per instance; instances of such a template are
/// found by name only. Deleted instances are not listed. A location that
/// cannot be read is a warning of the command that lists it, and the template is
/// reported in [`Listing::unavailable`] so checks across environments fail closed.
pub fn list_instances(inventory: &GitOpsInventory, offline: bool) -> Listing {
    let mut listing = Listing::default();
    let (declared_environments, _) = declared(inventory);
    for template in nyl_render::gitops::instances::templates(inventory) {
        let Some((located, parent_location)) = listing_location(inventory, template) else {
            continue;
        };
        let records = open_store_mode(inventory, &located, None, offline)
            .map_err(|error| error.to_string())
            .and_then(|store| store.list_records(&parent_location).map_err(|error| error.to_string()));
        match records {
            Ok(records) => {
                for record in records {
                    if record.template.as_deref() != Some(template.metadata.name.as_str()) {
                        continue;
                    }
                    // A deleted instance holds no incarnation, and its location still answers
                    // that, so a fence on its old prefix reads Gone rather than unknown.
                    if record.deleted().is_some() {
                        listing.deleted.push(ListedInstance {
                            name: record.environment.clone(),
                            descriptor: InstanceDescriptor {
                                template: template.metadata.name.clone(),
                                params: record.params.clone(),
                            },
                            record,
                        });
                        continue;
                    }
                    // A declared Environment of the same name wins: leases, run refs and
                    // state are named after it, so the instance is a taken name, not shadowed silently.
                    if declared_environments
                        .iter()
                        .any(|environment| environment.metadata.name == record.environment.as_str())
                    {
                        listing.warnings.push(name_taken(
                            record.environment.as_str(),
                            &format!(
                                "is a declared Environment now, and also the recorded instance of EnvironmentTemplate {}; the instance is ignored until its state is deleted or the Environment is renamed",
                                template.metadata.name
                            ),
                        ));
                        continue;
                    }
                    listing.instances.push(ListedInstance {
                        name: record.environment.clone(),
                        descriptor: InstanceDescriptor {
                            template: template.metadata.name.clone(),
                            params: record.params.clone(),
                        },
                        record,
                    });
                }
            }
            Err(message) => {
                listing.warnings.push(Finding::new(
                    Code::StateUnavailable,
                    format!(
                        "cannot list the instances of EnvironmentTemplate {}: {message}",
                        template.metadata.name
                    ),
                    "check access to the template's state repository, or run without network access with --local; checks that need every environment's state refuse until the listing works",
                ));
                listing.unavailable.push(template.metadata.name.clone());
            }
        }
    }
    listing
}

/// Where the instances of `template` are listed, when one location holds
/// them all: the template's state location for a sentinel name, and the
/// directory of that location whose records are the instances. `None` when
/// the refs or the repository depend on the instance name, or `state.path`
/// does not end in it, so the instances are found by name only.
pub fn listing_location(
    inventory: &GitOpsInventory,
    template: &EnvironmentTemplate,
) -> Option<(ResolvedEnvironment, StateLocation)> {
    let sentinel = EnvironmentName::new(SENTINEL_NAME).ok()?;
    let (_, groups) = declared(inventory);
    let state = nyl_render::gitops::instances::instance_state(template, &sentinel).ok()?;
    let mut skeleton = template.environment_skeleton(SENTINEL_NAME);
    skeleton.spec.state = state;
    let located = locate(inventory, &skeleton, &groups).ok()?;
    let location = &located.location;
    let by_name = [&location.repository, &location.desired_ref, &location.observed_ref]
        .iter()
        .any(|part| part.contains(SENTINEL_NAME));
    let parent = location.path.strip_suffix(SENTINEL_NAME).filter(|_| !by_name)?;
    let mut parent_location = location.clone();
    parent.trim_matches('/').clone_into(&mut parent_location.path);
    Some((located, parent_location))
}

/// What listing the instances of every template found.
#[derive(Default)]
pub struct Listing {
    pub instances: Vec<ListedInstance>,
    /// Instances whose state was deleted: not live, but their locations are
    /// still where their old incarnations are looked up.
    pub deleted: Vec<ListedInstance>,
    /// Why a listing is incomplete.
    pub warnings: Vec<Finding>,
    /// Templates whose state location could not be read: their instances are
    /// unknown, so a check across environments must treat them as unavailable.
    pub unavailable: Vec<String>,
}
