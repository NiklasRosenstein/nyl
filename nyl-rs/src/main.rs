use clap::Parser;
use nyl::cli::Cli;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    // Parse CLI first to get verbose flag
    let cli = Cli::parse();

    // Initialize tracing based on verbose flag
    let log_level = if cli.verbose { "nyl=debug,info" } else { "nyl=info,warn" };

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(log_level)))
        .init();

    // Execute command
    if let Err(e) = cli.execute().await {
        tracing::error!("{e}");
        std::process::exit(1);
    }
}
