use clap::Args;

use crate::{
    cli::commands::release::{format, OutputFormat},
    kubernetes::{KubernetesReleaseStorage, ReleaseStorage},
    NylError, Result,
};

/// Show details of a specific release
#[derive(Args, Debug)]
pub struct ShowArgs {
    /// Release name
    pub name: String,

    /// Release namespace
    #[arg(short, long)]
    pub namespace: String,

    /// Specific revision (default: latest)
    #[arg(short, long)]
    pub revision: Option<u32>,

    /// Also show full rendered manifest
    #[arg(short, long)]
    pub manifest: bool,

    /// Output format
    #[arg(short, long, default_value = "text")]
    pub output: OutputFormat,

    /// Kubernetes context to use
    #[arg(long)]
    pub context: Option<String>,
}

#[allow(clippy::too_many_lines)]
pub async fn execute(args: ShowArgs) -> Result<()> {
    // Create Kubernetes client
    let config = if let Some(ctx) = &args.context {
        let kubeconfig = kube::config::Kubeconfig::read()?;
        kube::Config::from_custom_kubeconfig(
            kubeconfig,
            &kube::config::KubeConfigOptions {
                context: Some(ctx.clone()),
                ..Default::default()
            },
        )
        .await?
    } else {
        kube::Config::infer().await?
    };
    let client = kube::Client::try_from(config)?;

    let storage = KubernetesReleaseStorage::new(client);

    // Get release
    let release: Option<crate::kubernetes::ReleaseState> = if let Some(revision) = args.revision {
        storage.get_release(&args.name, &args.namespace, revision).await?
    } else {
        storage.get_latest_release(&args.name, &args.namespace).await?
    };

    let release = release.ok_or_else(|| {
        NylError::Config(format!(
            "Release '{}' not found in namespace '{}'{}",
            args.name,
            args.namespace,
            args.revision.map_or(String::new(), |r| format!(" (revision {})", r))
        ))
    })?;

    // Get all revisions for context
    let revisions: Vec<u32> = storage.list_revisions(&args.name, &args.namespace).await?;

    // Output based on format
    match args.output {
        OutputFormat::Table => {
            // Text format - human-readable
            println!("Release: {}", release.release_name);
            println!("Namespace: {}", release.release_namespace);
            println!("Revision: {}", release.revision);
            println!("Status: {:?}", release.status);
            println!();

            println!("Timestamps:");
            println!("  Rendered: {}", format::format_timestamp(&release.rendered_at));
            if let Some(applied_at) = &release.applied_at {
                println!("  Applied:  {}", format::format_timestamp(applied_at));
            }
            println!();

            if let Some(error) = &release.error {
                println!("Error: {}", error);
                println!();
            }

            println!("Resources ({}):", release.resource_keys.len());
            for key in &release.resource_keys {
                let namespace = key.namespace.as_deref().unwrap_or("default");
                println!("  - {} {}/{}", key.gvk.kind, namespace, key.name);
            }
            println!();

            if revisions.len() > 1 {
                let other_revisions: Vec<u32> = revisions.into_iter().filter(|r| *r != release.revision).collect();
                if !other_revisions.is_empty() {
                    println!(
                        "Previous Revisions: {}",
                        other_revisions
                            .iter()
                            .map(|r| r.to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                }
            }

            if args.manifest {
                println!();
                println!("Manifest:");
                println!("---");
                print!("{}", release.manifest);
                if !release.manifest.ends_with('\n') {
                    println!();
                }
            }
        }
        OutputFormat::Json => {
            let json = if args.manifest {
                serde_json::to_string_pretty(&release)?
            } else {
                // Create a filtered version without manifest
                let mut value = serde_json::to_value(&release)?;
                if let Some(obj) = value.as_object_mut() {
                    obj.remove("manifest");
                }
                serde_json::to_string_pretty(&value)?
            };
            println!("{}", json);
        }
        OutputFormat::Yaml => {
            if args.manifest {
                let yaml = serde_norway::to_string(&release)?;
                print!("{}", yaml);
            } else {
                // Create a filtered version without manifest
                let mut value = serde_json::to_value(&release)?;
                if let Some(obj) = value.as_object_mut() {
                    obj.remove("manifest");
                }
                let yaml = serde_norway::to_string(&value)?;
                print!("{}", yaml);
            }
        }
        OutputFormat::Wide => {
            // Wide is same as table for show command - just use table format
            // Rerun the logic with table format by changing the match
            let mut new_args = args;
            new_args.output = OutputFormat::Table;

            // Text format - human-readable
            println!("Release: {}", release.release_name);
            println!("Namespace: {}", release.release_namespace);
            println!("Revision: {}", release.revision);
            println!("Status: {:?}", release.status);
            println!();

            println!("Timestamps:");
            println!("  Rendered: {}", format::format_timestamp(&release.rendered_at));
            if let Some(applied_at) = &release.applied_at {
                println!("  Applied:  {}", format::format_timestamp(applied_at));
            }
            println!();

            if let Some(error) = &release.error {
                println!("Error: {}", error);
                println!();
            }

            println!("Resources ({}):", release.resource_keys.len());
            for key in &release.resource_keys {
                let namespace = key.namespace.as_deref().unwrap_or("default");
                println!("  - {} {}/{}", key.gvk.kind, namespace, key.name);
            }
            println!();

            if revisions.len() > 1 {
                let other_revisions: Vec<u32> = revisions.into_iter().filter(|r| *r != release.revision).collect();
                if !other_revisions.is_empty() {
                    println!(
                        "Previous Revisions: {}",
                        other_revisions
                            .iter()
                            .map(|r: &u32| r.to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                }
            }

            if new_args.manifest {
                println!();
                println!("Manifest:");
                println!("---");
                print!("{}", release.manifest);
                if !release.manifest.ends_with('\n') {
                    println!();
                }
            }
        }
    }

    Ok(())
}
