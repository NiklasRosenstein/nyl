/// Resource definitions (HelmChart, Component, etc.)
///
/// This module provides Kubernetes-style resource definitions for Nyl
mod application_generator;
mod argocd_application_field_catalog;
mod component;
mod gitops;
mod helmchart;
mod kyverno;
mod path_glob;
mod release;
mod remote_manifest;

pub use application_generator::{
    extract_application_generators, ApplicationDestination, ApplicationGenerator, ApplicationGeneratorMetadata,
    ApplicationGeneratorSpec, ApplicationSource, AutomatedSyncPolicy, ReleaseCustomizationPolicy, SyncPolicy,
};
pub use argocd_application_field_catalog::{
    is_supported_application_array_field_path, is_supported_application_field_path,
};
pub use component::{
    component_kind_to_chart_ref, is_nyl_component, is_remote_helm_chart_shortcut, parse_component_kind,
    ComponentKindParsed, NylComponent,
};
pub use gitops::*;
pub use helmchart::{ChartRef, HelmChart, HelmChartSpec, ObjectMetadata};
pub use kyverno::{
    extract_all_kyverno_policies, extract_policies_by_scope, has_kyverno_scope_annotation, is_kyverno_policy,
    AnnotatedKyvernoPolicy, KyvernoScope,
};
pub use path_glob::{join_segments as join_field_path_segments, path_matches_glob, validate_path_glob_pattern};
pub use release::{
    extract_release, generate_release_schema, Release, ReleaseArgoCdSpec, ReleaseMetadata, ReleaseSpec, KIND_RELEASE,
    RELEASE_SCHEMA_FILENAME,
};
pub use remote_manifest::{RemoteManifest, RemoteManifestSpec};
