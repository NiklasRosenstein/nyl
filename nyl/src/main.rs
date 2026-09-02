use clap::Parser;
use nyl::cli::Cli;
use nyl::NylError;
use rustls::crypto::aws_lc_rs;
use std::path::{Path, PathBuf};
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
    let render_input_path = cli.render_input_path().map(std::string::ToString::to_string);

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
        if let Some(path) = render_input_path.as_deref() {
            emit_argocd_file_not_found_diagnostics(&e, path);
        }
        std::process::exit(1);
    }
}

fn install_rustls_crypto_provider() {
    // reqwest's rustls backend requires a process-global provider in rustls 0.23.
    let _ = aws_lc_rs::default_provider().install_default();
}

fn emit_argocd_file_not_found_diagnostics(error: &NylError, render_input_path: &str) {
    let NylError::Config(message) = error else {
        return;
    };

    if !message.starts_with("File not found: ") {
        return;
    }

    if !(std::env::var_os("ARGOCD_APP_NAME").is_some() || std::env::var_os("ARGOCD_APP_NAMESPACE").is_some()) {
        return;
    }

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("<unavailable>"));
    let raw = Path::new(render_input_path);
    let resolved = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        cwd.join(raw)
    };

    eprintln!("[nyl-debug] ---- begin argocd file-not-found diagnostics ----");
    eprintln!("[nyl-debug] cwd={}", cwd.display());
    eprintln!("[nyl-debug] render_input.raw={}", raw.display());
    eprintln!("[nyl-debug] render_input.resolved={}", resolved.display());
    eprintln!(
        "[nyl-debug] render_input.raw.exists={} is_file={}",
        raw.exists(),
        raw.is_file()
    );
    eprintln!(
        "[nyl-debug] render_input.resolved.exists={} is_file={}",
        resolved.exists(),
        resolved.is_file()
    );

    for key in [
        "ARGOCD_APP_NAME",
        "ARGOCD_APP_NAMESPACE",
        "ARGOCD_APP_PROJECT_NAME",
        "ARGOCD_APP_SOURCE_REPO_URL",
        "ARGOCD_APP_SOURCE_PATH",
        "ARGOCD_APP_SOURCE_TARGET_REVISION",
        "ARGOCD_APP_REVISION",
        "ARGOCD_ENV_NYL_CMP_TEMPLATE_INPUT",
        "NYL_CMP_TEMPLATE_INPUT",
        "ARGOCD_ENV_NYL_CMP_TARGET",
        "NYL_CMP_TARGET",
    ] {
        match std::env::var(key) {
            Ok(value) => eprintln!("[nyl-debug] env.{key}={value}"),
            Err(_) => eprintln!("[nyl-debug] env.{key}=<unset>"),
        }
    }

    eprintln!(
        "[nyl-debug] hint=Verify NYL_CMP_TEMPLATE_INPUT uses source-path-relative format, or starts with '/' for repo-root-relative format."
    );
    eprintln!("[nyl-debug] ---- end argocd file-not-found diagnostics ----");
}
