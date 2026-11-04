//! Shared `gitops.nyl/v1` resource pieces: the GitRepository resource, the
//! static resource metadata, Git repository coordinates, and the validators
//! every resource envelope uses. Kubernetes GitOps control resources live in
//! nyl-render's `resources` module.

use std::collections::BTreeMap;
use std::path::{Component, Path};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::constants::API_VERSION_GITOPS;
use crate::{CoreError, Result};

pub const KIND_GIT_REPOSITORY: &str = "GitRepository";

/// Metadata shared by local GitOps compiler resources.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GitOpsResourceMetadata {
    /// Project-local identity, validated by the resource kind; remains static during discovery.
    pub name: String,
    /// Literal labels used for organization and target selection.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct LabelSelector {
    /// Exact label matches, combined with AND. An empty map selects every ApplicationGroup.
    #[serde(default, rename = "matchLabels", skip_serializing_if = "BTreeMap::is_empty")]
    pub match_labels: BTreeMap<String, String>,
}

/// Names reusable, credential-free Git read and publication coordinates.
///
/// This is shared compiler configuration referenced by Kubernetes GitOps resources; it is not emitted as a workload manifest.
///
/// ## When needed
///
/// Required when a publication, remote ApplicationGroup source, or AppProjectDefinition refers to a named GitRepository.
///
/// ## If omitted
///
/// Publications and remote sources can use inline `repository` coordinates instead. Local ApplicationGroup sources need no repository. Named references must resolve to a declared GitRepository; Nyl does not infer one from the current Git remote.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
#[schemars(example = super::schema::git_repository_example())]
pub struct GitRepository {
    /// API group and version defining this resource.
    #[serde(rename = "apiVersion")]
    pub api_version: String,
    /// Resource kind within the API group.
    pub kind: String,
    /// Resource identity and metadata.
    pub metadata: GitOpsResourceMetadata,
    /// Configuration for this resource.
    pub spec: GitRepositorySpec,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GitRepositorySpec {
    /// Credential-free Git read URL used by renderers and generated Applications.
    #[serde(rename = "repoURL")]
    pub repo_url: String,
    /// Optional Git write URL. Publication uses `repoURL` when omitted.
    #[serde(rename = "publishURL", skip_serializing_if = "Option::is_none")]
    pub publish_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LocalReference {
    /// Project-local name of the referenced resource; its kind and API group are determined by the containing field.
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InlineGitRepository {
    /// Credential-free Git read URL used by renderers and generated Applications.
    #[serde(rename = "repoURL")]
    pub repo_url: String,
    /// Optional Git write URL. Publication uses `repoURL` when omitted.
    #[serde(rename = "publishURL", skip_serializing_if = "Option::is_none")]
    pub publish_url: Option<String>,
}

impl GitRepository {
    pub fn validate(&self) -> Result<()> {
        if self.api_version != API_VERSION_GITOPS {
            return Err(CoreError::config(format!(
                "{KIND_GIT_REPOSITORY} apiVersion must be {API_VERSION_GITOPS:?}"
            )));
        }
        if self.kind != KIND_GIT_REPOSITORY {
            return Err(CoreError::config(format!(
                "Expected kind {KIND_GIT_REPOSITORY:?}, got {:?}",
                self.kind
            )));
        }
        validate_dns_subdomain("metadata.name", &self.metadata.name)?;
        validate_repository_coordinates(&self.spec.repo_url, self.spec.publish_url.as_deref())
    }
}

impl InlineGitRepository {
    fn validate(&self, field: &str) -> Result<()> {
        validate_repository_coordinates(&self.repo_url, self.publish_url.as_deref())
            .map_err(|error| CoreError::config(format!("{field}: {error}")))
    }
}

pub fn validate_dns_subdomain(field: &str, value: &str) -> Result<()> {
    validate_static_required(field, value)?;
    let valid = value.len() <= 253
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-'))
        && value.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric)
        && value.as_bytes().last().is_some_and(u8::is_ascii_alphanumeric);
    if valid {
        Ok(())
    } else {
        Err(CoreError::config(format!("{field} must be a Kubernetes DNS subdomain")))
    }
}

pub fn validate_repository_choice(
    repository_ref: Option<&LocalReference>,
    repository: Option<&InlineGitRepository>,
    field: &str,
) -> Result<()> {
    match (repository_ref, repository) {
        (Some(reference), None) => validate_static_required(&format!("{field}.repositoryRef.name"), &reference.name),
        (None, Some(repository)) => repository.validate(&format!("{field}.repository")),
        (Some(_), Some(_)) => Err(CoreError::config(format!(
            "{field}.repositoryRef and {field}.repository are mutually exclusive"
        ))),
        (None, None) => Err(CoreError::config(format!(
            "Exactly one of {field}.repositoryRef or {field}.repository is required"
        ))),
    }
}

/// Validate Git repository URLs: static, non-empty, and free of HTTP user
/// information. `publish_url` is checked only when present.
pub fn validate_repository_coordinates(repo_url: &str, publish_url: Option<&str>) -> Result<()> {
    validate_static_required("spec.repoURL", repo_url)?;
    reject_http_userinfo("spec.repoURL", repo_url)?;
    if let Some(publish_url) = publish_url {
        validate_static_required("spec.publishURL", publish_url)?;
        reject_http_userinfo("spec.publishURL", publish_url)?;
    }
    Ok(())
}

pub fn reject_http_userinfo(field: &str, value: &str) -> Result<()> {
    let lower = value.to_ascii_lowercase();
    let Some(authority_and_path) = lower.strip_prefix("https://").or_else(|| lower.strip_prefix("http://")) else {
        return Ok(());
    };
    let authority = authority_and_path
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(authority_and_path);
    if authority.contains('@') {
        return Err(CoreError::config(format!(
            "{field} must not contain HTTP user information; configure Git credentials outside GitOps resources"
        )));
    }
    Ok(())
}

pub fn validate_required(field: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        return Err(CoreError::config(format!("{field} must not be empty")));
    }
    Ok(())
}

/// Require a literal, non-empty value in the pre-template resource envelope or coordinates.
pub fn validate_static_required(field: &str, value: &str) -> Result<()> {
    validate_required(field, value)?;
    if value.contains("{{") || value.contains("{%") || value.contains("{#") {
        return Err(CoreError::config(format!("{field} must be a static value")));
    }
    Ok(())
}

/// Validate a slash-separated, project-relative path without traversal.
pub fn validate_relative_path(field: &str, value: &str, allow_empty: bool, allow_dot: bool) -> Result<()> {
    if value.is_empty() && allow_empty {
        return Ok(());
    }
    validate_static_required(field, value)?;
    if value == "." && allow_dot {
        return Ok(());
    }
    if value.contains('\\')
        || value
            .split('/')
            .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    {
        return Err(CoreError::config(format!(
            "{field} must be a normalized relative path without traversal"
        )));
    }
    if Path::new(value)
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(CoreError::config(format!(
            "{field} must be a normalized relative path without traversal"
        )));
    }
    Ok(())
}

/// Convert a platform-native relative path to the slash-separated form stored
/// in Kubernetes GitOps metadata.
pub fn relative_path_to_posix(field: &str, value: &Path) -> Result<String> {
    let mut segments = Vec::new();
    for component in value.components() {
        let Component::Normal(segment) = component else {
            return Err(CoreError::config(format!(
                "{field} must be a normalized relative path without traversal"
            )));
        };
        segments.push(
            segment
                .to_str()
                .ok_or_else(|| CoreError::config(format!("{field} must be valid UTF-8")))?,
        );
    }
    let value = segments.join("/");
    validate_relative_path(field, &value, false, false)?;
    Ok(value)
}

/// Accept only a full commit ID in the form Git reports it: 40 lowercase
/// hexadecimal digits, never a mutable ref or an abbreviation.
///
/// Every Git commit lock shares this one rule. Rendering reads the commit as
/// written, `nyl update source-locks` writes and compares this form, and
/// `@git/` ownership-index keys contain it, so any other spelling of the same
/// commit would always look stale.
pub fn validate_immutable_git_commit(field: &str, value: &str) -> Result<()> {
    validate_static_required(field, value)?;
    if value.len() != 40 || !value.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f')) {
        return Err(CoreError::config(format!(
            "{field} must be a full 40-character lowercase hexadecimal Git commit ID"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    /// Group sources and fromGit locks share one commit rule.
    #[test]
    fn test_immutable_git_commit_is_the_full_lowercase_form() {
        let commit = "3f1c9a0000000000000000000000000000000000";
        validate_immutable_git_commit("spec.source.commit", commit).unwrap();
        for value in ["3f1c9a0", &commit.to_uppercase(), &"a".repeat(64), "main"] {
            let error = validate_immutable_git_commit("spec.source.commit", value)
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("spec.source.commit must be a full 40-character lowercase hexadecimal Git commit ID"),
                "{value}: {error}"
            );
        }
    }

    #[test]
    fn serializes_platform_paths_with_posix_separators() {
        let path = PathBuf::from("applications").join("cloud").join("deployment.yaml");
        assert_eq!(
            relative_path_to_posix("rendered path", &path).unwrap(),
            "applications/cloud/deployment.yaml"
        );
    }
}
