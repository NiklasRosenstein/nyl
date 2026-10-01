//! Deterministic on-disk layout for rendered Kubernetes manifests.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
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

/// Remove and return a managed Namespace for callers that place it outside the
/// resource list being processed.
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
/// Each v1 CRD is stored as `crd/<metadata.name>.yaml`. Every other resource is
/// stored in its own file at [`resource_path`]. Paths are returned relative to
/// the application directory and sorted lexicographically.
pub fn render_manifest_layout(resources: &[Value]) -> Result<BTreeMap<PathBuf, Vec<u8>>> {
    render_manifest_layout_with_provenance(resources, &HashMap::new())
}

pub(crate) fn render_manifest_layout_with_provenance(
    resources: &[Value],
    provenance: &HashMap<crate::kubernetes::ResourceKey, crate::render::Provenance>,
) -> Result<BTreeMap<PathBuf, Vec<u8>>> {
    let mut output = BTreeMap::new();
    // Paths are compared case-insensitively so that the layout can be checked
    // out on case-insensitive filesystems without two files merging into one.
    let mut folded_paths = HashMap::new();

    for resource in resources {
        let key = crate::kubernetes::ResourceKey::from_json_value(resource)?;
        let path = if is_v1_crd(resource) {
            validate_safe_path_segment("CustomResourceDefinition metadata.name", &key.name)?;
            PathBuf::from("crd").join(format!("{}.yaml", key.name))
        } else {
            resource_path(&key)?
        };

        let folded = path.to_string_lossy().to_lowercase();
        if let Some(previous) = folded_paths.insert(folded, key.clone()) {
            let message = if is_v1_crd(resource) && previous.gvk == key.gvk {
                format!(
                    "Rendered resources contain duplicate CustomResourceDefinition {:?}",
                    key.name
                )
            } else if previous.gvk.group == key.gvk.group
                && previous.gvk.kind == key.gvk.kind
                && previous.namespace.as_deref().unwrap_or_default() == key.namespace.as_deref().unwrap_or_default()
                && previous.name == key.name
            {
                format!("Rendered resources contain duplicate resource {key}")
            } else {
                format!(
                    "Rendered resources {previous} and {key} map to the same file {} on case-insensitive filesystems",
                    path.display()
                )
            };
            return Err(NylError::config(message));
        }
        output.insert(path, serialize_documents(&[resource], provenance)?);
    }

    Ok(output)
}

/// Return the file that stores one non-CRD resource in the rendered layout.
///
/// The path is `<kind>[.<group>]/[<namespace>/]<name>.yaml`, where `<kind>` is
/// lowercased and the type segment matches kubectl's `<kind>.<group>` resource
/// form, for example `deployment.apps/api/web.yaml` or `clusterrole.rbac.authorization.k8s.io/admin.yaml`.
/// The API version is omitted, so changing it keeps the object in the same file.
/// A resource without `metadata.namespace` has no namespace directory.
///
/// The mapping is injective: every byte outside a per-segment safe set is
/// percent-encoded, `%` included. The namespace safe set excludes `.`, so a
/// namespace directory never equals a `<name>.yaml` file beside it.
pub fn resource_path(key: &crate::kubernetes::ResourceKey) -> Result<PathBuf> {
    let kind = encode_path_segment(&key.gvk.kind.to_lowercase(), |byte| byte.is_ascii_alphanumeric());
    let resource_type = if key.gvk.group.is_empty() {
        kind
    } else {
        let group = encode_path_segment(&key.gvk.group, |byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-')
        });
        format!("{kind}.{group}")
    };
    if key.name.is_empty() {
        return Err(NylError::config(format!(
            "Rendered {} has an empty metadata.name",
            key.gvk.kind
        )));
    }
    let name = encode_path_segment(&key.name, |byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-')
    });

    let mut segments = vec![resource_type];
    if let Some(namespace) = key.namespace.as_deref().filter(|namespace| !namespace.is_empty()) {
        segments.push(encode_path_segment(namespace, |byte| {
            byte.is_ascii_alphanumeric() || byte == b'-'
        }));
    }
    segments.push(format!("{name}.yaml"));

    let mut path = PathBuf::new();
    for segment in segments {
        if segment.len() > MAX_PATH_SEGMENT_BYTES {
            return Err(NylError::config(format!(
                "Rendered resource {key} needs the path segment {segment:?}, which exceeds {MAX_PATH_SEGMENT_BYTES} bytes"
            )));
        }
        path.push(segment);
    }
    Ok(path)
}

/// The common file name length limit of Linux, macOS, and Windows filesystems.
const MAX_PATH_SEGMENT_BYTES: usize = 255;

fn encode_path_segment(value: &str, safe: impl Fn(u8) -> bool) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if safe(byte) {
            encoded.push(byte as char);
        } else {
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
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

fn serialize_documents(
    resources: &[&Value],
    provenance: &HashMap<crate::kubernetes::ResourceKey, crate::render::Provenance>,
) -> Result<Vec<u8>> {
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
        let key = crate::kubernetes::ResourceKey::from_json_value(&resource)?;
        if let Some(provenance) = provenance.get(&key) {
            for line in provenance.to_string().lines() {
                yaml.push_str("# Nyl-Provenance: ");
                yaml.push_str(line);
                yaml.push('\n');
            }
        }
        let document = crate::yaml::serialize_yaml_value(&resource)
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
    fn render_manifest_layout_writes_one_file_per_resource() {
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

        let provenance = resources
            .iter()
            .map(|resource| {
                (
                    crate::kubernetes::ResourceKey::from_json_value(resource).unwrap(),
                    crate::render::Provenance(vec![
                        crate::render::ProvenanceFrame::Source {
                            path: "applications/api.yaml".into(),
                            document: 2,
                        },
                        crate::render::ProvenanceFrame::Resource {
                            identity: resource["kind"].as_str().unwrap().to_owned(),
                        },
                    ]),
                )
            })
            .collect::<HashMap<_, _>>();
        let output = render_manifest_layout_with_provenance(&resources, &provenance).unwrap();
        let paths = output.keys().cloned().collect::<Vec<_>>();
        assert_eq!(
            paths,
            vec![
                PathBuf::from("crd/gadgets.example.com.yaml"),
                PathBuf::from("crd/widgets.example.com.yaml"),
                PathBuf::from("deployment.apps/api.yaml"),
                PathBuf::from("service/api.yaml"),
            ]
        );
        let service = String::from_utf8(output[&PathBuf::from("service/api.yaml")].clone()).unwrap();
        assert!(service.starts_with(
            "# Nyl-Provenance: Source: applications/api.yaml (document 2)\n# Nyl-Provenance: Resource: Service\n"
        ));
        assert!(!service.contains("---"));
        let widget = String::from_utf8(output[&PathBuf::from("crd/widgets.example.com.yaml")].clone()).unwrap();
        assert!(widget.starts_with(
            "# Source: widget/crds/widgets.yaml\n# Nyl-Provenance: Source: applications/api.yaml (document 2)\n# Nyl-Provenance: Resource: CustomResourceDefinition\n"
        ));
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

    fn key(api_version: &str, kind: &str, namespace: Option<&str>, name: &str) -> crate::kubernetes::ResourceKey {
        let mut resource = serde_json::json!({"apiVersion": api_version, "kind": kind, "metadata": {"name": name}});
        if let Some(namespace) = namespace {
            resource["metadata"]["namespace"] = namespace.into();
        }
        crate::kubernetes::ResourceKey::from_json_value(&resource).unwrap()
    }

    #[test]
    fn resource_path_uses_kubectl_type_namespace_and_name() {
        let cases = [
            (
                key("v1", "ConfigMap", Some("api"), "settings"),
                "configmap/api/settings.yaml",
            ),
            (
                key("apps/v1", "Deployment", Some("api"), "web"),
                "deployment.apps/api/web.yaml",
            ),
            (key("apps/v1", "Deployment", None, "web"), "deployment.apps/web.yaml"),
            (
                key(
                    "rbac.authorization.k8s.io/v1",
                    "ClusterRole",
                    None,
                    "system:aggregate-to-admin",
                ),
                "clusterrole.rbac.authorization.k8s.io/system%3Aaggregate-to-admin.yaml",
            ),
        ];
        for (key, expected) in cases {
            assert_eq!(resource_path(&key).unwrap(), PathBuf::from(expected), "{key}");
        }
    }

    #[test]
    fn resource_path_is_independent_of_api_version() {
        assert_eq!(
            resource_path(&key("autoscaling/v1", "HorizontalPodAutoscaler", Some("api"), "web")).unwrap(),
            resource_path(&key("autoscaling/v2", "HorizontalPodAutoscaler", Some("api"), "web")).unwrap(),
        );
    }

    #[test]
    fn resource_path_never_collides_for_distinct_identities() {
        // Each pair would collide under a naive `<kind>-<namespace>-<name>` or
        // unescaped `<kind>.<group>/<namespace>/<name>` scheme.
        let keys = [
            key("v1", "ConfigMap", Some("a-b"), "c"),
            key("v1", "ConfigMap", Some("a"), "b-c"),
            key("v1", "ConfigMap", None, "a"),
            key("v1", "ConfigMap", Some("a.yaml"), "x"),
            key("v1", "ConfigMap", None, "a.yaml"),
            key("cert-manager.io/v1", "Certificate", Some("a"), "x"),
            key("example.com/v1", "Certificate", Some("a"), "x"),
            key("v1", "ConfigMap", None, "../escape"),
            key("v1", "ConfigMap", None, "%2E%2E%2Fescape"),
        ];
        let paths = keys.iter().map(|key| resource_path(key).unwrap()).collect::<Vec<_>>();
        let unique = paths.iter().collect::<std::collections::BTreeSet<_>>();
        assert_eq!(unique.len(), keys.len(), "{paths:?}");
        for path in &paths {
            assert!(path
                .components()
                .all(|component| matches!(component, std::path::Component::Normal(_))));
        }
    }

    #[test]
    fn render_manifest_layout_rejects_resources_sharing_a_file() {
        let v1 = serde_json::json!({"apiVersion": "autoscaling/v1", "kind": "HorizontalPodAutoscaler", "metadata": {"name": "web"}});
        let v2 = serde_json::json!({"apiVersion": "autoscaling/v2", "kind": "HorizontalPodAutoscaler", "metadata": {"name": "web"}});
        let error = render_manifest_layout(&[v1, v2]).unwrap_err();
        assert!(error.to_string().contains("duplicate resource"), "{error}");

        let lower = serde_json::json!({"apiVersion": "rbac.authorization.k8s.io/v1", "kind": "ClusterRole", "metadata": {"name": "admin"}});
        let upper = serde_json::json!({"apiVersion": "rbac.authorization.k8s.io/v1", "kind": "ClusterRole", "metadata": {"name": "Admin"}});
        let error = render_manifest_layout(&[lower, upper]).unwrap_err();
        assert!(error.to_string().contains("case-insensitive filesystems"), "{error}");
    }
}
