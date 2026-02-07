/// Post-processing module for applying Kyverno policies
///
/// This module handles the execution of Kyverno policies to post-process
/// Kubernetes manifests before they are emitted.
mod kyverno;

pub use kyverno::apply_kyverno_policies;
