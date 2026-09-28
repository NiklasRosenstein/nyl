//! Project settings shared by resources and rendering.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Controls when empty `metadata.labels` maps are stripped from emitted manifests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StripEmptyMetadataLabelsMode {
    /// Always strip empty `metadata.labels` maps from emitted manifests.
    #[default]
    Always,

    /// Never strip empty `metadata.labels` maps from emitted manifests.
    Never,

    /// Strip empty `metadata.labels` maps only when running in an ArgoCD environment.
    Argocd,
}

impl StripEmptyMetadataLabelsMode {
    /// Return whether empty metadata labels should be stripped for the current environment.
    pub fn should_strip(self, is_argocd: bool) -> bool {
        match self {
            Self::Always => true,
            Self::Never => false,
            Self::Argocd => is_argocd,
        }
    }
}
