use clap::Parser;
use nyl::cli::Cli;
use nyl::NylError;
use std::path::{Path, PathBuf};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
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

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(log_level)))
        .with_writer(std::io::stderr)
        .with_ansi(cli.color.should_use_ansi())
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
    ] {
        match std::env::var(key) {
            Ok(value) => eprintln!("[nyl-debug] env.{}={}", key, value),
            Err(_) => eprintln!("[nyl-debug] env.{}=<unset>", key),
        }
    }

    eprintln!(
        "[nyl-debug] hint=Verify ARGOCD_APP_SOURCE_PATH + NYL_CMP_TEMPLATE_INPUT resolve to an existing file in the repo-server checkout."
    );
    eprintln!("[nyl-debug] ---- end argocd file-not-found diagnostics ----");
}
