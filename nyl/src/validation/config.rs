//! Project validation and capture settings.

use clap::Args;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{NylError, Result};

/// Project-wide validation policy. Named validator sections select implementations.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ValidationSettings {
    /// Run configured validators automatically on rendering commands.
    pub enabled: bool,
    /// Configure kubeconform schema validation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kubeconform: Option<KubeconformSettings>,
}

/// Kubeconform's schema inputs and bounded execution policy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct KubeconformSettings {
    /// Reject unknown properties and duplicate YAML keys.
    pub strict: bool,
    /// Require built-in schemas materialized by nyl vendor; never download during validation.
    pub vendor_builtin_schemas: bool,
    /// Ordered local schema lookup templates, relative to nyl.toml.
    pub schema_locations: Vec<String>,
    /// Explicit apiVersion/kind exclusions. Skipped resources appear in the summary.
    pub skip: Vec<String>,
    /// Maximum duration in seconds for one validator invocation.
    pub timeout_seconds: u64,
    /// Immutable commit of yannh/kubernetes-json-schema. Omitted uses Nyl's pinned revision.
    pub builtin_schema_revision: Option<String>,
}

impl Default for KubeconformSettings {
    fn default() -> Self {
        Self {
            strict: true,
            vendor_builtin_schemas: false,
            schema_locations: Vec::new(),
            skip: Vec::new(),
            timeout_seconds: 60,
            builtin_schema_revision: None,
        }
    }
}

/// Defaults for explicit live capture commands.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct CaptureSettings {
    /// Cluster capture defaults.
    pub cluster: ClusterCaptureSettings,
}

/// Optional cluster API schema capture.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ClusterCaptureSettings {
    /// Capture schemas for all served CRD versions.
    pub crds: bool,
}

/// Shared switches for all manifest-producing commands.
// Clap exposes independent invocation switches rather than one combined state.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Default, Args)]
pub struct ValidationArgs {
    /// Export a validation report (repeatable). Formats: text, json. PATH=- requires render-tree.
    #[arg(long, value_name = "FORMAT:PATH", conflicts_with = "no_validate")]
    pub validation_output: Vec<super::report::ValidationOutput>,
    /// Suppress human findings and the validation summary on stderr; progress remains visible.
    #[arg(long)]
    pub no_validation_stderr: bool,
    /// Run every validator configured in nyl.toml.
    #[arg(long, conflicts_with = "no_validate")]
    pub validate: bool,
    /// Suppress project-default manifest validation.
    #[arg(long, conflicts_with = "use_desired_crds")]
    pub no_validate: bool,
    /// Validate using desired CRDs; asserts that every affected CR is included.
    /// Cannot be used with resource filters or append-release.
    #[arg(long)]
    pub use_desired_crds: bool,
}

impl ValidationArgs {
    /// Whether a report owns stdout for this invocation.
    pub fn report_stdout(&self) -> bool {
        self.validation_output
            .iter()
            .any(|output| output.path == std::path::Path::new("-"))
    }

    /// Validate report destinations before rendering or changing external state.
    pub fn validate_outputs(
        &self,
        stdout_allowed: bool,
        protected: &[std::path::PathBuf],
        trees: &[std::path::PathBuf],
    ) -> Result<()> {
        super::report::validate_outputs(&self.validation_output, stdout_allowed, protected, trees)
    }

    /// Resolve invocation overrides and reject an empty validator selection.
    pub fn enabled(&self, settings: &ValidationSettings) -> Result<bool> {
        let enabled = !self.no_validate
            && (self.validate || self.use_desired_crds || !self.validation_output.is_empty() || settings.enabled);
        if enabled && settings.kubeconform.is_none() {
            return Err(NylError::config(
                "Validation requested but no validators configured; add [validation.kubeconform] to nyl.toml",
            ));
        }
        Ok(enabled)
    }

    /// A filtered or incremental input cannot assert complete CRD coverage.
    pub fn check_complete(&self, partial: bool) -> Result<()> {
        if self.use_desired_crds && partial {
            return Err(NylError::config(
                "--use-desired-crds cannot be used with resource filters or --append-release",
            ));
        }
        Ok(())
    }
}
