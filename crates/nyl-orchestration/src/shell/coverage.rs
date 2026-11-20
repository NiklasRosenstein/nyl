//! Asking the units' drivers what the PromotionPath checks `nyl validate`
//! and `nyl plan` share need from them: what the units a path's `coverage`
//! names run as, and the declared types of the fields `$promotion`
//! bindings fill that the units' kinds' schemas cannot type.
//!
//! Contract: [PromotionPath](../../../../design/promotion.md#promotionpath),
//! "Validation checks every path", and [Kubernetes publications](../../../../design/promotion.md#kubernetes-publications),
//! "Coverage". The rule is [`crate::rules::promotion_paths::check`]; answering
//! may render a unit or read the sources it names, so the shell asks each
//! unit's driver
//! ([`Driver::deployments`](nyl_core::orchestration::driver::Driver::deployments),
//! [`Driver::bound_field_types`](nyl_core::orchestration::driver::Driver::bound_field_types))
//! and hands the answers to the rule as data.

use std::collections::BTreeMap;
use std::path::Path;

use nyl_core::orchestration::driver::{DeploymentContext, Driver, DriverSet};
use nyl_core::orchestration::ids::{EnvironmentName, UnitName};
use nyl_core::orchestration::instance::Instances;
use nyl_core::orchestration::ports::RenderedSource;
use nyl_core::resources::promotion::PromotionPath;
use serde_json::Value;

use crate::rules::promotion_paths::{coverage_units, BoundFieldTypes, DeploymentListings};

/// What the drivers may read while answering for one check: the worktree
/// the units were rendered from, shared by every unit the check asks about.
pub struct ProjectReads<'a> {
    pub worktree: &'a Path,
    pub instances: &'a Instances,
    /// A render cache the check's questions share, so units that render one
    /// target render it once.
    pub cache_dir: Option<&'a Path>,
    /// The cache of remote sources that outlives the invocation, when the
    /// invocation has one.
    pub source_cache_dir: Option<&'a Path>,
    /// Read remote sources from the caches only (`nyl validate --offline`,
    /// `--local`).
    pub offline: bool,
}

/// The unit `unit` of `environment` with its rendered spec and its driver, when both exist.
fn unit_and_driver<'a>(
    sources: &'a BTreeMap<EnvironmentName, RenderedSource>,
    drivers: &'a DriverSet,
    (environment, unit): (&EnvironmentName, &UnitName),
) -> Option<(&'a RenderedSource, &'a Value, &'a dyn Driver)> {
    let source = sources.get(environment)?;
    let rendered = source.units.get(unit)?;
    let spec = rendered.spec.as_ref()?;
    let driver = drivers.get(&rendered.envelope.api_version, &rendered.envelope.kind)?;
    Some((source, spec, driver))
}

fn context<'a>(reads: &ProjectReads<'a>, source: &'a RenderedSource) -> DeploymentContext<'a> {
    DeploymentContext {
        worktree: reads.worktree,
        project_dir: &source.project_dir,
        instances: reads.instances,
        cache_dir: reads.cache_dir,
        source_cache_dir: reads.source_cache_dir,
        offline: reads.offline,
    }
}

/// The deployments of every unit `paths` name in `coverage`
/// ([`coverage_units`]), as their drivers list them from the specs
/// `sources` rendered.
pub fn deployments(
    paths: &[PromotionPath],
    sources: &BTreeMap<EnvironmentName, RenderedSource>,
    drivers: &DriverSet,
    reads: &ProjectReads<'_>,
) -> DeploymentListings {
    let mut listings = DeploymentListings::new();
    for (environment, unit) in coverage_units(paths, sources, drivers) {
        let Some((source, spec, driver)) = unit_and_driver(sources, drivers, (&environment, &unit)) else {
            continue;
        };
        let listing = driver
            .deployments(&context(reads, source), spec)
            .map_err(|error| error.to_string());
        listings.insert((environment, unit), listing);
    }
    listings
}

/// The declared types of the fields in `wanted`
/// ([`untyped_fields`](crate::rules::promotion_paths::untyped_fields)), as
/// each unit's driver reads them from what declares them.
pub fn bound_field_types(
    wanted: &BTreeMap<(EnvironmentName, UnitName), Vec<String>>,
    sources: &BTreeMap<EnvironmentName, RenderedSource>,
    drivers: &DriverSet,
    reads: &ProjectReads<'_>,
) -> BoundFieldTypes {
    let mut types = BoundFieldTypes::new();
    for ((environment, unit), pointers) in wanted {
        let Some((source, spec, driver)) = unit_and_driver(sources, drivers, (environment, unit)) else {
            continue;
        };
        let answer = driver
            .bound_field_types(&context(reads, source), spec, pointers)
            .map_err(|error| error.to_string());
        types.insert((environment.clone(), unit.clone()), answer);
    }
    types
}
