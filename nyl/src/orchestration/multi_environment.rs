//! Selection and composition of ordinary single-environment commands.

use nyl_orchestration::rules::sources::{source_request, SourceRequest};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::Path;

use nyl_core::orchestration::codes::{Code, Finding};
use nyl_core::orchestration::ids::{CommitId, EnvironmentName, UnitName};
use nyl_core::orchestration::ports::{
    EnvironmentSnapshot, RenderedSource, SourceRenderer, SourceRepository, SourceResolver, StateStore,
};
use nyl_orchestration::rules::multi_environment::{dependencies, order};
use nyl_orchestration::run_report::ReportKind;

use super::project::{declared, resolve_environment_with, InstanceRequest};
use super::run::CommandOutcome;
use super::session::{pretty_json, stopped, Stop};
use crate::cli::commands::reconcile::RunOutput;
use nyl_render::gitops::discover_gitops_inventory;
use nyl_render::gitops::units::UnitSourceRenderer;
use nyl_render::gitops::GitOpsInventory;

/// What a command that removes expired instances says about expiry: an
/// instance it removes is not rendered by the preflight, because removing it
/// needs its stored documents and not its source.
///
/// Contract: [Expiry](../../../design/orchestration-core.md#expiry).
#[derive(Debug, Clone, Copy)]
pub struct Expiry {
    pub intent: nyl_orchestration::rules::expiry::Intent,
    pub now: nyl_core::orchestration::time::Timestamp,
}

/// Selected environments in dependency order, with independent preflight errors.
pub struct Selection {
    names: Vec<String>,
    errors: BTreeMap<String, Finding>,
}

/// Resolve one of explicit names, an equality label selector, or a group,
/// then order the selected environments by their rendered references.
#[allow(clippy::too_many_arguments)] // Selection and state access options are independent.
pub fn select(
    path: &Path,
    names: &[String],
    labels: Option<&str>,
    group: Option<&str>,
    source_graph: bool,
    source_override: Option<&str>,
    offline: bool,
    drivers: &nyl_core::orchestration::driver::DriverSet,
    instance: &InstanceRequest,
    expiry: Option<Expiry>,
) -> Result<Selection, Stop> {
    let inventory = discover_gitops_inventory(path, None, crate::resource_catalogue())?;
    let (environments, groups) = declared(&inventory);
    // Named environments that are not declared are instances of an
    // EnvironmentTemplate, known by their records; `--template` and
    // `--param` name the one being created or updated.
    let hint = |name: &str| InstanceRequest {
        offline,
        ..if names.len() == 1 && names[0] == name {
            instance.clone()
        } else {
            InstanceRequest::default()
        }
    };
    let (instances, mut errors, undeclared, unlisted, expiries) = known_instances(&inventory, names, &hint, offline);
    let inventory = inventory.with_instances(instances);
    let selection = selection_names(&environments, &groups, names, labels, group, &undeclared)?;
    if selection.is_empty() {
        return Ok(Selection {
            names: Vec::new(),
            errors: BTreeMap::new(),
        });
    }
    single_source(source_override, &selection.iter().cloned().collect::<Vec<_>>())?;
    let removing = removing_instances(&inventory, &expiries, &selection, expiry);
    let repository = git2::Repository::discover(&inventory.project_root)
        .map_err(|error| nyl_render::NylError::config(error.to_string()))?;
    let commit = repository
        .head()
        .and_then(|head| head.peel_to_commit())
        .map_err(|error| nyl_render::NylError::config(error.to_string()))?;
    let commit = CommitId::new(commit.id().to_string())
        .map_err(|error| Finding::new(Code::SourceUnavailable, error.to_string(), "check out a source commit"))?;
    let renderer = UnitSourceRenderer::new(super::session::project_dir(&inventory), crate::resource_catalogue())
        .with_previous_paths(inventory.project_config.config.project.previous_paths.clone())
        .with_instances(inventory.instances.clone());
    let mut graph = BTreeMap::new();
    let mut rendered_sources = BTreeMap::new();
    let mut all = nyl_render::gitops::instances::environment_names(&inventory)
        .map_err(|error| nyl_render::NylError::config(error.to_string()))?;
    // A named environment that could not be resolved still takes part, so its
    // finding is reported instead of the name vanishing from the selection.
    all.extend(errors.keys().filter_map(|name| EnvironmentName::new(name).ok()));
    all.sort();
    all.dedup();
    for name in all {
        if errors.contains_key(name.as_str()) {
            graph.insert(name, BTreeSet::new());
            continue;
        }
        match read_environment(
            &inventory,
            &name,
            &commit,
            &renderer,
            source_graph && !removing.contains(&name),
            source_override.filter(|_| selection.contains(name.as_str())),
            &hint(name.as_str()),
        ) {
            Ok((dependencies, rendered)) => {
                graph.insert(name.clone(), dependencies);
                if let Some(rendered) = rendered {
                    rendered_sources.insert(name, rendered);
                }
            }
            Err(stop) => {
                let finding = match stop {
                    Stop::Finding(finding) => finding,
                    Stop::Error(error) => Finding::new(
                        Code::SourceUnavailable,
                        error.to_string(),
                        "correct this environment’s configuration or restore access to its source and state",
                    ),
                };
                errors.insert(name.to_string(), finding);
                graph.insert(name, BTreeSet::new());
            }
        }
    }
    if source_graph {
        ownership_preflight(&inventory, &rendered_sources, &mut errors, drivers, &hint, &unlisted)?;
    }
    let ordered = if source_graph {
        validate_rendered_sources(&inventory, &rendered_sources, &mut errors, &removing)?
    } else {
        order(&graph)?
    };
    Ok(Selection {
        names: ordered
            .into_iter()
            .map(|name| name.to_string())
            .filter(|name| selection.contains(name))
            .collect(),
        errors,
    })
}

/// Validate the rendered sources of every environment together and order them.
/// A consumer's bad reference is that environment's error, never the others'.
fn validate_rendered_sources(
    inventory: &GitOpsInventory,
    rendered_sources: &BTreeMap<EnvironmentName, RenderedSource>,
    errors: &mut BTreeMap<String, Finding>,
    removing: &BTreeSet<EnvironmentName>,
) -> Result<Vec<EnvironmentName>, Stop> {
    let validation = nyl_orchestration::rules::multi_environment::validate_available_sources(
        rendered_sources,
        &errors
            .keys()
            .filter_map(|name| EnvironmentName::new(name).ok())
            .chain(removing.iter().cloned())
            .collect(),
        &nyl_render::gitops::instances::instance_only_names(inventory)
            .map_err(|error| nyl_render::NylError::config(error.to_string()))?,
    )?;
    for (name, finding) in validation.errors {
        errors.entry(name.to_string()).or_insert(finding);
    }
    Ok(validation.order)
}

/// The instances commands across environments know: the ones state lists,
/// and the named environments that are not declared, resolved with their
/// `--template` and `--param` hints. A named environment that cannot be
/// resolved is a finding of its own, and is left out of the instances.
fn known_instances(
    inventory: &GitOpsInventory,
    names: &[String],
    hint: &dyn Fn(&str) -> InstanceRequest,
    offline: bool,
) -> (
    nyl_core::orchestration::instance::Instances,
    BTreeMap<String, Finding>,
    Vec<String>,
    Vec<String>,
    Expiries,
) {
    let (declared_environments, _) = declared(inventory);
    let mut instances = nyl_core::orchestration::instance::Instances::new();
    let mut expiries = Expiries::new();
    let mut errors = BTreeMap::new();
    let mut undeclared = Vec::new();
    let listing = super::project::list_instances(inventory, offline);
    let unlisted = listing.unavailable;
    for instance in listing.instances {
        expiries.insert(
            instance.name.clone(),
            (instance.descriptor.template.clone(), instance.record.expires_at),
        );
        instances.insert(instance.name, instance.descriptor);
    }
    for name in names {
        if declared_environments
            .iter()
            .any(|environment| environment.metadata.name == *name)
        {
            continue;
        }
        match resolve_environment_with(inventory, name, &hint(name)) {
            Ok(resolved) => {
                if let Some(found) = resolved.instance {
                    // The record the command read is newer than the listing's.
                    expiries.insert(
                        resolved.name.clone(),
                        (
                            found.template.clone(),
                            found.record.as_ref().and_then(|record| record.expires_at),
                        ),
                    );
                    instances.insert(resolved.name, found.descriptor());
                }
            }
            Err(finding) => {
                errors.insert(name.clone(), finding);
            }
        }
        undeclared.push(name.clone());
    }
    (instances, errors, undeclared, unlisted, expiries)
}

/// The selected instances a reconcile with `expiry` removes: expired ones are
/// not rendered by the preflight, because removing one runs from its stored
/// documents, so a lost source commit cannot keep it alive.
fn removing_instances(
    inventory: &GitOpsInventory,
    expiries: &Expiries,
    selection: &BTreeSet<String>,
    expiry: Option<Expiry>,
) -> BTreeSet<EnvironmentName> {
    let Some(expiry) = expiry else {
        return BTreeSet::new();
    };
    expiries
        .iter()
        .filter(|(name, _)| selection.contains(name.as_str()))
        .filter(|(_, (template, expires_at))| {
            let ttl = nyl_render::gitops::instances::find_template(inventory, template)
                .and_then(|template| template.spec.ttl);
            nyl_orchestration::rules::expiry::decide(expiry.intent, *expires_at, ttl, expiry.now)
                == nyl_orchestration::rules::expiry::Disposition::Remove
        })
        .map(|(name, _)| name.clone())
        .collect()
}

/// Each known instance's template and recorded expiry.
type Expiries = BTreeMap<EnvironmentName, (String, Option<nyl_core::orchestration::time::Timestamp>)>;

/// Stored incarnations keep their resource claims until they leave state,
/// including when source changes or stops selecting a held or deleting unit.
fn ownership_preflight(
    inventory: &GitOpsInventory,
    sources: &BTreeMap<EnvironmentName, RenderedSource>,
    errors: &mut BTreeMap<String, Finding>,
    drivers: &nyl_core::orchestration::driver::DriverSet,
    hint: &dyn Fn(&str) -> InstanceRequest,
    unlisted: &[String],
) -> Result<(), Stop> {
    let mut snapshots = BTreeMap::new();
    // Instances that could not be listed hold claims nobody can check.
    let mut unknown = unlisted
        .iter()
        .map(|template| format!("the instances of EnvironmentTemplate {template}"))
        .collect::<Vec<_>>();
    for name in nyl_render::gitops::instances::environment_names(inventory)
        .map_err(|error| nyl_render::NylError::config(error.to_string()))?
    {
        match snapshot(inventory, &name, &hint(name.as_str())) {
            Ok(snapshot) => {
                snapshots.insert(name, snapshot);
            }
            Err(_) => unknown.push(name.to_string()),
        }
    }
    if let Some(finding) =
        nyl_orchestration::rules::desired::check_ownership_with_snapshots(sources, &snapshots, drivers)
            .into_iter()
            .next()
    {
        return Err(finding.into());
    }
    if !unknown.is_empty() {
        for owner in nyl_orchestration::rules::desired::scope_owners(sources, drivers) {
            errors.entry(owner.to_string()).or_insert_with(|| {
                Finding::new(
                    Code::StateUnavailable,
                    format!(
                        "cannot check stored resource ownership claims of environments {}: state is unavailable",
                        unknown.join(", ")
                    ),
                    "restore access before running units that claim native resources",
                )
            });
        }
    }
    Ok(())
}

fn snapshot(
    inventory: &GitOpsInventory,
    name: &EnvironmentName,
    request: &InstanceRequest,
) -> Result<EnvironmentSnapshot, Stop> {
    let resolved = resolve_environment_with(inventory, name.as_str(), request)?;
    let store = super::project::open_store_mode(inventory, &resolved, None, request.offline)?;
    store.snapshot(&resolved.location).map_err(|error| {
        Finding::new(
            Code::StateUnavailable,
            error.to_string(),
            "check access to the environment’s state",
        )
        .into()
    })
}

/// Resolve one environment independently; failures do not skip other runs.
fn read_environment(
    inventory: &GitOpsInventory,
    name: &EnvironmentName,
    commit: &CommitId,
    renderer: &UnitSourceRenderer,
    source_graph: bool,
    source_override: Option<&str>,
    request: &InstanceRequest,
) -> Result<(BTreeSet<EnvironmentName>, Option<RenderedSource>), Stop> {
    if !source_graph {
        let snapshot = snapshot(inventory, name, request)?;
        return Ok((
            snapshot
                .desired
                .values()
                .flat_map(|unit| dependencies(&unit.spec))
                .filter(|other| other != name)
                .collect(),
            None,
        ));
    }
    let resolved = resolve_environment_with(inventory, name.as_str(), request)?;
    let (_, groups) = declared(inventory);
    let entry = nyl_core::resources::environment::effective_environment(&resolved.environment, &groups)?;
    if let Some(finding) = nyl_orchestration::rules::sources::unsupported_source(&entry) {
        return Err(finding.into());
    }
    let auth = crate::cli::git_auth::project_git_auth(&inventory.project_config.config)?;
    let mut sources = nyl_state::source::GitSourceResolver::new(&resolved.git_dir, auth);
    // An instance's recorded commit stays fetchable through its keep refs.
    if let Some(id) = resolved
        .instance
        .as_ref()
        .and_then(|instance| instance.record.as_ref()?.keep_id.as_ref())
        .filter(|_| !request.offline)
    {
        sources = sources.with_kept_source(nyl_orchestration::rules::keep::keep_prefix(id));
    }
    // An instance not entered from its own checkout renders at its recorded commit.
    let recorded = resolved
        .instance
        .as_ref()
        .filter(|_| request.template.is_none())
        .and_then(|instance| instance.record.as_ref()?.source_commit.clone());
    let commit = recorded.as_ref().unwrap_or(commit);
    let source_commit = match source_request(&entry, commit, source_override, recorded.is_some()) {
        SourceRequest::Worktree(snapshot) => snapshot.commit,
        SourceRequest::Entry(commit) | SourceRequest::Recorded(commit) | SourceRequest::Commit { commit, .. } => commit,
        SourceRequest::Revision(revision) | SourceRequest::Override(revision) => sources
            .resolve_revision(&SourceRepository::This, &revision)
            .map_err(|error| {
                Finding::new(
                    Code::SourceUnavailable,
                    error.to_string(),
                    "fetch the environment's source revision",
                )
            })?,
    };
    let workspace = tempfile::TempDir::new().map_err(nyl_render::NylError::from)?;
    let tree = workspace.path().join("source");
    sources
        .worktree(&SourceRepository::This, &source_commit, &tree)
        .map_err(|error| {
            Finding::new(
                Code::SourceUnavailable,
                error.to_string(),
                "check access to the environment's source commit",
            )
        })?;
    let rendered = renderer.render(&tree, name, &source_commit).map_err(|error| {
        Finding::new(
            Code::SourceUnavailable,
            error.to_string(),
            "correct the environment's unit declarations",
        )
    })?;
    Ok((
        rendered
            .units
            .values()
            .filter(|unit| unit.selected)
            .filter_map(|unit| unit.spec.as_ref())
            .flat_map(dependencies)
            .filter(|other| other != name)
            .collect(),
        Some(rendered),
    ))
}

fn selection_names(
    environments: &[nyl_core::resources::environment::Environment],
    groups: &[nyl_core::resources::environment::EnvironmentGroup],
    names: &[String],
    labels: Option<&str>,
    group: Option<&str>,
    undeclared: &[String],
) -> Result<BTreeSet<String>, Stop> {
    let matches = if let Some(group) = group {
        groups
            .iter()
            .find(|item| item.metadata.name == group)
            .ok_or_else(|| {
                Finding::new(
                    Code::EnvironmentUnknown,
                    format!("the project declares no EnvironmentGroup {group:?}"),
                    "list groups with `nyl get environment-groups`",
                )
            })?
            .spec
            .environment_selector
            .match_labels
            .clone()
    } else if let Some(labels) = labels {
        let mut matches = BTreeMap::new();
        for item in labels.split(',') {
            let Some((key, value)) = item.split_once('=') else {
                return Err(Finding::new(
                    Code::EnvironmentUnknown,
                    format!("invalid environment label selector {labels:?}"),
                    "use key=value, separated by commas",
                )
                .into());
            };
            if key.is_empty() || value.is_empty() || matches.insert(key.to_owned(), value.to_owned()).is_some() {
                return Err(Finding::new(
                    Code::EnvironmentUnknown,
                    format!("invalid environment label selector {labels:?}"),
                    "use distinct nonempty key=value pairs",
                )
                .into());
            }
        }
        matches
    } else {
        BTreeMap::new()
    };
    let selection = environments
        .iter()
        .filter(|environment| {
            if labels.is_some() || group.is_some() {
                matches
                    .iter()
                    .all(|(key, value)| environment.metadata.labels.get(key) == Some(value))
            } else {
                names.contains(&environment.metadata.name)
            }
        })
        .map(|environment| environment.metadata.name.clone())
        .chain(undeclared.iter().cloned())
        .collect::<BTreeSet<_>>();
    for name in names {
        if !selection.contains(name) {
            return Err(Finding::new(
                Code::EnvironmentUnknown,
                format!("the project declares no Environment {name:?}"),
                "list environments with `nyl get environments`",
            )
            .into());
        }
    }
    Ok(selection)
}

/// Run every selected environment and merge their reports in run order.
/// A failed environment never prevents an independent environment from running.
pub fn dispatch(
    kind: ReportKind,
    output: RunOutput,
    selection: Result<Selection, Stop>,
    mut operation: impl FnMut(
        &str,
        &[String],
        &BTreeMap<EnvironmentName, BTreeSet<UnitName>>,
        &BTreeSet<EnvironmentName>,
    ) -> (CommandOutcome, BTreeSet<UnitName>),
) -> CommandOutcome {
    let selection = match selection {
        Ok(selected) => selected,
        Err(stop) => return stopped(kind, "unknown", output, stop),
    };
    let mut result = CommandOutcome {
        exit: 0,
        report: serde_json::json!({"apiVersion": "gitops.nyl/v1", "kind": kind, "environments": []}),
        stdout: String::new(),
        stderr: String::new(),
    };
    let severity = |exit| match exit {
        4 => 4,
        3 => 3,
        1 => 2,
        2 => 1,
        _ => 0,
    };
    let selected = &selection.names;
    let mut unavailable = selection
        .errors
        .keys()
        .filter(|name| selected.contains(name))
        .filter_map(|name| EnvironmentName::new(name).ok())
        .collect::<BTreeSet<_>>();
    let mut planned = BTreeMap::new();
    for environment in selected {
        let (outcome, changes) = match selection.errors.get(environment) {
            Some(finding) => (
                stopped(kind, environment, output, Stop::Finding(finding.clone())),
                BTreeSet::new(),
            ),
            None => operation(environment, selected, &planned, &unavailable),
        };
        if kind == ReportKind::ReconcileReport && outcome.exit != 0 {
            unavailable.insert(EnvironmentName::new(environment).expect("selection validated the name"));
        }
        if kind == ReportKind::PlanReport {
            planned.insert(
                EnvironmentName::new(environment).expect("selection validated the name"),
                changes,
            );
        }
        if severity(outcome.exit) > severity(result.exit) {
            result.exit = outcome.exit;
        }
        if let Some(environments) = outcome.report.get("environments").and_then(serde_json::Value::as_array) {
            result.report["environments"]
                .as_array_mut()
                .unwrap()
                .extend(environments.iter().cloned());
        }
        if selected.len() > 1 && matches!(output, RunOutput::Table) {
            let _ = writeln!(result.stdout, "{environment}:");
        }
        result.stdout.push_str(&outcome.stdout);
        result.stderr.push_str(&outcome.stderr);
    }
    if matches!(output, RunOutput::Json) {
        result.stdout = pretty_json(&result.report);
    }
    result
}

/// Source overrides select one environment only.
pub fn single_source(source: Option<&str>, selected: &[String]) -> Result<(), Stop> {
    if source.is_some() && selected.len() != 1 {
        return Err(Finding::new(
            Code::SourceUnavailable,
            "--source requires exactly one environment",
            "select one environment with -e",
        )
        .into());
    }
    Ok(())
}
