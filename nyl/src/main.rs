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
    let log_level = default_log_filter(cli.verbose);

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
        if !matches!(e, nyl::NylError::ValidationReported(_)) {
            tracing::error!("{e}");
        }
        std::process::exit(1);
    }
}

/// Default tracing filter when `RUST_LOG` is unset. Every Nyl crate logs under
/// its own target, so each one is named explicitly.
fn default_log_filter(verbose: bool) -> &'static str {
    if verbose {
        "nyl=debug,nyl_render=debug,nyl_core=debug,kube_client::client::builder=off,info"
    } else {
        "nyl=info,nyl_render=info,nyl_core=info,kube_client::client::builder=off,warn"
    }
}

fn install_rustls_crypto_provider() {
    // reqwest's rustls backend requires a process-global provider in rustls 0.23.
    let _ = aws_lc_rs::default_provider().install_default();
}

#[cfg(test)]
mod tests {
    use super::default_log_filter;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::{EnvFilter, Layer};

    struct Counter(Arc<AtomicUsize>);

    impl<S: tracing::Subscriber> Layer<S> for Counter {
        fn on_event(&self, _: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn count_events(verbose: bool, emit: impl FnOnce()) -> usize {
        let count = Arc::new(AtomicUsize::new(0));
        let subscriber = tracing_subscriber::registry()
            .with(Counter(count.clone()).with_filter(EnvFilter::new(default_log_filter(verbose))));
        tracing::subscriber::with_default(subscriber, emit);
        count.load(Ordering::SeqCst)
    }

    #[test]
    fn test_default_log_filter_shows_library_crate_info_events() {
        let shown = count_events(false, || {
            tracing::info!(target: "nyl_render::gitops::tree", "shown");
            tracing::info!(target: "nyl_core::settings", "shown");
            tracing::debug!(target: "nyl_render::gitops::tree", "hidden");
        });
        assert_eq!(shown, 2);
    }

    #[test]
    fn test_verbose_log_filter_shows_library_crate_debug_events() {
        let shown = count_events(true, || {
            tracing::debug!(target: "nyl_render::helm", "shown");
            tracing::debug!(target: "nyl_core::digest", "shown");
            tracing::debug!(target: "other_crate", "hidden");
        });
        assert_eq!(shown, 2);
    }
}
