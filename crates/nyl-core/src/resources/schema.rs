//! Resource identities and portable, Rust-derived schema artifacts.

use super::{
    generate_gitops_resource_schema, generate_release_schema, GitOpsResourceKind, HelmChart, NylComponent,
    RemoteManifest,
};
use crate::constants::{API_VERSION, API_VERSION_COMPONENTS, API_VERSION_GITOPS, API_VERSION_K8S_GITOPS};
use crate::{CoreError, Result};
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
    #[value(name = "Environment", alias = "environment")]
    Environment,
    #[value(name = "EnvironmentGroup", alias = "environment-group")]
    EnvironmentGroup,
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
    pub const ALL: [Self; 12] = [
        Self::GitRepository,
        Self::Environment,
        Self::EnvironmentGroup,
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
            Self::Environment => "Environment",
            Self::EnvironmentGroup => "EnvironmentGroup",
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
            Self::GitRepository | Self::Environment | Self::EnvironmentGroup => API_VERSION_GITOPS,
            Self::HelmChart | Self::RemoteManifest | Self::Cluster | Self::Release => API_VERSION,
            Self::Component => API_VERSION_COMPONENTS,
            _ => API_VERSION_K8S_GITOPS,
        }
    }

    /// Stable URL slug.
    pub fn slug(self) -> &'static str {
        match self {
            Self::GitRepository => "git-repository",
            Self::Environment => "environment",
            Self::EnvironmentGroup => "environment-group",
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

/// An API version a kind still accepts after moving to another one. Nyl reads
/// it as the replacement and warns; the documentation's deprecation list names
/// when support ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeprecatedApiVersion {
    pub kind: &'static str,
    pub api_version: &'static str,
    pub replacement: &'static str,
}

/// Every deprecated API version Nyl still accepts.
pub const DEPRECATED_API_VERSIONS: [DeprecatedApiVersion; 2] = [
    DeprecatedApiVersion {
        kind: "Cluster",
        api_version: API_VERSION_K8S_GITOPS,
        replacement: API_VERSION,
    },
    DeprecatedApiVersion {
        kind: "Release",
        api_version: API_VERSION_K8S_GITOPS,
        replacement: API_VERSION,
    },
];

/// The deprecation a manifest's API version and kind fall under, if any.
pub fn deprecated_api_version(value: &Value) -> Option<DeprecatedApiVersion> {
    let api = value.get("apiVersion")?.as_str()?;
    let kind = value.get("kind")?.as_str()?;
    DEPRECATED_API_VERSIONS
        .into_iter()
        .find(|deprecated| deprecated.api_version == api && deprecated.kind == kind)
}

/// Rewrite a deprecated API version to its replacement, returning the
/// deprecation that applied.
pub fn canonicalize_api_version(value: &mut Value) -> Option<DeprecatedApiVersion> {
    let deprecated = deprecated_api_version(value)?;
    value["apiVersion"] = Value::String(deprecated.replacement.to_owned());
    Some(deprecated)
}

/// Reject recognized Nyl resources in an API group that does not define them.
/// Unrelated Kubernetes APIs and custom component aliases remain available,
/// and so do deprecated API versions.
pub fn validate_resource_api(value: &Value) -> Result<()> {
    if deprecated_api_version(value).is_some() {
        return Ok(());
    }
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
            return Err(CoreError::config(format!(
                "{kind} uses unsupported apiVersion {api:?}; set apiVersion to {expected:?}"
            )));
        }
    }
    Ok(())
}

/// Published resource artifacts, including portable aliases for stable schema
/// URLs. The project configuration schema, `nyl.schema.json`, is published next
/// to them by the rendering crate that owns `nyl.toml`, whose
/// `config::schema::published_schema_artifacts` returns the complete set.
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
    // Files that reference a deprecated API version's schema URL keep working.
    for deprecated in DEPRECATED_API_VERSIONS {
        let kind = ResourceKind::ALL
            .into_iter()
            .find(|kind| kind.name() == deprecated.kind)
            .expect("deprecations name known kinds");
        files.insert(
            format!("{}/{}.schema.json", deprecated.api_version, kind.slug()),
            json!({"$schema": "https://json-schema.org/draft/2020-12/schema", "$ref": format!("../../{}", kind.schema_path())}),
        );
    }
    files.insert("resources.json".into(), json!({"resources": manifest}));
    files.insert(
        "gitops-resource.schema.json".into(),
        super::generate_gitops_aggregate_schema(),
    );
    files.extend(crate::orchestration::state::state_document_schemas());
    files
}

/// Minimal authored example shared by schema consumers and the documentation.
pub fn resource_example(kind: ResourceKind) -> Value {
    let spec = match kind {
        ResourceKind::GitRepository => json!({"repoURL": "https://github.com/example/deploy.git"}),
        ResourceKind::Environment => {
            json!({"unitSelector": {"matchLabels": {"stage": "production"}}, "values": {"target": "production"}, "state": {"repositoryRef": {"name": "deploy"}}, "protectedRefs": ["main"]})
        }
        ResourceKind::EnvironmentGroup => {
            json!({"environmentSelector": {"matchLabels": {"stage": "production"}}, "values": {"cluster": "prod-1"}, "driftRepair": "manual"})
        }
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
        ResourceKind::GitRepository => "deploy",
        ResourceKind::Cluster | ResourceKind::ArgoCDInstance => "primary",
        ResourceKind::DeploymentTarget | ResourceKind::Environment | ResourceKind::EnvironmentGroup => "production",
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
/// Constrain a set of fields so at most one of them is set, and, when `neither`
/// is absent, so exactly one is.
///
/// Each field is given with the description of what choosing it does, and
/// `neither` describes omitting them all. The generated reference and validator
/// messages quote these branches; without them a reader sees only
/// `required: [<field>]` from one branch, which reads as if that field were
/// required outright, and the branch has no name to report.
pub(crate) fn exclusive_fields(schema: &mut schemars::Schema, fields: &[(&str, &str)], neither: Option<&str>) {
    let mut alternatives = Vec::new();
    for (field, description) in fields {
        let excluded = fields
            .iter()
            .filter(|(other, _)| other != field)
            .map(|(other, _)| json!({"required": [other], "properties": {*other: {"not": {"type": "null"}}}}))
            .collect::<Vec<_>>();
        alternatives.push(json!({
            "title": *field,
            "description": *description,
            "required": [field],
            "properties": {*field: {"not": {"type": "null"}}},
            "not": {"anyOf": excluded}
        }));
    }
    if let Some(description) = neither {
        alternatives.push(json!({
            "title": "Neither",
            "description": description,
            "properties": fields.iter().map(|(field, _)| ((*field).to_owned(), json!({"type": "null"}))).collect::<serde_json::Map<_,_>>()
        }));
    }
    schema.insert("oneOf".into(), json!(alternatives));
}

/// Mark the [`GitSource`](super::GitSource) fields a use requires and close
/// the flattened schema, as `deny_unknown_fields` would.
pub(crate) fn require_git_source_fields(schema: &mut schemars::Schema, fields: &[&str]) {
    super::git_source::require_fields(schema, fields);
    super::strict::deny_additional_properties(schema);
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
    fn test_exclusive_fields_publishes_the_call_site_meaning_of_each_branch() {
        let mut schema = schemars::Schema::default();
        exclusive_fields(
            &mut schema,
            &[("first", "Does the first thing."), ("second", "Does the second thing.")],
            Some("Does neither thing."),
        );
        let branches = schema.get("oneOf").and_then(Value::as_array).unwrap().clone();
        assert_eq!(branches.len(), 3);
        assert_eq!(branches[0]["title"], "first");
        assert_eq!(branches[0]["description"], "Does the first thing.");
        assert_eq!(branches[0]["required"], json!(["first"]));
        // The branch for one field excludes every other field.
        assert_eq!(branches[0]["not"]["anyOf"][0]["required"], json!(["second"]));
        assert_eq!(branches[2]["title"], "Neither");
        assert_eq!(branches[2]["description"], "Does neither thing.");
        assert_eq!(
            branches[2]["properties"],
            json!({"first": {"type": "null"}, "second": {"type": "null"}})
        );

        // Omitting every field is a branch only where the call site describes it.
        let mut required = schemars::Schema::default();
        exclusive_fields(&mut required, &[("first", "Does the first thing.")], None);
        assert_eq!(required.get("oneOf").and_then(Value::as_array).unwrap().len(), 1);
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
        for kind in ResourceKind::ALL.into_iter().filter(|kind| {
            !matches!(
                kind,
                ResourceKind::GitRepository | ResourceKind::Environment | ResourceKind::EnvironmentGroup
            )
        }) {
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
