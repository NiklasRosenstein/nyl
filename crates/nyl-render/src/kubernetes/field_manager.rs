//! Field ownership of the server-side applies Nyl makes, and handing it over.
//!
//! Server-side apply removes a field only when its last owner stops applying
//! it. Fields Nyl applied as [`NYL_FIELD_MANAGER`] therefore outlive their
//! removal from the source once another manager, such as Argo CD's
//! [`ARGOCD_FIELD_MANAGER`], applies the resource. A handoff applies as the
//! receiving manager and then releases Nyl's ownership, so the fields only Nyl
//! still owned are removed and the receiver alone owns the rest.
//!
//! Contract: [Hand over to Argo CD](https://niklasrosenstein.github.io/nyl/kubernetes-gitops/workflow/handoff/).

use kube::api::DynamicObject;
use kube::ResourceExt;

use crate::{NylError, Result};

/// The field manager Nyl applies as unless it hands ownership over.
pub const NYL_FIELD_MANAGER: &str = "nyl";

/// The field manager Argo CD's server-side apply syncs as, and the default
/// receiver of a handoff.
pub const ARGOCD_FIELD_MANAGER: &str = "argocd-controller";

/// Who owns the fields of the resources a command applies.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum FieldOwnership {
    /// Apply as [`NYL_FIELD_MANAGER`].
    #[default]
    Nyl,
    /// Apply as the named manager and release what [`NYL_FIELD_MANAGER`] owns.
    HandOff(String),
}

impl FieldOwnership {
    /// The ownership a `--handoff[=MANAGER]` option selects. Handing over to
    /// Nyl itself is no handoff.
    pub fn from_handoff(manager: Option<&str>) -> Result<Self> {
        match manager.map(str::trim) {
            None | Some(NYL_FIELD_MANAGER) => Ok(Self::Nyl),
            Some("") => Err(NylError::Config(
                "--handoff names an empty field manager; omit the value to hand over to argocd-controller".to_string(),
            )),
            Some(manager) => Ok(Self::HandOff(manager.to_string())),
        }
    }

    /// The field manager resources are applied as.
    pub fn manager(&self) -> &str {
        match self {
            Self::Nyl => NYL_FIELD_MANAGER,
            Self::HandOff(manager) => manager,
        }
    }

    /// The manager whose applied fields are released after each apply.
    pub fn released_manager(&self) -> Option<&str> {
        match self {
            Self::Nyl => None,
            Self::HandOff(_) => Some(NYL_FIELD_MANAGER),
        }
    }
}

/// Whether `manager` owns fields of `object` through server-side apply.
///
/// Only `Apply` entries count: fields a manager set with an update (client-side
/// apply, `kubectl edit`) are not released by applying an empty configuration.
pub fn has_applied_fields(object: &DynamicObject, manager: &str) -> bool {
    object
        .metadata
        .managed_fields
        .iter()
        .flatten()
        .any(|entry| entry.manager.as_deref() == Some(manager) && entry.operation.as_deref() == Some("Apply"))
}

/// The configuration that, applied as a manager, releases every field it owns.
///
/// It names only the object, so the manager stops applying everything else:
/// fields another manager also owns stay, and fields it alone owned are removed.
pub fn ownership_release_body(resource: &DynamicObject) -> DynamicObject {
    let mut body = DynamicObject {
        types: resource.types.clone(),
        metadata: kube::api::ObjectMeta::default(),
        data: serde_json::Value::Object(serde_json::Map::default()),
    };
    body.metadata.name = Some(resource.name_any());
    body.metadata.namespace = resource.namespace();
    body
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn object(managed_fields: serde_json::Value) -> DynamicObject {
        serde_json::from_value(json!({
            "apiVersion": "apps/v1",
            "kind": "Deployment",
            "metadata": {
                "name": "api",
                "namespace": "prod",
                "labels": {"app": "api"},
                "managedFields": managed_fields,
            },
            "spec": {"replicas": 2},
        }))
        .unwrap()
    }

    #[test]
    fn test_from_handoff_selects_manager_and_release() {
        assert_eq!(FieldOwnership::from_handoff(None).unwrap(), FieldOwnership::Nyl);
        assert_eq!(FieldOwnership::from_handoff(Some("nyl")).unwrap(), FieldOwnership::Nyl);
        let handoff = FieldOwnership::from_handoff(Some(ARGOCD_FIELD_MANAGER)).unwrap();
        assert_eq!(handoff.manager(), ARGOCD_FIELD_MANAGER);
        assert_eq!(handoff.released_manager(), Some(NYL_FIELD_MANAGER));
        assert_eq!(FieldOwnership::Nyl.released_manager(), None);
        assert!(FieldOwnership::from_handoff(Some(" ")).is_err());
    }

    #[test]
    fn test_has_applied_fields_counts_only_apply_entries_of_the_manager() {
        let live = object(json!([
            {"manager": "nyl", "operation": "Update"},
            {"manager": "argocd-controller", "operation": "Apply"},
        ]));
        assert!(!has_applied_fields(&live, NYL_FIELD_MANAGER));
        assert!(has_applied_fields(&live, ARGOCD_FIELD_MANAGER));
        assert!(!has_applied_fields(&object(json!(null)), NYL_FIELD_MANAGER));
    }

    #[test]
    fn test_ownership_release_body_names_only_the_object() {
        let body = ownership_release_body(&object(json!([{"manager": "nyl", "operation": "Apply"}])));
        assert_eq!(
            serde_json::to_value(&body).unwrap(),
            json!({
                "apiVersion": "apps/v1",
                "kind": "Deployment",
                "metadata": {"name": "api", "namespace": "prod"},
            })
        );
    }
}
