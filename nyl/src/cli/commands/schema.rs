use clap::{Args, Subcommand};
use std::path::{Path, PathBuf};

use crate::{
    resources::schema::{schema_artifacts, ResourceKind},
    NylError, Result,
};

/// Resource selector accepted by the schema CLI.
pub type SchemaResourceKind = ResourceKind;

/// Generate project and resource schemas.
#[derive(Args, Debug)]
pub struct SchemaArgs {
    #[command(subcommand)]
    command: SchemaCommand,
}

#[derive(Subcommand, Debug)]
enum SchemaCommand {
    /// Generate JSON schema for nyl.toml project configuration.
    Config,

    /// Generate JSON schema for one resource kind.
    Resource {
        #[arg(value_enum)]
        kind: SchemaResourceKind,
        /// Require this exact API version for the selected resource.
        #[arg(long)]
        api_version: Option<String>,
    },

    /// Generate the aggregate schema for all GitOps resource kinds.
    Gitops,

    /// Write all project and resource schemas to a directory.
    All {
        #[arg(long)]
        output_dir: PathBuf,
    },
}

pub fn execute(args: SchemaArgs) -> Result<()> {
    match args.command {
        SchemaCommand::Config => print_schema(&crate::config::schema::generate_project_config_schema()),
        SchemaCommand::Resource { kind, api_version } => {
            if let Some(api_version) = api_version {
                if api_version != kind.api_version() {
                    return Err(NylError::config(format!(
                        "{} is defined in {}, not {api_version}",
                        kind.name(),
                        kind.api_version()
                    )));
                }
            }
            print_schema(&kind.schema())
        }
        SchemaCommand::Gitops => print_schema(&crate::resources::generate_gitops_aggregate_schema()),
        SchemaCommand::All { output_dir } => write_all_schemas(&output_dir),
    }
}

fn print_schema(schema: &serde_json::Value) -> Result<()> {
    print!("{}", serialize_schema(schema)?);
    Ok(())
}

fn serialize_schema(schema: &serde_json::Value) -> Result<String> {
    serde_json::to_string_pretty(schema)
        .map(|mut output| {
            output.push('\n');
            output
        })
        .map_err(|error| NylError::Config(format!("Failed to serialize schema JSON: {error}")))
}

fn write_all_schemas(output_dir: &Path) -> Result<()> {
    for (path, schema) in schema_artifacts() {
        let path = output_dir.join(path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, serialize_schema(&schema)?)?;
        println!("Generated {}", path.display());
    }
    Ok(())
}
