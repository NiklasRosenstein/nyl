//! Whether a publication's teardown can complete, from its target's
//! effective settings: the preflight every teardown runs.
//!
//! Contract: [Kubernetes publication unit](../../../../design/orchestration-core.md#kubernetes-publication-unit),
//! teardown readiness and catalog ownership. The driver owns these
//! semantics; the orchestration shell only coordinates and displays the
//! findings. Three blocking requirements decide whether the teardown can
//! succeed (the catalog prunes removed workload Applications on its own, with
//! automated pruning and no `Prune=confirm` or `Prune=false` option,
//! `NYL-TEARDOWN-CATALOG-MANUAL-SYNC`; deleting an Application deletes its
//! workloads, `NYL-TEARDOWN-APPLICATIONS-ORPHANED`; no resource requires
//! confirmation to delete, `NYL-TEARDOWN-NAMESPACE-CONFIRM`); two remnant rows leave
//! objects behind (`NYL-TEARDOWN-NAMESPACES-REMAIN`,
//! `NYL-TEARDOWN-CATALOG-REMAINS`). Settings a parent Application owns are
//! `external: not checked` and never block. An unmet blocking requirement
//! keeps the unit pending teardown unless the invocation passes
//! `--allow-incomplete`, and an incomplete teardown is recorded with a
//! condition listing what may remain.

use nyl_core::orchestration::codes::{Code, Finding};
use nyl_core::orchestration::driver::TeardownReadiness;
use nyl_core::orchestration::ids::{EnvironmentName, UnitName};
use nyl_render::gitops::publication::{PruneHold, TeardownSettings};

/// What a removal nobody attends would wait for: a `manual` `teardownWait`
/// stops it after phase 1 until an operator confirms the removal, so a
/// template whose instances expire unattended wants `observe` or `delay`.
pub fn unattended_wait(
    environment: &EnvironmentName,
    unit: &UnitName,
    wait: nyl_core::orchestration::driver::WaitStrategy,
) -> Option<Finding> {
    (wait == nyl_core::orchestration::driver::WaitStrategy::Manual).then(|| {
        Finding::new(
            Code::TeardownWaitManual,
            format!(
                "{environment}/{unit}: its teardownWait is manual, so a removal nobody attends, such as the expiry of an instance, stops after the first phase until an operator confirms it"
            ),
            "set teardownWait to observe or delay on the publication unit, or confirm each removal with `nyl teardown --confirm-removed`",
        )
    })
}

/// How many resources a confirmation finding names before it counts the rest.
const MAX_LISTED_CONFIRMATIONS: usize = 8;

/// The catalog row: pruning on its own takes automated pruning, and no `Prune`
/// sync option that waits for a confirmation or skips an Application.
fn catalog_prune_finding(subject: &str, settings: &TeardownSettings) -> Option<Finding> {
    let mut not_pruning = Vec::new();
    if settings.catalog_prunes == Some(false) {
        not_pruning.push("it has no syncPolicy.automated with prune: true".to_owned());
    }
    if !settings.catalog_prune_holds.is_empty() {
        let holding = settings
            .catalog_prune_holds
            .iter()
            .map(|hold| match (&hold.application, hold.prune) {
                (Some(application), PruneHold::Confirm) => {
                    format!("Application {application} requires confirmation to be pruned (Prune=confirm)")
                }
                (Some(application), PruneHold::False) => {
                    format!("Application {application} is never pruned (Prune=false)")
                }
                (None, PruneHold::Confirm) => {
                    "its spec.syncPolicy.syncOptions make pruning wait for confirmation (Prune=confirm)".to_owned()
                }
                (None, PruneHold::False) => "its spec.syncPolicy.syncOptions skip pruning (Prune=false)".to_owned(),
            })
            .collect::<Vec<_>>();
        not_pruning.extend(holding.into_iter().take(MAX_LISTED_CONFIRMATIONS));
        let more = settings
            .catalog_prune_holds
            .len()
            .saturating_sub(MAX_LISTED_CONFIRMATIONS);
        if more > 0 {
            not_pruning.push(format!("and {more} more Applications"));
        }
    }
    (!not_pruning.is_empty()).then(|| {
        let manual_sync = settings.catalog_prunes == Some(false);
        Finding::new(
            Code::TeardownCatalogManualSync,
            if manual_sync && settings.catalog_prune_holds.is_empty() {
                format!(
                    "{subject}: the catalog does not prune removed workload Applications on its own, so they stay until someone syncs it with pruning"
                )
            } else {
                format!(
                    "{subject}: the catalog does not prune removed workload Applications on its own: {}",
                    not_pruning.join("; ")
                )
            },
            "enable the catalog Application's syncPolicy.automated with prune: true, and remove Prune=confirm and Prune=false from its syncOptions and from the generated Applications' argocd.argoproj.io/sync-options annotation (catalogApplication.syncPolicy on the DeploymentTarget, catalogApplicationDefaults.syncPolicy on its ArgoCDInstance, or the ApplicationGroup annotations), or pass --allow-incomplete",
        )
    })
}

/// Evaluate a unit's teardown settings against the contract's readiness
/// table. Each finding names the unit and the setting that fixes it; a
/// setting Nyl cannot see (`None`) is listed in `not_checked` and never
/// blocks.
pub fn evaluate(environment: &EnvironmentName, unit: &UnitName, settings: &TeardownSettings) -> TeardownReadiness {
    let mut readiness = TeardownReadiness::default();
    let subject = format!("{environment}/{unit}");
    readiness.blocking.extend(catalog_prune_finding(&subject, settings));
    if settings.catalog_prunes.is_none() {
        readiness
            .not_checked
            .push("catalog syncPolicy.automated.prune: external: not checked".to_owned());
    }
    match settings.application_deletion_policy.as_deref() {
        Some("Orphan") => readiness.blocking.push(Finding::new(
            Code::TeardownApplicationsOrphaned,
            format!("{subject}: deleting a workload Application leaves its workloads running (applicationDeletionPolicy: Orphan)"),
            "set the catalog's applicationDeletionPolicy to Foreground or Background, or pass --allow-incomplete",
        )),
        Some(_) => {}
        None => readiness
            .not_checked
            .push("catalog applicationDeletionPolicy: external: not checked".to_owned()),
    }
    let with_policy = |wanted: &dyn Fn(&str) -> bool| {
        settings
            .namespace_delete_policies
            .iter()
            .filter(|(_, policy)| wanted(policy))
            .map(|(namespace, _)| namespace.as_str())
            .collect::<Vec<_>>()
    };
    // Argo CD's cascade deletion holds an Application while a resource it
    // would delete requires confirmation, whether the resource's own
    // `Delete=confirm` or `Prune=confirm` annotation or its Application's
    // `syncPolicy.syncOptions` says so. Settings already left out resources
    // that keep their own, such as `Delete=false`.
    if !settings.deletion_confirmations.is_empty() {
        let holding = settings
            .deletion_confirmations
            .iter()
            .map(|confirmation| {
                let options = confirmation
                    .options
                    .iter()
                    .map(|option| format!("{}=confirm", option.name()))
                    .collect::<Vec<_>>()
                    .join(", ");
                match &confirmation.resource {
                    Some(resource) => format!("{resource} of Application {} ({options})", confirmation.application),
                    None => format!(
                        "Application {} (spec.syncPolicy.syncOptions: {options})",
                        confirmation.application
                    ),
                }
            })
            .collect::<Vec<_>>();
        let shown = holding
            .iter()
            .take(MAX_LISTED_CONFIRMATIONS)
            .cloned()
            .collect::<Vec<_>>();
        let more = holding.len() - shown.len();
        readiness.blocking.push(Finding::new(
            Code::TeardownNamespaceConfirm,
            format!(
                "{subject}: {}{} require confirmation to delete, which holds their Application and its workloads until the deletion is confirmed in Argo CD",
                shown.join(", "),
                if more > 0 { format!(", and {more} more") } else { String::new() },
            ),
            "remove Delete=confirm and Prune=confirm from the named resources and Applications (a Namespace's prunePolicy and deletePolicy to Automatic, or deletePolicy to Retain; an Application's ApplicationGroup syncPolicy.syncOptions or Release +syncOptions override), or pass --allow-incomplete",
        ));
    }
    let remaining = with_policy(&|policy| policy != "Automatic");
    if !remaining.is_empty() {
        readiness.remnants.push(Finding::new(
            Code::TeardownNamespacesRemain,
            format!("{subject}: Namespaces {} remain after teardown", remaining.join(", ")),
            "set the Namespaces' deletePolicy to Automatic to remove them with the teardown",
        ));
    }
    if !settings.catalog_owned_by_parent {
        readiness.remnants.push(Finding::new(
            Code::TeardownCatalogRemains,
            format!("{subject}: the self-managing catalog Application and its AppProjects remain after teardown"),
            "let a parent Application or ApplicationSet own the target's catalog (catalogApplication.enabled: false) to remove them with the teardown",
        ));
    }
    readiness
}

/// The readiness of a direct target's teardown. Nothing here holds a
/// finalizer, so nothing blocks: a Namespace whose `deletePolicy` is not
/// `Automatic` is a remnant, and what a teardown's credentials may delete is
/// not checked. `deletionConfirmations` do not matter, unlike for Argo CD,
/// which holds an Application's cascade deletion for a resource requiring
/// confirmation: a direct teardown purges by each object's `Delete` option alone,
/// and `Prune` only decides whether the next apply deletes a Namespace a
/// Release stopped declaring.
///
/// Contract: [Direct reconciler](../../../../design/orchestration-core.md#teardown), readiness.
pub fn evaluate_direct(
    environment: &EnvironmentName,
    unit: &UnitName,
    settings: &TeardownSettings,
) -> TeardownReadiness {
    let mut readiness = TeardownReadiness::default();
    let remaining = settings
        .namespace_delete_policies
        .iter()
        .filter(|(_, policy)| policy.as_str() != "Automatic")
        .map(|(namespace, _)| namespace.as_str())
        .collect::<Vec<_>>();
    if !remaining.is_empty() {
        readiness.remnants.push(Finding::new(
            Code::TeardownNamespacesRemain,
            format!(
                "{environment}/{unit}: Namespaces {} remain after teardown",
                remaining.join(", ")
            ),
            "set the Namespaces' deletePolicy to Automatic to remove them with the teardown",
        ));
    }
    readiness.not_checked.push(
        "resources with Delete=false or Delete=confirm, and the credentials' permission to delete: not checked"
            .to_owned(),
    );
    readiness
}

/// The readiness of a target whose catalog is shared: the rows of the
/// settings read from the Application the branch holds (`reading`, see
/// [`super::run::read_shared_catalog`]), plus a blocking row when that
/// manifest could not be read or understood, because a catalog that cannot be
/// shown to prune must not be trusted to.
pub fn evaluate_reading(
    environment: &EnvironmentName,
    unit: &UnitName,
    reading: &serde_json::Value,
) -> Result<TeardownReadiness, String> {
    let settings = reading
        .get("settings")
        .ok_or("the shared catalog reading records no settings")?;
    let settings: TeardownSettings =
        serde_json::from_value(settings.clone()).map_err(|error| format!("shared catalog settings: {error}"))?;
    let mut readiness = evaluate(environment, unit, &settings);
    if let Some(problem) = reading.get("problem").and_then(serde_json::Value::as_str) {
        readiness.blocking.push(Finding::new(
            Code::TeardownCatalogManualSync,
            format!("{environment}/{unit}: the shared catalog cannot be shown to prune removed workload Applications: {problem}"),
            "restore access to the publication branch, or publish a readable shared catalog with `nyl build <unit> -e <environment> -- --shared-catalog --push`, or pass --allow-incomplete",
        ));
    }
    Ok(readiness)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use nyl_render::gitops::publication::{CatalogPruneHold, ConfirmedOption, DeletionConfirmation};

    use super::*;

    fn env() -> EnvironmentName {
        EnvironmentName::new("dev").unwrap()
    }

    fn name(unit: &str) -> UnitName {
        UnitName::new(unit).unwrap()
    }

    fn ready_settings() -> TeardownSettings {
        TeardownSettings {
            catalog_prunes: Some(true),
            application_deletion_policy: Some("Foreground".to_owned()),
            namespace_delete_policies: BTreeMap::from([("web".to_owned(), "Automatic".to_owned())]),
            deletion_confirmations: Vec::new(),
            catalog_prune_holds: Vec::new(),
            catalog_owned_by_parent: true,
        }
    }

    fn confirmation(application: &str, resource: Option<&str>, options: &[&str]) -> DeletionConfirmation {
        DeletionConfirmation {
            application: application.to_owned(),
            resource: resource.map(str::to_owned),
            options: options
                .iter()
                .map(|option| match *option {
                    "Delete" => ConfirmedOption::Delete,
                    _ => ConfirmedOption::Prune,
                })
                .collect(),
        }
    }

    fn confirming(confirmations: Vec<DeletionConfirmation>) -> TeardownSettings {
        TeardownSettings {
            deletion_confirmations: confirmations,
            ..ready_settings()
        }
    }

    /// Catches a direct target being held by a resource requiring confirmation,
    /// which Argo CD holds an Application's deletion for and a direct teardown
    /// never consults: nothing blocks, and `deletePolicy` alone decides the
    /// Namespace remnant.
    #[test]
    fn test_a_direct_teardown_ignores_deletion_confirmations_and_never_blocks() {
        for (delete, remnant) in [("Automatic", false), ("Confirm", true), ("Retain", true)] {
            let settings = TeardownSettings {
                namespace_delete_policies: BTreeMap::from([("web".to_owned(), delete.to_owned())]),
                deletion_confirmations: vec![confirmation("web", Some("Namespace web"), &["Prune"])],
                ..ready_settings()
            };
            let readiness = evaluate_direct(&env(), &name("kubernetes"), &settings);
            assert!(readiness.ready(), "{delete}");
            assert_eq!(
                codes(&readiness.remnants),
                if remnant {
                    vec![Code::TeardownNamespacesRemain]
                } else {
                    Vec::new()
                },
                "{delete}"
            );
        }
    }

    fn codes(findings: &[Finding]) -> Vec<Code> {
        findings.iter().map(|finding| finding.code).collect()
    }

    /// Only a `manual` wait needs an operator, so only it warns an unattended
    /// removal: catches `observe` and `delay` previews being warned about, and
    /// the default (manual) passing silently.
    #[test]
    fn test_only_a_manual_wait_warns_an_unattended_removal() {
        use nyl_core::orchestration::driver::WaitStrategy;
        let warned = unattended_wait(&env(), &name("kubernetes"), WaitStrategy::Manual).unwrap();
        assert_eq!(warned.code, Code::TeardownWaitManual);
        assert!(warned.message.contains("dev/kubernetes"));
        assert!(unattended_wait(&env(), &name("kubernetes"), WaitStrategy::Observe).is_none());
        let delay = WaitStrategy::Delay {
            duration: nyl_core::orchestration::time::Duration::from_mins(5),
        };
        assert!(unattended_wait(&env(), &name("kubernetes"), delay).is_none());
    }

    /// Catches a teardown-ready target being held back or reported with remnants.
    #[test]
    fn test_ready_settings_have_no_findings() {
        let readiness = evaluate(&env(), &name("kubernetes"), &ready_settings());
        assert!(readiness.ready());
        assert_eq!(readiness, TeardownReadiness::default());
    }

    /// Catches a teardown starting although its Applications or workloads
    /// would stay: each blocking row of the readiness table.
    #[test]
    fn test_each_blocking_requirement_blocks_and_names_its_setting() {
        let cases = [
            (
                TeardownSettings {
                    catalog_prunes: Some(false),
                    ..ready_settings()
                },
                Code::TeardownCatalogManualSync,
                "syncPolicy.automated",
            ),
            (
                TeardownSettings {
                    application_deletion_policy: Some("Orphan".to_owned()),
                    ..ready_settings()
                },
                Code::TeardownApplicationsOrphaned,
                "applicationDeletionPolicy",
            ),
            (
                confirming(vec![confirmation("web", Some("Namespace web"), &["Delete", "Prune"])]),
                Code::TeardownNamespaceConfirm,
                "prunePolicy and deletePolicy",
            ),
        ];
        for (settings, code, setting) in cases {
            let readiness = evaluate(&env(), &name("kubernetes"), &settings);
            assert!(!readiness.ready(), "{code}");
            assert_eq!(codes(&readiness.blocking), vec![code]);
            assert!(
                readiness.blocking[0].fix.contains(setting),
                "{}",
                readiness.blocking[0].fix
            );
            assert!(readiness.blocking[0].message.starts_with("dev/kubernetes: "));
        }
    }

    /// Catches the stall found against Argo CD v3.5.7: its cascade deletion
    /// holds an Application while any resource it would delete requires
    /// confirmation, whether that resource's own annotation or the
    /// Application's `syncOptions` says so. The finding names each one with
    /// the options that do, and says where to remove them; a target with no
    /// such resource is ready.
    #[test]
    fn test_a_resource_or_application_option_requiring_confirmation_blocks_and_is_named() {
        let readiness = evaluate(
            &env(),
            &name("kubernetes"),
            &confirming(vec![
                confirmation("api", None, &["Prune"]),
                confirmation("web", Some("Deployment shop/web"), &["Delete"]),
                confirmation("web", Some("Namespace shop"), &["Delete", "Prune"]),
            ]),
        );
        assert_eq!(codes(&readiness.blocking), vec![Code::TeardownNamespaceConfirm]);
        let finding = &readiness.blocking[0];
        assert!(
            finding
                .message
                .contains("Application api (spec.syncPolicy.syncOptions: Prune=confirm)"),
            "{}",
            finding.message
        );
        assert!(
            finding
                .message
                .contains("Deployment shop/web of Application web (Delete=confirm)"),
            "{}",
            finding.message
        );
        assert!(
            finding
                .message
                .contains("Namespace shop of Application web (Delete=confirm, Prune=confirm)"),
            "{}",
            finding.message
        );
        for fix in [
            "Release +syncOptions",
            "ApplicationGroup syncPolicy.syncOptions",
            "Retain",
        ] {
            assert!(finding.fix.contains(fix), "{}", finding.fix);
        }

        assert!(evaluate(&env(), &name("kubernetes"), &ready_settings()).ready());
    }

    /// Catches a catalog that syncs with pruning but cannot prune a removed
    /// workload Application being ready: Argo CD's sync waits for confirmation
    /// on `Prune=confirm` and skips a `Prune=false` Application. The finding
    /// keeps the catalog row's code, so a shared catalog's pre-teardown check
    /// holds on it too, names the option or Application, and says where to
    /// remove it; a catalog that also does not sync automatically says so.
    #[test]
    fn test_a_prune_option_the_catalog_cannot_prune_through_blocks_and_is_named() {
        let holding = |holds: Vec<CatalogPruneHold>| TeardownSettings {
            catalog_prune_holds: holds,
            ..ready_settings()
        };
        let hold = |application: Option<&str>, prune: PruneHold| CatalogPruneHold {
            application: application.map(str::to_owned),
            prune,
        };
        let readiness = evaluate(
            &env(),
            &name("kubernetes"),
            &holding(vec![
                hold(None, PruneHold::Confirm),
                hold(Some("api"), PruneHold::False),
            ]),
        );
        assert_eq!(codes(&readiness.blocking), vec![Code::TeardownCatalogManualSync]);
        let finding = &readiness.blocking[0];
        for part in [
            "dev/kubernetes: ",
            "its spec.syncPolicy.syncOptions make pruning wait for confirmation (Prune=confirm)",
            "Application api is never pruned (Prune=false)",
        ] {
            assert!(finding.message.contains(part), "{}", finding.message);
        }
        for fix in [
            "Prune=confirm and Prune=false",
            "ApplicationGroup annotations",
            "--allow-incomplete",
        ] {
            assert!(finding.fix.contains(fix), "{}", finding.fix);
        }

        let single = evaluate(
            &env(),
            &name("kubernetes"),
            &holding(vec![hold(Some("web"), PruneHold::Confirm)]),
        );
        assert!(
            single.blocking[0]
                .message
                .contains("Application web requires confirmation to be pruned (Prune=confirm)"),
            "{}",
            single.blocking[0].message
        );

        let both = TeardownSettings {
            catalog_prunes: Some(false),
            catalog_prune_holds: vec![hold(None, PruneHold::False)],
            ..ready_settings()
        };
        let readiness = evaluate(&env(), &name("kubernetes"), &both);
        assert_eq!(codes(&readiness.blocking), vec![Code::TeardownCatalogManualSync]);
        let message = &readiness.blocking[0].message;
        assert!(message.contains("it has no syncPolicy.automated with prune: true; its spec.syncPolicy.syncOptions skip pruning (Prune=false)"), "{message}");

        let many = (0..MAX_LISTED_CONFIRMATIONS + 2)
            .map(|index| hold(Some(&format!("app-{index:02}")), PruneHold::Confirm))
            .collect();
        let message = &evaluate(&env(), &name("kubernetes"), &holding(many)).blocking[0].message;
        assert!(message.contains("app-07") && !message.contains("app-08"), "{message}");
        assert!(message.contains("and 2 more Applications"), "{message}");

        // A direct teardown has no catalog to prune through.
        assert!(evaluate_direct(
            &env(),
            &name("kubernetes"),
            &holding(vec![hold(None, PruneHold::Confirm)])
        )
        .ready());
    }

    /// Catches a long list of confirmations flooding the finding: it names a
    /// few and counts the rest.
    #[test]
    fn test_many_confirmations_are_counted_after_the_first_few() {
        let confirmations = (0..MAX_LISTED_CONFIRMATIONS + 3)
            .map(|index| confirmation("web", Some(&format!("ConfigMap shop/cm-{index:02}")), &["Prune"]))
            .collect();
        let readiness = evaluate(&env(), &name("kubernetes"), &confirming(confirmations));
        let message = &readiness.blocking[0].message;
        assert!(message.contains("ConfigMap shop/cm-07"), "{message}");
        assert!(!message.contains("ConfigMap shop/cm-08"), "{message}");
        assert!(message.contains(", and 3 more require confirmation"), "{message}");
    }

    /// Catches remnants going unreported, or blocking a teardown that can complete.
    #[test]
    fn test_remnant_rows_are_reported_without_blocking() {
        let settings = TeardownSettings {
            namespace_delete_policies: BTreeMap::from([
                ("web".to_owned(), "Retain".to_owned()),
                ("api".to_owned(), "Automatic".to_owned()),
            ]),
            catalog_owned_by_parent: false,
            ..ready_settings()
        };
        let readiness = evaluate(&env(), &name("kubernetes"), &settings);
        assert!(readiness.ready());
        assert_eq!(
            codes(&readiness.remnants),
            vec![Code::TeardownNamespacesRemain, Code::TeardownCatalogRemains]
        );
        assert!(readiness.remnants[0].message.contains("Namespaces web remain"));

        // A Namespace that keeps its Confirm policy is a remnant; what holds the deletion is separate.
        let confirm = TeardownSettings {
            namespace_delete_policies: BTreeMap::from([("web".to_owned(), "Confirm".to_owned())]),
            ..ready_settings()
        };
        let readiness = evaluate(&env(), &name("kubernetes"), &confirm);
        assert_eq!(codes(&readiness.remnants), vec![Code::TeardownNamespacesRemain]);
    }

    /// Catches a parent-owned catalog, whose settings Nyl cannot see,
    /// blocking a teardown or vanishing from the preflight.
    #[test]
    fn test_external_settings_are_not_checked() {
        let settings = TeardownSettings {
            catalog_prunes: None,
            application_deletion_policy: None,
            namespace_delete_policies: BTreeMap::new(),
            deletion_confirmations: Vec::new(),
            catalog_prune_holds: Vec::new(),
            catalog_owned_by_parent: true,
        };
        let readiness = evaluate(&env(), &name("kubernetes"), &settings);
        assert!(readiness.ready());
        assert!(readiness.remnants.is_empty());
        assert_eq!(
            readiness.not_checked,
            vec![
                "catalog syncPolicy.automated.prune: external: not checked".to_owned(),
                "catalog applicationDeletionPolicy: external: not checked".to_owned(),
            ]
        );
    }
}
