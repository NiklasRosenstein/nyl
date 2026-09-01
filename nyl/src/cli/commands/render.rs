//! CLI adapter for Kubernetes manifest rendering.

pub(crate) use crate::render::{
    best_effort_parse_yaml_documents, run_render_preflight, ClusterClientRequirement, RenderPreflightOptions,
};
pub use crate::render::{execute, RenderArgs, RenderOptions};
