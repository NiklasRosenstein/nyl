/// Resource definitions (HelmChart, Component, etc.)
///
/// This module provides Kubernetes-style resource definitions for Nyl

mod application_generator;
mod helmchart;
mod nyl_release;

pub use application_generator::{
    extract_application_generators, ApplicationDestination, ApplicationGenerator,
    ApplicationGeneratorMetadata, ApplicationGeneratorSpec, ApplicationSource,
    AutomatedSyncPolicy, SyncPolicy,
};
pub use helmchart::{ChartRef, HelmChart, HelmChartSpec, ObjectMetadata, ReleaseMetadata};
pub use nyl_release::{extract_nyl_release, NylRelease, NylReleaseMetadata, NylReleaseSpec};
