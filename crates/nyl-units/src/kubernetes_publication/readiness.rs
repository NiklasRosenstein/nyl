//! Whether a publication's teardown can complete, from its target's
//! effective settings: the preflight every teardown runs.
//!
//! Contract: [Kubernetes publication unit](../../../../design/orchestration-core.md#kubernetes-publication-unit),
//! teardown readiness and catalog ownership. The driver owns these
//! semantics; the orchestration shell only coordinates and displays the
//! findings. Three blocking requirements decide whether the teardown can
//! succeed (the catalog prunes removed workload Applications on its own,
//! `NYL-TEARDOWN-CATALOG-MANUAL-SYNC`; deleting an Application deletes its
//! workloads, `NYL-TEARDOWN-APPLICATIONS-ORPHANED`; owned Namespaces do not
//! stop deletion, `NYL-TEARDOWN-NAMESPACE-CONFIRM`); two remnant rows leave
//! objects behind (`NYL-TEARDOWN-NAMESPACES-REMAIN`,
//! `NYL-TEARDOWN-CATALOG-REMAINS`). Settings a parent Application owns are
//! `external: not checked` and never block. An unmet blocking requirement
//! keeps the unit pending teardown unless the invocation passes
//! `--allow-incomplete`, and an incomplete teardown is recorded with a
//! condition listing what may remain.

use nyl_core::orchestration::codes::{Code, Finding};
use nyl_core::orchestration::driver::TeardownReadiness;
use nyl_core::orchestration::ids::{EnvironmentName, UnitName};
use nyl_render::gitops::publication::TeardownSettings;

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

/// Evaluate a unit's teardown settings against the contract's readiness
/// table. Each finding names the unit and the setting that fixes it; a
/// setting Nyl cannot see (`None`) is listed in `not_checked` and never
/// blocks.
pub fn evaluate(environment: &EnvironmentName, unit: &UnitName, settings: &TeardownSettings) -> TeardownReadiness {
    let mut readiness = TeardownReadiness::default();
    let subject = format!("{environment}/{unit}");
    match settings.catalog_prunes {
        Some(false) => readiness.blocking.push(Finding::new(
            Code::TeardownCatalogManualSync,
            format!(
                "{subject}: the catalog does not prune removed workload Applications on its own, so they stay until someone syncs it with pruning"
            ),
            "enable the catalog Application's syncPolicy.automated with prune: true (catalogApplication.syncPolicy on the DeploymentTarget, or catalogApplicationDefaults.syncPolicy on its ArgoCDInstance), or pass --allow-incomplete",
        )),
        Some(true) => {}
        None => readiness
            .not_checked
            .push("catalog syncPolicy.automated.prune: external: not checked".to_owned()),
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
    // would delete carries `Delete=confirm` or `Prune=confirm`. `Delete=false`
    // keeps the resource, so neither confirmation is consulted for it, and
    // `Prune=false` only affects syncs, so the Namespace is still deleted.
    let holding = settings
        .namespace_delete_policies
        .iter()
        .filter_map(|(namespace, delete)| {
            let prune = settings
                .namespace_prune_policies
                .get(namespace)
                .map_or("Automatic", String::as_str);
            let setting = if delete == "Confirm" {
                "deletePolicy"
            } else if delete != "Retain" && prune == "Confirm" {
                "prunePolicy"
            } else {
                return None;
            };
            Some(format!("{namespace} ({setting}: Confirm)"))
        })
        .collect::<Vec<_>>();
    if !holding.is_empty() {
        readiness.blocking.push(Finding::new(
            Code::TeardownNamespaceConfirm,
            format!(
                "{subject}: Namespaces {} require confirmation to delete, which holds their Application and its workloads until the deletion is confirmed in Argo CD",
                holding.join(", ")
            ),
            "set the Namespaces' prunePolicy and deletePolicy to Automatic (or deletePolicy to Retain, which keeps the Namespace), or pass --allow-incomplete",
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
/// not checked.
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
            namespace_prune_policies: BTreeMap::from([("web".to_owned(), "Automatic".to_owned())]),
            catalog_owned_by_parent: true,
        }
    }

    fn namespace_policies(delete: &str, prune: &str) -> TeardownSettings {
        TeardownSettings {
            namespace_delete_policies: BTreeMap::from([("web".to_owned(), delete.to_owned())]),
            namespace_prune_policies: BTreeMap::from([("web".to_owned(), prune.to_owned())]),
            ..ready_settings()
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
                namespace_policies("Confirm", "Automatic"),
                Code::TeardownNamespaceConfirm,
                "deletePolicy",
            ),
            (
                namespace_policies("Automatic", "Confirm"),
                Code::TeardownNamespaceConfirm,
                "prunePolicy",
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
    /// holds an Application while a Namespace it would delete carries
    /// `Prune=confirm` or `Delete=confirm`, so readiness must name whichever
    /// holds, treat both Automatic as ready, and let `Delete=false` (Retain)
    /// keep the Namespace without any confirmation being consulted.
    #[test]
    fn test_namespace_confirmation_blocks_by_prune_or_delete_policy() {
        let blocked = |delete: &str, prune: &str| {
            let readiness = evaluate(&env(), &name("kubernetes"), &namespace_policies(delete, prune));
            (codes(&readiness.blocking), readiness)
        };
        let (codes_, readiness) = blocked("Automatic", "Confirm");
        assert_eq!(codes_, vec![Code::TeardownNamespaceConfirm]);
        let finding = &readiness.blocking[0];
        assert!(
            finding.message.contains("web (prunePolicy: Confirm)"),
            "{}",
            finding.message
        );
        assert!(!finding.message.contains("deletePolicy"), "{}", finding.message);
        assert!(
            finding.fix.contains("prunePolicy and deletePolicy to Automatic") && finding.fix.contains("Retain"),
            "{}",
            finding.fix
        );

        let (codes_, readiness) = blocked("Confirm", "Automatic");
        assert_eq!(codes_, vec![Code::TeardownNamespaceConfirm]);
        assert!(readiness.blocking[0].message.contains("web (deletePolicy: Confirm)"));

        let (codes_, readiness) = blocked("Confirm", "Confirm");
        assert_eq!(codes_, vec![Code::TeardownNamespaceConfirm]);
        assert!(readiness.blocking[0].message.contains("web (deletePolicy: Confirm)"));

        // Both Automatic: nothing holds the Application and the Namespace goes.
        let (codes_, readiness) = blocked("Automatic", "Automatic");
        assert!(codes_.is_empty() && readiness.remnants.is_empty());

        // Retain (`Delete=false`) keeps the Namespace without a confirmation,
        // whatever the prune policy: a remnant, not a blocker.
        for prune in ["Automatic", "Confirm", "Retain"] {
            let (codes_, readiness) = blocked("Retain", prune);
            assert!(codes_.is_empty(), "prune {prune}: {codes_:?}");
            assert_eq!(codes(&readiness.remnants), vec![Code::TeardownNamespacesRemain]);
        }

        // `Prune=false` only affects syncs: the Namespace is still deleted.
        let (codes_, readiness) = blocked("Automatic", "Retain");
        assert!(codes_.is_empty() && readiness.remnants.is_empty());
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

        // Confirm both blocks and remains.
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
            namespace_prune_policies: BTreeMap::new(),
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
