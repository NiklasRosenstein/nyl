//! Builders for Argo CD resources that consume rendered manifest directories.

use std::collections::BTreeMap;

use serde_json::{json, Value};

use crate::resources::{ApplicationDeletionPolicy, ClusterDestination, GitOpsSyncPolicy};
use crate::{NylError, Result};

const FOREGROUND_FINALIZER: &str = "resources-finalizer.argocd.argoproj.io";
const BACKGROUND_FINALIZER: &str = "resources-finalizer.argocd.argoproj.io/background";
const APPLY_OUT_OF_SYNC_ONLY_DEFAULT: &str = "ApplyOutOfSyncOnly=true";

/// Validated inputs for an Argo CD Application backed by a plain Git directory.
pub struct DirectoryApplicationInput {
    pub name: String,
    pub application_namespace: String,
    pub project: String,
    pub repo_url: String,
    pub revision: String,
    pub rendered_path: String,
    pub destination: ClusterDestination,
    pub destination_namespace: String,
    pub sync_policy: Option<GitOpsSyncPolicy>,
    pub deletion_policy: ApplicationDeletionPolicy,
    pub labels: BTreeMap<String, String>,
    pub annotations: BTreeMap<String, String>,
}

/// Build an ordinary directory-source Argo CD Application.
pub fn build_directory_application(input: &DirectoryApplicationInput) -> Result<Value> {
    validate_required("application name", &input.name)?;
    validate_required("application namespace", &input.application_namespace)?;
    validate_required("Argo CD project", &input.project)?;
    validate_required("repository URL", &input.repo_url)?;
    validate_required("revision", &input.revision)?;
    crate::resources::validate_relative_path("rendered path", &input.rendered_path, false, false)?;
    validate_required("destination namespace", &input.destination_namespace)?;
    input.destination.validate()?;

    let mut metadata = serde_json::Map::from_iter([
        ("name".to_string(), input.name.clone().into()),
        ("namespace".to_string(), input.application_namespace.clone().into()),
    ]);
    if !input.labels.is_empty() {
        metadata.insert("labels".to_string(), serde_json::to_value(&input.labels)?);
    }
    if !input.annotations.is_empty() {
        metadata.insert("annotations".to_string(), serde_json::to_value(&input.annotations)?);
    }
    match input.deletion_policy {
        ApplicationDeletionPolicy::Foreground => {
            metadata.insert("finalizers".to_string(), json!([FOREGROUND_FINALIZER]));
        }
        ApplicationDeletionPolicy::Background => {
            metadata.insert("finalizers".to_string(), json!([BACKGROUND_FINALIZER]));
        }
        ApplicationDeletionPolicy::Orphan => {}
    }

    let mut destination = serde_json::Map::new();
    if let Some(server) = &input.destination.server {
        destination.insert("server".to_string(), server.clone().into());
    }
    if let Some(name) = &input.destination.name {
        destination.insert("name".to_string(), name.clone().into());
    }
    destination.insert("namespace".to_string(), input.destination_namespace.clone().into());

    let mut spec = serde_json::Map::from_iter([
        ("project".to_string(), input.project.clone().into()),
        (
            "source".to_string(),
            json!({
                "repoURL": input.repo_url,
                "targetRevision": input.revision,
                "path": input.rendered_path,
                "directory": {"recurse": true},
            }),
        ),
        ("destination".to_string(), destination.into()),
    ]);
    let mut sync_policy = input.sync_policy.clone().unwrap_or(GitOpsSyncPolicy {
        automated: None,
        sync_options: Vec::new(),
    });
    if !sync_policy
        .sync_options
        .iter()
        .any(|option| sync_option_key(option) == sync_option_key(APPLY_OUT_OF_SYNC_ONLY_DEFAULT))
    {
        sync_policy.sync_options.push(APPLY_OUT_OF_SYNC_ONLY_DEFAULT.to_owned());
    }
    spec.insert("syncPolicy".to_string(), serde_json::to_value(sync_policy)?);

    Ok(json!({
        "apiVersion": "argoproj.io/v1alpha1",
        "kind": "Application",
        "metadata": metadata,
        "spec": spec,
    }))
}

/// Merge Argo CD sync options by key, replacing an existing value in place.
pub(crate) fn merge_sync_options(existing: &mut Vec<String>, additions: impl IntoIterator<Item = String>) {
    for addition in additions {
        let key = sync_option_key(&addition);
        if let Some(index) = existing.iter().position(|option| sync_option_key(option) == key) {
            existing[index] = addition;
        } else {
            existing.push(addition);
        }
    }
}

fn sync_option_key(option: &str) -> &str {
    option.split_once('=').map_or(option, |(key, _)| key)
}

fn validate_required(field: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        Err(NylError::config(format!("{field} must not be empty")))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(policy: ApplicationDeletionPolicy) -> DirectoryApplicationInput {
        DirectoryApplicationInput {
            name: "api".to_string(),
            application_namespace: "argocd".to_string(),
            project: "workloads".to_string(),
            repo_url: "https://example.invalid/deploy.git".to_string(),
            revision: "deploy/production".to_string(),
            rendered_path: "production/workloads/api".to_string(),
            destination: ClusterDestination {
                server: Some("https://kubernetes.default.svc".to_string()),
                name: None,
            },
            destination_namespace: "api".to_string(),
            sync_policy: None,
            deletion_policy: policy,
            labels: BTreeMap::new(),
            annotations: BTreeMap::new(),
        }
    }

    #[test]
    fn builds_recursive_directory_source() {
        let application = build_directory_application(&input(ApplicationDeletionPolicy::Foreground)).unwrap();
        assert_eq!(application["spec"]["source"]["directory"]["recurse"], true);
        assert_eq!(application["spec"]["source"]["path"], "production/workloads/api");
        assert_eq!(application["metadata"]["finalizers"][0], FOREGROUND_FINALIZER);
        assert!(application["spec"]["source"].get("plugin").is_none());
        assert_eq!(
            application["spec"]["syncPolicy"]["syncOptions"][0],
            APPLY_OUT_OF_SYNC_ONLY_DEFAULT
        );
    }

    #[test]
    fn explicit_apply_out_of_sync_only_value_overrides_the_default() {
        let mut input = input(ApplicationDeletionPolicy::Foreground);
        input.sync_policy = Some(GitOpsSyncPolicy {
            automated: None,
            sync_options: vec!["ApplyOutOfSyncOnly=false".to_owned()],
        });

        let application = build_directory_application(&input).unwrap();

        assert_eq!(
            application["spec"]["syncPolicy"]["syncOptions"],
            json!(["ApplyOutOfSyncOnly=false"])
        );
    }

    #[test]
    fn merge_sync_options_replaces_values_by_key() {
        let mut options = vec!["ApplyOutOfSyncOnly=true".to_owned(), "PruneLast=true".to_owned()];
        merge_sync_options(
            &mut options,
            ["ApplyOutOfSyncOnly=false".to_owned(), "ServerSideApply=true".to_owned()],
        );
        assert_eq!(
            options,
            vec!["ApplyOutOfSyncOnly=false", "PruneLast=true", "ServerSideApply=true"]
        );
    }

    #[test]
    fn maps_background_and_orphan_deletion() {
        let background = build_directory_application(&input(ApplicationDeletionPolicy::Background)).unwrap();
        assert_eq!(background["metadata"]["finalizers"][0], BACKGROUND_FINALIZER);

        let orphan = build_directory_application(&input(ApplicationDeletionPolicy::Orphan)).unwrap();
        assert!(orphan["metadata"].get("finalizers").is_none());
    }
}
