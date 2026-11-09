use clap::Args;
use colored::Colorize;
use dialoguer::Confirm;

use crate::{
    cli::apply_output::{ApplyRenderer, Phase, PhaseTiming},
    cli::commands::apply::{HandoffArgs, DEFAULT_APPLY_CONCURRENCY},
    cli::commands::cluster::resolve_command_cluster,
};
use nyl_render::{
    config::ProjectConfig,
    kubernetes::{
        fence::{ReleaseClaim, Requester},
        lifecycle::{
            collapse_rendered_duplicates, ensure_namespace_exists, ApplySettings, Interruption, ReleaseRecorder,
            RevisionPolicy,
        },
        KubeRsClient, KubernetesReleaseStorage, ReleaseState, ReleaseStorage, ResourceOrdering,
    },
    NylError, Result,
};

/// Roll back a release to a previously stored revision
#[derive(Args, Debug)]
pub struct RollbackArgs {
    /// Deployment target whose cluster stores the release. Optional when exactly one target is configured.
    #[arg(long)]
    pub target: Option<String>,

    /// Release name
    pub name: String,

    /// Release namespace
    #[arg(short, long)]
    pub namespace: String,

    /// Revision to roll back to (default: the revision before the latest)
    #[arg(short, long)]
    pub revision: Option<u32>,

    /// Skip confirmation prompt
    #[arg(short, long)]
    pub yes: bool,

    /// Kubernetes context: overrides the target Cluster's context, or selects a cluster when no GitOps configuration is declared
    #[arg(long)]
    pub context: Option<String>,

    /// Maximum number of resources applied or pruned at the same time (see `nyl apply --concurrency`).
    #[arg(long, default_value_t = DEFAULT_APPLY_CONCURRENCY, value_parser = clap::value_parser!(u16).range(1..))]
    pub concurrency: u16,

    /// Maximum number of release revisions to keep; older revisions are deleted
    /// after this rollback. 0 keeps every revision. Overrides `[release]
    /// history_limit` in nyl.toml.
    #[arg(long, value_name = "N", value_parser = nyl_render::config::parse_release_history_limit)]
    pub history_limit: Option<u32>,

    /// Prune resources whose Argo CD sync option is `Prune=confirm` (see `nyl apply --confirm-prune`).
    #[arg(long)]
    pub confirm_prune: bool,

    /// Roll back a release an environment owns anyway, recording no owner (see `nyl apply --take-over`).
    #[arg(long)]
    pub take_over: bool,

    #[command(flatten)]
    pub handoff: HandoffArgs,
}

pub async fn execute(args: RollbackArgs) -> Result<()> {
    let ownership = args.handoff.ownership()?;
    // Read configuration before anything is recorded, so a broken nyl.toml fails
    // the rollback before it leaves a revision behind.
    let history_limit = match args.history_limit {
        Some(limit) => limit,
        None => ProjectConfig::load(None, crate::resource_catalogue())?.release_history_limit(),
    };

    // Create Kubernetes clients
    let cluster = resolve_command_cluster(args.target.as_deref(), args.context.as_deref()).await?;
    let client = kube::Client::try_from(cluster.config)?;
    let kube_client = KubeRsClient::from_client(client.clone()).await?;
    let storage = KubernetesReleaseStorage::new(client);

    // Resolve the revision we are rolling back to.
    let target = resolve_rollback_target(&storage, &args.name, &args.namespace, args.revision).await?;

    // Parse the stored manifest back into individual documents.
    let manifests = nyl_render::yaml::parse_yaml_documents_k8s_compatible(&target.manifest)
        .map_err(|e| NylError::Config(format!("Failed to parse stored manifest for rollback: {}", e)))?;
    if manifests.is_empty() {
        return Err(NylError::Config(format!(
            "Revision {} of release '{}' has no manifest to roll back to",
            target.revision, args.name
        )));
    }

    // Confirm unless --yes.
    if !args.yes {
        let prompt = format!(
            "{} Roll back release '{}' in namespace '{}' to revision {} (creates a new revision)?",
            "⚠".yellow(),
            args.name,
            args.namespace,
            target.revision,
        );
        let confirmed = Confirm::new()
            .with_prompt(prompt)
            .default(false)
            .interact()
            .map_err(|e| NylError::Other(format!("Confirmation prompt failed: {}", e)))?;
        if !confirmed {
            println!("Cancelled");
            return Ok(());
        }
    }

    println!(
        "Rolling back release '{}' to revision {}...\n",
        args.name, target.revision
    );

    // Revisions recorded before duplicates were collapsed at apply time may still
    // hold several documents for one object; apply only the last, as apply did.
    let mut duplicates = std::collections::HashMap::new();
    let (mut manifests, mut scopes) =
        collapse_rendered_duplicates(&kube_client, manifests, &mut duplicates, cluster.capabilities.as_ref()).await?;

    // Sort resources into apply order (CRDs, Namespaces, then Argo CD's kind order), record the rollback as
    // a new revision before touching the cluster, and apply.
    ResourceOrdering::sort_by_priority(&mut manifests)?;
    ensure_namespace_exists(&kube_client, &args.namespace, &ownership).await?;
    let claim = ReleaseClaim {
        requester: Requester::Manual {
            take_over: args.take_over,
        },
        publication: cluster.publication,
    };
    let recorder = ReleaseRecorder::reserve(
        &storage,
        &args.name,
        &args.namespace,
        &manifests,
        false,
        &kube_client,
        &mut scopes,
        &claim,
    )
    .await?;

    let concurrency = usize::from(args.concurrency);
    let renderer = ApplyRenderer::new(&duplicates);
    let (discovery_mode, discovery_elapsed) = kube_client.initial_discovery();
    let mut timings = vec![PhaseTiming::new(Phase::Discovery(discovery_mode), discovery_elapsed)];
    // Apply, then complete the revision, supersede the previous one, and prune,
    // reusing the apply path.
    let run = recorder
        .run(
            &kube_client,
            &manifests,
            &mut scopes,
            ApplySettings {
                concurrency,
                history_limit,
                // A rollback is an explicit change of the release, so it is always recorded.
                revisions: RevisionPolicy::Always,
                confirm_prune: args.confirm_prune,
                ownership: &ownership,
                protected: None,
            },
            Interruption {
                interrupt: async {
                    let _ = tokio::signal::ctrl_c().await;
                },
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
    run.error("Rollback").map_or(Ok(()), Err)
}

/// Resolve which stored revision a rollback should target.
///
/// When `revision` is given, that exact revision is loaded. Otherwise the rollback
/// defaults to the revision immediately before the latest one (i.e. undo the most
/// recent deployment). Returns a clear error when the release or revision does not
/// exist, or when there is no previous revision to roll back to.
async fn resolve_rollback_target(
    storage: &dyn ReleaseStorage,
    name: &str,
    namespace: &str,
    revision: Option<u32>,
) -> Result<ReleaseState> {
    if let Some(rev) = revision {
        return storage.get_release(name, namespace, rev).await?.ok_or_else(|| {
            NylError::Config(format!(
                "Revision {} of release '{}' not found in namespace '{}'",
                rev, name, namespace
            ))
        });
    }

    let revisions = storage.list_revisions(name, namespace).await?;
    let Some(&latest) = revisions.iter().max() else {
        return Err(NylError::Config(format!(
            "Release '{}' not found in namespace '{}'",
            name, namespace
        )));
    };

    if latest <= 1 {
        return Err(NylError::Config(format!(
            "Release '{}' has no previous revision to roll back to (only revision {} exists). \
             Specify --revision to target a specific revision.",
            name, latest
        )));
    }

    let target_revision = latest - 1;
    storage
        .get_release(name, namespace, target_revision)
        .await?
        .ok_or_else(|| {
            NylError::Config(format!(
                "Cannot roll back release '{}' in namespace '{}': the previous revision ({}) no longer \
                 exists (it may have been deleted). Use --revision to choose a specific revision to roll back to.",
                name, namespace, target_revision
            ))
        })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use nyl_render::kubernetes::ReleaseStatus;

    use nyl_render::kubernetes::memory::InMemoryReleaseStorage as MockReleaseStorage;

    use nyl_render::kubernetes::memory::sample_release as make_release;

    async fn seed(storage: &MockReleaseStorage, name: &str, ns: &str, revisions: &[u32]) {
        for (i, rev) in revisions.iter().enumerate() {
            let status = if i + 1 == revisions.len() {
                ReleaseStatus::Deployed
            } else {
                ReleaseStatus::Superseded
            };
            storage
                .save_release(&make_release(name, ns, *rev, status))
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn test_resolve_explicit_revision() {
        let storage = MockReleaseStorage::new();
        seed(&storage, "app", "default", &[1, 2, 3, 4]).await;

        let target = resolve_rollback_target(&storage, "app", "default", Some(2))
            .await
            .unwrap();
        assert_eq!(target.revision, 2);
        assert_eq!(target.manifest, "# manifest for revision 2");
    }

    #[tokio::test]
    async fn test_resolve_default_is_latest_minus_one() {
        let storage = MockReleaseStorage::new();
        seed(&storage, "app", "default", &[1, 2, 3, 4]).await;

        // Latest is 4, so the default rollback target is revision 3.
        let target = resolve_rollback_target(&storage, "app", "default", None).await.unwrap();
        assert_eq!(target.revision, 3);
    }

    #[tokio::test]
    async fn test_resolve_missing_release() {
        let storage = MockReleaseStorage::new();
        let err = resolve_rollback_target(&storage, "ghost", "default", None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[tokio::test]
    async fn test_resolve_missing_explicit_revision() {
        let storage = MockReleaseStorage::new();
        seed(&storage, "app", "default", &[1, 2]).await;

        let err = resolve_rollback_target(&storage, "app", "default", Some(9))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Revision 9"));
    }

    #[tokio::test]
    async fn test_resolve_default_into_deleted_hole_errors() {
        let storage = MockReleaseStorage::new();
        // Revision 3 was deleted, leaving a hole immediately before the latest (4).
        seed(&storage, "app", "default", &[1, 2, 4]).await;

        let err = resolve_rollback_target(&storage, "app", "default", None)
            .await
            .unwrap_err();
        // Default targets latest-1 (3), which is gone: surface a clear error rather
        // than silently rolling back to a surprising revision.
        assert!(err.to_string().contains("no longer"));
    }

    #[tokio::test]
    async fn test_resolve_no_previous_revision() {
        let storage = MockReleaseStorage::new();
        seed(&storage, "app", "default", &[1]).await;

        let err = resolve_rollback_target(&storage, "app", "default", None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no previous revision"));
    }

    /// When the latest revision Failed, pruning must reconcile against the last
    /// Deployed revision's live resources (plus any partial resources the Failed
    /// revision applied), not just the numerically previous secret.
    #[tokio::test]
    async fn test_collect_live_state_spans_failed_revision_to_last_deployed() {
        use nyl_render::kubernetes::lifecycle::collect_live_state;
        use nyl_render::kubernetes::{GroupVersionKind, ResourceKey};

        let key = |name: &str| ResourceKey {
            gvk: GroupVersionKind {
                group: String::new(),
                version: "v1".to_string(),
                kind: "ConfigMap".to_string(),
            },
            namespace: Some("ns".to_string()),
            name: name.to_string(),
        };

        let storage = MockReleaseStorage::new();

        // rev3 Deployed (live: a, x); rev4 Failed (partially applied: y).
        let mut rev3 = make_release("app", "ns", 3, ReleaseStatus::Deployed);
        rev3.resource_keys = vec![key("a"), key("x")];
        storage.save_release(&rev3).await.unwrap();

        let mut rev4 = make_release("app", "ns", 4, ReleaseStatus::Failed);
        rev4.resource_keys = vec![key("y")];
        storage.save_release(&rev4).await.unwrap();

        // Recording rev5 (e.g. a rollback): live state is rev3 + rev4's partials,
        // and the superseded revision is the last Deployed one (rev3).
        let revisions = storage.list_revisions("app", "ns").await.unwrap();
        let (superseded, live) = collect_live_state(&storage, "app", "ns", &revisions, 5).await.unwrap();
        assert_eq!(superseded.map(|release| release.revision), Some(3));
        let names: std::collections::HashSet<&str> = live.iter().map(|k| k.name.as_str()).collect();
        assert_eq!(names, ["a", "x", "y"].into_iter().collect());
    }
}
