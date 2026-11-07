use clap::{Args, ValueEnum};
use colored::Colorize;
use dialoguer::Confirm;
use kube::api::{Api, ApiResource, DeleteParams, DynamicObject, Patch, PatchParams};

use super::{
    connect_destination, display_namespace, locate_application, print_serialized, ApplicationArgs, OutputFormat,
};
use crate::cli::table::{render as render_table, Cell};
use nyl_render::gitops::application_purge::{
    application_gvk, execute_purge, plan_purge, ApplicationAction, ApplicationPurgeReport, PurgeOptions, PurgeOutcome,
};
use nyl_render::kubernetes::{DeleteOptions, DeletePropagation, KubeRsClient, PruneEvent};
use nyl_render::{NylError, Result};

/// Delete the live resources an Argo CD Application tracks, without generating manifests
#[derive(Args, Debug)]
#[allow(clippy::struct_excessive_bools)] // Independent CLI switches compose without hidden state.
pub struct PurgeArgs {
    #[command(flatten)]
    pub application: ApplicationArgs,

    /// Kubernetes context of the cluster the Application deploys to, when that is not the cluster Argo CD runs in
    /// (or overrides the target Cluster's context)
    #[arg(long, value_name = "CONTEXT")]
    pub destination_context: Option<String>,

    /// Show what would be deleted without deleting anything
    #[arg(long)]
    pub dry_run: bool,

    /// Skip the confirmation prompt
    #[arg(short, long)]
    pub yes: bool,

    /// Also delete resources whose Argo CD sync option is `Delete=confirm`, such as managed Namespaces
    #[arg(long)]
    pub confirm_delete: bool,

    /// Delete resources even when their live object does not carry the Application's Argo CD tracking
    #[arg(long)]
    pub ignore_tracking: bool,

    /// How dependents of deleted resources are handled
    #[arg(long, value_enum, default_value_t = Propagation::Foreground)]
    pub propagation: Propagation,

    /// Grace period in seconds for deleted resources (default: each resource's own)
    #[arg(long, value_name = "SECONDS")]
    pub grace_period: Option<u32>,

    /// Maximum number of resources deleted at the same time
    #[arg(long, default_value_t = 8, value_parser = clap::value_parser!(u16).range(1..))]
    pub concurrency: u16,

    /// After every resource is deleted, remove the Application's finalizers and delete it
    #[arg(long)]
    pub delete_application: bool,
}

/// Kubernetes deletion propagation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Propagation {
    Foreground,
    Background,
    Orphan,
}

impl From<Propagation> for DeletePropagation {
    fn from(value: Propagation) -> Self {
        match value {
            Propagation::Foreground => Self::Foreground,
            Propagation::Background => Self::Background,
            Propagation::Orphan => Self::Orphan,
        }
    }
}

pub async fn execute(args: PurgeArgs) -> Result<()> {
    let located = locate_application(&args.application).await?;
    let destination = connect_destination(&located, args.destination_context.as_deref()).await?;
    let client = KubeRsClient::from_client(destination).await?;
    let options = PurgeOptions {
        confirm_delete: args.confirm_delete,
        ignore_tracking: args.ignore_tracking,
        delete: DeleteOptions {
            propagation: Some(args.propagation.into()),
            grace_period_seconds: args.grace_period,
        },
        concurrency: usize::from(args.concurrency),
    };
    let table = args.application.output == OutputFormat::Table;
    let mut report = plan_purge(&client, &located.inventory, &options).await;
    let to_delete = count(&report, &PurgeOutcome::WouldDelete);

    if args.dry_run {
        if args.delete_application {
            report.set_application_action(if report.application_deletable() {
                ApplicationAction::WouldDelete
            } else {
                ApplicationAction::Kept
            });
        }
        return finish(&report, args.application.output);
    }

    if !args.yes && (to_delete > 0 || args.delete_application) {
        if table {
            print_report(&report);
        }
        let prompt = format!(
            "Delete {to_delete} resource(s) of Application {:?}{}?",
            report.application,
            if args.delete_application {
                " and then the Application"
            } else {
                ""
            }
        );
        let confirmed = Confirm::new()
            .with_prompt(prompt)
            .default(false)
            .interact()
            .map_err(|error| NylError::Other(format!("Confirmation prompt failed: {error}")))?;
        if !confirmed {
            eprintln!("Cancelled");
            return Ok(());
        }
    }

    report = execute_purge(&client, report, &options, &mut |event| {
        if table {
            print_progress(&event);
        }
    })
    .await;

    if args.delete_application {
        if report.application_deletable() {
            delete_application(&located.argocd_client, &report.namespace, &report.application).await?;
            report.set_application_action(ApplicationAction::Deleted);
        } else {
            report.set_application_action(ApplicationAction::Kept);
        }
    }
    finish(&report, args.application.output)
}

fn count(report: &ApplicationPurgeReport, outcome: &PurgeOutcome) -> usize {
    report
        .resources
        .iter()
        .filter(|entry| &entry.outcome == outcome)
        .count()
}

/// Remove the Application's finalizers, so Argo CD does not try to cascade a
/// deletion it cannot generate manifests for, and delete it.
async fn delete_application(client: &kube::Client, namespace: &str, name: &str) -> Result<()> {
    let gvk = application_gvk();
    let api: Api<DynamicObject> = Api::namespaced_with(
        client.clone(),
        namespace,
        &ApiResource::from_gvk(&kube::core::GroupVersionKind::gvk(&gvk.group, &gvk.version, &gvk.kind)),
    );
    let not_found = |error: &kube::Error| matches!(error, kube::Error::Api(response) if response.code == 404);
    match api
        .patch(
            name,
            &PatchParams::default(),
            &Patch::Merge(serde_json::json!({"metadata": {"finalizers": null}})),
        )
        .await
    {
        Err(error) if !not_found(&error) => {
            return Err(NylError::Kubernetes(format!(
                "Cannot remove the finalizers of Application {name:?}: {error}"
            )))
        }
        _ => {}
    }
    match api.delete(name, &DeleteParams::default()).await {
        Err(error) if !not_found(&error) => Err(NylError::Kubernetes(format!(
            "Cannot delete Application {name:?}: {error}"
        ))),
        _ => Ok(()),
    }
}

fn print_progress(event: &PruneEvent<'_>) {
    match event {
        PruneEvent::Started { count } => eprintln!("Deleting {count} resource(s)..."),
        PruneEvent::Deleted { key, result: Ok(()) } => eprintln!("  {} Deleted {key}", "✓".green()),
        PruneEvent::Deleted {
            key,
            result: Err(error),
        } => {
            eprintln!("  {} Failed to delete {key}: {error}", "✗".red());
        }
        PruneEvent::Waiting { count } => eprintln!("  Waiting for {count} resource(s) to terminate..."),
        PruneEvent::WaitTimedOut { keys } => {
            eprintln!(
                "  {} {} resource(s) still terminating; continuing",
                "!".yellow(),
                keys.len()
            );
        }
        PruneEvent::Finished => {}
    }
}

fn print_report(report: &ApplicationPurgeReport) {
    let rows: Vec<Vec<Cell>> = report
        .resources
        .iter()
        .map(|entry| {
            let key = entry.resource.key();
            let label = entry.outcome.label();
            let result = match &entry.outcome {
                PurgeOutcome::Deleted | PurgeOutcome::WouldDelete | PurgeOutcome::AlreadyGone => Cell::plain(label),
                PurgeOutcome::Failed { .. } => Cell::styled(label, label.red()),
                _ => Cell::styled(label, label.yellow()),
            };
            let detail = match &entry.outcome {
                PurgeOutcome::Failed { error } => error.clone(),
                _ => entry.code.unwrap_or_default().to_owned(),
            };
            vec![
                Cell::plain(&entry.resource.kind),
                Cell::plain(display_namespace(key.namespace.as_deref())),
                Cell::plain(&entry.resource.name),
                result,
                Cell::plain(detail),
            ]
        })
        .collect();
    if !rows.is_empty() {
        println!(
            "{}",
            render_table(&["KIND", "NAMESPACE", "NAME", "RESULT", "DETAIL"], &rows)
        );
    }
}

fn finish(report: &ApplicationPurgeReport, format: OutputFormat) -> Result<()> {
    if format == OutputFormat::Table {
        print_report(report);
        let mut fixes: Vec<(&str, &str)> = report
            .resources
            .iter()
            .filter_map(|entry| entry.code.zip(entry.fix))
            .collect();
        fixes.sort_unstable();
        fixes.dedup();
        for (code, fix) in fixes {
            eprintln!("{}  {fix}", code.yellow());
        }
        for finding in &report.findings {
            eprintln!("{}  {}\n  {}", finding.code.yellow(), finding.message, finding.fix);
        }
        let application = match report.application_action {
            ApplicationAction::NotRequested => None,
            ApplicationAction::WouldDelete => Some("would be deleted"),
            ApplicationAction::Deleted => Some("deleted"),
            ApplicationAction::Kept => Some("kept"),
        };
        if let Some(action) = application {
            println!("\n  application: {}/{} {action}", report.namespace, report.application);
        }
        if report.dry_run {
            println!("{}", "[DRY RUN] Nothing was deleted".cyan());
        }
    } else {
        print_serialized(report, format)?;
    }
    let code = report.exit_category().exit_code();
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}
