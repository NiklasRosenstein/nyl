use clap::Parser;
use nyl::cli::Cli;
use rustls::crypto::aws_lc_rs;
use tracing_indicatif::filter::{hide_indicatif_span_fields, IndicatifFilter};
use tracing_indicatif::IndicatifLayer;
use tracing_subscriber::fmt::format::DefaultFields;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

#[tokio::main]
async fn main() {
    install_rustls_crypto_provider();

    // Parse CLI first to get verbose flag and color choice
    let cli = Cli::parse();

    // Apply color choice before any output
    cli.color.apply();

    // Initialize tracing based on verbose flag
    // Suppress kube_client::client::builder errors since we handle and display them ourselves
    let log_level = if cli.verbose {
        "nyl=debug,kube_client::client::builder=off,info"
    } else {
        "nyl=info,kube_client::client::builder=off,warn"
    };

    let indicatif_layer =
        IndicatifLayer::new().with_span_field_formatter(hide_indicatif_span_fields(DefaultFields::new()));
    let stderr_writer = indicatif_layer.get_stderr_writer();
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(stderr_writer)
                .with_ansi(cli.color.should_use_ansi())
                .with_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(log_level))),
        )
        .with(indicatif_layer.with_filter(IndicatifFilter::new(false)))
        .init();

    // Execute command
    if let Err(e) = cli.execute().await {
        tracing::error!("{e}");
        std::process::exit(1);
    }
}

fn install_rustls_crypto_provider() {
    // reqwest's rustls backend requires a process-global provider in rustls 0.23.
    let _ = aws_lc_rs::default_provider().install_default();
}
