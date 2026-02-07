/// Resource definitions (HelmChart, Component, etc.)
///
/// This module provides Kubernetes-style resource definitions for Nyl
mod application_generator;
mod component;
mod helmchart;
mod kyverno;
mod nyl_release;

pub use application_generator::{
    extract_application_generators, ApplicationDestination, ApplicationGenerator, ApplicationGeneratorMetadata,
    ApplicationGeneratorSpec, ApplicationSource, AutomatedSyncPolicy, SyncPolicy,
};
pub use component::{is_nyl_component, NylComponent};
pub use helmchart::{ChartRef, HelmChart, HelmChartSpec, ObjectMetadata, ReleaseMetadata};
pub use kyverno::{extract_kyverno_resources, Kyverno, KyvernoMetadata, KyvernoScope, KyvernoSpec};
pub use nyl_release::{extract_nyl_release, NylRelease, NylReleaseMetadata, NylReleaseSpec};
