//! Resource identities and portable, Rust-derived schema artifacts.
//!
//! Each crate that owns resources contributes [`ResourceDescriptor`]s; the
//! application assembles them into one catalogue and passes it to discovery,
//! rendering, editor annotations, and schema export. This module describes
//! the `gitops.nyl/v1` resources nyl-core owns.

use super::environment::{
    Environment, EnvironmentGroup, EnvironmentTemplate, KIND_ENVIRONMENT, KIND_ENVIRONMENT_GROUP,
    KIND_ENVIRONMENT_TEMPLATE,
};
use super::{GitRepository, KIND_GIT_REPOSITORY};
use crate::constants::{API_VERSION, API_VERSION_COMPONENTS, API_VERSION_GITOPS, API_VERSION_K8S_GITOPS};
use crate::{CoreError, Result};
use schemars::schema_for;
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// Static metadata and schema generator for one resource kind.
#[derive(Debug, Clone, Copy)]
pub struct ResourceDescriptor {
    /// Canonical kind name.
    pub name: &'static str,
    /// Canonical API version.
    pub api_version: &'static str,
    /// Stable schema filename stem.
    pub slug: &'static str,
    /// Additional CLI selectors.
    pub aliases: &'static [&'static str],
    /// Whether this schema describes a dynamic kind envelope.
    pub dynamic_kind: bool,
    /// Generate the schema and its examples.
    pub schema: fn() -> Value,
}
impl PartialEq for ResourceDescriptor {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
            && self.api_version == other.api_version
            && self.slug == other.slug
            && self.aliases == other.aliases
            && self.dynamic_kind == other.dynamic_kind
    }
}
impl ResourceDescriptor {
    /// Version-qualified schema path.
    pub fn schema_path(&self) -> String {
        format!("{}/{}.schema.json", self.api_version, self.slug)
    }
}

/// The resources nyl-core owns, in catalogue order.
pub fn resource_descriptors() -> Vec<ResourceDescriptor> {
    vec![
        ResourceDescriptor {
            name: KIND_GIT_REPOSITORY,
            api_version: API_VERSION_GITOPS,
            slug: "git-repository",
            aliases: &["repository"],
            dynamic_kind: false,
            schema: || {
                gitops_schema(
                    KIND_GIT_REPOSITORY,
                    json!(schema_for!(GitRepository)),
                    git_repository_example(),
                )
            },
        },
        ResourceDescriptor {
            name: KIND_ENVIRONMENT,
            api_version: API_VERSION_GITOPS,
            slug: "environment",
            aliases: &[],
            dynamic_kind: false,
            schema: || gitops_schema(KIND_ENVIRONMENT, json!(schema_for!(Environment)), environment_example()),
        },
        ResourceDescriptor {
            name: KIND_ENVIRONMENT_GROUP,
            api_version: API_VERSION_GITOPS,
            slug: "environment-group",
            aliases: &[],
            dynamic_kind: false,
            schema: || {
                gitops_schema(
                    KIND_ENVIRONMENT_GROUP,
                    json!(schema_for!(EnvironmentGroup)),
                    environment_group_example(),
                )
            },
        },
        ResourceDescriptor {
            name: KIND_ENVIRONMENT_TEMPLATE,
            api_version: API_VERSION_GITOPS,
            slug: "environment-template",
            aliases: &[],
            dynamic_kind: false,
            schema: || {
                gitops_schema(
                    KIND_ENVIRONMENT_TEMPLATE,
                    json!(schema_for!(EnvironmentTemplate)),
                    environment_template_example(),
                )
            },
        },
    ]
}

/// A `gitops.nyl/v1` resource schema with its constant envelope, title, and example.
fn gitops_schema(kind: &str, mut schema: Value, example: Value) -> Value {
    set_envelope(&mut schema, API_VERSION_GITOPS, Some(kind));
    schema["title"] = json!(kind);
    if schema.get("examples").is_none() {
        schema["examples"] = json!([example]);
    }
    schema
}

/// Minimal authored GitRepository, shared by its schema and the documentation.
pub fn git_repository_example() -> Value {
    json!({"apiVersion": API_VERSION_GITOPS, "kind": KIND_GIT_REPOSITORY, "metadata": {"name": "deploy"}, "spec": {"repoURL": "https://github.com/example/deploy.git"}})
}

/// Minimal authored Environment, shared by its schema and the documentation.
pub fn environment_example() -> Value {
    json!({"apiVersion": API_VERSION_GITOPS, "kind": KIND_ENVIRONMENT, "metadata": {"name": "production"}, "spec": {"unitSelector": {"matchLabels": {"stage": "production"}}, "values": {"target": "production"}, "state": {"repositoryRef": {"name": "deploy"}}, "protectedRefs": ["main"]}})
}

/// Minimal authored EnvironmentGroup, shared by its schema and the documentation.
pub fn environment_group_example() -> Value {
    json!({"apiVersion": API_VERSION_GITOPS, "kind": KIND_ENVIRONMENT_GROUP, "metadata": {"name": "production"}, "spec": {"environmentSelector": {"matchLabels": {"stage": "production"}}, "values": {"cluster": "prod-1"}, "driftRepair": "manual"}})
}

/// Minimal authored EnvironmentTemplate, shared by its schema and the documentation.
pub fn environment_template_example() -> Value {
    json!({"apiVersion": API_VERSION_GITOPS, "kind": KIND_ENVIRONMENT_TEMPLATE, "metadata": {"name": "preview"}, "spec": {"parameters": {"pr": {"type": "integer"}}, "unitSelector": {"matchLabels": {"preview": "true"}}, "values": {"hostname": "pr-{{ params.pr }}.preview.example.com"}, "state": {"repositoryRef": {"name": "platform-state"}, "desiredRef": "nyl/previews", "observedRef": "nyl/previews", "path": "{{ environment.name }}"}, "allowUnprotectedSource": true, "deletionPolicy": "Teardown", "allowTeardown": true, "ttl": "7d", "maxInstances": 20}})
}

/// Add discriminators without discarding descriptions from Rust documentation.
pub fn set_envelope(schema: &mut Value, api_version: &str, kind: Option<&str>) {
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

/// Reject recognized Nyl resources in an API group that does not define them,
/// against the composed resource catalogue. Unrelated Kubernetes APIs and
/// custom component aliases remain available, and so do deprecated API
/// versions.
pub fn validate_resource_api_with_catalog(value: &Value, descriptors: &[ResourceDescriptor]) -> Result<()> {
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
    ) || descriptors.iter().any(|resource| resource.api_version == api)
    {
        descriptors
            .iter()
            .find(|resource| !resource.dynamic_kind && resource.name == kind)
            .map(|resource| resource.api_version)
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
pub fn schema_artifacts(descriptors: &[ResourceDescriptor]) -> BTreeMap<String, Value> {
    let mut files = BTreeMap::new();
    let mut manifest = Vec::new();
    for kind in descriptors {
        let path = kind.schema_path();
        files.insert(path.clone(), (kind.schema)());
        files.insert(
            format!("{}.schema.json", kind.slug),
            json!({"$schema": "https://json-schema.org/draft/2020-12/schema", "$ref": path}),
        );
        manifest.push(json!({"name": kind.name, "apiVersion": kind.api_version, "slug": kind.slug, "schema": path, "dynamicKind": kind.dynamic_kind}));
    }
    // Files that reference a deprecated API version's schema URL keep working.
    for deprecated in DEPRECATED_API_VERSIONS {
        let Some(kind) = descriptors.iter().find(|kind| kind.name == deprecated.kind) else {
            continue;
        };
        files.insert(
            format!("{}/{}.schema.json", deprecated.api_version, kind.slug),
            json!({"$schema": "https://json-schema.org/draft/2020-12/schema", "$ref": format!("../../{}", kind.schema_path())}),
        );
    }
    files.insert("resources.json".into(), json!({"resources": manifest}));
    files.extend(crate::orchestration::state::state_document_schemas());
    files
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
pub fn exclusive_fields(schema: &mut schemars::Schema, fields: &[(&str, &str)], neither: Option<&str>) {
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
pub fn require_git_source_fields(schema: &mut schemars::Schema, fields: &[&str]) {
    super::git_source::require_fields(schema, fields);
    super::strict::deny_additional_properties(schema);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_core_examples_follow_resource_runtime_contracts() {
        for descriptor in resource_descriptors() {
            let example = (descriptor.schema)()["examples"][0].clone();
            validate_resource_api_with_catalog(&example, &resource_descriptors()).unwrap();
            match descriptor.name {
                KIND_GIT_REPOSITORY => serde_json::from_value::<GitRepository>(example)
                    .unwrap()
                    .validate()
                    .unwrap(),
                KIND_ENVIRONMENT => serde_json::from_value::<Environment>(example)
                    .unwrap()
                    .validate()
                    .unwrap(),
                KIND_ENVIRONMENT_GROUP => serde_json::from_value::<EnvironmentGroup>(example)
                    .unwrap()
                    .validate()
                    .unwrap(),
                KIND_ENVIRONMENT_TEMPLATE => serde_json::from_value::<EnvironmentTemplate>(example)
                    .unwrap()
                    .validate()
                    .unwrap(),
                _ => unreachable!(),
            }
        }
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
}
