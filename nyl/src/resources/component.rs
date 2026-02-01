/// Component resource definition
///
/// A Component is a lightweight wrapper that references a Helm chart in the
/// configured `components/` directory.  The `kind` field encodes the relative
/// path to the chart directory (e.g. `myapiversion/v1/MyComponent`), and the
/// `spec` is forwarded directly as Helm values.
use serde::{Deserialize, Serialize};

use crate::constants::API_VERSION_COMPONENTS;
use crate::resources::ObjectMetadata;

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
}
