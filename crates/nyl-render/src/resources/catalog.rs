//! The Kubernetes rendering and Kubernetes GitOps resource catalogue: each
//! kind's identity, schema, and authored example, and the schema constraints
//! Rust types cannot express.

use super::{
    generate_gitops_resource_schema, generate_release_schema, GitOpsResourceKind, HelmChart, NylComponent,
    RemoteManifest,
};
use clap::ValueEnum;
use nyl_core::constants::{API_VERSION, API_VERSION_COMPONENTS, API_VERSION_K8S_GITOPS};
use nyl_core::resources::schema::{
    exclusive_fields, require_git_source_fields, set_envelope, validate_resource_api_with_catalog, ResourceDescriptor,
};
use nyl_core::Result;
use schemars::schema_for;
use serde_json::{json, Value};

/// A built-in resource schema; Component describes the dynamic-kind envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ResourceKind {
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
    pub const ALL: [Self; 9] = [
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
            Self::HelmChart | Self::RemoteManifest | Self::Cluster | Self::Release => API_VERSION,
            Self::Component => API_VERSION_COMPONENTS,
            _ => API_VERSION_K8S_GITOPS,
        }
    }

    /// Stable URL slug.
    pub fn slug(self) -> &'static str {
        match self {
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
        if schema.get("examples").is_none() {
            schema["examples"] = json!([resource_example(self)]);
        }
        schema
    }
}

/// This crate's resource catalogue in publication order.
pub fn resource_descriptors() -> Vec<ResourceDescriptor> {
    ResourceKind::ALL
        .into_iter()
        .map(|kind| ResourceDescriptor {
            name: kind.name(),
            api_version: kind.api_version(),
            slug: kind.slug(),
            aliases: match kind {
                ResourceKind::ArgoCDInstance => &["argocd"],
                ResourceKind::DeploymentTarget => &["target"],
                ResourceKind::AppProjectDefinition => &["project"],
                ResourceKind::ApplicationGroup => &["group"],
                ResourceKind::HelmChart => &["helmchart"],
                _ => &[],
            },
            dynamic_kind: kind == ResourceKind::Component,
            schema: match kind {
                ResourceKind::Cluster => || ResourceKind::Cluster.schema(),
                ResourceKind::DeploymentTarget => || ResourceKind::DeploymentTarget.schema(),
                ResourceKind::ArgoCDInstance => || ResourceKind::ArgoCDInstance.schema(),
                ResourceKind::ApplicationGroup => || ResourceKind::ApplicationGroup.schema(),
                ResourceKind::AppProjectDefinition => || ResourceKind::AppProjectDefinition.schema(),
                ResourceKind::Release => || ResourceKind::Release.schema(),
                ResourceKind::HelmChart => || ResourceKind::HelmChart.schema(),
                ResourceKind::RemoteManifest => || ResourceKind::RemoteManifest.schema(),
                ResourceKind::Component => || ResourceKind::Component.schema(),
            },
        })
        .collect()
}

/// Every resource rendering and Kubernetes GitOps read: nyl-core's resources
/// followed by this crate's, in catalogue order. Applications extend it with
/// their own kinds, such as the built-in units.
pub fn resource_catalogue() -> Vec<ResourceDescriptor> {
    let mut descriptors = nyl_core::resources::schema::resource_descriptors();
    descriptors.extend(resource_descriptors());
    descriptors
}

/// Reject recognized Nyl resources in an API group that does not define them,
/// against [`resource_catalogue`].
pub fn validate_resource_api(value: &Value) -> Result<()> {
    validate_resource_api_with_catalog(value, &resource_catalogue())
}

/// Minimal authored example shared by schema consumers and the documentation.
pub fn resource_example(kind: ResourceKind) -> Value {
    let spec = match kind {
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
            json!({"projectTemplate": {"destinations": [{"server": "https://kubernetes.default.svc", "namespace": "workloads"}]}, "applicationNamespace": "argocd", "source": {"path": "applications/workloads"}, "destinationNamespace": "workloads"})
        }
        ResourceKind::Release => json!({"include": ["manifests/*.yaml"]}),
        ResourceKind::HelmChart => json!({"chart": {"name": "./charts/app"}, "values": {"replicaCount": 2}}),
        ResourceKind::RemoteManifest => json!({"url": "https://example.com/manifests/app.yaml"}),
        ResourceKind::Component => json!({"replicaCount": 2}),
    };
    let name = match kind {
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

pub(crate) fn cluster_destination_constraints(schema: &mut schemars::Schema) {
    exclusive_fields(
        schema,
        &[
            (
                "name",
                "Addresses the destination by its registered Argo CD cluster name.",
            ),
            ("server", "Addresses the destination by its Kubernetes API server URL."),
        ],
        None,
    );
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
    exclusive_fields(
        schema,
        &[
            (
                "repositoryRef",
                "Publishes to the coordinates of the named GitRepository.",
            ),
            ("repository", "Publishes to the inline coordinates declared here."),
        ],
        None,
    );
}

pub(crate) fn application_group_constraints(schema: &mut schemars::Schema) {
    if let Some(properties) = schema.get_mut("properties").and_then(Value::as_object_mut) {
        properties["retainedProjectNamespaces"]["items"] = json!({
            "type": "string", "minLength": 1, "maxLength": 63,
            "pattern": "^[a-z0-9]([a-z0-9-]*[a-z0-9])?$"
        });
        let items = properties["retainedNamespaceResourceWhitelist"]["items"].take();
        properties["retainedNamespaceResourceWhitelist"]["items"] =
            json!({"allOf": [items, {"not": {"required": ["name"]}}]});
    }
    exclusive_fields(
        schema,
        &[
            (
                "projectRef",
                "Uses the named AppProjectDefinition, so the project can be shared between groups or managed outside Nyl.",
            ),
            (
                "projectTemplate",
                "Generates an AppProject for this group from the declared AppProject fields; omitted fields admit only what the group deploys.",
            ),
        ],
        Some("Generates an AppProject named after the group that admits only what it deploys: the target publication repository, the target Cluster with the group's Release namespaces, and the resource kinds it renders."),
    );
    for field in [
        "retainedProjectNamespaces",
        "retainedNamespaceResourceWhitelist",
        "retainedClusterResourceWhitelist",
    ] {
        schema.get_mut("oneOf").unwrap()[0]["properties"][field] = json!({"maxItems": 0});
    }
}

pub(crate) fn remote_manifest_constraints(schema: &mut schemars::Schema) {
    exclusive_fields(
        schema,
        &[
            ("url", "Fetches the manifests at this single URL."),
            ("urls", "Fetches the manifests at each URL, in order."),
        ],
        None,
    );
}

pub(crate) fn source_constraints(schema: &mut schemars::Schema) {
    exclusive_fields(
        schema,
        &[
            (
                "repositoryRef",
                "Reads Releases from a checkout of the named GitRepository.",
            ),
            (
                "repository",
                "Reads Releases from a checkout of the inline coordinates declared here.",
            ),
        ],
        Some("Reads Releases from this project, at the declared `path`."),
    );
    require_git_source_fields(schema, &["path"]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use nyl_core::constants::API_VERSION_GITOPS;
    use nyl_core::resources::schema::{canonicalize_api_version, deprecated_api_version, DEPRECATED_API_VERSIONS};
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
    fn test_exclusive_field_branches_are_labelled_for_readers_and_validators() {
        // The generated reference and validator messages quote these branches;
        // an anonymous branch only shows `required: [<field>]`, which reads as
        // if that field were required outright.
        let mut checked = 0;
        for kind in ResourceKind::ALL {
            let mut stack = vec![kind.schema()];
            while let Some(node) = stack.pop() {
                match node {
                    Value::Object(map) => {
                        if let Some(Value::Array(branches)) = map.get("oneOf") {
                            let constraint_only = branches
                                .iter()
                                .all(|branch| !["type", "$ref", "const"].iter().any(|key| branch.get(key).is_some()));
                            if constraint_only {
                                for branch in branches {
                                    assert!(
                                        branch
                                            .get("title")
                                            .and_then(Value::as_str)
                                            .is_some_and(|title| !title.is_empty()),
                                        "{}: exclusive branch needs a title: {branch}",
                                        kind.name()
                                    );
                                    assert!(
                                        // Say what choosing the branch does, rather than restating its title.
                                        branch
                                            .get("description")
                                            .and_then(Value::as_str)
                                            .is_some_and(
                                                |text| text.ends_with('.') && text.split_whitespace().count() > 3
                                            ),
                                        "{}: exclusive branch needs a description of what it does: {branch}",
                                        kind.name()
                                    );
                                    checked += 1;
                                }
                            }
                        }
                        stack.extend(map.into_values());
                    }
                    Value::Array(items) => stack.extend(items),
                    _ => {}
                }
            }
        }
        assert!(
            checked >= 9,
            "expected every exclusive-field branch to be checked, saw {checked}"
        );
    }

    #[test]
    fn test_deprecated_api_versions_are_accepted_and_canonicalized() {
        for deprecated in DEPRECATED_API_VERSIONS {
            let kind = ResourceKind::ALL
                .into_iter()
                .find(|kind| kind.name() == deprecated.kind)
                .unwrap();
            assert_eq!(kind.api_version(), deprecated.replacement, "{}", deprecated.kind);
            let mut example = resource_example(kind);
            example["apiVersion"] = json!(deprecated.api_version);
            validate_resource_api(&example).unwrap();
            assert_eq!(canonicalize_api_version(&mut example), Some(deprecated));
            assert_eq!(example["apiVersion"], deprecated.replacement);
            assert_eq!(deprecated_api_version(&example), None);
        }
        // Only the listed kinds keep their old API version.
        let mut target = resource_example(ResourceKind::DeploymentTarget);
        target["apiVersion"] = json!(API_VERSION);
        assert!(validate_resource_api(&target).is_err());
    }

    #[test]
    fn test_moved_envelopes_require_the_canonical_api() {
        for kind in ResourceKind::ALL {
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
