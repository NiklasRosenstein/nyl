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

/// Resource descriptors assembled for every CLI workflow.
pub fn resource_catalogue() -> Vec<nyl_core::resources::schema::ResourceDescriptor> {
    let mut descriptors = nyl_render::resources::catalog::resource_catalogue();
    descriptors.extend_from_slice(nyl_units::catalog::RESOURCES);
    descriptors
}

/// Discover a GitOps project with the resource APIs registered by this
/// application.
pub fn discover_gitops_inventory(
    start_dir: &std::path::Path,
    output_subtree: Option<&std::path::Path>,
) -> nyl_render::Result<nyl_render::gitops::GitOpsInventory> {
    nyl_render::gitops::discover_gitops_inventory_with_catalogue(
        start_dir,
        output_subtree,
        nyl_render::config::ResourceCatalogue(resource_catalogue()),
    )
}
