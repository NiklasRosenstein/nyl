//! Listing what the units a PromotionPath's `coverage` names run as, for the
//! path checks `nyl validate` and `nyl plan` share.
//!
//! Contract: [PromotionPath](../../../../design/promotion.md#promotionpath),
//! "Validation checks every path", and [Kubernetes publications](../../../../design/promotion.md#kubernetes-publications),
//! "Coverage". The rule is [`crate::rules::promotion_paths::check`]; listing a
//! unit's deployments may render it, so the shell asks each unit's driver
//! ([`Driver::deployments`](nyl_core::orchestration::driver::Driver::deployments))
//! and hands the answers to the rule as data.

use std::collections::BTreeMap;
use std::path::Path;

use nyl_core::orchestration::driver::{DeploymentContext, DriverSet};
use nyl_core::orchestration::ids::EnvironmentName;
use nyl_core::orchestration::instance::Instances;
use nyl_core::orchestration::ports::RenderedSource;
use nyl_core::resources::promotion::PromotionPath;

use crate::rules::promotion_paths::{coverage_units, DeploymentListings};

/// The deployments of every unit `paths` name in `coverage`
/// ([`coverage_units`]), as their drivers list them from the specs
/// `sources` rendered from `worktree`. A unit whose kind has no registered
/// driver is left out; the unit checks report it.
pub fn deployments(
    paths: &[PromotionPath],
    sources: &BTreeMap<EnvironmentName, RenderedSource>,
    drivers: &DriverSet,
    worktree: &Path,
    instances: &Instances,
) -> DeploymentListings {
    let mut listings = DeploymentListings::new();
    for (environment, unit) in coverage_units(paths, sources) {
        let Some(source) = sources.get(&environment) else {
            continue;
        };
        let Some(rendered) = source.units.get(&unit) else {
            continue;
        };
        let (Some(spec), Some(driver)) = (
            rendered.spec.as_ref(),
            drivers.get(&rendered.envelope.api_version, &rendered.envelope.kind),
        ) else {
            continue;
        };
        let ctx = DeploymentContext {
            worktree,
            project_dir: &source.project_dir,
            instances,
        };
        let listing = driver.deployments(&ctx, spec).map_err(|error| error.to_string());
        listings.insert((environment, unit), listing);
    }
    listings
}
