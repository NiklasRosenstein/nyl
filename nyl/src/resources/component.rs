/// Component resource definition
///
/// A Component is a lightweight wrapper that references a Helm chart in the
/// configured `components/` directory.  The `kind` field encodes the relative
/// path to the chart directory (e.g. `myapiversion/v1/MyComponent`), and the
/// `spec` is forwarded directly as Helm values.
use serde::{Deserialize, Serialize};

use crate::constants::API_VERSION_COMPONENTS;
use crate::resources::{ChartRef, ObjectMetadata};

fn default_spec() -> serde_json::Value {
    serde_json::Value::Object(serde_json::Map::new())
}

/// Component resource
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NylComponent {
    #[serde(rename = "apiVersion")]
    pub api_version: String,

    /// Relative path under `components/` that identifies the Helm chart
    pub kind: String,

    pub metadata: ObjectMetadata,

    /// Helm values — defaults to an empty object when omitted
    #[serde(default = "default_spec")]
    pub spec: serde_json::Value,
}

/// Return `true` when the manifest's `apiVersion` matches the Component API version.
///
/// The `kind` field is intentionally NOT checked here because it is dynamic —
/// it encodes a filesystem path rather than a fixed resource type.
pub fn is_nyl_component(manifest: &serde_json::Value) -> bool {
    manifest.get("apiVersion").and_then(|v| v.as_str()) == Some(API_VERSION_COMPONENTS)
}

/// Parsed result from parsing a component kind shortcut
#[derive(Debug, Clone, PartialEq)]
pub struct ComponentKindParsed {
    /// The base part (repository URL or local path)
    pub base: String,
    /// The chart name (after '#'), if present
    pub name: Option<String>,
    /// The version (after '@'), if present
    pub version: Option<String>,
}

/// Check if a string is a remote repository URL
///
/// Remote repositories are identified by these prefixes:
/// - `http://` or `https://` - Traditional Helm repositories or HTTP URLs
/// - `git+` - Git repositories
/// - `oci://` - OCI registries
fn is_remote_repository(s: &str) -> bool {
    s.starts_with("http://")
        || s.starts_with("https://")
        || s.starts_with("git+")
        || s.starts_with("oci://")
}

/// Parse a component kind string into its components
///
/// The format is: `<base>[#<name>][@<version>]`
/// - `base`: Repository URL (if remote) or local path/name
/// - `name`: Chart name (optional, after '#')
/// - `version`: Version or Git ref (optional, after '@')
///
/// Examples:
/// - `http://my-repo.org#my-chart@1.0.0` -> repository URL with name and version
/// - `https://charts.example.com#nginx` -> repository URL with name only
/// - `oci://ghcr.io/owner/chart@v1.0.0` -> OCI registry with version only
/// - `my-chart` -> local path/name
/// - `path/to/chart` -> local path
pub fn parse_component_kind(kind: &str) -> ComponentKindParsed {
    // First, find the '@' for version (rightmost '@')
    let (base_and_name, version) = if let Some(at_pos) = kind.rfind('@') {
        let version_part = &kind[at_pos + 1..];
        (kind[..at_pos].to_string(), Some(version_part.to_string()))
    } else {
        (kind.to_string(), None)
    };

    // Then, find the '#' for name (rightmost '#' in the remaining part)
    let (base, name) = if let Some(hash_pos) = base_and_name.rfind('#') {
        let name_part = &base_and_name[hash_pos + 1..];
        (base_and_name[..hash_pos].to_string(), Some(name_part.to_string()))
    } else {
        (base_and_name, None)
    };

    ComponentKindParsed { base, name, version }
}

/// Check if a component kind uses the shortcut format for remote Helm charts
///
/// Returns `true` if the kind appears to be a remote repository URL rather than
/// a local component path.
pub fn is_helm_chart_shortcut(kind: &str) -> bool {
    // Extract the base part (before '#' and '@')
    let parsed = parse_component_kind(kind);
    is_remote_repository(&parsed.base)
}

/// Convert a parsed component kind to a ChartRef
///
/// This maps the shortcut format to the HelmChart's ChartRef structure:
/// - For remote repositories: Sets repository, name, and version fields
/// - For local paths: Sets only the name field (as local path)
pub fn component_kind_to_chart_ref(parsed: &ComponentKindParsed) -> ChartRef {
    if is_remote_repository(&parsed.base) {
        // Remote repository: use repository field
        ChartRef {
            repository: Some(parsed.base.clone()),
            name: parsed.name.clone(),
            version: parsed.version.clone(),
        }
    } else {
        // Local path: use name field only
        ChartRef {
            repository: None,
            name: Some(parsed.base.clone()),
            version: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // --- detection --------------------------------------------------------

    #[test]
    fn test_is_nyl_component_positive() {
        let manifest = json!({
            "apiVersion": "components.nyl.niklasrosenstein.github.com/v1",
            "kind": "example/v1/Nginx",
            "metadata": { "name": "my-nginx", "namespace": "default" },
            "spec": { "replicas": 3 }
        });
        assert!(is_nyl_component(&manifest));
    }

    #[test]
    fn test_is_nyl_component_negative_wrong_api_version() {
        let manifest = json!({
            "apiVersion": "nyl.niklasrosenstein.github.com/v1",
            "kind": "example/v1/Nginx",
            "metadata": { "name": "my-nginx" }
        });
        assert!(!is_nyl_component(&manifest));
    }

    #[test]
    fn test_is_nyl_component_negative_missing_api_version() {
        let manifest = json!({
            "kind": "example/v1/Nginx",
            "metadata": { "name": "my-nginx" }
        });
        assert!(!is_nyl_component(&manifest));
    }

    // --- deserialization / round-trip --------------------------------------

    #[test]
    fn test_deserialize_full() {
        let manifest = json!({
            "apiVersion": "components.nyl.niklasrosenstein.github.com/v1",
            "kind": "example/v1/Nginx",
            "metadata": { "name": "my-nginx", "namespace": "default" },
            "spec": { "replicas": 3, "image": "nginx:latest" }
        });

        let component: NylComponent = serde_json::from_value(manifest).unwrap();
        assert_eq!(component.api_version, "components.nyl.niklasrosenstein.github.com/v1");
        assert_eq!(component.kind, "example/v1/Nginx");
        assert_eq!(component.metadata.name, "my-nginx");
        assert_eq!(component.metadata.namespace, Some("default".to_string()));
        assert_eq!(component.spec["replicas"], 3);
        assert_eq!(component.spec["image"], "nginx:latest");
    }

    #[test]
    fn test_round_trip() {
        let manifest = json!({
            "apiVersion": "components.nyl.niklasrosenstein.github.com/v1",
            "kind": "libs/v2/Redis",
            "metadata": { "name": "my-redis", "namespace": "infra" },
            "spec": { "port": 6379 }
        });

        let component: NylComponent = serde_json::from_value(manifest).unwrap();
        let serialized = serde_json::to_value(&component).unwrap();
        let round_tripped: NylComponent = serde_json::from_value(serialized).unwrap();

        assert_eq!(round_tripped.kind, "libs/v2/Redis");
        assert_eq!(round_tripped.metadata.name, "my-redis");
        assert_eq!(round_tripped.spec["port"], 6379);
    }

    // --- spec defaulting --------------------------------------------------

    #[test]
    fn test_spec_defaults_to_empty_object_when_omitted() {
        let manifest = json!({
            "apiVersion": "components.nyl.niklasrosenstein.github.com/v1",
            "kind": "example/v1/Nginx",
            "metadata": { "name": "no-spec" }
        });

        let component: NylComponent = serde_json::from_value(manifest).unwrap();
        assert!(component.spec.is_object());
        assert!(component.spec.as_object().unwrap().is_empty());
    }

    // --- shortcut parsing tests -------------------------------------------

    #[test]
    fn test_is_remote_repository() {
        assert!(is_remote_repository("http://example.com"));
        assert!(is_remote_repository("https://example.com"));
        assert!(is_remote_repository("git+https://github.com/user/repo"));
        assert!(is_remote_repository("oci://ghcr.io/owner/chart"));

        assert!(!is_remote_repository("my-chart"));
        assert!(!is_remote_repository("path/to/chart"));
        assert!(!is_remote_repository("./charts/app"));
    }

    #[test]
    fn test_parse_component_kind_full_format() {
        let parsed = parse_component_kind("http://my-repo.org#my-chart@1.0.0");
        assert_eq!(parsed.base, "http://my-repo.org");
        assert_eq!(parsed.name, Some("my-chart".to_string()));
        assert_eq!(parsed.version, Some("1.0.0".to_string()));
    }

    #[test]
    fn test_parse_component_kind_no_version() {
        let parsed = parse_component_kind("https://charts.example.com#nginx");
        assert_eq!(parsed.base, "https://charts.example.com");
        assert_eq!(parsed.name, Some("nginx".to_string()));
        assert_eq!(parsed.version, None);
    }

    #[test]
    fn test_parse_component_kind_no_name() {
        let parsed = parse_component_kind("oci://ghcr.io/owner/chart@v1.0.0");
        assert_eq!(parsed.base, "oci://ghcr.io/owner/chart");
        assert_eq!(parsed.name, None);
        assert_eq!(parsed.version, Some("v1.0.0".to_string()));
    }

    #[test]
    fn test_parse_component_kind_only_base() {
        let parsed = parse_component_kind("http://my-chart-repo.org");
        assert_eq!(parsed.base, "http://my-chart-repo.org");
        assert_eq!(parsed.name, None);
        assert_eq!(parsed.version, None);
    }

    #[test]
    fn test_parse_component_kind_local_path() {
        let parsed = parse_component_kind("my-chart");
        assert_eq!(parsed.base, "my-chart");
        assert_eq!(parsed.name, None);
        assert_eq!(parsed.version, None);
    }

    #[test]
    fn test_parse_component_kind_local_path_with_slashes() {
        let parsed = parse_component_kind("path/to/my-chart");
        assert_eq!(parsed.base, "path/to/my-chart");
        assert_eq!(parsed.name, None);
        assert_eq!(parsed.version, None);
    }

    #[test]
    fn test_parse_component_kind_git_format() {
        let parsed = parse_component_kind("git+https://github.com/user/repo#charts/app@main");
        assert_eq!(parsed.base, "git+https://github.com/user/repo");
        assert_eq!(parsed.name, Some("charts/app".to_string()));
        assert_eq!(parsed.version, Some("main".to_string()));
    }

    #[test]
    fn test_is_helm_chart_shortcut_remote() {
        assert!(is_helm_chart_shortcut("http://my-repo.org#chart@1.0.0"));
        assert!(is_helm_chart_shortcut("https://charts.example.com#nginx"));
        assert!(is_helm_chart_shortcut("oci://ghcr.io/chart@v1.0.0"));
        assert!(is_helm_chart_shortcut("git+https://github.com/user/repo"));
    }

    #[test]
    fn test_is_helm_chart_shortcut_local() {
        assert!(!is_helm_chart_shortcut("my-chart"));
        assert!(!is_helm_chart_shortcut("path/to/chart"));
        assert!(!is_helm_chart_shortcut("example/v1/Nginx"));
    }

    #[test]
    fn test_component_kind_to_chart_ref_remote_full() {
        let parsed = parse_component_kind("http://my-repo.org#my-chart@1.0.0");
        let chart_ref = component_kind_to_chart_ref(&parsed);

        assert_eq!(chart_ref.repository, Some("http://my-repo.org".to_string()));
        assert_eq!(chart_ref.name, Some("my-chart".to_string()));
        assert_eq!(chart_ref.version, Some("1.0.0".to_string()));
    }

    #[test]
    fn test_component_kind_to_chart_ref_remote_no_version() {
        let parsed = parse_component_kind("https://charts.example.com#nginx");
        let chart_ref = component_kind_to_chart_ref(&parsed);

        assert_eq!(chart_ref.repository, Some("https://charts.example.com".to_string()));
        assert_eq!(chart_ref.name, Some("nginx".to_string()));
        assert_eq!(chart_ref.version, None);
    }

    #[test]
    fn test_component_kind_to_chart_ref_local() {
        let parsed = parse_component_kind("my-chart");
        let chart_ref = component_kind_to_chart_ref(&parsed);

        assert_eq!(chart_ref.repository, None);
        assert_eq!(chart_ref.name, Some("my-chart".to_string()));
        assert_eq!(chart_ref.version, None);
    }

    #[test]
    fn test_component_kind_to_chart_ref_local_path() {
        let parsed = parse_component_kind("path/to/my-chart");
        let chart_ref = component_kind_to_chart_ref(&parsed);

        assert_eq!(chart_ref.repository, None);
        assert_eq!(chart_ref.name, Some("path/to/my-chart".to_string()));
        assert_eq!(chart_ref.version, None);
    }
}
