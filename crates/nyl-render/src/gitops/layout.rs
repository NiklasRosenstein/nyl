//! Deterministic on-disk layout for rendered Kubernetes manifests.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use serde_json::{Map, Value};

use crate::resources::{ManagedNamespacePolicy, ManagedResourceDeletionPolicy};
use crate::{NylError, Result};

/// The directory beneath a target prefix that holds the generated Argo CD
/// Applications and AppProjects, laid out like any rendered Application.
pub const CATALOG_DIRECTORY: &str = "_nyl/catalog";

const ARGOCD_SYNC_OPTIONS_ANNOTATION: &str = "argocd.argoproj.io/sync-options";
const CRD_GROUP: &str = "apiextensions.k8s.io";
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
        let path = if is_crd(&key) {
            PathBuf::from("crd").join(name_file(&key)?)
        } else {
            resource_path(&key)?
        };

        let folded = path.to_string_lossy().to_lowercase();
        if let Some(previous) = folded_paths.insert(folded, key.clone()) {
            let same_object = previous.gvk.group == key.gvk.group
                && previous.gvk.kind == key.gvk.kind
                && previous.namespace.as_deref().unwrap_or_default() == key.namespace.as_deref().unwrap_or_default()
                && previous.name == key.name;
            let message = if same_object && is_crd(&key) {
                format!(
                    "Rendered resources contain duplicate CustomResourceDefinition {:?}",
                    key.name
                )
            } else if same_object {
                format!("Rendered resources contain duplicate resource {key}")
            } else if previous.gvk.kind != key.gvk.kind && previous.gvk.kind.eq_ignore_ascii_case(&key.gvk.kind) {
                format!(
                    "Rendered resources {previous} and {key} map to the same file {} because their kinds differ only in case",
                    path.display()
                )
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
/// percent-encoded, `%` and `_` included. The namespace safe set excludes `.`,
/// so a namespace directory never equals a `<name>.yaml` file beside it. Each
/// segment is then made portable by [`portable_segment`].
pub fn resource_path(key: &crate::kubernetes::ResourceKey) -> Result<PathBuf> {
    let kind = utf8_percent_encode(&key.gvk.kind.to_lowercase(), NON_ALPHANUMERIC).to_string();
    let resource_type = if key.gvk.group.is_empty() {
        kind
    } else {
        format!("{kind}.{}", utf8_percent_encode(&key.gvk.group, GROUP_OR_NAME))
    };

    let mut path = PathBuf::from(portable_segment(resource_type, ""));
    if let Some(namespace) = key.namespace.as_deref().filter(|namespace| !namespace.is_empty()) {
        path.push(portable_segment(
            utf8_percent_encode(namespace, NAMESPACE).to_string(),
            "",
        ));
    }
    path.push(name_file(key)?);
    Ok(path)
}

/// Bytes kept verbatim in an API group or object name segment.
const GROUP_OR_NAME: &AsciiSet = &NON_ALPHANUMERIC.remove(b'.').remove(b'-');
/// Bytes kept verbatim in a namespace segment; `.` is encoded, see [`resource_path`].
const NAMESPACE: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-');

/// The common file name length limit of Linux, macOS, and Windows filesystems.
const MAX_PATH_SEGMENT_BYTES: usize = 255;
/// Hex digits of the name digest appended to a shortened segment.
const SHORTENED_DIGEST_LENGTH: usize = 16;

/// Device names that Windows reserves regardless of case or extension.
const WINDOWS_RESERVED_NAMES: &[&str] = &[
    "con", "prn", "aux", "nul", "com0", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8", "com9", "lpt0",
    "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

fn is_crd(key: &crate::kubernetes::ResourceKey) -> bool {
    key.gvk.group == CRD_GROUP && key.gvk.kind == CRD_KIND
}

/// Return `<name>.yaml` for the object's encoded, portable name.
fn name_file(key: &crate::kubernetes::ResourceKey) -> Result<String> {
    if key.name.is_empty() {
        return Err(NylError::config(format!(
            "Rendered {} has an empty metadata.name",
            key.gvk.kind
        )));
    }
    Ok(portable_segment(
        utf8_percent_encode(&key.name, GROUP_OR_NAME).to_string(),
        ".yaml",
    ))
}

/// Make an encoded segment portable to Windows and to file name length limits.
///
/// A Windows device name has its first byte percent-encoded, and a trailing `.`
/// that Windows would strip is encoded. A segment longer than
/// [`MAX_PATH_SEGMENT_BYTES`] keeps a prefix of `stem` followed by `_` and a
/// digest of the whole stem; encoded stems never contain `_`, so a shortened
/// segment cannot equal an unshortened one.
fn portable_segment(mut stem: String, suffix: &str) -> String {
    let device = stem.split('.').next().unwrap_or_default();
    if WINDOWS_RESERVED_NAMES
        .iter()
        .any(|reserved| reserved.eq_ignore_ascii_case(device))
    {
        stem = format!("%{:02X}{}", stem.as_bytes()[0], &stem[1..]);
    }
    if suffix.is_empty() && stem.ends_with('.') {
        stem.pop();
        stem.push_str("%2E");
    }
    if stem.len() + suffix.len() <= MAX_PATH_SEGMENT_BYTES {
        return format!("{stem}{suffix}");
    }

    let digest = nyl_core::digest::sha256_hex(stem.as_bytes());
    let mut keep = MAX_PATH_SEGMENT_BYTES - suffix.len() - 1 - SHORTENED_DIGEST_LENGTH;
    // Never cut through a `%XX` escape.
    if let Some(escape) = stem[keep.saturating_sub(2)..keep].find('%') {
        keep = keep - 2 + escape;
    }
    format!("{}_{}{suffix}", &stem[..keep], &digest[..SHORTENED_DIGEST_LENGTH])
}

fn is_namespace_named(resource: &Value, namespace: &str) -> bool {
    resource.get("apiVersion").and_then(Value::as_str) == Some("v1")
        && resource.get("kind").and_then(Value::as_str) == Some("Namespace")
        && resource.pointer("/metadata/name").and_then(Value::as_str) == Some(namespace)
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
                "apiVersion": "apiextensions.k8s.io/v1",
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
                "apiVersion": "apiextensions.k8s.io/v1",
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
    fn render_manifest_layout_rejects_the_same_crd_at_two_versions() {
        let v1 = serde_json::json!({
            "apiVersion": "apiextensions.k8s.io/v1",
            "kind": CRD_KIND,
            "metadata": {"name": "widgets.example.com"}
        });
        let v1beta1 = serde_json::json!({
            "apiVersion": "apiextensions.k8s.io/v1beta1",
            "kind": CRD_KIND,
            "metadata": {"name": "widgets.example.com"}
        });
        let output = render_manifest_layout(std::slice::from_ref(&v1beta1)).unwrap();
        assert!(output.contains_key(&PathBuf::from("crd/widgets.example.com.yaml")));

        let error = render_manifest_layout(&[v1beta1, v1]).unwrap_err();
        assert!(
            error.to_string().contains("duplicate CustomResourceDefinition"),
            "{error}"
        );
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

        let widget = serde_json::json!({"apiVersion": "example.com/v1", "kind": "Widget", "metadata": {"name": "x"}});
        let shouting = serde_json::json!({"apiVersion": "example.com/v1", "kind": "WIDGET", "metadata": {"name": "x"}});
        let error = render_manifest_layout(&[widget, shouting]).unwrap_err();
        assert!(error.to_string().contains("kinds differ only in case"), "{error}");
    }

    #[test]
    fn resource_path_escapes_windows_device_names() {
        assert_eq!(
            resource_path(&key("v1", "ConfigMap", Some("aux"), "con")).unwrap(),
            PathBuf::from("configmap/%61ux/%63on.yaml")
        );
        assert_eq!(
            resource_path(&key("v1", "ConfigMap", None, "NUL.backup")).unwrap(),
            PathBuf::from("configmap/%4EUL.backup.yaml")
        );
        assert_eq!(
            resource_path(&key("v1", "ConfigMap", None, "console")).unwrap(),
            PathBuf::from("configmap/console.yaml")
        );
    }

    #[test]
    fn resource_path_shortens_only_segments_over_the_file_name_limit() {
        let longest = "a".repeat(250);
        assert_eq!(
            resource_path(&key("v1", "ConfigMap", None, &longest)).unwrap(),
            PathBuf::from(format!("configmap/{longest}.yaml"))
        );

        let long = "a".repeat(253);
        let longer = format!("{}b", "a".repeat(252));
        let shortened = [&long, &longer].map(|name| {
            let path = resource_path(&key("v1", "ConfigMap", None, name)).unwrap();
            path.file_name().unwrap().to_str().unwrap().to_owned()
        });
        assert_ne!(shortened[0], shortened[1]);
        for file in &shortened {
            assert_eq!(file.len(), MAX_PATH_SEGMENT_BYTES);
            assert!(file.starts_with(&"a".repeat(233)));
            assert_eq!(&file[file.len() - ".yaml".len()..], ".yaml");
        }

        // Shortening never cuts through a percent escape.
        let colons = ":".repeat(100);
        let file = resource_path(&key("rbac.authorization.k8s.io/v1", "ClusterRole", None, &colons)).unwrap();
        let file = file.file_name().unwrap().to_str().unwrap();
        assert!(file.len() <= MAX_PATH_SEGMENT_BYTES);
        let (prefix, _) = file.rsplit_once('_').unwrap();
        assert_eq!(prefix.len() % 3, 0, "{file}");
        assert!(prefix
            .chars()
            .collect::<Vec<_>>()
            .chunks(3)
            .all(|escape| escape == ['%', '3', 'A']));
    }
}
