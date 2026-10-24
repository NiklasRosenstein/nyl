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
    validate_immutable_git_commit, validate_relative_path, validate_repository_choice, validate_static_required,
    InlineGitRepository, LocalReference,
};
use crate::error::{CoreError, Result};

/// A directory or file in a Git repository: this repository when no
/// repository is named, otherwise a remote one, optionally pinned to a commit.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct GitSource {
    /// Project-local `gitops.nyl/v1` GitRepository to read from. Mutually exclusive with inline `repository`.
    #[serde(rename = "repositoryRef", default, skip_serializing_if = "Option::is_none")]
    pub repository_ref: Option<LocalReference>,
    /// Inline repository coordinates to read from. Mutually exclusive with `repositoryRef`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<InlineGitRepository>,
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
            validate_repository_choice(self.repository_ref.as_ref(), self.repository.as_ref(), field)?;
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
        validate_repository_choice(self.repository_ref.as_ref(), self.repository.as_ref(), field)?;
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
