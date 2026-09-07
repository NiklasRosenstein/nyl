//! Resource identities and portable, Rust-derived schema artifacts.

use super::{
    generate_gitops_resource_schema, generate_release_schema, GitOpsResourceKind, HelmChart, NylComponent,
    RemoteManifest,
};
use crate::constants::{API_VERSION, API_VERSION_COMPONENTS, API_VERSION_GITOPS, API_VERSION_K8S_GITOPS};
use crate::{NylError, Result};
use clap::ValueEnum;
use schemars::schema_for;
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// A built-in resource schema; Component describes the dynamic-kind envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ResourceKind {
    #[value(name = "GitRepository", alias = "git-repository", alias = "repository")]
    GitRepository,
    #[value(name = "Cluster", alias = "cluster")]
    Cluster,
    #[value(name = "ArgoCDInstance", alias = "argocd-instance", alias = "argocd")]
    ArgoCDInstance,
    #[value(name = "DeploymentTarget", alias = "deployment-target", alias = "target")]
    DeploymentTarget,
    #[value(name = "AppProjectDefinition", alias = "app-project-definition", alias = "project")]
    AppProjectDefinition,
    #[value(name = "ApplicationGroup", alias = "application-group", alias = "group")]
    ApplicationGroup,
    #[value(name = "Release", alias = "release")]
    Release,
    #[value(name = "HelmChart", alias = "helm-chart", alias = "helmchart")]
    HelmChart,
    #[value(name = "RemoteManifest", alias = "remote-manifest")]
    RemoteManifest,
    #[value(name = "Component", alias = "component")]
    Component,
}

impl ResourceKind {
    /// All supported schemas in catalog order.
    pub const ALL: [Self; 10] = [
        Self::GitRepository,
        Self::Cluster,
        Self::DeploymentTarget,
        Self::ArgoCDInstance,
        Self::ApplicationGroup,
        Self::AppProjectDefinition,
        Self::Release,
        Self::HelmChart,
        Self::RemoteManifest,
        Self::Component,
    ];

    /// Public schema name (Component is an envelope, not a literal kind).
    pub fn name(self) -> &'static str {
        match self {
            Self::GitRepository => "GitRepository",
            Self::Cluster => "Cluster",
            Self::DeploymentTarget => "DeploymentTarget",
            Self::ArgoCDInstance => "ArgoCDInstance",
            Self::ApplicationGroup => "ApplicationGroup",
            Self::AppProjectDefinition => "AppProjectDefinition",
            Self::Release => "Release",
            Self::HelmChart => "HelmChart",
            Self::RemoteManifest => "RemoteManifest",
            Self::Component => "Component",
        }
    }

    /// Canonical API version.
    pub fn api_version(self) -> &'static str {
        match self {
            Self::GitRepository => API_VERSION_GITOPS,
            Self::HelmChart | Self::RemoteManifest => API_VERSION,
            Self::Component => API_VERSION_COMPONENTS,
            _ => API_VERSION_K8S_GITOPS,
        }
    }

    /// Stable URL slug.
    pub fn slug(self) -> &'static str {
        match self {
            Self::GitRepository => "git-repository",
            Self::Cluster => "cluster",
            Self::DeploymentTarget => "deployment-target",
            Self::ArgoCDInstance => "argocd-instance",
            Self::ApplicationGroup => "application-group",
            Self::AppProjectDefinition => "app-project-definition",
            Self::Release => "release",
            Self::HelmChart => "helm-chart",
            Self::RemoteManifest => "remote-manifest",
            Self::Component => "component",
        }
    }

    /// Relative, version-qualified schema path.
    pub fn schema_path(self) -> String {
        format!("{}/{}.schema.json", self.api_version(), self.slug())
    }

    /// Generate the resource schema from its Rust model and documentation.
    pub fn schema(self) -> Value {
        let mut schema = if let Some(kind) = GitOpsResourceKind::parse(self.name()) {
            generate_gitops_resource_schema(kind)
        } else {
            match self {
                Self::Release => generate_release_schema(),
                Self::HelmChart => json!(schema_for!(HelmChart)),
                Self::RemoteManifest => json!(schema_for!(RemoteManifest)),
                Self::Component => json!(schema_for!(NylComponent)),
                _ => unreachable!("control resources handled above"),
            }
        };
        set_envelope(
            &mut schema,
            self.api_version(),
            (self != Self::Component).then(|| self.name()),
        );
        schema["title"] = json!(self.name());
        schema
    }
}

/// Add discriminators without discarding descriptions from Rust documentation.
pub(crate) fn set_envelope(schema: &mut Value, api_version: &str, kind: Option<&str>) {
    schema["properties"]["apiVersion"]["const"] = json!(api_version);
    if let Some(kind) = kind {
        schema["properties"]["kind"]["const"] = json!(kind);
    }
}

/// Reject recognized Nyl resources in an API group that does not define them.
/// Unrelated Kubernetes APIs and custom component aliases remain available.
pub fn validate_resource_api(value: &Value) -> Result<()> {
    let Some(api) = value.get("apiVersion").and_then(Value::as_str) else {
        return Ok(());
    };
    let Some(kind) = value.get("kind").and_then(Value::as_str) else {
        return Ok(());
    };
    let expected = if api == "components.nyl.niklasrosenstein.github.com/v1" {
        Some(API_VERSION_COMPONENTS)
    } else if matches!(
        api,
        API_VERSION | API_VERSION_GITOPS | API_VERSION_K8S_GITOPS | "nyl.niklasrosenstein.github.com/v1"
    ) {
        ResourceKind::ALL
            .into_iter()
            .find(|resource| *resource != ResourceKind::Component && resource.name() == kind)
            .map(ResourceKind::api_version)
    } else {
        None
    };
    if let Some(expected) = expected {
        if api != expected {
            return Err(NylError::config(format!(
                "{kind} uses unsupported apiVersion {api:?}; set apiVersion to {expected:?}"
            )));
        }
    }
    Ok(())
}

/// All published artifacts, including portable aliases for stable schema URLs.
pub fn schema_artifacts() -> BTreeMap<String, Value> {
    let mut files = BTreeMap::new();
    let mut manifest = Vec::new();
    for kind in ResourceKind::ALL {
        let path = kind.schema_path();
        files.insert(path.clone(), kind.schema());
        files.insert(
            format!("{}.schema.json", kind.slug()),
            json!({"$schema": "https://json-schema.org/draft/2020-12/schema", "$ref": path}),
        );
        manifest.push(json!({"name": kind.name(), "apiVersion": kind.api_version(), "slug": kind.slug(), "schema": path, "dynamicKind": kind == ResourceKind::Component}));
    }
    files.insert("resources.json".into(), json!({"resources": manifest}));
    files.insert(
        "nyl.schema.json".into(),
        crate::config::schema::generate_project_config_schema(),
    );
    files.insert(
        "gitops-resource.schema.json".into(),
        super::generate_gitops_aggregate_schema(),
    );
    files
}

/// Minimal authored example shared by schema consumers and the documentation.
pub fn resource_example(kind: ResourceKind) -> Value {
    let spec = match kind {
        ResourceKind::GitRepository => json!({"repoURL": "https://github.com/example/deploy.git"}),
        ResourceKind::Cluster => {
            json!({"destination": {"server": "https://kubernetes.default.svc"}, "kubernetes": {"kubeVersion": "1.31.4", "apiVersions": ["v1", "apps/v1"]}})
        }
        ResourceKind::ArgoCDInstance => json!({"clusterRef": {"name": "primary"}, "namespace": "argocd"}),
        ResourceKind::DeploymentTarget => {
            json!({"clusterRef": {"name": "primary"}, "argocdRef": {"name": "primary"}, "publication": {"repositoryRef": {"name": "deploy"}, "revision": "deploy", "pathPrefix": "targets/production"}})
        }
        ResourceKind::AppProjectDefinition => {
            json!({"management": "External", "manifest": {"apiVersion": "argoproj.io/v1alpha1", "kind": "AppProject", "metadata": {"name": "workloads", "namespace": "argocd"}, "spec": {}}})
        }
        ResourceKind::ApplicationGroup => {
            json!({"projectTemplate": {"destinationNamespaces": ["workloads"]}, "applicationNamespace": "argocd", "source": {"path": "applications/workloads"}, "destinationNamespace": "workloads"})
        }
        ResourceKind::Release => json!({"include": ["manifests/*.yaml"]}),
        ResourceKind::HelmChart => json!({"chart": {"name": "./charts/app"}, "values": {"replicaCount": 2}}),
        ResourceKind::RemoteManifest => json!({"url": "https://example.com/manifests/app.yaml"}),
        ResourceKind::Component => json!({"replicaCount": 2}),
    };
    let name = match kind {
        ResourceKind::GitRepository => "deploy",
        ResourceKind::Cluster | ResourceKind::ArgoCDInstance => "primary",
        ResourceKind::DeploymentTarget => "production",
        _ => "workloads",
    };
    let mut example = json!({"apiVersion": kind.api_version(), "kind": if kind == ResourceKind::Component { "app/v1/WebService" } else { kind.name() }, "metadata": {"name": name}, "spec": spec});
    if matches!(
        kind,
        ResourceKind::Release | ResourceKind::HelmChart | ResourceKind::RemoteManifest | ResourceKind::Component
    ) {
        example["metadata"]["namespace"] = json!("workloads");
    }
    example
}

/// Require exactly one non-null alternative, matching Option deserialization.
pub(crate) fn exclusive_fields(schema: &mut schemars::Schema, fields: &[&str], required: bool) {
    let mut alternatives = Vec::new();
    for field in fields {
        let others: Vec<_> = fields
            .iter()
            .filter(|other| *other != field)
            .map(|other| json!({"required": [other], "properties": {*other: {"not": {"type": "null"}}}}))
            .collect();
        alternatives.push(
            json!({"required": [field], "properties": {*field: {"not": {"type": "null"}}}, "not": {"anyOf": others}}),
        );
    }
    if !required {
        alternatives.push(json!({"properties": fields.iter().map(|field| ((*field).to_owned(), json!({"type": "null"}))).collect::<serde_json::Map<_,_>>()}));
    }
    schema.insert("oneOf".into(), json!(alternatives));
}

pub(crate) fn cluster_destination_constraints(schema: &mut schemars::Schema) {
    exclusive_fields(schema, &["name", "server"], true);
}

pub(crate) fn cluster_contract_constraints(schema: &mut schemars::Schema) {
    schema.insert(
        "if".into(),
        json!({
            "required":["apiContractFrom"],
            "properties":{"apiContractFrom":{"type":"object","required":["mode"],"properties":{"mode":{"const":"all"}}}}
        }),
    );
    schema.insert("then".into(), json!({"properties":{"kubernetes":{"type":"null"}}}));
    schema.insert(
        "else".into(),
        json!({"required":["kubernetes"],"properties":{"kubernetes":{"not":{"type":"null"}}}}),
    );
}
pub(crate) fn publication_constraints(schema: &mut schemars::Schema) {
    exclusive_fields(schema, &["repositoryRef", "repository"], true);
}
pub(crate) fn application_group_constraints(schema: &mut schemars::Schema) {
    exclusive_fields(schema, &["projectRef", "projectTemplate"], true);
}
pub(crate) fn remote_manifest_constraints(schema: &mut schemars::Schema) {
    exclusive_fields(schema, &["url", "urls"], true);
}
pub(crate) fn source_constraints(schema: &mut schemars::Schema) {
    exclusive_fields(schema, &["repositoryRef", "repository"], false);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_examples_follow_resource_runtime_contracts() {
        for kind in ResourceKind::ALL {
            let example = resource_example(kind);
            validate_resource_api(&example).unwrap();
            if GitOpsResourceKind::parse(kind.name()).is_some() {
                super::super::parse_gitops_resource(&example).unwrap().unwrap();
            } else {
                match kind {
                    ResourceKind::Release => {
                        super::super::Release::from_value(&example).unwrap();
                    }
                    ResourceKind::HelmChart => {
                        serde_json::from_value::<HelmChart>(example).unwrap();
                    }
                    ResourceKind::RemoteManifest => {
                        RemoteManifest::from_value(&example).unwrap().validate().unwrap();
                    }
                    ResourceKind::Component => {
                        serde_json::from_value::<NylComponent>(example).unwrap();
                    }
                    _ => unreachable!(),
                }
            }
        }
    }

    #[test]
    fn test_moved_envelopes_require_the_canonical_api() {
        for kind in ResourceKind::ALL
            .into_iter()
            .filter(|kind| *kind != ResourceKind::GitRepository)
        {
            let old_api = match kind {
                ResourceKind::HelmChart | ResourceKind::RemoteManifest => "nyl.niklasrosenstein.github.com/v1",
                ResourceKind::Component => "components.nyl.niklasrosenstein.github.com/v1",
                _ => API_VERSION_GITOPS,
            };
            let mut example = resource_example(kind);
            example["apiVersion"] = json!(old_api);
            let error = validate_resource_api(&example).unwrap_err().to_string();
            assert!(error.contains(kind.api_version()), "{error}");
        }
        validate_resource_api(&json!({"apiVersion": "apps/v1", "kind": "Deployment"})).unwrap();
        validate_resource_api(&json!({"apiVersion": "example.org/v1", "kind": "Cluster"})).unwrap();
    }
}
