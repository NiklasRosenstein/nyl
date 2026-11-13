/// Resource definitions shared across Nyl's layers.
///
/// The `gitops.nyl/v1` resources (GitRepository, Environment,
/// EnvironmentGroup), the fields every unit kind shares, and the schema
/// catalogue machinery. Kubernetes rendering and Kubernetes GitOps resources
/// live in nyl-render's `resources` module.
pub mod environment;
pub mod git_source;
mod gitops;
pub mod parse;
pub mod schema;
pub mod strict;
pub mod units;

pub use git_source::{GitSource, ReadGitRepository};
pub use gitops::*;
pub use parse::parse_resource;
pub use strict::DenyUnknownFields;
