/// Profile management module
///
/// A profile holds the resolved configuration used to render manifests and
/// connect to a cluster:
/// - Template values exposed during rendering
/// - The local kubeconfig path and target context for cluster access
///
/// Profiles are built in-memory from `nyl.toml` (see `crate::config`); they are
/// not loaded from a standalone file.
use std::collections::HashMap;
use std::path::PathBuf;

/// A profile defines environment-specific configuration.
#[derive(Debug, Clone, Default)]
pub struct Profile {
    /// Template values for this profile.
    pub values: HashMap<String, serde_json::Value>,

    /// Path to the kubeconfig file (defaults to the standard location when None).
    pub kubeconfig_path: Option<PathBuf>,

    /// Kubernetes context to connect to (uses the current context when None).
    pub context: Option<String>,
}

impl Profile {
    /// Create a new empty profile.
    pub fn new() -> Self {
        Self::default()
    }
}

/// Deep merge two JSON values (overlay wins over base)
///
/// Merge behavior:
/// - If base doesn't exist, use overlay
/// - Both objects: recursively merge keys
/// - Arrays or scalars: overlay replaces base
pub fn deep_merge_value(base: Option<serde_json::Value>, overlay: serde_json::Value) -> serde_json::Value {
    match (base, overlay) {
        (None, overlay) => overlay,

        // Both objects - recursive merge
        (Some(serde_json::Value::Object(mut base_map)), serde_json::Value::Object(overlay_map)) => {
            for (key, overlay_value) in overlay_map {
                let base_value = base_map.remove(&key);
                base_map.insert(key, deep_merge_value(base_value, overlay_value));
            }
            serde_json::Value::Object(base_map)
        }

        // Arrays or scalars - overlay replaces
        (Some(_base), overlay) => overlay,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_profile_default() {
        let profile = Profile::default();
        assert!(profile.values.is_empty());
        assert!(profile.kubeconfig_path.is_none());
        assert!(profile.context.is_none());
    }

    #[test]
    fn test_deep_merge_value_none_base() {
        let overlay = serde_json::json!({"key": "value"});
        let result = deep_merge_value(None, overlay);
        assert_eq!(result, serde_json::json!({"key": "value"}));
    }

    #[test]
    fn test_deep_merge_value_scalar_replace() {
        let base = Some(serde_json::json!("old"));
        let overlay = serde_json::json!("new");
        let result = deep_merge_value(base, overlay);
        assert_eq!(result, serde_json::json!("new"));
    }

    #[test]
    fn test_deep_merge_value_array_replace() {
        let base = Some(serde_json::json!([1, 2, 3]));
        let overlay = serde_json::json!([4, 5]);
        let result = deep_merge_value(base, overlay);
        assert_eq!(result, serde_json::json!([4, 5]));
    }

    #[test]
    fn test_deep_merge_value_object_merge() {
        let base = Some(serde_json::json!({
            "a": 1,
            "b": 2,
            "nested": {
                "x": 10,
                "y": 20
            }
        }));
        let overlay = serde_json::json!({
            "b": 99,
            "c": 3,
            "nested": {
                "y": 99,
                "z": 30
            }
        });
        let result = deep_merge_value(base, overlay);
        assert_eq!(
            result,
            serde_json::json!({
                "a": 1,
                "b": 99,
                "c": 3,
                "nested": {
                    "x": 10,
                    "y": 99,
                    "z": 30
                }
            })
        );
    }

    #[test]
    fn test_deep_merge_value_deeply_nested() {
        let base = Some(serde_json::json!({
            "level1": {
                "level2": {
                    "level3": {
                        "a": 1,
                        "b": 2
                    }
                }
            }
        }));
        let overlay = serde_json::json!({
            "level1": {
                "level2": {
                    "level3": {
                        "b": 99,
                        "c": 3
                    }
                }
            }
        });
        let result = deep_merge_value(base, overlay);
        assert_eq!(
            result,
            serde_json::json!({
                "level1": {
                    "level2": {
                        "level3": {
                            "a": 1,
                            "b": 99,
                            "c": 3
                        }
                    }
                }
            })
        );
    }

    #[test]
    fn test_deep_merge_value_type_conflict() {
        // When types differ, overlay wins
        let base = Some(serde_json::json!({"key": "string"}));
        let overlay = serde_json::json!({"key": 123});
        let result = deep_merge_value(base, overlay);
        assert_eq!(result, serde_json::json!({"key": 123}));
    }
}
