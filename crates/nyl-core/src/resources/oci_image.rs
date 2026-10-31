//! Container image units, built with Docker Buildx.
use super::units::{CommonUnitFields, KindSupport, UnitMetadata};
use super::{DenyUnknownFields, GitSource};
use crate::{CoreError, Result};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// Builds and pushes an immutable container image with Docker Buildx.
///
/// Consumers read the `image` ContainerImage artifact by digest. Registry
/// credentials come only from the declared `registryAuth` keys or `env`.
///
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct OciImage {
    /// The built-in unit API version, `units.gitops.nyl/v1`.
    #[serde(rename = "apiVersion")]
    pub api_version: String,
    /// The unit kind, `OciImage`.
    pub kind: String,
    /// Literal unit name and labels used for environment selection.
    pub metadata: UnitMetadata,
    /// The build inputs, registry destination, and common unit policy.
    pub spec: OciImageSpec,
}

/// Docker Buildx inputs. Every directory context is a separately pinned Git source.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(transform = super::strict::deny_additional_properties)]
pub struct OciImageSpec {
    /// Main build context, following the common Git source and local path rules.
    pub context: GitSource,
    /// Named directories or digest-pinned image references, passed with `--build-context`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub contexts: BTreeMap<String, ImageContext>,
    /// Dockerfile relative to the main context, default `Dockerfile`; cannot leave the context.
    #[serde(default = "dockerfile_default")]
    pub dockerfile: String,
    /// Optional Dockerfile build stage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// Registry repository without a tag or digest, such as `registry.example.com/team/web`.
    pub repository: String,
    /// Target platforms, such as `linux/amd64`. Omitted uses the builder's platform.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub platforms: Vec<String>,
    /// Build arguments, with references resolved before execution. Values must resolve to scalars.
    #[serde(rename = "buildArgs", default, skip_serializing_if = "BTreeMap::is_empty")]
    pub build_args: BTreeMap<String, Value>,
    /// Extra moving tags in this repository. Consumers use the immutable artifact reference.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Runner's Buildx builder; excluded from the execution key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub builder: Option<String>,
    /// Buildx cache imports and exports; excluded from the execution key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<ImageCache>,
    /// Registry host to username/password secret keys. Values never enter state or execution keys.
    #[serde(rename = "registryAuth", default, skip_serializing_if = "BTreeMap::is_empty")]
    pub registry_auth: BTreeMap<String, RegistryAuth>,
    #[serde(flatten)]
    pub common: CommonUnitFields,
    #[serde(flatten)]
    #[schemars(skip)]
    pub unknown_fields: DenyUnknownFields,
}
fn dockerfile_default() -> String {
    "Dockerfile".into()
}

/// A directory source or an image reference for a named Buildx context.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum ImageContext {
    Image(ImageReference),
    Directory(GitSource),
}
/// An image context. Its reference can be supplied by a `fromUnit` artifact binding.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ImageReference {
    /// A digest-pinned image reference or a `fromUnit` artifact binding.
    pub image: Value,
}
/// Buildx cache specifications.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ImageCache {
    #[serde(default)]
    /// Buildx cache import specifications passed as `--cache-from`.
    pub from: Vec<String>,
    #[serde(default)]
    /// Buildx cache export specifications passed as `--cache-to`.
    pub to: Vec<String>,
}
/// Secret keys for one registry; the runner never reads undeclared credentials.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RegistryAuth {
    /// Project secret key containing this registry's username.
    #[serde(rename = "usernameSecret")]
    pub username_secret: String,
    /// Project secret key containing this registry's password or access token.
    #[serde(rename = "passwordSecret")]
    pub password_secret: String,
}

fn invalid(message: impl Into<String>) -> CoreError {
    CoreError::config(message.into())
}
fn clean_value(value: &str) -> bool {
    !value.is_empty() && !value.chars().any(char::is_control)
}
fn source_path(source: &GitSource, field: &str) -> Result<()> {
    let path = source
        .path
        .as_deref()
        .ok_or_else(|| invalid(format!("{field}.path is required")))?;
    if path != "/" {
        crate::local_path::validate_local_path(&format!("{field}.path"), path)?;
    }
    if source.is_remote() {
        source.validate_repository(field)?;
    }
    Ok(())
}
impl OciImageSpec {
    /// Validate static fields before resolution. Dynamic reference values are admitted by the common resolver.
    #[allow(clippy::too_many_lines)] // Validate each independent authored field before any build or secret admission.
    pub fn validate(&self) -> Result<()> {
        self.common.validate(KindSupport {
            declared_outputs: false,
            teardown: false,
        })?;
        source_path(&self.context, "spec.context")?;
        if !clean_value(&self.repository)
            || self.repository.contains('@')
            || self.repository.contains("://")
            || self
                .repository
                .rsplit('/')
                .next()
                .is_some_and(|name| name.contains(':'))
            || self.repository.contains(char::is_whitespace)
            || self.repository.starts_with('-')
        {
            return Err(invalid(
                "spec.repository must name a registry repository without a tag or digest",
            ));
        }
        if self.dockerfile.starts_with('/')
            || self
                .dockerfile
                .split('/')
                .any(|part| part.is_empty() || part == "." || part == "..")
            || self.dockerfile.contains('\\')
        {
            return Err(invalid(
                "spec.dockerfile must be relative to the context and cannot leave it",
            ));
        }
        for (name, context) in &self.contexts {
            if !clean_value(name)
                || !name.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
                || name.starts_with('-')
            {
                return Err(invalid("spec.contexts names must use letters, digits, '.', '_' or '-'"));
            }
            match context {
                ImageContext::Directory(source) => source_path(source, &format!("spec.contexts.{name}"))?,
                ImageContext::Image(reference)
                    if reference.image.as_str().is_some_and(clean_value)
                        || reference.image.as_object().is_some_and(|object| {
                            object.contains_key("fromUnit") || object.contains_key("fromPromotion")
                        }) => {}
                ImageContext::Image(_) => {
                    return Err(invalid(format!(
                        "spec.contexts.{name}.image must be a reference or binding"
                    )))
                }
            }
        }
        for tag in &self.tags {
            if tag.is_empty()
                || tag.len() > 128
                || !tag.chars().all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c))
                || tag.starts_with(['.', '-'])
            {
                return Err(invalid(
                    "spec.tags must contain valid tag names, not complete image references",
                ));
            }
        }
        for value in self
            .platforms
            .iter()
            .chain(self.target.iter())
            .chain(self.builder.iter())
        {
            if !clean_value(value) {
                return Err(invalid(
                    "image platforms, target, and builder must be nonempty and contain no control characters",
                ));
            }
        }
        for value in self.build_args.values() {
            if !(value.is_string()
                || value.is_number()
                || value.is_boolean()
                || value
                    .as_object()
                    .is_some_and(|object| object.contains_key("fromUnit") || object.contains_key("fromPromotion")))
            {
                return Err(invalid(
                    "spec.buildArgs values must be scalars or unit/promotion bindings",
                ));
            }
        }
        for name in self.build_args.keys() {
            if !clean_value(name) || name.contains('=') {
                return Err(invalid(
                    "build argument names must be nonempty and contain no '=' or control characters",
                ));
            }
        }
        for (registry, auth) in &self.registry_auth {
            if !clean_value(registry)
                || registry.contains('/')
                || !clean_value(&auth.username_secret)
                || !clean_value(&auth.password_secret)
            {
                return Err(invalid(
                    "registryAuth requires a registry host and nonempty usernameSecret/passwordSecret keys",
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_native_read_source_rejects_credentials_and_dynamic_repository_selectors() {
        for repository in [
            serde_json::json!({"repository": {"repoURL": "https://user:password@example.invalid/source.git"}}),
            serde_json::json!({"repository": {"repoURL": "${{ values.source }}"}}),
            serde_json::json!({"repositoryRef": {"name": "${{ values.source }}"}}),
        ] {
            let mut source = repository;
            source["path"] = serde_json::json!("module");
            let spec: OciImageSpec =
                serde_json::from_value(serde_json::json!({"context": source, "repository": "registry.invalid/web"}))
                    .unwrap();
            assert!(spec.validate().is_err());
        }
    }
}
