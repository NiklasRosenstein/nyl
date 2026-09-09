//! Effective cluster facts resolved from explicit, acyclic API contract references.

use std::collections::BTreeSet;
use std::path::PathBuf;

use crate::resources::{Cluster, ClusterApiContractMode, GitOpsResource, GitOpsResourceKind};
use crate::{NylError, Result};

use super::GitOpsInventory;

/// Resolved facts retain the destination's identity and the providers' provenance.
#[derive(Debug, Clone)]
pub struct EffectiveCluster {
    /// Materialized rendering context with inherited capabilities and local destination/values.
    pub cluster: Cluster,
    /// Cluster whose local capabilities supply the effective Kubernetes version and APIs.
    pub capabilities_source: String,
    /// Cluster whose schema snapshot supplies the CRD contract.
    pub schemas_source: String,
    /// Source files traversed during resolution, relative to the project root.
    pub inputs: BTreeSet<PathBuf>,
}

/// Resolve a Cluster's API contract without contacting Kubernetes.
pub fn resolve_cluster_contract(inventory: &GitOpsInventory, name: &str) -> Result<EffectiveCluster> {
    resolve(inventory, name, &mut Vec::new())
}

fn resolve(inventory: &GitOpsInventory, name: &str, chain: &mut Vec<String>) -> Result<EffectiveCluster> {
    if chain.iter().any(|entry| entry == name) {
        chain.push(name.to_owned());
        return Err(NylError::config(format!(
            "Cluster API contract cycle: {}",
            chain.join(" -> ")
        )));
    }
    chain.push(name.to_owned());
    let discovered = inventory.get(GitOpsResourceKind::Cluster, name).ok_or_else(|| {
        NylError::config(format!(
            "Cluster API contract reference not found: {}",
            chain.join(" -> ")
        ))
    })?;
    let Some(GitOpsResource::Cluster(declared)) = &discovered.resource else {
        return Err(NylError::config(format!("Cluster {name:?} must be static")));
    };
    declared.validate()?;
    let mut result = EffectiveCluster {
        cluster: declared.clone(),
        capabilities_source: name.to_owned(),
        schemas_source: name.to_owned(),
        inputs: BTreeSet::from([discovered.source_path.clone()]),
    };
    if let Some(reference) = &declared.spec.api_contract_from {
        let source = resolve(inventory, &reference.cluster_ref.name, chain)?;
        if reference.mode == ClusterApiContractMode::All {
            result.cluster.spec.kubernetes = source.cluster.spec.kubernetes;
            result.capabilities_source = source.capabilities_source;
        }
        result.schemas_source = source.schemas_source;
        result.inputs.extend(source.inputs);
    }
    // Materialized contexts have exactly one capabilities authority.
    result.cluster.spec.api_contract_from = None;
    chain.pop();
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn fixture() -> TempDir {
        let directory = TempDir::new().unwrap();
        git2::Repository::init(directory.path()).unwrap();
        std::fs::write(directory.path().join("nyl.toml"), "").unwrap();
        directory
    }

    fn cluster(directory: &std::path::Path, name: &str, mode: Option<&str>, source: &str) {
        let mut value = json!({"apiVersion":"k8s.gitops.nyl/v1","kind":"Cluster","metadata":{"name":name},
            "spec":{"destination":{"server":format!("https://{name}.example.com")},"values":{"identity":name},
                "live":{"context":format!("{name}-admin")}}});
        if mode != Some("all") {
            value["spec"]["kubernetes"] =
                json!({"kubeVersion":if name == "staging" {"1.31.4"} else {"1.32.0"},"apiVersions":["v1","apps/v1"]});
        }
        if let Some(mode) = mode {
            value["spec"]["apiContractFrom"] = json!({"clusterRef":{"name":source},"mode":mode});
        }
        std::fs::write(
            directory.join(format!("{name}.yaml")),
            serde_norway::to_string(&value).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn test_inheritance_keeps_local_destination_values_and_live_settings() {
        let directory = fixture();
        cluster(directory.path(), "staging", None, "");
        cluster(directory.path(), "production", Some("all"), "staging");
        cluster(directory.path(), "separate", Some("schemas"), "production");
        let inventory = super::super::discover_gitops_inventory(directory.path(), None).unwrap();
        let production = resolve_cluster_contract(&inventory, "production").unwrap();
        assert_eq!(production.capabilities_source, "staging");
        assert_eq!(production.schemas_source, "staging");
        assert_eq!(
            production
                .cluster
                .spec
                .kubernetes
                .as_ref()
                .unwrap()
                .kube_version
                .as_deref(),
            Some("1.31.4")
        );
        assert_eq!(
            production.cluster.spec.destination.server.as_deref(),
            Some("https://production.example.com")
        );
        assert_eq!(production.cluster.spec.values["identity"], "production");
        assert_eq!(
            production.cluster.spec.live.as_ref().unwrap().context,
            "production-admin"
        );
        assert_eq!(production.inputs.len(), 2);
        let separate = resolve_cluster_contract(&inventory, "separate").unwrap();
        assert_eq!(separate.capabilities_source, "separate");
        assert_eq!(separate.schemas_source, "staging");
        assert_eq!(
            separate
                .cluster
                .spec
                .kubernetes
                .as_ref()
                .unwrap()
                .kube_version
                .as_deref(),
            Some("1.32.0")
        );
        assert_eq!(separate.inputs.len(), 3);
    }

    #[test]
    fn test_inheritance_rejects_missing_sources_and_cycles() {
        let directory = fixture();
        cluster(directory.path(), "production", Some("all"), "staging");
        let inventory = super::super::discover_gitops_inventory(directory.path(), None).unwrap();
        assert!(resolve_cluster_contract(&inventory, "production")
            .unwrap_err()
            .to_string()
            .contains("production -> staging"));
        cluster(directory.path(), "staging", Some("schemas"), "production");
        let inventory = super::super::discover_gitops_inventory(directory.path(), None).unwrap();
        assert!(resolve_cluster_contract(&inventory, "production")
            .unwrap_err()
            .to_string()
            .contains("production -> staging -> production"));
    }
}
