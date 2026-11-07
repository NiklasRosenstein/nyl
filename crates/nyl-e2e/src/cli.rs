//! The command line: `run`, `setup`, and `teardown`.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};

use crate::infra::Infra;
use crate::log;
use crate::reconciler::reconciler_for;
use crate::runner::{self, Outcome, RunOptions};
use crate::variant::{Selection, Variant};

/// The cluster end-to-end suite of Nyl.
///
/// One scenario (build an image, apply OpenTofu, publish and deploy a Release
/// that consumes both, attest, observe, change an input, tear down) runs on a
/// kind cluster with each reconciler. It drives the real `nyl` binary, so it
/// needs docker, kind, kubectl, and OpenTofu; see crates/nyl-e2e/README.md.
#[derive(Parser, Debug)]
#[command(name = "nyl-e2e", version)]
pub struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the scenario for one or all variants.
    Run(RunArgs),
    /// Create the infrastructure of a variant for debugging and leave it
    /// running: the kind cluster, the registry, the Git server, and Argo CD.
    Setup(Common),
    /// Remove the infrastructure of a variant.
    Teardown(Common),
}

#[derive(Args, Debug)]
struct Common {
    /// Which reconciler to run: `direct`, `argocd-kubernetes`,
    /// `argocd-api`, or `all`.
    #[arg(long, value_enum, default_value = "all")]
    variant: Selection,

    /// Where each variant keeps its kubeconfig, checkout, and logs
    /// (default: nyl-e2e in the temporary directory).
    #[arg(long, env = "NYL_E2E_WORK_DIR")]
    work_dir: Option<PathBuf>,
}

#[derive(Args, Debug)]
struct RunArgs {
    #[command(flatten)]
    common: Common,

    /// The `nyl` binary to test (default: the `nyl` next to this executable).
    #[arg(long, env = "NYL_E2E_NYL")]
    nyl: Option<PathBuf>,

    /// Leave the cluster and containers running afterwards, also after a failure.
    #[arg(long)]
    keep: bool,
}

fn work_dir(common: &Common) -> PathBuf {
    common
        .work_dir
        .clone()
        .unwrap_or_else(|| std::env::temp_dir().join("nyl-e2e"))
}

/// The `nyl` binary built beside this one, which `cargo build` produces.
fn default_nyl() -> Option<PathBuf> {
    let executable = std::env::current_exe().ok()?;
    let candidate = executable
        .parent()?
        .join(format!("nyl{}", std::env::consts::EXE_SUFFIX));
    candidate.is_file().then_some(candidate)
}

pub fn main() -> ExitCode {
    log::init();
    let cli = Cli::parse();
    match cli.command {
        Command::Run(args) => run(args),
        Command::Setup(common) => setup(&common),
        Command::Teardown(common) => teardown(&common),
    }
}

fn run(args: RunArgs) -> ExitCode {
    let variants = args.common.variant.variants();
    let nyl = match args.nyl.clone().or_else(default_nyl) {
        Some(nyl) => nyl,
        None => {
            eprintln!("error: no nyl binary: pass --nyl, set NYL_E2E_NYL, or build `nyl` beside nyl-e2e");
            return ExitCode::from(1);
        }
    };
    // A variant that cannot run yet needs no tools, so it is reported without them.
    let runnable = variants
        .iter()
        .any(|variant| reconciler_for(*variant).availability().is_ok());
    if runnable {
        if let Err(error) = runner::check_prerequisites(Some(&nyl)) {
            eprintln!("error: {error}");
            return ExitCode::from(1);
        }
    }
    let options = RunOptions {
        nyl,
        work_dir: work_dir(&args.common),
        keep: args.keep,
    };
    let outcomes = variants
        .into_iter()
        .map(|variant| (variant, runner::run_variant(variant, &options)))
        .collect::<Vec<(Variant, Outcome)>>();
    eprint!("{}", runner::summary(&outcomes));
    ExitCode::from(runner::exit_code(&outcomes))
}

fn setup(common: &Common) -> ExitCode {
    if let Err(error) = runner::check_prerequisites(None) {
        eprintln!("error: {error}");
        return ExitCode::from(1);
    }
    let work_dir = work_dir(common);
    let mut failed = false;
    for variant in common.variant.variants() {
        if let Err(error) = reconciler_for(variant).availability() {
            eprintln!("{}: {error}", variant.name());
            continue;
        }
        let directory = runner::variant_directory(&work_dir, variant);
        log::open_transcript(&directory.join("logs").join("transcript.log"));
        match Infra::up(variant, &directory) {
            Ok((infra, _)) => print_setup(&infra),
            Err(error) => {
                eprintln!("error: {}: {error}", variant.name());
                failed = true;
            }
        }
    }
    ExitCode::from(u8::from(failed))
}

fn print_setup(infra: &Infra) {
    let name = infra.variant.name();
    eprintln!("\n{name} is up:");
    eprintln!(
        "  kubectl:   kubectl --kubeconfig {} --context {} get pods -A",
        infra.kubeconfig.display(),
        crate::infra::KUBE_CONTEXT
    );
    eprintln!(
        "  registry:  {} (host), pulled by the nodes as the same name",
        infra.registry_host()
    );
    eprintln!(
        "  git:       {} (host), {} (cluster)",
        infra.git_publish_url(),
        infra.git_repo_url()
    );
    if let Some(url) = infra.argocd_url() {
        eprintln!("  argo cd:   {url} (API server, plain HTTP on the loopback)");
    }
    eprintln!("  run it:    nyl-e2e run --variant {name} --keep");
    eprintln!("  remove it: nyl-e2e teardown --variant {name}");
}

fn teardown(common: &Common) -> ExitCode {
    let work_dir = work_dir(common);
    for variant in common.variant.variants() {
        let directory = runner::variant_directory(&work_dir, variant);
        log::line(format!("removing the infrastructure of {}", variant.name()));
        Infra::down(variant, &directory);
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    #[test]
    fn test_the_command_line_is_well_formed() {
        Cli::command().debug_assert();
    }

    #[test]
    fn test_run_takes_a_variant_keep_and_a_binary() {
        let cli = Cli::try_parse_from([
            "nyl-e2e",
            "run",
            "--variant",
            "argocd-api",
            "--keep",
            "--nyl",
            "/bin/nyl",
        ])
        .unwrap();
        let Command::Run(args) = cli.command else {
            panic!("expected run");
        };
        assert_eq!(args.common.variant, Selection::ArgocdApi);
        assert!(args.keep);
        assert_eq!(args.nyl, Some(PathBuf::from("/bin/nyl")));
    }

    #[test]
    fn test_variant_defaults_to_all() {
        let cli = Cli::try_parse_from(["nyl-e2e", "teardown"]).unwrap();
        let Command::Teardown(common) = cli.command else {
            panic!("expected teardown");
        };
        assert_eq!(common.variant, Selection::All);
    }

    #[test]
    fn test_an_unknown_variant_is_rejected() {
        assert!(Cli::try_parse_from(["nyl-e2e", "run", "--variant", "flux"]).is_err());
    }
}
