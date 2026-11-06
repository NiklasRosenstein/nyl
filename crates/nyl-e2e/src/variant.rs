//! The reconcilers the scenario runs against. One scenario, three ways: the
//! DeploymentTarget declares which reconciler runs its tree, and everything
//! else (units, release, assertions) is shared.

use clap::ValueEnum;
use serde::{Deserialize, Serialize};

/// One way of running the scenario.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Variant {
    /// Nyl's direct reconciler applies the tree itself (pending: it lands in
    /// the M5 direct-reconciler branch).
    Direct,
    /// Argo CD, read through the Kubernetes API of its control-plane Cluster.
    ArgocdKubernetes,
    /// Argo CD, read through its API server with a token.
    ArgocdApi,
}

impl Variant {
    /// Every variant, in the order `--variant all` runs them.
    pub const ALL: [Variant; 3] = [Variant::ArgocdKubernetes, Variant::ArgocdApi, Variant::Direct];

    /// The name used on the command line, in resource names, and in CI.
    pub fn name(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::ArgocdKubernetes => "argocd-kubernetes",
            Self::ArgocdApi => "argocd-api",
        }
    }

    /// Whether the variant needs Argo CD installed in the cluster.
    pub fn needs_argocd(self) -> bool {
        matches!(self, Self::ArgocdKubernetes | Self::ArgocdApi)
    }

    /// The name of everything the variant creates (kind cluster, containers),
    /// so variants never share or collide.
    pub fn resource_prefix(self) -> String {
        format!("nyl-e2e-{}", self.name())
    }
}

/// What `--variant` accepts: one variant or all of them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Selection {
    Direct,
    ArgocdKubernetes,
    ArgocdApi,
    All,
}

impl Selection {
    pub fn variants(self) -> Vec<Variant> {
        match self {
            Self::Direct => vec![Variant::Direct],
            Self::ArgocdKubernetes => vec![Variant::ArgocdKubernetes],
            Self::ArgocdApi => vec![Variant::ArgocdApi],
            Self::All => Variant::ALL.to_vec(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_command_line_names_are_the_variant_names() {
        for variant in Variant::ALL {
            let parsed = <Variant as ValueEnum>::from_str(variant.name(), false).unwrap();
            assert_eq!(parsed, variant);
            assert_eq!(
                serde_json::to_string(&variant).unwrap(),
                format!("\"{}\"", variant.name())
            );
        }
    }

    #[test]
    fn test_all_runs_every_variant_once_and_the_pending_one_last() {
        let all = Selection::All.variants();
        assert_eq!(all.len(), 3);
        assert_eq!(all.last(), Some(&Variant::Direct));
        let mut unique = all.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), all.len());
    }

    #[test]
    fn test_only_the_argocd_variants_install_argocd() {
        assert!(Variant::ArgocdApi.needs_argocd());
        assert!(Variant::ArgocdKubernetes.needs_argocd());
        assert!(!Variant::Direct.needs_argocd());
    }
}
