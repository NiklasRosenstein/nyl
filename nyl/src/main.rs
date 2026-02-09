use clap::Parser;
use nyl::cli::Cli;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    // Parse CLI first to get verbose flag
    let cli = Cli::parse();

    // Initialize tracing based on verbose flag
    // Suppress kube_client::client::builder errors since we handle and display them ourselves
    let log_level = if cli.verbose {
        "nyl=debug,kube_client::client::builder=off,info"
    } else {
        "nyl=info,kube_client::client::builder=off,warn"
    };

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(log_level)))
        .with_writer(std::io::stderr)
        .init();

    // Execute command
    if let Err(e) = cli.execute().await {
        tracing::error!("{e}");
        std::process::exit(1);
    }
}
