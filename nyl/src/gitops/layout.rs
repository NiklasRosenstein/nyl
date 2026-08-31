//! Deterministic on-disk layout for rendered Kubernetes manifests.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde_json::{Map, Value};

use crate::resources::{ManagedNamespacePolicy, ManagedResourceDeletionPolicy};
use crate::{NylError, Result};

const ARGOCD_SYNC_OPTIONS_ANNOTATION: &str = "argocd.argoproj.io/sync-options";
const CRD_API_VERSION: &str = "apiextensions.k8s.io/v1";
const CRD_KIND: &str = "CustomResourceDefinition";

/// Ensure that the rendered resources contain the configured destination namespace.
///
/// An existing namespace is annotated in place. A missing namespace is synthesized
/// only when `policy.create` is enabled. Existing Argo CD sync options are preserved;
/// an option that contradicts the configured namespace policy is rejected.
pub fn ensure_managed_namespace(
    resources: &mut Vec<Value>,
    namespace: &str,
    policy: &ManagedNamespacePolicy,
) -> Result<()> {
    if let Some(namespace) = take_managed_namespace(resources, namespace, policy)? {
        resources.push(namespace);
    }
    Ok(())
}

/// Remove and return the destination Namespace so it can be owned by one
/// dedicated Argo CD Application rather than every workload Application.
pub fn take_managed_namespace(
    resources: &mut Vec<Value>,
    namespace: &str,
    policy: &ManagedNamespacePolicy,
) -> Result<Option<Value>> {
    validate_safe_path_segment("namespace", namespace)?;

    let matching_indices = resources
        .iter()
        .enumerate()
        .filter_map(|(index, resource)| is_namespace_named(resource, namespace).then_some(index))
        .collect::<Vec<_>>();

    let namespace_index = match matching_indices.as_slice() {
        [] if !policy.create => return Ok(None),
        [] => {
            let mut namespace = serde_json::json!({
                "apiVersion": "v1",
                "kind": "Namespace",
                "metadata": { "name": namespace },
            });
            apply_namespace_policy(&mut namespace, policy)?;
            return Ok(Some(namespace));
        }
        [index] => *index,
        _ => {
            return Err(NylError::config(format!(
                "Rendered resources contain more than one Namespace named {namespace:?}"
            )))
        }
    };

    let mut namespace = resources.remove(namespace_index);
    apply_namespace_policy(&mut namespace, policy)?;
    Ok(Some(namespace))
}

/// Serialize manifests into the rendered application directory layout.
///
/// Non-CRD resources are stored as a multi-document `resources.yaml` stream.
/// Each v1 CRD is stored separately as `crd/<metadata.name>.yaml`. Paths are
/// returned relative to the application directory and sorted lexicographically.
pub fn render_manifest_layout(resources: &[Value]) -> Result<BTreeMap<PathBuf, Vec<u8>>> {
    let mut output = BTreeMap::new();
    let mut ordinary_resources = Vec::new();
    let mut crd_names = BTreeSet::new();

    for resource in resources {
        if is_v1_crd(resource) {
            let name = resource
                .pointer("/metadata/name")
                .and_then(Value::as_str)
                .ok_or_else(|| NylError::config("CustomResourceDefinition metadata.name must be a string"))?;
            validate_safe_path_segment("CustomResourceDefinition metadata.name", name)?;
            if !crd_names.insert(name.to_owned()) {
                return Err(NylError::config(format!(
                    "Rendered resources contain duplicate CustomResourceDefinition {name:?}"
                )));
            }

            let path = PathBuf::from("crd").join(format!("{name}.yaml"));
            output.insert(path, serialize_documents(&[resource])?);
        } else {
            ordinary_resources.push(resource);
        }
    }

    if !ordinary_resources.is_empty() {
        let mut ordered = ordinary_resources
            .into_iter()
            .map(|resource| {
                let key = crate::kubernetes::ResourceKey::from_json_value(resource)?;
                Ok((
                    (
                        crate::kubernetes::ResourceOrdering::priority(resource),
                        key.gvk.group,
                        key.gvk.version,
                        key.gvk.kind,
                        key.namespace.unwrap_or_default(),
                        key.name,
                    ),
                    resource,
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        ordered.sort_by(|left, right| left.0.cmp(&right.0));
        let ordinary_resources = ordered.into_iter().map(|(_, resource)| resource).collect::<Vec<_>>();
        output.insert(
            PathBuf::from("resources.yaml"),
            serialize_documents(&ordinary_resources)?,
        );
    }

    Ok(output)
}

fn is_namespace_named(resource: &Value, namespace: &str) -> bool {
    resource.get("apiVersion").and_then(Value::as_str) == Some("v1")
        && resource.get("kind").and_then(Value::as_str) == Some("Namespace")
        && resource.pointer("/metadata/name").and_then(Value::as_str) == Some(namespace)
}

fn is_v1_crd(resource: &Value) -> bool {
    resource.get("apiVersion").and_then(Value::as_str) == Some(CRD_API_VERSION)
        && resource.get("kind").and_then(Value::as_str) == Some(CRD_KIND)
}

fn apply_namespace_policy(namespace: &mut Value, policy: &ManagedNamespacePolicy) -> Result<()> {
    let namespace = namespace
        .as_object_mut()
        .ok_or_else(|| NylError::config("Namespace manifest must be an object"))?;
    let metadata = object_field(namespace, "metadata", "Namespace metadata")?;
    let existing = match metadata.get("annotations") {
        Some(Value::Object(annotations)) => match annotations.get(ARGOCD_SYNC_OPTIONS_ANNOTATION) {
            Some(Value::String(value)) => value.as_str(),
            Some(_) => {
                return Err(NylError::config(format!(
                    "Namespace annotation {ARGOCD_SYNC_OPTIONS_ANNOTATION:?} must be a string"
                )))
            }
            None => "",
        },
        Some(_) => return Err(NylError::config("Namespace metadata.annotations must be an object")),
        None => "",
    };

    let mut options = parse_sync_options(existing)?;
    merge_policy_option(&mut options, "Prune", deletion_policy_value(policy.prune_policy))?;
    merge_policy_option(&mut options, "Delete", deletion_policy_value(policy.delete_policy))?;

    if !options.is_empty() {
        let annotations = object_field(metadata, "annotations", "Namespace metadata.annotations")?;
        annotations.insert(
            ARGOCD_SYNC_OPTIONS_ANNOTATION.to_owned(),
            Value::String(options.into_values().collect::<Vec<_>>().join(",")),
        );
    }
    Ok(())
}

fn object_field<'a>(parent: &'a mut Map<String, Value>, key: &str, field: &str) -> Result<&'a mut Map<String, Value>> {
    let value = parent
        .entry(key.to_owned())
        .or_insert_with(|| Value::Object(Map::new()));
    value
        .as_object_mut()
        .ok_or_else(|| NylError::config(format!("{field} must be an object")))
}

fn parse_sync_options(value: &str) -> Result<BTreeMap<String, String>> {
    let mut options = BTreeMap::new();
    for option in value.split(',').map(str::trim).filter(|option| !option.is_empty()) {
        let key = option.split_once('=').map_or(option, |(key, _)| key).trim();
        if key.is_empty() {
            return Err(NylError::config("Argo CD sync option has an empty name"));
        }
        match options.get(key) {
            Some(existing) if existing != option => {
                return Err(NylError::config(format!(
                    "Argo CD sync option {key:?} is configured more than once with conflicting values"
                )))
            }
            _ => {
                options.insert(key.to_owned(), option.to_owned());
            }
        }
    }
    Ok(options)
}

fn merge_policy_option(options: &mut BTreeMap<String, String>, key: &str, value: Option<&str>) -> Result<()> {
    let Some(value) = value else {
        return Ok(());
    };
    let required = format!("{key}={value}");
    if let Some(existing) = options.get(key) {
        if existing != &required {
            return Err(NylError::config(format!(
                "Namespace sync option {existing:?} conflicts with required policy {required:?}"
            )));
        }
    } else {
        options.insert(key.to_owned(), required);
    }
    Ok(())
}

fn deletion_policy_value(policy: ManagedResourceDeletionPolicy) -> Option<&'static str> {
    match policy {
        ManagedResourceDeletionPolicy::Automatic => None,
        ManagedResourceDeletionPolicy::Confirm => Some("confirm"),
        ManagedResourceDeletionPolicy::Retain => Some("false"),
    }
}

fn validate_safe_path_segment(field: &str, segment: &str) -> Result<()> {
    if segment.is_empty()
        || matches!(segment, "." | "..")
        || segment.contains('/')
        || segment.contains('\\')
        || segment.contains('\0')
    {
        return Err(NylError::config(format!(
            "{field} {segment:?} is not a safe path segment"
        )));
    }
    Ok(())
}

fn serialize_documents(resources: &[&Value]) -> Result<Vec<u8>> {
    let mut yaml = String::new();
    for (index, resource) in resources.iter().enumerate() {
        if index > 0 {
            yaml.push_str("---\n");
        }
        let mut resource = (*resource).clone();
        if let Some(source) = take_helm_source(&mut resource)? {
            yaml.push_str("# Source: ");
            yaml.push_str(&source);
            yaml.push('\n');
        }
        let document = crate::yaml::serialize_yaml_document(&resource)
            .map_err(|error| NylError::config(format!("Failed to serialize rendered manifest: {error}")))?;
        yaml.push_str(&document);
        if !yaml.ends_with('\n') {
            yaml.push('\n');
        }
    }
    Ok(yaml.into_bytes())
}

fn take_helm_source(resource: &mut Value) -> Result<Option<String>> {
    let Some(metadata) = resource.get_mut("metadata").and_then(Value::as_object_mut) else {
        return Ok(None);
    };
    let Some(annotations) = metadata.get_mut("annotations").and_then(Value::as_object_mut) else {
        return Ok(None);
    };
    let source = annotations.remove(crate::helm::HELM_SOURCE_ANNOTATION);
    if annotations.is_empty() {
        metadata.remove("annotations");
    }
    match source {
        Some(Value::String(source)) => Ok(Some(source)),
        Some(_) => Err(NylError::config(format!(
            "Reserved Helm source annotation {} must be a string",
            crate::helm::HELM_SOURCE_ANNOTATION
        ))),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_namespace_policy() -> ManagedNamespacePolicy {
        ManagedNamespacePolicy::default()
    }

    #[test]
    fn ensure_managed_namespace_creates_namespace_with_confirmation_policies() {
        let mut resources = vec![serde_json::json!({"apiVersion": "v1", "kind": "ConfigMap"})];

        ensure_managed_namespace(&mut resources, "workloads", &default_namespace_policy()).unwrap();

        let namespace = &resources[1];
        assert_eq!(
            namespace.pointer("/metadata/name"),
            Some(&Value::String("workloads".into()))
        );
        assert_eq!(
            namespace.pointer("/metadata/annotations/argocd.argoproj.io~1sync-options"),
            Some(&Value::String("Delete=confirm,Prune=confirm".into()))
        );
    }

    #[test]
    fn ensure_managed_namespace_merges_existing_options_deterministically() {
        let mut resources = vec![serde_json::json!({
            "apiVersion": "v1",
            "kind": "Namespace",
            "metadata": {
                "name": "workloads",
                "annotations": {
                    "argocd.argoproj.io/sync-options": "ServerSideApply=true,Prune=confirm"
                }
            }
        })];
        let policy = ManagedNamespacePolicy {
            create: false,
            prune_policy: ManagedResourceDeletionPolicy::Confirm,
            delete_policy: ManagedResourceDeletionPolicy::Retain,
        };

        ensure_managed_namespace(&mut resources, "workloads", &policy).unwrap();

        assert_eq!(
            resources[0].pointer("/metadata/annotations/argocd.argoproj.io~1sync-options"),
            Some(&Value::String("Delete=false,Prune=confirm,ServerSideApply=true".into()))
        );
    }

    #[test]
    fn ensure_managed_namespace_rejects_conflicting_existing_policy() {
        let mut resources = vec![serde_json::json!({
            "apiVersion": "v1",
            "kind": "Namespace",
            "metadata": {
                "name": "workloads",
                "annotations": { "argocd.argoproj.io/sync-options": "Prune=false" }
            }
        })];

        let error = ensure_managed_namespace(&mut resources, "workloads", &default_namespace_policy()).unwrap_err();
        assert!(error.to_string().contains("conflicts with required policy"));
    }

    #[test]
    fn ensure_managed_namespace_does_not_create_when_disabled() {
        let mut resources = Vec::new();
        let policy = ManagedNamespacePolicy {
            create: false,
            ..default_namespace_policy()
        };

        ensure_managed_namespace(&mut resources, "workloads", &policy).unwrap();

        assert!(resources.is_empty());
    }

    #[test]
    fn ensure_managed_namespace_automatic_policy_does_not_add_empty_annotations() {
        let mut resources = vec![serde_json::json!({
            "apiVersion": "v1",
            "kind": "Namespace",
            "metadata": { "name": "workloads" }
        })];
        let policy = ManagedNamespacePolicy {
            create: true,
            prune_policy: ManagedResourceDeletionPolicy::Automatic,
            delete_policy: ManagedResourceDeletionPolicy::Automatic,
        };

        ensure_managed_namespace(&mut resources, "workloads", &policy).unwrap();

        assert!(resources[0].pointer("/metadata/annotations").is_none());
    }

    #[test]
    fn render_manifest_layout_splits_and_sorts_crds() {
        let resources = vec![
            serde_json::json!({"apiVersion": "v1", "kind": "Service", "metadata": {"name": "api"}}),
            serde_json::json!({
                "apiVersion": CRD_API_VERSION,
                "kind": CRD_KIND,
                "metadata": {
                    "name": "widgets.example.com",
                    "annotations": {
                        "gitops.nyl.niklasrosenstein.github.com/helm-source": "widget/crds/widgets.yaml"
                    }
                },
                "spec": {}
            }),
            serde_json::json!({"apiVersion": "apps/v1", "kind": "Deployment", "metadata": {"name": "api"}}),
            serde_json::json!({
                "apiVersion": CRD_API_VERSION,
                "kind": CRD_KIND,
                "metadata": {"name": "gadgets.example.com"},
                "spec": {}
            }),
        ];

        let output = render_manifest_layout(&resources).unwrap();
        let paths = output.keys().cloned().collect::<Vec<_>>();
        assert_eq!(
            paths,
            vec![
                PathBuf::from("crd/gadgets.example.com.yaml"),
                PathBuf::from("crd/widgets.example.com.yaml"),
                PathBuf::from("resources.yaml"),
            ]
        );
        let ordinary = String::from_utf8(output[&PathBuf::from("resources.yaml")].clone()).unwrap();
        assert!(ordinary.contains("kind: Service"));
        assert!(ordinary.contains("---\napiVersion: apps/v1"));
        assert!(!ordinary.contains("CustomResourceDefinition"));
        let widget = String::from_utf8(output[&PathBuf::from("crd/widgets.example.com.yaml")].clone()).unwrap();
        assert!(widget.starts_with("# Source: widget/crds/widgets.yaml\n"));
        assert!(!widget.contains(crate::helm::HELM_SOURCE_ANNOTATION));
    }

    #[test]
    fn render_manifest_layout_rejects_duplicate_or_unsafe_crd_names() {
        let duplicate = serde_json::json!({
            "apiVersion": CRD_API_VERSION,
            "kind": CRD_KIND,
            "metadata": {"name": "widgets.example.com"}
        });
        let error = render_manifest_layout(&[duplicate.clone(), duplicate]).unwrap_err();
        assert!(error.to_string().contains("duplicate CustomResourceDefinition"));

        let unsafe_name = serde_json::json!({
            "apiVersion": CRD_API_VERSION,
            "kind": CRD_KIND,
            "metadata": {"name": "../widgets.example.com"}
        });
        let error = render_manifest_layout(&[unsafe_name]).unwrap_err();
        assert!(error.to_string().contains("not a safe path segment"));
    }
}
