//! The `KubernetesPublication` unit: renders one DeploymentTarget with the
//! Release inputs orchestration resolved, publishes its tree and ownership
//! index, and tears the tree down in two phases.
//!
//! Contract: [Kubernetes publication unit](../../../../design/orchestration-core.md#kubernetes-publication-unit)
//! and [Artifacts](../../../../design/orchestration-core.md#artifacts).
//!
//! - [`spec`] owns the declaration and its validation.
//! - [`run`] holds the operations that render, publish, and tear down
//!   through `nyl_render::gitops::publication`, the narrow API that keeps
//!   rendering ignorant of environments, receipts, and state. The test kind
//!   `FakePublication` shares them, so both kinds publish the same bytes.
//! - [`build`] implements `nyl build`: render, `--diff` against the published
//!   tree, and `--push` for a target no environment owns.
//! - [`readiness`] evaluates the recorded teardown settings against the
//!   contract's readiness table.
//! - [`driver`] implements the [`Driver`](nyl_core::orchestration::driver::Driver)
//!   contract: the execution key from the render's portable input record,
//!   the teardown settings recorded at resolution, and drift detection.

mod build;
mod driver;
mod readiness;
mod run;
mod spec;

pub use driver::KubernetesPublicationDriver;

pub(crate) use build::build;
#[cfg(any(test, feature = "test-kinds"))]
pub(crate) use readiness::evaluate as evaluate_teardown_readiness;
pub(crate) use run::{plan, publish, render, resolution_inputs, teardown, unit_owner, unit_target};
#[cfg(any(test, feature = "test-kinds"))]
pub(crate) use spec::validate_teardown_wait;

/// The kind's name.
pub const KIND: &str = "KubernetesPublication";
