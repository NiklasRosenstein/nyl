use clap::{Args, ValueEnum};
use serde::Serialize;

use crate::{
    config::ProjectConfig,
    kubernetes::{KubeClient, KubeRsClient},
    NylError, Result,
};

/// Display Kubernetes cluster version information
#[derive(Args, Debug)]
pub struct ClusterInfoArgs {
    /// Profile to use for connecting to the cluster
    #[arg(short, long)]
    pub profile: Option<String>,

    /// Output format
    #[arg(long, default_value = "text", value_enum)]
    pub output: OutputFormat,
}

/// Output format for cluster info
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum OutputFormat {
    /// Human-readable text output
    Text,
    /// JSON output
    Json,
    /// YAML output
    Yaml,
    /// CSV output (for scripting with --offline mode)
    Csv,
}

/// Cluster information structure
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ClusterInfo {
    kube_version: String,
    api_versions: Vec<String>,
}

pub async fn execute(args: ClusterInfoArgs) -> Result<()> {
    // 1. Load profile configuration
    let project_config = ProjectConfig::load_with_warning(None)?;

    // 2. Get profile (honors the configured target kube context)
    let (profile, _profile_name) =
        crate::cli::commands::render::select_profile_from_project(&project_config, args.profile.as_deref())?;

    // 3. Initialize Kubernetes client
    let client = KubeRsClient::from_profile(&profile, None).await?;

    // 4. Fetch cluster information
    let kube_version = client.get_server_version().await?;
    let api_versions = client.get_api_versions().await?;

    let info = ClusterInfo {
        kube_version,
        api_versions,
    };

    // 5. Output in requested format
    match args.output {
        OutputFormat::Text => output_text(&info),
        OutputFormat::Json => output_json(&info)?,
        OutputFormat::Yaml => output_yaml(&info)?,
        OutputFormat::Csv => output_csv(&info),
    }

    Ok(())
}

fn output_text(info: &ClusterInfo) {
    println!("Kubernetes Version: {}", info.kube_version);
    println!("API Versions:");
    for version in &info.api_versions {
        println!("  - {}", version);
    }
}

fn output_json(info: &ClusterInfo) -> Result<()> {
    let json =
        serde_json::to_string_pretty(info).map_err(|e| NylError::Config(format!("Failed to serialize JSON: {}", e)))?;
    println!("{}", json);
    Ok(())
}

fn output_yaml(info: &ClusterInfo) -> Result<()> {
    let yaml = serde_norway::to_string(info)?;
    print!("{}", yaml);
    Ok(())
}

fn output_csv(info: &ClusterInfo) {
    // First line: kube version
    println!("{}", info.kube_version);
    // Second line: comma-separated API versions
    println!("{}", info.api_versions.join(","));
}
