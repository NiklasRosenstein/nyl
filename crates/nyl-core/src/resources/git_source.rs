//! The one Git source shape that every resource reading files from Git uses.
//!
//! ApplicationGroup sources and `fromGit` bindings flatten [`GitSource`] into
//! their own fields today; unit sources, image build contexts, and
//! Environment sources follow. Each use validates the fields it admits, and
//! one module, nyl-render's `gitops::source_locks`, resolves and locks every
//! commit. See the implementation architecture's "Types that rule out illegal
//! states" (`design/implementation-architecture.md`).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::gitops::{
    validate_immutable_git_commit, validate_relative_path, validate_repository_coordinates, validate_static_required,
    LocalReference,
};
use crate::error::{CoreError, Result};

/// Credential-free coordinates for a repository that is only read.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReadGitRepository {
    /// Git read URL. Source checkouts never publish to this repository.
    #[serde(rename = "repoURL")]
    pub repo_url: String,
}

/// A directory or file in a Git repository: this repository when no
/// repository is named, otherwise a remote one, optionally pinned to a commit.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct GitSource {
    /// Project-local `gitops.nyl/v1` GitRepository to read from. Mutually exclusive with inline `repository`.
    #[serde(rename = "repositoryRef", default, skip_serializing_if = "Option::is_none")]
    pub repository_ref: Option<LocalReference>,
    /// Inline repository coordinates to read from. Mutually exclusive with `repositoryRef`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<ReadGitRepository>,
    /// Human-readable branch or tag that `nyl update source-locks` resolves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    /// Full 40-character lowercase commit ID that is read, refreshed by `nyl update source-locks`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// Path in the repository. In this repository it follows the local path rule: relative to the directory containing `nyl.toml`, may begin with `..` segments, or starts with `/` to name a path from the Git worktree root, and never leaves the worktree. In a remote repository it is relative to the checkout root, with an optional leading `/`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

impl GitSource {
    /// Validate the exclusive named or inline repository used by a remote read.
    pub fn validate_repository(&self, field: &str) -> Result<()> {
        match (self.repository_ref.as_ref(), self.repository.as_ref()) {
            (Some(reference), None) => {
                validate_static_required(&format!("{field}.repositoryRef.name"), &reference.name)
            }
            (None, Some(repository)) => validate_repository_coordinates(&repository.repo_url, None)
                .map_err(|error| CoreError::config(format!("{field}.repository: {error}"))),
            (Some(_), Some(_)) => Err(CoreError::config(format!(
                "{field}.repositoryRef and {field}.repository are mutually exclusive"
            ))),
            (None, None) => Err(CoreError::config(format!(
                "Exactly one of {field}.repositoryRef or {field}.repository is required"
            ))),
        }
    }

    /// Whether the source names a repository other than this one.
    pub fn is_remote(&self) -> bool {
        self.repository_ref.is_some() || self.repository.is_some()
    }

    /// `path`, or the empty string when it is absent.
    pub fn path(&self) -> &str {
        self.path.as_deref().unwrap_or_default()
    }

    /// The path inside a remote checkout, without the optional leading `/`.
    pub fn checkout_path(&self) -> &str {
        let path = self.path();
        path.strip_prefix('/').unwrap_or(path)
    }

    /// Validate the source of an ApplicationGroup: a local path, or a remote
    /// repository with both `revision` and its `commit` lock.
    pub fn validate_application_group(&self, field: &str) -> Result<()> {
        let path = self.required_path(field)?;
        let remote = self.is_remote();
        if remote {
            validate_relative_path(&format!("{field}.path"), self.checkout_path(), false, false)?;
            self.validate_repository(field)?;
            let revision = self
                .revision
                .as_deref()
                .ok_or_else(|| CoreError::config(format!("{field}.revision is required for a remote source")))?;
            validate_static_required(&format!("{field}.revision"), revision)?;
            let commit = self
                .commit
                .as_deref()
                .ok_or_else(|| CoreError::config(format!("{field}.commit is required for a remote source")))?;
            validate_immutable_git_commit(&format!("{field}.commit"), commit)
        } else {
            crate::local_path::validate_local_path(&format!("{field}.path"), path)?;
            if self.revision.is_some() || self.commit.is_some() {
                return Err(CoreError::config(format!(
                    "{field}.revision and {field}.commit require a remote repository"
                )));
            }
            Ok(())
        }
    }

    /// Validate the source of a file read at a locked commit, such as a
    /// `fromGit` binding: a named repository, `revision`, its `commit` lock,
    /// and a repository-relative file path.
    pub fn validate_locked_file(&self, field: &str) -> Result<()> {
        self.validate_repository(field)?;
        let revision = self
            .revision
            .as_deref()
            .ok_or_else(|| CoreError::config(format!("{field}.revision is required")))?;
        validate_static_required(&format!("{field}.revision"), revision)?;
        let commit = self
            .commit
            .as_deref()
            .ok_or_else(|| CoreError::config(format!("{field}.commit is required")))?;
        validate_immutable_git_commit(&format!("{field}.commit"), commit)?;
        validate_relative_path(&format!("{field}.path"), self.required_path(field)?, false, false)
    }

    fn required_path(&self, field: &str) -> Result<&str> {
        self.path
            .as_deref()
            .ok_or_else(|| CoreError::config(format!("{field}.path is required")))
    }
}

/// Mark `fields` required in the schema of a resource that flattens
/// [`GitSource`], for the fields its use requires, and drop `null` from their
/// types, as for a plain required field.
pub(crate) fn require_fields(schema: &mut schemars::Schema, fields: &[&str]) {
    let object = schema.ensure_object();
    if let Some(serde_json::Value::Object(properties)) = object.get_mut("properties") {
        for field in fields {
            if let Some(serde_json::Value::Object(property)) = properties.get_mut(*field) {
                if let Some(serde_json::Value::Array(types)) = property.get("type") {
                    let types = types
                        .iter()
                        .filter(|kind| kind.as_str() != Some("null"))
                        .cloned()
                        .collect::<Vec<_>>();
                    let kind = match types.as_slice() {
                        [single] => single.clone(),
                        _ => serde_json::Value::Array(types),
                    };
                    property.insert("type".into(), kind);
                }
            }
        }
    }
    let required = object
        .entry("required")
        .or_insert_with(|| serde_json::Value::Array(Vec::new()));
    if let serde_json::Value::Array(required) = required {
        for field in fields {
            if !required.iter().any(|existing| existing == field) {
                required.push(serde_json::Value::String((*field).to_owned()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_read_sources_reject_write_coordinates_while_repository_destinations_accept_them() {
        let coordinates =
            json!({"repoURL": "https://example.invalid/read.git", "publishURL": "ssh://git@example.invalid/write.git"});
        let error =
            serde_json::from_value::<GitSource>(json!({"repository": coordinates, "path": "module"})).unwrap_err();
        assert!(error.to_string().contains("unknown field `publishURL`"));
        let source: GitSource = serde_json::from_value(json!({"repository": {"repoURL": "https://example.invalid/read.git"}, "revision": "main", "commit": "a".repeat(40), "path": "module"})).unwrap();
        source.validate_application_group("source").unwrap();
        let destination: super::super::gitops::GitRepository = serde_json::from_value(json!({
            "apiVersion": "gitops.nyl/v1", "kind": "GitRepository", "metadata": {"name": "destination"}, "spec": coordinates,
        })).unwrap();
        destination.validate().unwrap();
        assert_eq!(
            destination.spec.publish_url.as_deref(),
            Some("ssh://git@example.invalid/write.git")
        );
    }
}
