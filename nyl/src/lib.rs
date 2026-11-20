#![warn(clippy::all, clippy::pedantic)]
#![allow(
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::must_use_candidate,
    clippy::redundant_else,
    clippy::doc_markdown,
    clippy::implicit_hasher,
    clippy::needless_pass_by_value,
    clippy::redundant_closure_for_method_calls,
    clippy::unnecessary_wraps,
    clippy::similar_names,
    clippy::uninlined_format_args,
    clippy::inefficient_to_string,
    clippy::derivable_impls
)]

//! Nyl - Kubernetes manifest generator with Helm integration
//!
//! This crate is the `nyl` command line. It parses arguments, formats output,
//! and wires the rendering crates together; the rendering pipeline lives in
//! `nyl-render` and the shared resource types in `nyl-core`.

pub mod cli;
pub mod orchestration;

/// The resource catalogue every CLI workflow passes to discovery, rendering,
/// editor annotations, and schema export: the resources nyl-core, nyl-render,
/// and nyl-units own, and the fields each registered unit kind declares
/// resolvable.
pub fn resource_catalogue() -> nyl_render::config::ResourceCatalogue {
    static UNIT_FIELDS: std::sync::OnceLock<nyl_core::resources::units::ResolvableUnitFields> =
        std::sync::OnceLock::new();
    let mut descriptors = nyl_render::resources::catalog::resource_catalogue();
    descriptors.extend_from_slice(nyl_units::catalog::RESOURCES);
    let catalogue = nyl_render::config::ResourceCatalogue::new(descriptors);
    // A driver's spec schema does not depend on the catalogue it renders
    // against, so these drivers only declare their kinds' fields.
    let unit_fields = UNIT_FIELDS
        .get_or_init(|| unit_drivers(catalogue.clone()).resolvable_unit_fields())
        .clone();
    catalogue.with_unit_fields(unit_fields)
}

/// The unit drivers an invocation dispatches to, rendering against
/// `catalogue`: the built-in ones, or with the `test-kinds` feature the fake
/// kinds in their place (unless `tier2` runs the real tools).
pub fn unit_drivers(catalogue: nyl_render::config::ResourceCatalogue) -> nyl_core::orchestration::driver::DriverSet {
    #[cfg(all(feature = "test-kinds", not(feature = "tier2")))]
    let drivers = nyl_units::registry::with_test_kinds(&nyl_units::fake::Script::new(), catalogue);
    #[cfg(not(all(feature = "test-kinds", not(feature = "tier2"))))]
    let drivers = nyl_units::registry::builtin(catalogue);
    drivers
}
