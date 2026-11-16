//! PromotionPath validation over the project, shared by `nyl validate` and
//! `nyl plan`.
//!
//! Contract: [PromotionPath](../../../design/promotion.md#promotionpath). The
//! rule is `nyl_orchestration::rules::promotion_paths`; this module only
//! gathers its inputs from the discovered project: the paths, the declared
//! environments, each environment's rendered units, and every place that
//! reads a promoted value. The renderer copies a DeploymentTarget's
//! `releaseInputs` into the spec of the publication unit that places it, so
//! walking the units' specs finds those bindings too, with the environment
//! that reads them.

use std::collections::{BTreeMap, BTreeSet};

use nyl_core::orchestration::codes::Finding;
use nyl_core::orchestration::driver::DriverSet;
use nyl_core::orchestration::ids::{CommitId, EnvironmentName};
use nyl_core::orchestration::ports::{RenderedSource, SourceRenderer};
use nyl_core::resources::promotion::PromotionPath;
use nyl_orchestration::rules::promotion_paths::{self, PathChecks};
use nyl_render::gitops::GitOpsInventory;
use nyl_render::resources::GitOpsResource;
use nyl_render::{NylError, Result};

/// The PromotionPaths the project declares, by name.
pub fn declared_paths(inventory: &GitOpsInventory) -> Vec<PromotionPath> {
    inventory
        .resources
        .values()
        .filter_map(|resource| match resource.resource.as_ref() {
            Some(GitOpsResource::PromotionPath(path)) => Some((**path).clone()),
            _ => None,
        })
        .collect()
}

/// Every finding of the project's PromotionPaths, given each environment's
/// rendered units. A project without paths and without `fromPromotion`
/// bindings has none.
pub fn findings(
    inventory: &GitOpsInventory,
    sources: &BTreeMap<EnvironmentName, RenderedSource>,
    drivers: &DriverSet,
) -> Vec<Finding> {
    let paths = declared_paths(inventory);
    findings_of(inventory, &paths, &paths, None, sources, drivers)
}

/// The findings of the paths that touch `environment`: what `nyl plan` reports
/// before it plans. Renders the worktree's units of `environment` and of the
/// environments its paths connect it to, as `nyl validate` does.
pub fn findings_for(
    inventory: &GitOpsInventory,
    environment: &EnvironmentName,
    drivers: &DriverSet,
    renderer: &dyn SourceRenderer,
) -> Result<Vec<Finding>> {
    let known = declared_paths(inventory);
    let paths: Vec<PromotionPath> = known
        .iter()
        .filter(|path| {
            [&path.spec.from, &path.spec.to]
                .iter()
                .any(|end| end.environment.as_deref() == Some(environment.as_str()))
        })
        .cloned()
        .collect();
    // The environment itself always renders: its own bindings and promoted
    // source are checked even when no path touches it.
    let mut wanted = BTreeSet::from([environment.clone()]);
    for path in &paths {
        for end in [&path.spec.from, &path.spec.to] {
            if let Some(name) = end
                .environment
                .as_deref()
                .and_then(|name| EnvironmentName::new(name).ok())
            {
                wanted.insert(name);
            }
        }
    }
    let declared: BTreeSet<EnvironmentName> = nyl_render::gitops::instances::environment_names(inventory)?
        .into_iter()
        .collect();
    let commit = CommitId::new("0".repeat(40)).expect("a commit ID");
    let mut sources = BTreeMap::new();
    for name in wanted.into_iter().filter(|name| declared.contains(name)) {
        // An environment that cannot render is reported by the command's own run.
        if let Ok(source) = renderer.render(&inventory.worktree_root, &name, &commit) {
            sources.insert(name, source);
        }
    }
    Ok(findings_of(
        inventory,
        &paths,
        &known,
        Some(environment),
        &sources,
        drivers,
    ))
}

fn findings_of(
    inventory: &GitOpsInventory,
    check: &[PromotionPath],
    known: &[PromotionPath],
    only: Option<&EnvironmentName>,
    sources: &BTreeMap<EnvironmentName, RenderedSource>,
    drivers: &DriverSet,
) -> Vec<Finding> {
    let declared: BTreeSet<EnvironmentName> = super::project::declared(inventory)
        .0
        .iter()
        .filter_map(|environment| environment.name().ok())
        .collect();
    let readers = promotion_paths::unit_readers(sources, drivers);
    promotion_paths::check(&PathChecks {
        paths: check,
        known,
        only,
        declared: &declared,
        sources,
        drivers,
        readers: &readers,
    })
}

/// The findings as one configuration error, each with its code and fix.
pub fn as_error(findings: &[Finding]) -> NylError {
    NylError::config(format!(
        "PromotionPaths are invalid:\n{}",
        findings
            .iter()
            .map(|finding| format!("  - {finding}"))
            .collect::<Vec<_>>()
            .join("\n")
    ))
}
