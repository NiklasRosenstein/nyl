//! Project discovery and compilation support for rendered GitOps workflows.

pub mod argocd;
pub mod discovery;
pub mod layout;
pub mod reconcile;
pub mod render;
pub mod tree;

pub use argocd::{build_directory_application, DirectoryApplicationInput};
pub use discovery::{discover_gitops_inventory, DiscoveredGitOpsResource, GitOpsInventory, GitOpsInventoryKey};
pub use layout::{ensure_managed_namespace, render_manifest_layout, take_managed_namespace};
pub use reconcile::{reconcile_rendered_tree, RenderIndex, RenderIndexDestination};
pub use render::{RenderSession, RenderedRelease};
pub use tree::{compile_target_tree, validate_gitops_inventory, CompiledTargetTree};
