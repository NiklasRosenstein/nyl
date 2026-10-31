//! Terraform and OpenTofu share one configuration and native lifecycle.
//!
//! Contract: [Infrastructure units](../../../../design/infrastructure-units.md#terraform-and-opentofu).

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::units::{CommonUnitFields, KindSupport};
use super::{DenyUnknownFields, GitSource};
use crate::{CoreError, Result};

/// A Terraform or OpenTofu infrastructure unit, preserving native state and locks.
///
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TerraformUnit {
    /// The built-in unit API version, `units.gitops.nyl/v1`.
    #[serde(rename = "apiVersion")]
    pub api_version: String,
    /// `Terraform` or `OpenTofu`, selecting the native executable.
    pub kind: String,
    /// Literal unit name and labels used for environment selection.
    pub metadata: super::units::UnitMetadata,
    /// Pinned configuration, native backend, variables, outputs, and common policy.
    pub spec: TerraformSpec,
}

/// Infrastructure managed by Terraform or OpenTofu, retaining native state and locking.
///
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[schemars(transform = super::strict::deny_additional_properties)]
pub struct TerraformSpec {
    /// Configuration directory at the effective source revision. Relative local modules use the same checkout.
    #[schemars(schema_with = "required_source_schema")]
    pub source: GitSource,
    /// Files entering the execution key, relative to the source project's nyl.toml or worktree-rooted. Defaults to source.path/**, excluding .terraform.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    /// Exact required native tool version. Without it, the installed version is audited but does not trigger execution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Backend configuration passed to init through a private temporary file. Credentials belong in env, never here.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub backend: BTreeMap<String, Value>,
    /// Terraform workspace, defaulting to the native default workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// Native input variables; fromUnit references may appear at any depth.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub variables: BTreeMap<String, Value>,
    /// Native variable files, evaluated in order before explicit variables.
    #[serde(rename = "varFiles", default, skip_serializing_if = "Vec::is_empty")]
    pub var_files: Vec<String>,
    #[serde(flatten)]
    pub common: CommonUnitFields,
    #[serde(flatten)]
    #[schemars(skip)]
    pub unknown_fields: DenyUnknownFields,
}

impl TerraformSpec {
    /// Validate static declarations before any native tool runs.
    pub fn validate(&self) -> Result<()> {
        let path = self
            .source
            .path
            .as_deref()
            .ok_or_else(|| CoreError::config("spec.source.path is required"))?;
        crate::local_path::validate_local_path("spec.source.path", path)?;
        if self.source.is_remote() {
            self.source.validate_repository("spec.source")?;
        }
        if let Some(version) = &self.version {
            if version.is_empty()
                || !version.bytes().all(|byte| {
                    byte.is_ascii_digit() || b".-+abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ".contains(&byte)
                })
            {
                return Err(CoreError::config("spec.version must name an exact native tool version"));
            }
        }
        if let Some(workspace) = &self.workspace {
            if workspace.is_empty()
                || !workspace
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
            {
                return Err(CoreError::config(
                    "spec.workspace must be a nonempty native workspace name",
                ));
            }
        }
        for (field, paths) in [("files", &self.files), ("varFiles", &self.var_files)] {
            for (index, path) in paths.iter().enumerate() {
                crate::local_path::validate_local_path(&format!("spec.{field}[{index}]"), path)?;
                if field == "files" {
                    glob::Pattern::new(path)
                        .map_err(|error| CoreError::config(format!("invalid spec.files glob: {error}")))?;
                }
            }
        }
        for (name, value) in &self.backend {
            if name.is_empty()
                || !name.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
                || !matches!(value, Value::String(_) | Value::Number(_) | Value::Bool(_))
            {
                return Err(CoreError::config(
                    "spec.backend must contain named scalar backend settings",
                ));
            }
            if credential_setting(name) {
                return Err(CoreError::config(format!(
                    "spec.backend.{name} is a credential; declare it through spec.env instead"
                )));
            }
        }
        self.common.validate(KindSupport {
            declared_outputs: true,
            teardown: true,
        })
    }
}

/// Backend credentials never enter resource ownership or desired state.
pub fn credential_setting(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "access_key"
            | "secret_key"
            | "session_token"
            | "token"
            | "password"
            | "client_secret"
            | "credentials"
            | "credentials_file"
            | "access_token"
            | "sas_token"
            | "secret_access_key"
    )
}

fn required_source_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
    let mut schema = GitSource::json_schema(generator);
    super::git_source::require_fields(&mut schema, &["path"]);
    schema
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_spec_rejects_undeclared_credentials_unknown_fields_and_escaping_paths() {
        let spec = |value| serde_json::from_value::<TerraformSpec>(value).unwrap().validate();
        assert!(spec(json!({"source": {"path": "infra/network"}, "backend": {"bucket": "state", "key": "dev"}, "variables": {"network": {"fromUnit": {"unit": "network", "output": "id"}}}})).is_ok());
        assert!(
            spec(json!({"source": {"path": "infra/network"}, "backend": {"secret_key": "never-persist"}}))
                .unwrap_err()
                .to_string()
                .contains("credential")
        );
        assert!(spec(json!({"source": {"path": "infra/network"}, "files": ["/../escape"]})).is_err());
        assert!(serde_json::from_value::<TerraformSpec>(
            json!({"source": {"path": "infra/network"}, "unrecognized": true})
        )
        .is_err());
    }
    #[test]
    fn test_native_read_source_rejects_credentials_and_dynamic_repository_selectors() {
        for repository in [
            serde_json::json!({"repository": {"repoURL": "https://user:password@example.invalid/source.git"}}),
            serde_json::json!({"repository": {"repoURL": "${{ values.source }}"}}),
            serde_json::json!({"repositoryRef": {"name": "${{ values.source }}"}}),
        ] {
            let mut source = repository;
            source["path"] = serde_json::json!("module");
            let spec: TerraformSpec = serde_json::from_value(serde_json::json!({"source": source})).unwrap();
            assert!(spec.validate().is_err());
        }
    }
}
