use clap::Parser;
use nyl::cli::Cli;
use tracing_subscriber::EnvFilter;

fn main() {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    // Parse CLI and execute
    let cli = Cli::parse();
    if let Err(e) = cli.execute() {
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
}
