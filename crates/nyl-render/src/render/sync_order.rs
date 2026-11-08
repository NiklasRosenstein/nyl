//! Argo CD sync order inherited by the resources an expanding resource
//! generates.
//!
//! A HelmChart, Component, or RemoteManifest is never applied itself; its
//! generated resources are. Argo CD orders those resources by their own
//! annotations, so the sync wave and phase declared on the expanding resource
//! are copied to every generated resource that does not declare its own. See
//! [Argo CD sync waves and phases](https://niklasrosenstein.github.io/nyl/kubernetes-rendering/pipeline/#argo-cd-sync-waves-and-phases).
//!
//! - `argocd.argoproj.io/sync-wave` is copied to a resource without its own
//!   sync wave or a `helm.sh/hook-weight`, which Argo CD uses as the wave of a
//!   Helm hook.
//! - `argocd.argoproj.io/hook` is the phase. It is copied to a resource
//!   without its own Argo CD hook or a `helm.sh/hook`, together with the
//!   parent's `argocd.argoproj.io/hook-delete-policy` when one is declared, so
//!   a resource never mixes its own phase with another resource's deletion policy.
//!
//! Nested expansion inherits transitively: a generated HelmChart passes on
//! what it inherited.

use serde_json::{Map, Value};

const SYNC_WAVE: &str = "argocd.argoproj.io/sync-wave";
const HOOK: &str = "argocd.argoproj.io/hook";
const HOOK_DELETE_POLICY: &str = "argocd.argoproj.io/hook-delete-policy";
const HELM_HOOK: &str = "helm.sh/hook";
const HELM_HOOK_WEIGHT: &str = "helm.sh/hook-weight";

/// Copies the parent's sync wave and phase onto `children` that declare neither.
pub(crate) fn inherit_sync_order(parent: &Value, children: &mut [Value]) {
    let Some(annotations) = parent.pointer("/metadata/annotations").and_then(Value::as_object) else {
        return;
    };
    let sync_wave = annotations.get(SYNC_WAVE);
    let hook = annotations.get(HOOK);
    if sync_wave.is_none() && hook.is_none() {
        return;
    }
    let hook_delete_policy = annotations.get(HOOK_DELETE_POLICY);
    for child in children.iter_mut() {
        let Some(child_annotations) = child_annotations(child) else {
            continue;
        };
        if let Some(sync_wave) = sync_wave {
            if !child_annotations.contains_key(SYNC_WAVE) && !child_annotations.contains_key(HELM_HOOK_WEIGHT) {
                child_annotations.insert(SYNC_WAVE.to_string(), sync_wave.clone());
            }
        }
        if let Some(hook) = hook {
            if !child_annotations.contains_key(HOOK) && !child_annotations.contains_key(HELM_HOOK) {
                child_annotations.insert(HOOK.to_string(), hook.clone());
                match hook_delete_policy {
                    Some(policy) => child_annotations.insert(HOOK_DELETE_POLICY.to_string(), policy.clone()),
                    // The policy belongs to the child's own phase, which it no longer has.
                    None => child_annotations.remove(HOOK_DELETE_POLICY),
                };
            }
        }
    }
}

/// The child's annotations, created when absent. `None` for a document that is
/// not an object with object metadata, which inherits nothing.
fn child_annotations(child: &mut Value) -> Option<&mut Map<String, Value>> {
    let metadata = child.as_object_mut()?.get_mut("metadata")?.as_object_mut()?;
    metadata
        .entry("annotations")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn chart(annotations: Value) -> Value {
        json!({
            "apiVersion": "k8s.nyl/v1",
            "kind": "HelmChart",
            "metadata": {"name": "app", "annotations": annotations},
        })
    }

    fn config_map(annotations: Option<Value>) -> Value {
        let mut value = json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "config"}});
        if let Some(annotations) = annotations {
            value["metadata"]["annotations"] = annotations;
        }
        value
    }

    #[test]
    fn test_inherit_sync_order_copies_wave_and_phase_to_children_without_their_own() {
        let parent = chart(json!({
            SYNC_WAVE: "-1",
            HOOK: "PreSync",
            HOOK_DELETE_POLICY: "HookSucceeded",
            "example.com/other": "kept on the parent",
        }));
        let mut children = vec![config_map(None), config_map(Some(json!({"team": "web"})))];

        inherit_sync_order(&parent, &mut children);

        for child in &children {
            assert_eq!(child["metadata"]["annotations"][SYNC_WAVE], "-1");
            assert_eq!(child["metadata"]["annotations"][HOOK], "PreSync");
            assert_eq!(child["metadata"]["annotations"][HOOK_DELETE_POLICY], "HookSucceeded");
            assert!(child["metadata"]["annotations"].get("example.com/other").is_none());
        }
        assert_eq!(children[1]["metadata"]["annotations"]["team"], "web");
    }

    #[test]
    fn test_inherit_sync_order_keeps_a_childs_own_wave_and_phase() {
        let parent = chart(json!({SYNC_WAVE: "5", HOOK: "PostSync", HOOK_DELETE_POLICY: "HookSucceeded"}));
        let mut children = vec![
            config_map(Some(json!({SYNC_WAVE: "1", HOOK: "Sync"}))),
            // Argo CD reads Helm hooks and their weights itself.
            config_map(Some(json!({HELM_HOOK: "pre-install", HELM_HOOK_WEIGHT: "2"}))),
        ];

        inherit_sync_order(&parent, &mut children);

        assert_eq!(
            children[0]["metadata"]["annotations"],
            json!({SYNC_WAVE: "1", HOOK: "Sync"})
        );
        assert_eq!(
            children[1]["metadata"]["annotations"],
            json!({HELM_HOOK: "pre-install", HELM_HOOK_WEIGHT: "2"})
        );
    }

    #[test]
    fn test_inherit_sync_order_inherits_wave_and_phase_independently() {
        let parent = chart(json!({SYNC_WAVE: "3", HOOK: "PreSync"}));
        let mut children = vec![
            config_map(Some(json!({HOOK: "Sync", HOOK_DELETE_POLICY: "BeforeHookCreation"}))),
            config_map(Some(json!({SYNC_WAVE: "0", HOOK_DELETE_POLICY: "HookFailed"}))),
        ];

        inherit_sync_order(&parent, &mut children);

        assert_eq!(
            children[0]["metadata"]["annotations"],
            json!({SYNC_WAVE: "3", HOOK: "Sync", HOOK_DELETE_POLICY: "BeforeHookCreation"})
        );
        // An inherited phase never carries a policy the parent did not declare.
        assert_eq!(
            children[1]["metadata"]["annotations"],
            json!({SYNC_WAVE: "0", HOOK: "PreSync"})
        );
    }

    #[test]
    fn test_inherit_sync_order_leaves_children_unchanged_without_parent_annotations() {
        let parent = chart(json!({"example.com/other": "value"}));
        let mut children = vec![config_map(None), json!("not a manifest")];

        inherit_sync_order(&parent, &mut children);

        assert_eq!(children, [config_map(None), json!("not a manifest")]);
    }
}
