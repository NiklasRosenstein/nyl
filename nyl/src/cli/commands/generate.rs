use clap::{Args, Subcommand, ValueEnum};
use std::path::{Path, PathBuf};

use crate::{
    resources::{
        generate_gitops_aggregate_schema, generate_gitops_resource_schema, generate_release_schema, GitOpsResourceKind,
        RELEASE_SCHEMA_FILENAME,
    },
    NylError, Result,
};

/// Generate project and resource schemas.
#[derive(Args, Debug)]
pub struct GenerateArgs {
    #[command(subcommand)]
    pub command: GenerateSubcommand,
}

#[derive(Subcommand, Debug)]
pub enum GenerateSubcommand {
    /// Generate JSON schemas.
    Schema {
        #[command(subcommand)]
        command: SchemaSubcommand,
    },
}

#[derive(Subcommand, Debug)]
pub enum SchemaSubcommand {
    /// Generate JSON schema for nyl.toml project configuration.
    Config,

    /// Generate JSON schema for one GitOps resource kind.
    Resource {
        #[arg(value_enum)]
        kind: SchemaResourceKind,
    },

    /// Generate the aggregate schema for all GitOps resource kinds.
    Gitops,

    /// Write all project and GitOps schemas to a directory.
    All {
        #[arg(long)]
        output_dir: PathBuf,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum SchemaResourceKind {
    #[value(name = "GitRepository", alias = "git-repository", alias = "repository")]
    GitRepository,
    #[value(name = "Cluster", alias = "cluster")]
    Cluster,
    #[value(name = "DeploymentTarget", alias = "deployment-target", alias = "target")]
    DeploymentTarget,
    #[value(name = "AppProjectDefinition", alias = "app-project-definition", alias = "project")]
    AppProjectDefinition,
    #[value(name = "ApplicationGroup", alias = "application-group", alias = "group")]
    ApplicationGroup,
    #[value(name = "Release", alias = "release")]
    Release,
}

impl SchemaResourceKind {
    fn gitops_kind(self) -> Option<GitOpsResourceKind> {
        match self {
            Self::GitRepository => Some(GitOpsResourceKind::GitRepository),
            Self::Cluster => Some(GitOpsResourceKind::Cluster),
            Self::DeploymentTarget => Some(GitOpsResourceKind::DeploymentTarget),
            Self::AppProjectDefinition => Some(GitOpsResourceKind::AppProjectDefinition),
            Self::ApplicationGroup => Some(GitOpsResourceKind::ApplicationGroup),
            Self::Release => None,
        }
    }
}

pub fn execute(args: GenerateArgs) -> Result<()> {
    match args.command {
        GenerateSubcommand::Schema { command } => execute_schema(command),
    }
}

fn execute_schema(command: SchemaSubcommand) -> Result<()> {
    match command {
        SchemaSubcommand::Config => print_schema(&crate::config::schema::generate_project_config_schema()),
        SchemaSubcommand::Resource { kind } => print_schema(&match kind.gitops_kind() {
            Some(kind) => generate_gitops_resource_schema(kind),
            None => generate_release_schema(),
        }),
        SchemaSubcommand::Gitops => print_schema(&generate_gitops_aggregate_schema()),
        SchemaSubcommand::All { output_dir } => write_all_schemas(&output_dir),
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
    std::fs::create_dir_all(output_dir)?;
    write_schema(
        &output_dir.join("nyl.schema.json"),
        &crate::config::schema::generate_project_config_schema(),
    )?;
    for kind in GitOpsResourceKind::all() {
        write_schema(
            &output_dir.join(kind.schema_filename()),
            &generate_gitops_resource_schema(kind),
        )?;
    }
    write_schema(&output_dir.join(RELEASE_SCHEMA_FILENAME), &generate_release_schema())?;
    write_schema(
        &output_dir.join("gitops-resource.schema.json"),
        &generate_gitops_aggregate_schema(),
    )?;
    Ok(())
}

fn write_schema(path: &Path, schema: &serde_json::Value) -> Result<()> {
    std::fs::write(path, serialize_schema(schema)?)?;
    println!("Generated {}", path.display());
    Ok(())
}
