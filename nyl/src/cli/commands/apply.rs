use clap::Args;
use std::collections::HashMap;
use std::time::Instant;

use crate::cli::{
    apply_output::{ApplyRenderer, ApplyReport, Phase, PhaseTiming},
    commands::render::{
        run_render_preflight, ClusterClientRequirement, DiscoveryProgress, RenderOptions, RenderPreflightOptions,
    },
};
use nyl_render::{
    kubernetes::{
        fence::{ReleaseClaim, Requester},
        field_manager::{FieldOwnership, ARGOCD_FIELD_MANAGER},
        lifecycle::{
            apply_sorted_manifests, ensure_namespace_exists, prepare_desired_manifests, ApplySettings, Interruption,
            ReleaseRecorder,
        },
        KubernetesReleaseStorage, ResourceKey, ResourceOrdering,
    },
    NylError, Result,
};

/// Apply rendered manifests to the cluster
#[derive(Args, Debug)]
#[allow(clippy::struct_excessive_bools)] // Independent CLI switches compose without hidden state.
pub struct ApplyArgs {
    #[command(flatten)]
    pub common: RenderOptions,

    /// Release name (required if no Release in file)
    #[arg(long)]
    pub name: Option<String>,

    /// Release namespace (required if no Release in file)
    #[arg(long)]
    pub namespace: Option<String>,

    /// Kubernetes context: overrides the target Cluster's context, or selects a cluster when no GitOps configuration is declared
    #[arg(long)]
    pub context: Option<String>,

    /// Append to previous release instead of replacing it.
    /// Merges current resources with previous release (union, current wins on duplicates).
    /// Skips pruning to preserve resources from previous releases.
    #[arg(long)]
    pub append_release: bool,

    /// Prune resources whose Argo CD sync option is `Prune=confirm`, such as managed Namespaces.
    /// Without it they are kept and stay tracked until a confirmed apply.
    #[arg(long, conflicts_with = "no_release")]
    pub confirm_prune: bool,

    /// Apply a release an environment owns anyway: record no owner on the new revision,
    /// so the owning unit's next reconcile adopts the release again and reverts this change.
    #[arg(long, conflicts_with = "no_release")]
    pub take_over: bool,

    /// Apply resources without creating release revisions or pruning.
    #[arg(long, conflicts_with_all = ["append_release", "name", "namespace"])]
    pub no_release: bool,

    /// Maximum number of resources applied or pruned at the same time.
    ///
    /// Resources are applied in Argo CD's kind order, one batch per kind; the
    /// resources of a batch are applied concurrently and a batch completes before
    /// the next one starts. Use 1 to apply serially.
    #[arg(long, default_value_t = DEFAULT_APPLY_CONCURRENCY, value_parser = clap::value_parser!(u16).range(1..))]
    pub concurrency: u16,

    /// Maximum number of release revisions to keep; older revisions are deleted
    /// after this apply. 0 keeps every revision. Overrides `[release] history_limit`
    /// in nyl.toml.
    #[arg(
        long,
        value_name = "N",
        conflicts_with = "no_release",
        value_parser = nyl_render::config::parse_release_history_limit
    )]
    pub history_limit: Option<u32>,

    #[command(flatten)]
    pub handoff: HandoffArgs,
}

/// Field ownership options shared by the commands that apply a release.
#[derive(Args, Debug, Default)]
pub struct HandoffArgs {
    /// Apply as field manager MANAGER (default `argocd-controller`) and release
    /// the fields Nyl applied as `nyl` before, so a later Argo CD sync can remove
    /// fields dropped from the source. Use it before Argo CD takes over a
    /// resource Nyl applied, and for any apply while Argo CD manages it.
    #[arg(
        long,
        value_name = "MANAGER",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = ARGOCD_FIELD_MANAGER
    )]
    pub handoff: Option<String>,
}

impl HandoffArgs {
    /// The field ownership these options select.
    pub(crate) fn ownership(&self) -> Result<FieldOwnership> {
        FieldOwnership::from_handoff(self.handoff.as_deref())
    }
}

/// Default bound on concurrent apply and prune requests.
pub(crate) const DEFAULT_APPLY_CONCURRENCY: u16 = 8;

#[allow(clippy::too_many_lines)]
pub async fn execute(args: ApplyArgs) -> Result<()> {
    args.common.validation.check_complete(args.append_release)?;
    let ownership = args.handoff.ownership()?;
    let preflight = run_render_preflight(RenderPreflightOptions {
        common: &args.common,
        offline: false,
        kube_version: None,
        kube_api_versions: &[],
        context_override: args.context.as_deref(),
        cluster_client_requirement: ClusterClientRequirement::Required,
        discovery_progress: DiscoveryProgress::Announce,
    })
    .await?;

    let mut desired_manifests = preflight.manifests;
    let release = preflight.release;
    let mut duplicates = preflight.duplicates;
    let kube_client = preflight
        .kube_client
        .ok_or_else(|| NylError::Config("Kubernetes client unavailable in online mode".to_string()))?;
    let client = preflight
        .raw_client
        .ok_or_else(|| NylError::Config("Raw Kubernetes client unavailable in online mode".to_string()))?;
    let concurrency = usize::from(args.concurrency);
    let (discovery_mode, discovery_elapsed) = kube_client.initial_discovery();
    let mut timings = vec![PhaseTiming::new(Phase::Discovery(discovery_mode), discovery_elapsed)];

    // A release that renders nothing is still recorded, so the resources of its
    // previous revision are pruned. Without a release identity there is nothing to do.
    let has_release_identity = release.is_some() || args.name.is_some() || args.namespace.is_some();
    if desired_manifests.is_empty() && (args.no_release || !has_release_identity) {
        tracing::info!("No manifests to apply");
        return Ok(());
    }

    let destination_namespace = preflight.destination_namespace.as_deref().or(args.namespace.as_deref());

    let (collapsed, mut scopes) =
        prepare_desired_manifests(&kube_client, desired_manifests, &mut duplicates, destination_namespace).await?;
    desired_manifests = collapsed;

    // Display duplicate resources warning if any
    if !duplicates.is_empty() {
        print_duplicate_warning(&duplicates);
    }

    // 3. Sort resources into apply order (Argo CD's kind order).
    // Sort in place so the recorded release manifest is stored in the same order it
    // is applied (and consistent with `release rollback`, which also stores sorted).
    ResourceOrdering::sort_by_priority(&mut desired_manifests)?;

    tracing::info!("Validating {} manifests", desired_manifests.len());
    let validation_started = Instant::now();
    nyl_render::validation::validate_manifest_input(
        &args.common.validation,
        &preflight.project_config,
        &preflight.project_root,
        preflight
            .resolved_target
            .as_ref()
            .map(|target| target.cluster.metadata.name.as_str()),
        preflight.live_kube_version.as_deref(),
        nyl_render::validation::ManifestValidationInput {
            manifests: &desired_manifests,
            source: &args.common.path,
            provenance: &preflight.provenance,
        },
    )
    .await?;
    timings.push(PhaseTiming::new(Phase::Validation, validation_started.elapsed()));

    // 4. Resolve the release identity before touching the cluster, so a missing
    //    --name/--namespace fails before anything is applied.
    let release_identity = if args.no_release {
        None
    } else {
        Some(resolve_release_identity(release.as_ref(), args.name, args.namespace)?)
    };

    // 5. Create the release namespace before the resources that live in it, and
    //    record the new revision before touching anything else, so an apply that
    //    computed the same revision concurrently fails here rather than after both
    //    changed the cluster. Nyl creates this namespace either way to store release
    //    state.
    let storage = KubernetesReleaseStorage::new(client);
    let claim = ReleaseClaim {
        requester: Requester::Manual {
            take_over: args.take_over,
        },
        publication: preflight.release_publication.clone(),
    };
    let renderer = ApplyRenderer::new(&duplicates);
    let recorder = match &release_identity {
        Some((release_name, release_namespace)) => {
            ensure_namespace_exists(&kube_client, release_namespace, &ownership).await?;
            tracing::info!("Recording release {release_name} in namespace {release_namespace}");
            Some(
                ReleaseRecorder::reserve(
                    &storage,
                    release_name,
                    release_namespace,
                    &desired_manifests,
                    args.append_release,
                    &kube_client,
                    &mut scopes,
                    &claim,
                )
                .await?,
            )
        }
        None => None,
    };

    // 6. Apply manifests, printing each outcome as it completes, then complete the
    //    revision, prune resources no longer desired, and supersede the previous one.
    let Some(recorder) = recorder else {
        let apply_started = Instant::now();
        let apply_result = apply_sorted_manifests(
            &kube_client,
            &desired_manifests,
            &ownership,
            concurrency,
            &mut |event| {
                renderer.event(event);
            },
        )
        .await?;
        timings.push(PhaseTiming::new(Phase::Apply, apply_started.elapsed()));
        renderer.summary(&ApplyReport {
            outcomes: &apply_result.outcomes,
            failed_count: apply_result.failed_count,
            ownership: crate::cli::apply_output::ownership_report(&apply_result),
            prune_failed_count: 0,
            release: None,
            timings: &timings,
        });
        return apply_result.error("Apply").map_or(Ok(()), Err);
    };

    let history_limit = args
        .history_limit
        .unwrap_or_else(|| preflight.project_config.release_history_limit());
    let run = recorder
        .run(
            &kube_client,
            &desired_manifests,
            &mut scopes,
            ApplySettings {
                concurrency,
                history_limit,
                confirm_prune: args.confirm_prune,
                ownership: &ownership,
                protected: None,
            },
            Interruption {
                interrupt: async {
                    let _ = tokio::signal::ctrl_c().await;
                },
                // A second Ctrl-C exits without waiting for the failed revision to be recorded.
                escalate: async {
                    let _ = tokio::signal::ctrl_c().await;
                    std::process::exit(130)
                },
            },
            &mut |event| renderer.event(event),
        )
        .await?;
    timings.push(PhaseTiming::new(Phase::Apply, run.apply_elapsed));
    timings.push(PhaseTiming::new(Phase::Release, run.release_elapsed));
    renderer.summary(&crate::cli::apply_output::report_of(&run, &timings));
    run.error("Apply").map_or(Ok(()), Err)
}

/// Resolve the release name and namespace from the rendered Release or the CLI flags.
fn resolve_release_identity(
    release: Option<&nyl_render::resources::Release>,
    name: Option<String>,
    namespace: Option<String>,
) -> Result<(String, String)> {
    if let Some(release) = release {
        return Ok((release.metadata.name.clone(), release.metadata.namespace.clone()));
    }
    let name =
        name.ok_or_else(|| NylError::Config("No Release resource found. Specify --name and --namespace".to_string()))?;
    let namespace = namespace
        .ok_or_else(|| NylError::Config("No Release resource found. Specify --name and --namespace".to_string()))?;
    Ok((name, namespace))
}

/// Print a warning trace about duplicate resources
pub(crate) fn print_duplicate_warning(duplicates: &HashMap<ResourceKey, usize>) {
    if duplicates.is_empty() {
        return;
    }

    let total_unique = duplicates.len();
    let total_ignored: usize = duplicates.values().map(|count| count - 1).sum();

    tracing::warn!(
        "Found {} unique resources with duplicates ({} total duplicates ignored, keeping last occurrence)",
        total_unique,
        total_ignored
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `--handoff` alone hands over to Argo CD, a value names the manager, and the
    /// option never takes the positional file as its value.
    #[test]
    fn test_handoff_option_parsing() {
        use clap::Parser;
        let parse = |args: &[&str]| {
            let cli = crate::cli::Cli::try_parse_from(std::iter::once("nyl").chain(args.iter().copied())).unwrap();
            match cli.command {
                crate::cli::Commands::Apply(apply) => (apply.handoff.handoff, apply.common.path),
                _ => unreachable!(),
            }
        };
        assert_eq!(parse(&["apply", "release.yaml"]).0, None);
        let (manager, path) = parse(&["apply", "--handoff", "release.yaml"]);
        assert_eq!(manager.as_deref(), Some(ARGOCD_FIELD_MANAGER));
        assert_eq!(path, "release.yaml");
        assert_eq!(
            parse(&["apply", "--handoff=flux", "release.yaml"]).0.as_deref(),
            Some("flux")
        );
    }
}
