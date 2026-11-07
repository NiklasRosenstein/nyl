use clap::Args;

use super::{display_namespace, locate_application, print_serialized, ApplicationArgs, OutputFormat};
use crate::cli::table::{render as render_table, Cell};
use nyl_render::Result;

/// List the resources an Application tracks in its status
#[derive(Args, Debug)]
pub struct ResourcesArgs {
    #[command(flatten)]
    pub application: ApplicationArgs,
}

pub async fn execute(args: ResourcesArgs) -> Result<()> {
    let located = locate_application(&args.application).await?;
    let inventory = &located.inventory;
    if args.application.output != OutputFormat::Table {
        return print_serialized(inventory, args.application.output);
    }
    if inventory.resources.is_empty() {
        println!(
            "Application {:?} records no resources in its status (NYL-ARGOCD-NO-RESOURCES)",
            inventory.name
        );
        return Ok(());
    }
    let rows: Vec<Vec<Cell>> = inventory
        .resources
        .iter()
        .map(|resource| {
            let key = resource.key();
            vec![
                Cell::plain(key.gvk.to_api_version()),
                Cell::plain(&resource.kind),
                Cell::plain(display_namespace(key.namespace.as_deref())),
                Cell::plain(&resource.name),
                Cell::plain(resource.status.as_deref().unwrap_or("-")),
            ]
        })
        .collect();
    println!(
        "{}",
        render_table(&["API VERSION", "KIND", "NAMESPACE", "NAME", "STATUS"], &rows)
    );
    Ok(())
}
