//! PromotionPath validation over the project, shared by `nyl validate` and
//! `nyl plan`.
//!
//! Contract: [PromotionPath](../../../design/promotion.md#promotionpath). The
//! rule is `nyl_orchestration::rules::promotion_paths`; this module only
//! gathers its inputs from the discovered project: the paths, the declared
//! environments, each environment's rendered units, and every place that
//! reads a promoted value (unit specs, and the `releaseInputs` of
//! DeploymentTargets).

use std::collections::{BTreeMap, BTreeSet};

use nyl_core::orchestration::codes::Finding;
use nyl_core::orchestration::driver::DriverSet;
use nyl_core::orchestration::ids::{CommitId, EnvironmentName};
use nyl_core::orchestration::ports::{RenderedSource, SourceRenderer};
use nyl_core::resources::promotion::PromotionPath;
use nyl_orchestration::rules::promotion_paths::{self, PathChecks, ValueReader};
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

/// The `fromPromotion` bindings of every DeploymentTarget's `releaseInputs`.
/// The Release input a binding fills declares its type, which is not read
/// here, so these readers accept any value; unit specs are walked by the rule.
fn target_readers(inventory: &GitOpsInventory) -> Vec<ValueReader> {
    let mut readers = Vec::new();
    for resource in inventory.resources.values() {
        let Some(GitOpsResource::DeploymentTarget(target)) = resource.resource.as_ref() else {
            continue;
        };
        for (release, inputs) in &target.spec.release_inputs {
            for (input, binding) in inputs {
                if let Some(promotion) = &binding.from_promotion {
                    readers.push(ValueReader {
                        location: format!(
                            "DeploymentTarget {} releaseInputs {release} input {input}",
                            resource.identity.name
                        ),
                        environment: None,
                        binding: promotion.clone(),
                        expected: None,
                    });
                }
            }
        }
    }
    readers
}

/// Every finding of the project's PromotionPaths, given each environment's
/// rendered units. A project without paths and without `fromPromotion`
/// bindings has none.
pub fn findings(
    inventory: &GitOpsInventory,
    sources: &BTreeMap<EnvironmentName, RenderedSource>,
    drivers: &DriverSet,
) -> Vec<Finding> {
    findings_of(inventory, &declared_paths(inventory), sources, drivers)
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
    let paths: Vec<PromotionPath> = declared_paths(inventory)
        .into_iter()
        .filter(|path| {
            [&path.spec.from, &path.spec.to]
                .iter()
                .any(|end| end.environment.as_deref() == Some(environment.as_str()))
        })
        .collect();
    if paths.is_empty() {
        return Ok(Vec::new());
    }
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
    Ok(findings_of(inventory, &paths, &sources, drivers))
}

fn findings_of(
    inventory: &GitOpsInventory,
    paths: &[PromotionPath],
    sources: &BTreeMap<EnvironmentName, RenderedSource>,
    drivers: &DriverSet,
) -> Vec<Finding> {
    let declared: BTreeSet<EnvironmentName> = super::project::declared(inventory)
        .0
        .iter()
        .filter_map(|environment| environment.name().ok())
        .collect();
    let mut readers = promotion_paths::unit_readers(sources, drivers);
    readers.extend(target_readers(inventory));
    promotion_paths::check(&PathChecks {
        paths,
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
