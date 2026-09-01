//! Project discovery and compilation support for rendered GitOps workflows.

pub mod argocd;
pub mod discovery;
pub mod layout;
pub mod reconcile;
pub mod tree;

pub use crate::render::cache::{CacheMode, GitOpsCache, TreeCacheArgs};
pub use crate::render::{RenderSession, RenderedRelease};
pub(crate) use argocd::merge_sync_options;
pub use argocd::{build_directory_application, DirectoryApplicationInput};
pub use discovery::{discover_gitops_inventory, DiscoveredGitOpsResource, GitOpsInventory, GitOpsInventoryKey};
pub(crate) use layout::render_manifest_layout_with_provenance;
pub use layout::{ensure_managed_namespace, render_manifest_layout, take_managed_namespace};
pub use reconcile::{
    reconcile_rendered_tree, reconcile_rendered_tree_with_options, ReconcileOptions, RenderIndex,
    RenderIndexPublication,
};
pub use tree::{compile_target_tree, compile_target_tree_cached, validate_gitops_inventory, CompiledTargetTree};
