//! The built-in `Terraform` and `OpenTofu` unit kinds: their shared
//! declaration and driver.
mod driver;
pub mod plan;
mod spec;

pub use driver::*;
pub use spec::*;
