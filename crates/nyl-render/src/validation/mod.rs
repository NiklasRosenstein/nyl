//! Final-artifact validation with explicit destination and schema provenance.

mod collections;
mod config;
mod report;
pub(crate) mod resolve;
pub use resolve::builtin_url;
mod runner;
pub use report::{
    Finding, ResourceIdentity, ResourceLocation, ResourceResult, ResourceStatus, SchemaOrigin, ValidationOutput,
    ValidationReport,
};
pub mod schemas;
pub mod store;

pub use config::{BuiltinSchemas, CaptureSettings, KubeconformSettings, ValidationArgs, ValidationSettings};
pub use runner::*;
