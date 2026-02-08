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
pub use component::{
    component_kind_to_chart_ref, is_nyl_component, is_remote_helm_chart_shortcut, parse_component_kind,
    ComponentKindParsed, NylComponent,
};
pub use helmchart::{ChartRef, HelmChart, HelmChartSpec, ObjectMetadata, ReleaseMetadata};
pub use kyverno::{
    extract_all_kyverno_policies, extract_policies_by_scope, has_kyverno_scope_annotation, is_kyverno_policy,
    AnnotatedKyvernoPolicy, KyvernoScope,
};
pub use nyl_release::{extract_nyl_release, NylRelease, NylReleaseMetadata, NylReleaseSpec};
