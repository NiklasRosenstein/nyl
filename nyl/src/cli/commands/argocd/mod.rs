//! `nyl argocd`: inspect and purge an Argo CD Application's live resources from
//! the inventory it records, without generating manifests.

mod purge;
mod resources;

use clap::{Args, Subcommand, ValueEnum};
use kube::api::{Api, ApiResource, DynamicObject};

use crate::cli::commands::cluster::{
    load_cluster_kube_config, resolve_cluster_source, resolve_target_cluster_from_inventory, ClusterSource,
};
use nyl_render::gitops::application_purge::{
    application_gvk, check_declared_destination, check_live_destination, parse_application, ApplicationInventory,
};
use nyl_render::kubernetes::KubeRsClient;
use nyl_render::{NylError, Result};

/// Inspect and purge Argo CD Applications' live resources
#[derive(Args, Debug)]
pub struct ArgocdArgs {
    #[command(subcommand)]
    pub command: ArgocdSubcommand,
}

#[derive(Subcommand, Debug)]
pub enum ArgocdSubcommand {
    /// List the resources an Application tracks in its status, without reading Git
    Resources(resources::ResourcesArgs),

    /// Delete the live resources an Application tracks, without generating manifests
    Purge(purge::PurgeArgs),
}

/// Options shared by the `argocd` subcommands to find an Application.
#[derive(Args, Debug)]
pub struct ApplicationArgs {
    /// Name of the Argo CD Application
    pub application: String,

    /// Deployment target whose ArgoCDInstance runs the Application. Optional when exactly one target is configured.
    #[arg(long)]
    pub target: Option<String>,

    /// Kubernetes context of the Argo CD cluster: overrides the ArgoCDInstance Cluster's context,
    /// or selects the cluster when no GitOps configuration is declared
    #[arg(long)]
    pub context: Option<String>,

    /// Namespace of the Application when no GitOps configuration is declared (default: argocd)
    #[arg(long, value_name = "NAMESPACE")]
    pub argocd_namespace: Option<String>,

    /// Output format
    #[arg(short, long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

/// Output format of the `argocd` subcommands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    Table,
    Json,
    Yaml,
}

pub async fn execute(args: ArgocdArgs) -> Result<()> {
    match args.command {
        ArgocdSubcommand::Resources(args) => resources::execute(args).await,
        ArgocdSubcommand::Purge(args) => purge::execute(args).await,
    }
}

/// How the destination cluster of an Application is reached.
enum Destination {
    /// The target's declared workload Cluster.
    Declared(Box<nyl_render::resources::Cluster>),
    /// The cluster Argo CD runs in, reached through `--context`.
    ArgoCluster,
    /// No GitOps configuration, and the Application deploys elsewhere.
    Undeclared,
}

/// The Application, the client of the cluster Argo CD runs in, and how its
/// destination is reached.
struct LocatedApplication {
    inventory: ApplicationInventory,
    argocd_client: kube::Client,
    destination: Destination,
}

/// Find the Application: on the target's ArgoCDInstance when GitOps
/// configuration is declared, otherwise through `--context`.
async fn locate_application(args: &ApplicationArgs) -> Result<LocatedApplication> {
    let cwd = std::env::current_dir()?;
    let (source, inventory) = resolve_cluster_source(&cwd, args.target.as_deref(), args.context.as_deref(), true)?;
    let (config, namespace, destination) = match &source {
        ClusterSource::Target(name) => {
            if args.argocd_namespace.is_some() {
                return Err(NylError::config(
                    "--argocd-namespace applies only without GitOps configuration; \
                     the target's ArgoCDInstance names the namespace",
                ));
            }
            let inventory = inventory
                .as_ref()
                .ok_or_else(|| NylError::config("GitOps configuration was not discovered"))?;
            let resolved = resolve_target_cluster_from_inventory(inventory, name)?;
            let (argocd_cluster, namespace) =
                nyl_render::gitops::tree::target_argocd_location(inventory, &resolved.target)?;
            let config = load_cluster_kube_config(&argocd_cluster, args.context.as_deref()).await?;
            (config, namespace, Destination::Declared(Box::new(resolved.cluster)))
        }
        ClusterSource::LiveContext(context) => {
            let config = KubeRsClient::load_kube_config(None, Some(context)).await?;
            let namespace = args.argocd_namespace.clone().unwrap_or_else(|| "argocd".to_owned());
            (config, namespace, Destination::Undeclared)
        }
        ClusterSource::None => {
            return Err(NylError::config(
                "This operation requires --target or --context to select a cluster",
            ));
        }
    };
    let argocd_client = kube::Client::try_from(config)?;
    let api: Api<DynamicObject> = Api::namespaced_with(
        argocd_client.clone(),
        &namespace,
        &ApiResource::from_gvk(&kube::core::GroupVersionKind::gvk(
            &application_gvk().group,
            &application_gvk().version,
            &application_gvk().kind,
        )),
    );
    let application = api
        .get_opt(&args.application)
        .await
        .map_err(|error| NylError::Kubernetes(format!("Cannot read Application {:?}: {error}", args.application)))?
        .ok_or_else(|| {
            NylError::config(format!(
                "NYL-ARGOCD-APPLICATION-UNKNOWN: Application {:?} was not found in namespace {namespace:?}; \
                 check the name and the Argo CD namespace",
                args.application
            ))
        })?;
    let inventory = parse_application(&serde_json::to_value(&application)?)?;
    let destination = match destination {
        Destination::Declared(cluster) => {
            check_declared_destination(&inventory, &cluster.spec.destination)?;
            Destination::Declared(cluster)
        }
        Destination::Undeclared if inventory.destination.is_in_cluster() => Destination::ArgoCluster,
        other => other,
    };
    Ok(LocatedApplication {
        inventory,
        argocd_client,
        destination,
    })
}

/// Connect to the cluster the Application deploys to.
async fn connect_destination(located: &LocatedApplication, destination_context: Option<&str>) -> Result<kube::Client> {
    match (&located.destination, destination_context) {
        (Destination::Declared(cluster), context) => Ok(kube::Client::try_from(
            load_cluster_kube_config(cluster, context).await?,
        )?),
        (_, Some(context)) => {
            let config = KubeRsClient::load_kube_config(None, Some(context)).await?;
            check_live_destination(&located.inventory, config.cluster_url.to_string().as_str())?;
            Ok(kube::Client::try_from(config)?)
        }
        (Destination::ArgoCluster, None) => Ok(located.argocd_client.clone()),
        (Destination::Undeclared, None) => Err(NylError::config(format!(
            "NYL-ARGOCD-DESTINATION-CONTEXT: Application {:?} deploys to another cluster than Argo CD runs in; \
             pass --destination-context with that cluster's kube context",
            located.inventory.name
        ))),
    }
}

fn print_serialized(value: &impl serde::Serialize, format: OutputFormat) -> Result<()> {
    match format {
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(value)?),
        OutputFormat::Yaml => print!("{}", nyl_render::yaml::serialize_yaml_document(value)?),
        OutputFormat::Table => unreachable!("tables are rendered by each subcommand"),
    }
    Ok(())
}

fn display_namespace(namespace: Option<&str>) -> &str {
    match namespace {
        Some(namespace) if !namespace.is_empty() => namespace,
        _ => "-",
    }
}
