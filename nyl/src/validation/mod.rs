//! Final-artifact validation with explicit destination and schema provenance.

mod config;
mod report;
mod resolve;
mod runner;
pub use report::{
    Finding, ResourceIdentity, ResourceLocation, ResourceResult, ResourceStatus, SchemaOrigin, ValidationOutput,
    ValidationReport,
};
pub(crate) mod schemas;
pub(crate) mod store;

pub use config::{CaptureSettings, KubeconformSettings, ValidationArgs, ValidationSettings};
pub use runner::*;
