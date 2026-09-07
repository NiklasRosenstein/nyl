//! Final-artifact validation with explicit destination and schema provenance.

mod config;
mod resolve;
mod runner;
pub(crate) mod schemas;
pub(crate) mod store;

pub use config::{CaptureSettings, KubeconformSettings, ValidationArgs, ValidationSettings};
pub use runner::*;
