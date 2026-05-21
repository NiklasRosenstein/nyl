use serde_json::Value;

pub const HELM_HOOK_ANNOTATION: &str = "helm.sh/hook";
pub const HELM_HOOK_DELETE_POLICY_ANNOTATION: &str = "helm.sh/hook-delete-policy";
pub const HOOK_DELETE_POLICY_BEFORE_HOOK_CREATION: &str = "before-hook-creation";

pub fn is_helm_hook(manifest: &Value) -> bool {
    annotation_value(manifest, HELM_HOOK_ANNOTATION).is_some_and(|value| !value.trim().is_empty())
}

pub fn has_hook_delete_policy(manifest: &Value, policy: &str) -> bool {
    annotation_value(manifest, HELM_HOOK_DELETE_POLICY_ANNOTATION).is_some_and(|value| {
        value
            .split(',')
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
            .any(|entry| entry.eq_ignore_ascii_case(policy))
    })
}

fn annotation_value<'a>(manifest: &'a Value, key: &str) -> Option<&'a str> {
    manifest
        .get("metadata")
        .and_then(|metadata| metadata.get("annotations"))
        .and_then(|annotations| annotations.get(key))
        .and_then(Value::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_is_helm_hook_true() {
        let manifest = json!({
            "metadata": {
                "annotations": {
                    "helm.sh/hook": "pre-install,pre-upgrade"
                }
            }
        });
        assert!(is_helm_hook(&manifest));
    }

    #[test]
    fn test_is_helm_hook_false_when_missing() {
        let manifest = json!({
            "metadata": {
                "annotations": {
                    "other": "value"
                }
            }
        });
        assert!(!is_helm_hook(&manifest));
    }

    #[test]
    fn test_is_helm_hook_false_when_empty() {
        let manifest = json!({
            "metadata": {
                "annotations": {
                    "helm.sh/hook": "   "
                }
            }
        });
        assert!(!is_helm_hook(&manifest));
    }

    #[test]
    fn test_has_hook_delete_policy_matches_comma_separated_values() {
        let manifest = json!({
            "metadata": {
                "annotations": {
                    "helm.sh/hook-delete-policy": "before-hook-creation,hook-succeeded"
                }
            }
        });
        assert!(has_hook_delete_policy(&manifest, "before-hook-creation"));
        assert!(has_hook_delete_policy(&manifest, "hook-succeeded"));
        assert!(!has_hook_delete_policy(&manifest, "hook-failed"));
    }

    #[test]
    fn test_has_hook_delete_policy_is_case_insensitive_and_trimmed() {
        let manifest = json!({
            "metadata": {
                "annotations": {
                    "helm.sh/hook-delete-policy": " before-hook-creation , Hook-Succeeded "
                }
            }
        });
        assert!(has_hook_delete_policy(&manifest, "hook-succeeded"));
    }
}
