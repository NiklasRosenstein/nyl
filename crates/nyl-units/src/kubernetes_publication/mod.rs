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
//! - [`reconciler`] reads the reconciler that runs the target into a neutral
//!   [`Reading`](reconciler::Reading) and judges it against the publication
//!   ([`reconciler::evaluate`]); [`argocd`] is the Argo CD reader (the
//!   Kubernetes API of the control-plane Cluster, or the Argo CD API
//!   server). [`observe`] is the driver's use of them, for `nyl attest` and
//!   the `observe` teardown wait.
//! - [`observed_values`] reads a field of a rendered resource from the
//!   workload Cluster for the unit's observed values, once the Application
//!   that owns the resource runs the current publication ([`workload`] reads
//!   the resource).
//! - [`readiness`] evaluates the recorded teardown settings against the
//!   contract's readiness table.
//! - [`driver`] implements the [`Driver`](nyl_core::orchestration::driver::Driver)
//!   contract: the execution key from the render's portable input record,
//!   the teardown settings recorded at resolution, and drift detection.

pub mod argocd;
mod build;
pub mod direct;
mod driver;
mod health_wait;
mod observe;
mod observed_values;
mod readiness;
pub mod reconciler;
mod run;
mod spec;
mod workload;

pub use driver::KubernetesPublicationDriver;
pub use observed_values::{ClusterReader, LiveReader, LiveRequest, ObservedSource};
pub use workload::{KubeResourceReader, ResourceReader};

pub(crate) use build::build;
#[cfg(any(test, feature = "test-kinds"))]
pub(crate) use readiness::evaluate as evaluate_teardown_readiness;
#[cfg(any(test, feature = "test-kinds"))]
pub(crate) use run::publish;
pub(crate) use run::{plan, render, resolution_inputs, teardown, unit_owner, unit_target};
pub(crate) use spec::teardown_wait;
#[cfg(any(test, feature = "test-kinds"))]
pub(crate) use spec::validate_observe;

/// What a teardown of `unit` that nobody attends would wait for, read from its
/// declaration (resolved or not): a `manual` `teardownWait` stops it after
/// phase 1. Shared by the kind and its fake, whose `Driver::unattended_teardown`
/// it is.
pub(crate) fn unattended_teardown(
    unit: &nyl_core::orchestration::state::DesiredUnit,
) -> Vec<nyl_core::orchestration::codes::Finding> {
    let spec = unit.resolved_spec.as_ref().unwrap_or(&unit.spec);
    readiness::unattended_wait(&unit.unit.environment, &unit.unit.name, teardown_wait(spec))
        .into_iter()
        .collect()
}

/// The kind's name.
pub const KIND: &str = "KubernetesPublication";
