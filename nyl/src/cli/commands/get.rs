//! `nyl get`: declarations without `-e`, an environment's state with it.
//!
//! Contract: [Inspection](../../../../design/orchestration-core.md#inspection).
//! State forms read one snapshot through a read-only store and never take
//! the lease. Document and table forms exit 0 whenever they can read the
//! requested state; `get output` and `get artifact` resolve like the
//! matching `fromUnit` reference and exit 1 when it would not resolve.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use clap::{Args, Subcommand, ValueEnum};
use nyl_core::bindings::UnitInputSource;
use nyl_core::orchestration::codes::{Code, Finding};
use nyl_core::orchestration::ids::UnitName;
use nyl_core::orchestration::ports::{Coordination, EnvironmentSnapshot};
use nyl_core::orchestration::state::ArtifactDocument;
use nyl_orchestration::run_report::UnitReport;
use nyl_orchestration::shell::inspect::{self, GetValue};
use serde_json::{json, Value};

use crate::cli::table::{render as render_table, Cell};
use crate::gitops::units::discover_units;
use crate::gitops::{discover_gitops_inventory, DiscoveredGitOpsResource, GitOpsInventory};
use crate::orchestration::project::{declared, state_tips_at};
use crate::orchestration::run::{CommandOutcome, Wiring};
use crate::orchestration::session::{cell_label, pretty_json, write_finding, Session, Stop};
use crate::resources::{GitOpsResource, GitOpsResourceKind};
use crate::{NylError, Result};

/// Inspect GitOps resources declared in the project, or an environment's state.
#[derive(Args, Debug)]
pub struct GetArgs {
    #[command(subcommand)]
    command: GetCommand,
}

#[derive(Subcommand, Debug)]
enum GetCommand {
    /// Get Git repository declarations.
    #[command(alias = "repository")]
    Repositories(GetResourceArgs),
    /// Get cluster declarations.
    #[command(alias = "cluster")]
    Clusters(GetResourceArgs),
    /// Get Argo CD instance declarations.
    #[command(name = "argocd-instances", alias = "argocd-instance", alias = "argocd")]
    ArgoCDInstances(GetResourceArgs),
    /// Get deployment target declarations.
    #[command(alias = "target")]
    Targets(GetResourceArgs),
    /// Get AppProject definitions.
    #[command(name = "app-projects", alias = "app-project")]
    AppProjects(GetResourceArgs),
    /// Get application group declarations.
    #[command(name = "application-groups", alias = "application-group")]
    ApplicationGroups(GetResourceArgs),
    /// Get Environment declarations.
    #[command(alias = "environment")]
    Environments(GetResourceArgs),
    /// Get unit declarations, or with -e the units' state.
    Units(GetUnitsArgs),
    /// Get one unit's persisted desired and observed documents.
    Unit(GetUnitArgs),
    /// Get a unit output's value, resolved like a fromUnit reference.
    Output(GetValueArgs),
    /// Get an artifact or a value inside it, resolved like a fromUnit reference.
    Artifact(GetValueArgs),
    /// Get the artifacts of the units' latest receipts.
    Artifacts(GetStateArgs),
}

#[derive(Args, Debug)]
struct GetResourceArgs {
    /// Return only the resource with this name.
    name: Option<String>,

    /// Project directory or a path beneath it.
    #[arg(default_value = ".")]
    path: PathBuf,
}

/// Output formats of `get`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum GetOutput {
    #[default]
    Table,
    Wide,
    Yaml,
    Json,
    Name,
}

/// Options of the state forms.
#[derive(Args, Debug)]
struct StateOptions {
    /// Read this environment's state instead of the declarations.
    #[arg(short = 'e', long = "environment")]
    environment: Option<String>,

    /// Read state as of this earlier state commit.
    #[arg(long, conflicts_with = "local")]
    state_revision: Option<String>,

    /// Read this clone's local state branches, including --local runs.
    #[arg(long)]
    local: bool,

    /// Output format.
    #[arg(short = 'o', long = "output", value_enum, default_value_t)]
    output: GetOutput,
}

#[derive(Args, Debug)]
struct GetUnitsArgs {
    #[command(flatten)]
    state: StateOptions,

    /// Project directory or a path beneath it.
    #[arg(default_value = ".")]
    path: PathBuf,
}

#[derive(Args, Debug)]
struct GetStateArgs {
    #[command(flatten)]
    state: StateOptions,

    /// Project directory or a path beneath it.
    #[arg(default_value = ".")]
    path: PathBuf,
}

#[derive(Args, Debug)]
struct GetUnitArgs {
    /// The unit.
    unit: String,

    #[command(flatten)]
    state: StateOptions,

    /// Print only the desired document.
    #[arg(long, conflicts_with = "observed")]
    desired: bool,

    /// Print only the observed document.
    #[arg(long)]
    observed: bool,

    /// Project directory or a path beneath it.
    #[arg(default_value = ".")]
    path: PathBuf,
}

#[derive(Args, Debug)]
struct GetValueArgs {
    /// `<unit>/<name>`.
    reference: String,

    #[command(flatten)]
    state: StateOptions,

    /// A JSON Pointer inside the value; for artifacts relative to the spec, as in fromUnit.
    #[arg(long)]
    pointer: Option<String>,

    /// Print the latest receipt's value, with a warning, when the reference would not resolve.
    #[arg(long)]
    allow_stale: bool,

    /// Project directory or a path beneath it.
    #[arg(default_value = ".")]
    path: PathBuf,
}

/// Run `get` in-process with `wiring`. Declarations and state documents
/// exit 0 when they can be read; value forms exit 1 when the reference
/// would not resolve.
pub fn run(args: &GetArgs, _wiring: &Wiring<'_>) -> Result<CommandOutcome> {
    match &args.command {
        GetCommand::Repositories(selection) => declarations(GitOpsResourceKind::GitRepository, selection),
        GetCommand::Clusters(selection) => declarations(GitOpsResourceKind::Cluster, selection),
        GetCommand::ArgoCDInstances(selection) => declarations(GitOpsResourceKind::ArgoCDInstance, selection),
        GetCommand::Targets(selection) => declarations(GitOpsResourceKind::DeploymentTarget, selection),
        GetCommand::AppProjects(selection) => declarations(GitOpsResourceKind::AppProjectDefinition, selection),
        GetCommand::ApplicationGroups(selection) => declarations(GitOpsResourceKind::ApplicationGroup, selection),
        GetCommand::Environments(selection) => declarations(GitOpsResourceKind::Environment, selection),
        GetCommand::Units(units) => match &units.state.environment {
            None => unit_declarations(&units.path, units.state.output),
            Some(environment) => state_form(&units.path, environment, &units.state, |snapshot, env| {
                units_table(env, snapshot, units.state.output)
            }),
        },
        GetCommand::Unit(unit) => {
            let environment = required_environment(&unit.state, "get unit")?;
            state_form(&unit.path, environment, &unit.state, |snapshot, _| {
                unit_form(unit, snapshot)
            })
        }
        GetCommand::Output(value) => {
            let environment = required_environment(&value.state, "get output")?;
            state_form(&value.path, environment, &value.state, |snapshot, env| {
                value_form(value, false, env, snapshot)
            })
        }
        GetCommand::Artifact(value) => {
            let environment = required_environment(&value.state, "get artifact")?;
            state_form(&value.path, environment, &value.state, |snapshot, env| {
                value_form(value, true, env, snapshot)
            })
        }
        GetCommand::Artifacts(artifacts) => {
            let environment = required_environment(&artifacts.state, "get artifacts")?;
            state_form(&artifacts.path, environment, &artifacts.state, |snapshot, _| {
                Ok(artifacts_table(snapshot, artifacts.state.output))
            })
        }
    }
}

fn required_environment<'a>(state: &'a StateOptions, command: &str) -> Result<&'a str> {
    state
        .environment
        .as_deref()
        .ok_or_else(|| NylError::config(format!("{command} reads state: name the environment with -e <env>")))
}

/// A successful outcome printing `stdout`.
fn printed(stdout: String, report: Value) -> CommandOutcome {
    CommandOutcome {
        exit: 0,
        report,
        stdout,
        stderr: String::new(),
    }
}

/// An outcome that failed with `finding`: exit 1, nothing on stdout.
fn refused(finding: &Finding) -> CommandOutcome {
    let mut stderr = String::new();
    write_finding(&mut stderr, "error", None, finding);
    CommandOutcome {
        exit: 1,
        report: json!({ "errors": [finding] }),
        stdout: String::new(),
        stderr,
    }
}

/// Read one snapshot of `environment`'s state, at its tips or at
/// `--state-revision`, through a read-only store, and render it with `form`.
fn state_form(
    path: &Path,
    environment: &str,
    options: &StateOptions,
    form: impl FnOnce(&EnvironmentSnapshot, &nyl_core::orchestration::ids::EnvironmentName) -> Result<CommandOutcome>,
) -> Result<CommandOutcome> {
    let session = match Session::open(path, environment) {
        Ok(session) => session,
        Err(Stop::Finding(finding)) => return Ok(refused(&finding)),
        Err(Stop::Error(error)) => return Err(error),
    };
    let store = session.store(None)?;
    let location = &session.resolved.location;
    // `--local`: the clone's local view, through the store a `--local` run uses.
    let local_store = if options.local {
        match session.local_run(false, &[]) {
            Ok(local) => Some(nyl_orchestration::shell::local::LocalStore::new(&store, local)),
            Err(finding) => return Ok(refused(&finding)),
        }
    } else {
        None
    };
    let state = local_store
        .as_ref()
        .map_or(&store as &dyn nyl_core::orchestration::ports::StateStore, |local| local);
    let snapshot = match inspect::snapshot(state, location, None) {
        Ok(snapshot) => snapshot,
        Err(finding) => return Ok(refused(&finding)),
    };
    let snapshot = match &options.state_revision {
        None => snapshot,
        Some(revision) => {
            let read = state_tips_at(&session.resolved.git_dir, &snapshot.tips, revision)
                .and_then(|tips| inspect::snapshot(&store, location, Some(&tips)));
            match read {
                Ok(snapshot) => snapshot,
                Err(finding) => return Ok(refused(&finding)),
            }
        }
    };
    let mut outcome = form(&snapshot, &session.resolved.name)?;
    let mut warnings = String::new();
    for finding in &session.resolved.warnings {
        write_finding(&mut warnings, "warning", None, finding);
    }
    outcome.stderr = warnings + &outcome.stderr;
    Ok(outcome)
}

/// `nyl get units -e <env>`: desired and observed joined per unit.
fn units_table(
    environment: &nyl_core::orchestration::ids::EnvironmentName,
    snapshot: &EnvironmentSnapshot,
    output: GetOutput,
) -> Result<CommandOutcome> {
    let report = // No run refs are read here, so `now` decides nothing.
    nyl_orchestration::rules::unit_state::status(environment, snapshot, &Coordination::default(), chrono::DateTime::UNIX_EPOCH);
    let documents = snapshot
        .desired
        .iter()
        .map(|(name, desired)| {
            (
                name.to_string(),
                json!({ "desired": desired, "observed": snapshot.observed.get(name) }),
            )
        })
        .collect::<serde_json::Map<_, _>>();
    let documents = Value::Object(documents);
    let stdout = match output {
        GetOutput::Json => pretty_json(&documents),
        GetOutput::Yaml => yaml(&documents)?,
        GetOutput::Name => lines(report.units.keys()),
        GetOutput::Table | GetOutput::Wide => {
            let rows = report
                .units
                .iter()
                .map(|(name, unit)| unit_state_row(name, unit, output == GetOutput::Wide))
                .collect::<Vec<_>>();
            let mut headers = vec!["NAME", "KIND", "STATE", "RECEIPT", "LAST RUN"];
            if output == GetOutput::Wide {
                headers.push("DETAIL");
            }
            format!("{}\n", render_table(&headers, &rows))
        }
    };
    Ok(printed(stdout, documents))
}

fn unit_state_row(name: &UnitName, unit: &UnitReport, wide: bool) -> Vec<Cell> {
    let receipt = cell_label(unit.receipt);
    let mut cells = vec![
        name.to_string(),
        unit.kind.clone(),
        unit.outcome_name().unwrap_or("-").to_owned(),
        receipt,
        unit.last_run
            .as_ref()
            .map_or_else(|| "-".to_owned(), ToString::to_string),
    ];
    if wide {
        let detail = unit.message.clone().unwrap_or_else(|| {
            unit.blocked_by
                .iter()
                .map(|blocked| format!("{}: {}", blocked.pointer, blocked.reason))
                .collect::<Vec<_>>()
                .join("; ")
        });
        cells.push(detail);
    }
    cells.into_iter().map(Cell::plain).collect()
}

/// `nyl get unit <u> -e <env>`: the exact persisted documents.
fn unit_form(args: &GetUnitArgs, snapshot: &EnvironmentSnapshot) -> Result<CommandOutcome> {
    let unit = match UnitName::new(&args.unit) {
        Ok(unit) => unit,
        Err(error) => {
            return Ok(refused(&Finding::new(
                Code::UnitUnknown,
                format!("{:?} is not a unit name: {error}", args.unit),
                "list the environment's units with nyl get units -e <env>",
            )))
        }
    };
    let documents = match inspect::unit_documents(snapshot, &unit) {
        Ok(documents) => documents,
        Err(finding) => return Ok(refused(&finding)),
    };
    let report = if args.desired {
        serde_json::to_value(&documents.desired)?
    } else if args.observed {
        serde_json::to_value(&documents.observed)?
    } else {
        json!({ "desired": documents.desired, "observed": documents.observed })
    };
    let stdout = match args.state.output {
        GetOutput::Json => pretty_json(&report),
        GetOutput::Yaml => yaml(&report)?,
        GetOutput::Name => format!("{unit}\n"),
        GetOutput::Table | GetOutput::Wide => {
            let mut single = EnvironmentSnapshot::default();
            if let Some(desired) = &documents.desired {
                single.desired.insert(unit.clone(), desired.clone());
            }
            if let Some(observed) = &documents.observed {
                single.observed.insert(unit.clone(), observed.clone());
            }
            let environment = documents
                .desired
                .as_ref()
                .map(|desired| desired.unit.environment.clone())
                .or_else(|| {
                    documents
                        .observed
                        .as_ref()
                        .map(|observed| observed.unit.environment.clone())
                });
            match environment {
                Some(environment) => units_table(&environment, &single, args.state.output)?.stdout,
                None => String::new(),
            }
        }
    };
    Ok(printed(stdout, report))
}

/// Parse `<unit>/<name>`.
fn split_reference(reference: &str) -> Option<(&str, &str)> {
    reference
        .split_once('/')
        .filter(|(unit, name)| !unit.is_empty() && !name.is_empty())
}

/// `nyl get output` and `nyl get artifact`: one value, resolved like `fromUnit`.
fn value_form(
    args: &GetValueArgs,
    artifact: bool,
    environment: &nyl_core::orchestration::ids::EnvironmentName,
    snapshot: &EnvironmentSnapshot,
) -> Result<CommandOutcome> {
    let Some((unit, name)) = split_reference(&args.reference) else {
        return Ok(refused(&Finding::new(
            Code::ReferenceInvalid,
            format!("{:?} is not of the form <unit>/<name>", args.reference),
            if artifact {
                "name the artifact as <unit>/<artifact>, such as web-image/image"
            } else {
                "name the output as <unit>/<output>, such as network/vpcId"
            },
        )));
    };
    let source = UnitInputSource {
        environment: None,
        unit: unit.to_owned(),
        output: (!artifact).then(|| name.to_owned()),
        artifact: artifact.then(|| name.to_owned()),
        kind: None,
        pointer: args.pointer.clone(),
        evidence: None,
        attestations: Vec::new(),
    };
    let (value, warning) = match inspect::get_value(environment, snapshot, &source, args.allow_stale) {
        Ok(GetValue::Value(value)) => (value, None),
        Ok(GetValue::Stale { value, warning }) => (value, Some(warning)),
        Err(finding) => return Ok(refused(&finding)),
    };
    let stdout = match args.state.output {
        GetOutput::Json => pretty_json(&value),
        GetOutput::Yaml => yaml(&value)?,
        GetOutput::Table | GetOutput::Wide | GetOutput::Name => match &value {
            Value::String(text) => format!("{text}\n"),
            other => format!("{other}\n"),
        },
    };
    let mut outcome = printed(stdout, value);
    if let Some(warning) = warning {
        write_finding(&mut outcome.stderr, "warning", None, &warning);
    }
    Ok(outcome)
}

/// `nyl get artifacts -e <env>`: the artifacts of the latest receipts.
fn artifacts_table(snapshot: &EnvironmentSnapshot, output: GetOutput) -> CommandOutcome {
    let documents: BTreeMap<String, &BTreeMap<String, ArtifactDocument>> = snapshot
        .artifacts
        .iter()
        .map(|(unit, artifacts)| (unit.to_string(), artifacts))
        .collect();
    let report = serde_json::to_value(&documents).expect("artifacts serialize");
    let stdout = match output {
        GetOutput::Json => pretty_json(&report),
        GetOutput::Yaml => yaml(&report).unwrap_or_default(),
        GetOutput::Name => lines(
            snapshot
                .artifacts
                .iter()
                .flat_map(|(unit, artifacts)| artifacts.keys().map(move |name| format!("{unit}/{name}"))),
        ),
        GetOutput::Table | GetOutput::Wide => {
            let rows = snapshot
                .artifacts
                .iter()
                .flat_map(|(unit, artifacts)| {
                    artifacts.iter().map(move |(name, document)| {
                        vec![
                            Cell::plain(unit),
                            Cell::plain(name),
                            Cell::plain(&document.kind),
                            Cell::plain(artifact_summary(document)),
                        ]
                    })
                })
                .collect::<Vec<_>>();
            format!("{}\n", render_table(&["UNIT", "NAME", "KIND", "SUMMARY"], &rows))
        }
    };
    printed(stdout, report)
}

/// One line about an artifact: its reference, digest, or commit when its
/// spec has one, else its compact spec.
fn artifact_summary(document: &ArtifactDocument) -> String {
    for field in ["reference", "digest", "commit", "url"] {
        if let Some(value) = document.spec.get(field).and_then(Value::as_str) {
            return value.to_owned();
        }
    }
    let compact = document.spec.to_string();
    if compact.chars().count() > 60 {
        format!("{}…", compact.chars().take(59).collect::<String>())
    } else {
        compact
    }
}

/// One item per line, as `-o name` prints them.
fn lines(items: impl IntoIterator<Item = impl std::fmt::Display>) -> String {
    items.into_iter().fold(String::new(), |mut text, item| {
        let _ = writeln!(text, "{item}");
        text
    })
}

fn yaml(value: &Value) -> Result<String> {
    crate::yaml::serialize_yaml_value(value).map_err(NylError::YamlEmit)
}

/// `nyl get units` without `-e`: unit declarations and the environments selecting each.
fn unit_declarations(path: &Path, output: GetOutput) -> Result<CommandOutcome> {
    let inventory = discover_gitops_inventory(path, None)?;
    let units = discover_units(&inventory)?;
    let (environments, _) = declared(&inventory);
    let selecting = |labels: &BTreeMap<String, String>| {
        environments
            .iter()
            .filter(|environment| {
                environment
                    .spec
                    .unit_selector
                    .match_labels
                    .iter()
                    .all(|(key, value)| labels.get(key) == Some(value))
            })
            .map(|environment| environment.metadata.name.clone())
            .collect::<Vec<_>>()
    };
    let report = units
        .iter()
        .map(|(name, unit)| {
            (
                name.to_string(),
                json!({
                    "apiVersion": unit.envelope.api_version,
                    "kind": unit.envelope.kind,
                    "labels": unit.envelope.metadata.labels,
                    "environments": selecting(&unit.envelope.metadata.labels),
                    "file": format!("{}#document-{}", unit.source_path.display(), unit.document_index),
                }),
            )
        })
        .collect::<serde_json::Map<_, _>>();
    let report = Value::Object(report);
    let stdout = match output {
        GetOutput::Json => pretty_json(&report),
        GetOutput::Yaml => yaml(&report)?,
        GetOutput::Name => lines(units.keys()),
        GetOutput::Table | GetOutput::Wide => {
            let rows = units
                .iter()
                .map(|(name, unit)| {
                    let environments = selecting(&unit.envelope.metadata.labels);
                    vec![
                        Cell::plain(name),
                        Cell::plain(&unit.envelope.kind),
                        Cell::plain(if unit.envelope.metadata.labels.is_empty() {
                            "-".to_owned()
                        } else {
                            selector(&unit.envelope.metadata.labels)
                        }),
                        Cell::plain(if environments.is_empty() {
                            "-".to_owned()
                        } else {
                            environments.join(",")
                        }),
                        Cell::plain(format!(
                            "{}#document-{}",
                            unit.source_path.display(),
                            unit.document_index
                        )),
                    ]
                })
                .collect::<Vec<_>>();
            format!(
                "{}\n",
                render_table(&["NAME", "KIND", "LABELS", "ENVIRONMENTS", "FILE"], &rows)
            )
        }
    };
    Ok(printed(stdout, report))
}

/// The declaration forms: one table of the resources of `kind`.
fn declarations(kind: GitOpsResourceKind, selection: &GetResourceArgs) -> Result<CommandOutcome> {
    let inventory = discover_gitops_inventory(&selection.path, None)?;
    let resources = inventory
        .resources
        .values()
        .filter(|resource| resource.identity.kind == kind)
        .filter(|resource| {
            selection
                .name
                .as_deref()
                .is_none_or(|name| resource.identity.name == name)
        })
        .collect::<Vec<_>>();
    if let Some(name) = &selection.name {
        if resources.is_empty() {
            return Err(NylError::config(format!("{} {name:?} was not found", kind.as_str())));
        }
    }
    let mut effective_resources = resources.into_iter().cloned().collect::<Vec<_>>();
    let mut stderr = String::new();
    if kind == GitOpsResourceKind::Cluster {
        effective_clusters(&inventory, &mut effective_resources, &mut stderr)?;
    }
    let stdout = format!(
        "{}\n",
        resources_table(kind, &effective_resources.iter().collect::<Vec<_>>())
    );
    let names = effective_resources
        .iter()
        .map(|resource| resource.identity.name.clone())
        .collect::<Vec<_>>();
    Ok(CommandOutcome {
        exit: 0,
        report: json!({ "kind": kind.as_str(), "names": names }),
        stdout,
        stderr,
    })
}

fn effective_clusters(
    inventory: &GitOpsInventory,
    resources: &mut [DiscoveredGitOpsResource],
    stderr: &mut String,
) -> Result<()> {
    for resource in resources {
        let effective = crate::gitops::resolve_cluster_contract(inventory, &resource.identity.name)?;
        if effective.capabilities_source != resource.identity.name || effective.schemas_source != resource.identity.name
        {
            let _ = writeln!(
                stderr,
                "Cluster {}: capabilities from {}, CRD schemas from {}",
                resource.identity.name, effective.capabilities_source, effective.schemas_source
            );
        }
        resource.resource = Some(GitOpsResource::Cluster(effective.cluster));
    }
    Ok(())
}

fn resources_table(kind: GitOpsResourceKind, resources: &[&DiscoveredGitOpsResource]) -> String {
    let headers = match kind {
        GitOpsResourceKind::GitRepository => vec!["NAME", "REPOSITORY", "PUBLISH", "FILE"],
        GitOpsResourceKind::Cluster => vec!["NAME", "DESTINATION", "CONTEXT", "VERSION", "FILE"],
        GitOpsResourceKind::ArgoCDInstance => vec!["NAME", "CLUSTER", "NAMESPACE", "FILE"],
        GitOpsResourceKind::DeploymentTarget => vec!["NAME", "CLUSTER", "PUBLICATION", "PATH-PREFIX", "FILE"],
        GitOpsResourceKind::AppProjectDefinition => vec!["NAME", "MANAGEMENT", "FILE"],
        GitOpsResourceKind::ApplicationGroup => vec!["NAME", "PROJECT", "SOURCE", "FILE"],
        GitOpsResourceKind::Environment => vec!["NAME", "UNITS", "STATE", "FILE"],
        GitOpsResourceKind::EnvironmentGroup => vec!["NAME", "SELECTOR", "FILE"],
    };
    let rows = resources
        .iter()
        .map(|resource| resource_row(kind, resource).into_iter().map(Cell::plain).collect())
        .collect::<Vec<_>>();
    render_table(&headers, &rows)
}

fn resource_row(kind: GitOpsResourceKind, resource: &DiscoveredGitOpsResource) -> Vec<String> {
    let name = &resource.identity.name;
    let file = format!(
        "{}#document-{}",
        resource.source_path.display(),
        resource.document_index
    );
    match resource.resource.as_ref() {
        Some(GitOpsResource::GitRepository(repository)) => vec![
            name.clone(),
            repository.spec.repo_url.clone(),
            repository.spec.publish_url.clone().unwrap_or_else(|| "-".to_owned()),
            file,
        ],
        Some(GitOpsResource::Cluster(cluster)) => {
            let destination = cluster
                .spec
                .destination
                .server
                .as_deref()
                .or(cluster.spec.destination.name.as_deref())
                .unwrap_or("-");
            let context = cluster.spec.live.as_ref().map_or("-", |live| live.context.as_str());
            let version = cluster
                .spec
                .kubernetes
                .as_ref()
                .and_then(|value| value.kube_version.as_deref())
                .unwrap_or("-");
            vec![
                name.clone(),
                destination.to_owned(),
                context.to_owned(),
                version.to_owned(),
                file,
            ]
        }
        Some(GitOpsResource::ArgoCDInstance(instance)) => vec![
            name.clone(),
            instance.spec.cluster_ref.name.clone(),
            instance.spec.namespace.clone(),
            file,
        ],
        Some(GitOpsResource::DeploymentTarget(target)) => {
            let repository = target
                .spec
                .publication
                .repository_ref
                .as_ref()
                .map(|reference| reference.name.as_str())
                .or_else(|| {
                    target
                        .spec
                        .publication
                        .repository
                        .as_ref()
                        .map(|repository| repository.repo_url.as_str())
                })
                .unwrap_or("-");
            vec![
                name.clone(),
                target.cluster_name().to_owned(),
                format!("{}@{}", repository, target.spec.publication.revision),
                target.publication_path_prefix().to_owned(),
                file,
            ]
        }
        Some(GitOpsResource::AppProjectDefinition(project)) => {
            vec![name.clone(), format!("{:?}", project.spec.management), file]
        }
        Some(GitOpsResource::ApplicationGroup(group)) => {
            // A group without a projectRef generates its project, from its own
            // template or from the implied permissive one.
            let project = group.spec.project_ref.clone().unwrap_or_else(|| {
                let generated = group
                    .spec
                    .project_template
                    .as_ref()
                    .and_then(|template| template.name.clone())
                    .unwrap_or_else(|| name.clone());
                format!("{generated} (generated)")
            });
            let source = group.spec.source.as_ref().map_or("-", |source| source.git.path());
            vec![name.clone(), project, source.to_owned(), file]
        }
        Some(GitOpsResource::Environment(environment)) => environment_row(name, environment, file),
        Some(GitOpsResource::EnvironmentGroup(group)) => vec![
            name.clone(),
            selector(&group.spec.environment_selector.match_labels),
            file,
        ],
        None => templated_resource_row(kind, name, file),
    }
}

fn environment_row(name: &str, environment: &crate::resources::environment::Environment, file: String) -> Vec<String> {
    let state = &environment.spec.state;
    let repository = state
        .repository_ref
        .as_ref()
        .map(|reference| reference.name.clone())
        .or_else(|| state.repository.as_ref().map(|repository| repository.repo_url.clone()))
        .unwrap_or_else(|| "default".to_owned());
    let desired = state
        .desired_ref
        .clone()
        .unwrap_or_else(|| format!("nyl/{name}/desired"));
    vec![
        name.to_owned(),
        selector(&environment.spec.unit_selector.match_labels),
        format!("{repository}@{desired}"),
        file,
    ]
}

fn selector(labels: &std::collections::BTreeMap<String, String>) -> String {
    if labels.is_empty() {
        return "*".to_owned();
    }
    labels
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(",")
}

fn templated_resource_row(kind: GitOpsResourceKind, name: &str, file: String) -> Vec<String> {
    match kind {
        GitOpsResourceKind::Cluster | GitOpsResourceKind::DeploymentTarget => {
            vec![
                name.to_owned(),
                "<templated>".to_owned(),
                "<templated>".to_owned(),
                "<templated>".to_owned(),
                file,
            ]
        }
        GitOpsResourceKind::GitRepository
        | GitOpsResourceKind::ArgoCDInstance
        | GitOpsResourceKind::ApplicationGroup
        | GitOpsResourceKind::Environment => vec![
            name.to_owned(),
            "<templated>".to_owned(),
            "<templated>".to_owned(),
            file,
        ],
        GitOpsResourceKind::AppProjectDefinition | GitOpsResourceKind::EnvironmentGroup => {
            vec![name.to_owned(), "<templated>".to_owned(), file]
        }
    }
}
