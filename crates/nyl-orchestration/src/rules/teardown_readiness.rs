//! Whether a publication's teardown can complete, from its target's
//! effective settings: the preflight every teardown runs.
//!
//! Contract: [Kubernetes publication unit](../../../../design/orchestration-core.md#kubernetes-publication-unit),
//! teardown readiness and catalog ownership. Three blocking requirements
//! decide whether the teardown can succeed (the catalog prunes removed
//! workload Applications on its own, `NYL-TEARDOWN-CATALOG-MANUAL-SYNC`;
//! deleting an Application deletes its workloads,
//! `NYL-TEARDOWN-APPLICATIONS-ORPHANED`; owned Namespaces do not stop
//! deletion, `NYL-TEARDOWN-NAMESPACE-CONFIRM`); two remnant rows leave
//! objects behind (`NYL-TEARDOWN-NAMESPACES-REMAIN`,
//! `NYL-TEARDOWN-CATALOG-REMAINS`). Settings a parent Application owns are
//! `external: not checked` and never block. An unmet blocking requirement
//! keeps the unit pending teardown unless the invocation passes
//! `--allow-incomplete`, and an incomplete teardown is recorded with a
//! condition listing what may remain.

use nyl_core::orchestration::codes::{Code, Finding};
use nyl_core::orchestration::driver::TeardownSettings;
use nyl_core::orchestration::ids::{EnvironmentName, UnitName};

/// The preflight's verdict for one unit.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Readiness {
    /// Unmet blocking requirements, each with the setting that fixes it.
    pub blocking: Vec<Finding>,
    /// What teardown leaves behind.
    pub remnants: Vec<Finding>,
    /// Requirements Nyl cannot see, such as a parent's catalog settings.
    pub not_checked: Vec<String>,
}

impl Readiness {
    /// Whether no blocking requirement is unmet.
    pub fn ready(&self) -> bool {
        self.blocking.is_empty()
    }
}

/// Evaluate a unit's teardown settings against the contract's readiness
/// table. Each finding names the unit and the setting that fixes it; a
/// setting Nyl cannot see (`None`) is listed in `not_checked` and never
/// blocks.
pub fn evaluate(environment: &EnvironmentName, unit: &UnitName, settings: &TeardownSettings) -> Readiness {
    let mut readiness = Readiness::default();
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
    let confirm = with_policy(&|policy| policy == "Confirm");
    if !confirm.is_empty() {
        readiness.blocking.push(Finding::new(
            Code::TeardownNamespaceConfirm,
            format!(
                "{subject}: Namespaces {} have deletePolicy: Confirm, which holds their Application and its workloads until the deletion is confirmed in Argo CD",
                confirm.join(", ")
            ),
            "set the Namespaces' deletePolicy to Automatic or Retain, or pass --allow-incomplete",
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::rules::fixtures::{env, name};

    fn ready_settings() -> TeardownSettings {
        TeardownSettings {
            catalog_prunes: Some(true),
            application_deletion_policy: Some("Foreground".to_owned()),
            namespace_delete_policies: BTreeMap::from([("web".to_owned(), "Automatic".to_owned())]),
            catalog_owned_by_parent: true,
        }
    }

    fn codes(findings: &[Finding]) -> Vec<Code> {
        findings.iter().map(|finding| finding.code).collect()
    }

    /// Catches a teardown-ready target being held back or reported with remnants.
    #[test]
    fn test_ready_settings_have_no_findings() {
        let readiness = evaluate(&env(), &name("kubernetes"), &ready_settings());
        assert!(readiness.ready());
        assert_eq!(readiness, Readiness::default());
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
                TeardownSettings {
                    namespace_delete_policies: BTreeMap::from([("web".to_owned(), "Confirm".to_owned())]),
                    ..ready_settings()
                },
                Code::TeardownNamespaceConfirm,
                "deletePolicy",
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
