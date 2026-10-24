//! Orchestration units as the source declares them: discovery, selection,
//! and per-environment rendering, and which DeploymentTargets environments
//! own.
//!
//! Contract: [Units](../../../../design/orchestration-core.md#units),
//! [Environment](../../../../design/orchestration-core.md#environment)
//! (values, group defaults), and the
//! [Kubernetes publication unit](../../../../design/orchestration-core.md#kubernetes-publication-unit).
//!
//! - Every document whose `apiVersion` is a unit group (`units.gitops.nyl/v1`)
//!   is a unit; one YAML document is one unit. Its envelope (`apiVersion`,
//!   `kind`, `metadata.name`, `metadata.labels`) is literal and read by the
//!   static envelope scanner, so a document that is not YAML before
//!   rendering needs a block-style envelope. Names are unique across kinds.
//! - An Environment's `unitSelector.matchLabels` selects on the literal
//!   labels before rendering; `{}` selects every unit.
//! - A selected unit's `spec` renders once per Environment with only
//!   `values` (group values merged beneath the Environment's, the
//!   Environment winning), the sanitized `environment`, and later `params`:
//!   never secrets or the process environment. A document that renders to
//!   nothing has no spec.
//! - A `KubernetesPublication` unit's rendered spec gains `releases`: for
//!   each `<group>/<release>` of its target's `releaseInputs`, the bindings
//!   orchestration resolves, as reference objects, so references at
//!   `/releases/<group>~1<release>/<input>` become graph edges.
//! - A target is placed into an environment by a publication unit of that
//!   environment naming it; a target placed by two environments is an error
//!   ([`Code::TargetEnvironmentConflict`](nyl_core::orchestration::codes::Code::TargetEnvironmentConflict)).
//!
//! Rendering never connects to anything and never learns about state.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use nyl_core::bindings::{InputBinding, InputDeclaration};
use nyl_core::orchestration::ids::{CommitId, EnvironmentName, UnitName};
use nyl_core::orchestration::ports::{RenderedSource, SourceRenderer, SourceResult};
use nyl_core::resources::units::UnitEnvelope;
use serde_json::Value;

use super::GitOpsInventory;
use crate::Result;

/// One unit document, before rendering.
#[derive(Debug, Clone, PartialEq)]
pub struct DiscoveredUnit {
    /// Path relative to the directory containing `nyl.toml`.
    pub source_path: PathBuf,
    /// One-based document index within the source file.
    pub document_index: usize,
    /// The document's text, rendered per environment.
    pub raw_document: String,
    pub envelope: UnitEnvelope,
}

/// Every unit document of the project, by name. Duplicate names across
/// kinds fail ([`Code::UnitNameDuplicate`](nyl_core::orchestration::codes::Code::UnitNameDuplicate)).
pub fn discover_units(inventory: &GitOpsInventory) -> Result<BTreeMap<UnitName, DiscoveredUnit>> {
    let _ = inventory;
    todo!("render-units")
}

/// Renders an environment's units from a pinned worktree of its source
/// commit: the [`SourceRenderer`] of orchestration.
#[derive(Debug, Clone)]
pub struct UnitSourceRenderer {
    /// Worktree-rooted directory of the entry `nyl.toml`, tried first, then
    /// `[project] previous_paths`, as `locate_checkout_project` does.
    entry_project_dir: String,
}

impl UnitSourceRenderer {
    pub fn new(entry_project_dir: impl Into<String>) -> Self {
        Self {
            entry_project_dir: entry_project_dir.into(),
        }
    }
}

impl SourceRenderer for UnitSourceRenderer {
    fn render(
        &self,
        worktree: &Path,
        environment: &EnvironmentName,
        commit: &CommitId,
    ) -> SourceResult<RenderedSource> {
        let _ = (worktree, environment, commit, &self.entry_project_dir);
        todo!("render-units")
    }
}

/// Which environment places each DeploymentTarget, by rendering every
/// Environment's selected `KubernetesPublication` units' `spec.target`.
/// Targets no publication unit names are absent.
pub fn target_placements(inventory: &GitOpsInventory) -> Result<BTreeMap<String, EnvironmentName>> {
    let _ = inventory;
    todo!("render-units")
}

/// Check the `fromUnit` bindings of a target placed into `environment`: each
/// names a selected, enabled unit of that environment and, for `output`, an
/// output it declares and does not mark sensitive. Problems are listed
/// together, each naming the binding.
pub fn check_unit_bindings(
    inventory: &GitOpsInventory,
    target: &str,
    environment: &EnvironmentName,
) -> Result<Vec<String>> {
    let _ = (inventory, target, environment);
    todo!("render-units")
}

/// Placeholder overrides for [`super::inputs::OrchestratedInputs::Placeholders`]:
/// a value of each declared type for every binding orchestration resolves.
pub(crate) fn placeholder_overrides(
    declarations: &BTreeMap<String, InputDeclaration>,
    bindings: Option<&BTreeMap<String, InputBinding>>,
) -> BTreeMap<String, Value> {
    let _ = (declarations, bindings);
    todo!("render-units")
}
