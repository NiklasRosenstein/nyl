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
/// environments its paths connect it to, as `nyl validate` does. The second
/// list holds the warnings: the values the supersede check cannot check
/// into `environment` (`NYL-PROMOTE-UNCHECKED`,
/// [`promotion_paths::unchecked`]).
pub fn findings_for(
    inventory: &GitOpsInventory,
    environment: &EnvironmentName,
    drivers: &DriverSet,
    renderer: &dyn SourceRenderer,
) -> Result<(Vec<Finding>, Vec<Finding>)> {
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
    let findings = findings_of(inventory, &paths, &known, Some(environment), &sources, drivers);
    let unchecked = promotion_paths::unchecked(&paths, environment, &sources, drivers);
    Ok((findings, unchecked))
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

/// The PromotionPath `name` with both ends resolved, as `nyl promote` and
/// `nyl get promotion-candidates` take it, or why neither can use it: no
/// such path, a form M6 does not support, or an end that names no
/// environment.
pub fn promotable_path(
    inventory: &GitOpsInventory,
    name: &str,
) -> std::result::Result<(PromotionPath, String, String), Finding> {
    use nyl_core::orchestration::codes::Code;
    use nyl_core::resources::promotion::ChangeGate;
    let Some(path) = declared_paths(inventory)
        .into_iter()
        .find(|path| path.metadata.name == name)
    else {
        return Err(Finding::new(
            Code::PromoteBindingUnknown,
            format!("the project declares no PromotionPath {name:?}"),
            "list the declared paths with `nyl get promotion-paths`",
        ));
    };
    if path.spec.change_gate != ChangeGate::None || path.spec.from.target.is_some() || path.spec.to.target.is_some() {
        return Err(Finding::new(
            Code::PromoteUnsupported,
            format!(
                "path {name} uses changeGate: pullRequest or a DeploymentTarget as its source or target, which M11 delivers"
            ),
            "use environments at both ends and `changeGate: none`",
        ));
    }
    let (Some(source), Some(target)) = (path.spec.from.environment.clone(), path.spec.to.environment.clone()) else {
        return Err(Finding::new(
            Code::PromoteEnvironmentUnknown,
            format!("path {name} names no source or target environment"),
            "set `spec.from.environment` and `spec.to.environment`",
        ));
    };
    Ok((path, source, target))
}

/// What a path's target contributes to judging the source's states: the
/// units it selects, which the whole-source rule covers, as the entry
/// worktree renders them (not at each candidate's source commit); whether its
/// source is promoted along the path; and the `NYL-PROMOTE-REBUILT` warnings.
pub struct TargetView {
    pub target_units: BTreeSet<nyl_core::orchestration::ids::UnitName>,
    pub promotes_source: bool,
    /// The target's `source.fromPromotion` as the entry worktree declares
    /// it; with `record: source` its block is the path's current record.
    pub promoted: Option<nyl_core::resources::environment::PromotedSource>,
    pub rebuilt: Vec<(String, Finding)>,
}

/// The [`TargetView`] of `path` from the target's session, the one way
/// `nyl promote` and `nyl get promotion-candidates` both take it.
pub fn target_view(
    session: &super::session::Session,
    drivers: &DriverSet,
    path: &PromotionPath,
) -> std::result::Result<TargetView, Finding> {
    use nyl_core::orchestration::codes::Code;
    let entry = session.entry()?;
    let promoted = entry
        .spec
        .source
        .as_ref()
        .and_then(|source| source.from_promotion.clone());
    let promotes_source = entry
        .spec
        .source
        .as_ref()
        .and_then(|source| source.from_promotion.as_ref())
        .is_some_and(|promoted| {
            promoted
                .path
                .iter()
                .chain(&promoted.paths)
                .any(|name| *name == path.metadata.name)
        });
    let target = session.resolved.name.clone();
    let commit = session.entry_commit()?;
    let rendered = session
        .renderer()
        .render(&session.inventory.worktree_root, &target, &commit)
        .map_err(|error| {
            Finding::new(
                Code::SourceUnavailable,
                format!("cannot render the units of {target}: {error}"),
                "correct the environment's unit declarations; `nyl validate` reports them",
            )
        })?;
    let readers = promotion_paths::unit_readers(&BTreeMap::from([(target.clone(), rendered.clone())]), drivers);
    let rebuilt = promotion_paths::rebuilt(path, &target, &rendered, &readers);
    let target_units = rendered
        .units
        .iter()
        .filter(|(_, unit)| unit.selected)
        .map(|(name, _)| name.clone())
        .collect();
    Ok(TargetView {
        target_units,
        promotes_source,
        promoted,
        rebuilt,
    })
}

/// The PromotionRecords of `entry`'s environment, read from `snapshot` as a
/// run reads them ([`nyl_orchestration::shell::promote::environment_promotions`]):
/// with `record: source` the block is refused below the applied sequence and
/// checked against its path and the source state it names, which is read
/// through `store` at the source environment's location in `inventory`.
pub fn environment_promotions(
    inventory: &GitOpsInventory,
    request: &super::project::InstanceRequest,
    store: &dyn nyl_core::orchestration::ports::StateStore,
    entry: &nyl_core::resources::environment::Environment,
    snapshot: &nyl_core::orchestration::ports::EnvironmentSnapshot,
) -> std::result::Result<
    BTreeMap<nyl_core::orchestration::ids::PromotionPathName, nyl_core::orchestration::promotion::PromotionRecord>,
    Finding,
> {
    nyl_orchestration::shell::promote::environment_promotions(
        store,
        &|environment, _path| {
            super::project::resolve_environment_with(inventory, environment.as_str(), request)
                .map(|resolved| resolved.location)
        },
        &declared_paths(inventory),
        entry,
        snapshot,
    )
}
