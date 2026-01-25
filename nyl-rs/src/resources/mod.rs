/// Resource definitions (HelmChart, Component, etc.)
///
/// This module provides Kubernetes-style resource definitions for Nyl

mod helmchart;
mod nyl_release;

pub use helmchart::{ChartRef, HelmChart, HelmChartSpec, ObjectMetadata, ReleaseMetadata};
pub use nyl_release::{extract_nyl_release, NylRelease, NylReleaseMetadata, NylReleaseSpec};
