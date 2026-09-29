//! Bindings: where a value comes from, with exactly one source each.
//!
//! Contract: [Bindings](../../../../design/release-inputs.md#bindings) and
//! [Binding kinds](../../../../design/release-inputs.md#binding-kinds); the
//! orchestration reference forms follow the orchestration core contract's
//! References.

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::resources::{schema, validate_relative_path, validate_repository_choice, validate_static_required};
use crate::resources::{InlineGitRepository, LocalReference};
use crate::{CoreError, Result};

/// Where one Release input's value comes from. Set exactly one field.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(transform = input_binding_constraints)]
pub struct InputBinding {
    /// Inline literal value. DeploymentTargets are static, so it is never templated.
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "present")]
    pub value: Option<Value>,
    /// A YAML or JSON file of this repository.
    #[serde(default, rename = "fromFile", skip_serializing_if = "Option::is_none")]
    pub from_file: Option<FileInputSource>,
    /// A YAML or JSON file of a Git repository at a locked commit.
    #[serde(default, rename = "fromGit", skip_serializing_if = "Option::is_none")]
    pub from_git: Option<GitInputSource>,
    /// A state file in this target's publication branch, read at the publication base commit.
    #[serde(default, rename = "fromPublication", skip_serializing_if = "Option::is_none")]
    pub from_publication: Option<PublicationInputSource>,
    /// Reserved for orchestration: a recorded unit output or artifact field. Rejected outside orchestrated execution.
    #[serde(default, rename = "fromUnit", skip_serializing_if = "Option::is_none")]
    pub from_unit: Option<UnitInputSource>,
    /// Reserved for orchestration: a value recorded through a PromotionPath. Rejected outside orchestrated execution.
    #[serde(default, rename = "fromPromotion", skip_serializing_if = "Option::is_none")]
    pub from_promotion: Option<PromotionInputSource>,
}

/// The source a binding selects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindingKind {
    /// `value`
    Value,
    /// `fromFile`
    FromFile,
    /// `fromGit`
    FromGit,
    /// `fromPublication`
    FromPublication,
    /// `fromUnit`
    FromUnit,
    /// `fromPromotion`
    FromPromotion,
}

impl BindingKind {
    /// The field name as authored.
    pub fn field(self) -> &'static str {
        match self {
            Self::Value => "value",
            Self::FromFile => "fromFile",
            Self::FromGit => "fromGit",
            Self::FromPublication => "fromPublication",
            Self::FromUnit => "fromUnit",
            Self::FromPromotion => "fromPromotion",
        }
    }

    /// Whether only orchestrated execution may resolve this kind.
    pub fn requires_orchestration(self) -> bool {
        matches!(self, Self::FromUnit | Self::FromPromotion)
    }
}

impl InputBinding {
    /// The single source this binding sets.
    pub fn kind(&self, field: &str) -> Result<BindingKind> {
        let set = [
            (self.value.is_some(), BindingKind::Value),
            (self.from_file.is_some(), BindingKind::FromFile),
            (self.from_git.is_some(), BindingKind::FromGit),
            (self.from_publication.is_some(), BindingKind::FromPublication),
            (self.from_unit.is_some(), BindingKind::FromUnit),
            (self.from_promotion.is_some(), BindingKind::FromPromotion),
        ]
        .into_iter()
        .filter_map(|(present, kind)| present.then_some(kind))
        .collect::<Vec<_>>();
        match set.as_slice() {
            [kind] => Ok(*kind),
            [] => Err(CoreError::config(format!(
                "{field} must set exactly one of value, fromFile, fromGit, fromPublication, fromUnit, or fromPromotion"
            ))),
            _ => Err(CoreError::config(format!(
                "{field} sets {}; set exactly one source",
                set.iter().map(|kind| kind.field()).collect::<Vec<_>>().join(" and ")
            ))),
        }
    }

    /// Validate the static form of this binding.
    pub fn validate(&self, field: &str) -> Result<()> {
        match self.kind(field)? {
            BindingKind::Value | BindingKind::FromPromotion => Ok(()),
            BindingKind::FromUnit => {
                let source = self.from_unit.as_ref().expect("kind agrees with the set field");
                match (&source.output, &source.artifact) {
                    (Some(_), None) | (None, Some(_)) => {}
                    _ => {
                        return Err(CoreError::config(format!(
                            "{field}.fromUnit must set exactly one of output and artifact"
                        )))
                    }
                }
                source.pointer.as_deref().map_or(Ok(()), |pointer| {
                    crate::json_pointer::validate(&format!("{field}.fromUnit.pointer"), pointer)
                })
            }
            BindingKind::FromFile => {
                let source = self.from_file.as_ref().expect("kind agrees with the set field");
                crate::local_path::validate_local_path(&format!("{field}.fromFile.path"), &source.path)?;
                crate::json_pointer::validate(&format!("{field}.fromFile.pointer"), &source.pointer)
            }
            BindingKind::FromGit => self
                .from_git
                .as_ref()
                .expect("kind agrees with the set field")
                .validate(&format!("{field}.fromGit")),
            BindingKind::FromPublication => self
                .from_publication
                .as_ref()
                .expect("kind agrees with the set field")
                .validate(&format!("{field}.fromPublication")),
        }
    }
}

/// A value read from a YAML or JSON file of this repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FileInputSource {
    /// File path relative to the directory containing `nyl.toml`, or from the Git worktree root with a leading `/`. The file must contain a single YAML or JSON document.
    pub path: String,
    /// JSON Pointer selecting the value inside the document; the whole document when empty.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub pointer: String,
}

/// A value read from a file of a Git repository at a locked commit.
///
/// Rendering reads only `commit` and never resolves `revision`; `nyl update
/// source-locks` moves the lock.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(transform = git_input_source_constraints)]
pub struct GitInputSource {
    /// Project-local `gitops.nyl/v1` GitRepository to read from.
    #[serde(rename = "repositoryRef", default, skip_serializing_if = "Option::is_none")]
    pub repository_ref: Option<LocalReference>,
    /// Inline repository coordinates to read from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<InlineGitRepository>,
    /// Human-readable branch or tag that `nyl update source-locks` resolves.
    pub revision: String,
    /// Full 40-character lowercase commit ID that rendering reads, refreshed by `nyl update source-locks`.
    pub commit: String,
    /// Repository-relative path of a YAML or JSON file holding one document.
    pub path: String,
    /// JSON Pointer selecting the value inside the document; the whole document when empty.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub pointer: String,
}

impl GitInputSource {
    /// Validate the static form of this source.
    pub fn validate(&self, field: &str) -> Result<()> {
        validate_repository_choice(self.repository_ref.as_ref(), self.repository.as_ref(), field)?;
        validate_static_required(&format!("{field}.revision"), &self.revision)?;
        // Rendering, staleness checks, and `@git/` index keys compare the
        // lowercase SHA-1 form that Git reports.
        if self.commit.len() != 40
            || !self
                .commit
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
        {
            return Err(CoreError::config(format!(
                "{field}.commit must be a full 40-character lowercase hexadecimal Git commit ID"
            )));
        }
        validate_relative_path(&format!("{field}.path"), &self.path, false, false)?;
        crate::json_pointer::validate(&format!("{field}.pointer"), &self.pointer)
    }
}

fn git_input_source_constraints(schema: &mut schemars::Schema) {
    schema::exclusive_fields(
        schema,
        &[
            ("repositoryRef", "Reads from the named GitRepository."),
            ("repository", "Reads from the inline coordinates declared here."),
        ],
        None,
    );
}

/// A value read from a state file in the target's own publication branch.
///
/// The file is read at the publication base commit, the branch head that
/// `publish-tree` builds on, so a published commit holds the state and the
/// manifests rendered from it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PublicationInputSource {
    /// State file path relative to the target's publication path prefix. It must lie outside every directory a generated Argo CD Application syncs: workload Release directories and `_nyl`.
    pub path: String,
    /// JSON Pointer selecting the value inside the document; the whole document when empty.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub pointer: String,
    /// Untracked working-tree file of this run, under the local path rule. When present, Nyl renders from it and writes its bytes to `path` in the publication commit; otherwise the base copy is written back. With `carryFileFromWorktree`, `path` is owned by this target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(rename = "carryFileFromWorktree")]
    pub carry_file_from_worktree: Option<String>,
}

impl PublicationInputSource {
    /// Validate the static form of this source.
    pub fn validate(&self, field: &str) -> Result<()> {
        validate_relative_path(&format!("{field}.path"), &self.path, false, false)?;
        crate::json_pointer::validate(&format!("{field}.pointer"), &self.pointer)?;
        if let Some(carry) = &self.carry_file_from_worktree {
            crate::local_path::validate_local_path(&format!("{field}.carryFileFromWorktree"), carry)?;
        }
        Ok(())
    }
}

/// Reserved orchestration reference to a unit output or artifact field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UnitInputSource {
    /// Environment of the unit, for cross-environment references.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
    /// Producing unit name.
    pub unit: String,
    /// Declared output name. Set exactly one of `output` and `artifact`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// Published artifact name. Set exactly one of `output` and `artifact`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<String>,
    /// Expected artifact kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// JSON Pointer inside the output or artifact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pointer: Option<String>,
    /// Required evidence level: `published` or `attested`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
    /// Required attestations.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attestations: Vec<String>,
}

/// Reserved orchestration reference to a promoted value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PromotionInputSource {
    /// Promoted value name, as declared by a PromotionPath.
    pub value: String,
    /// PromotionPath the value must come from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

fn input_binding_constraints(schema: &mut schemars::Schema) {
    schema::exclusive_fields(
        schema,
        &[
            ("value", "Binds an inline literal value."),
            (
                "fromFile",
                "Reads the value from a YAML or JSON file of this repository.",
            ),
            (
                "fromGit",
                "Reads the value from a file of a Git repository at a locked commit.",
            ),
            (
                "fromPublication",
                "Reads the value from a state file in this target's publication branch.",
            ),
            (
                "fromUnit",
                "Reserved for orchestration: reads a recorded unit output or artifact field.",
            ),
            (
                "fromPromotion",
                "Reserved for orchestration: reads a value recorded through a PromotionPath.",
            ),
        ],
        None,
    );
}

/// Deserialize a present field as `Some`, even when it is `null`, so `null`
/// reaches type checking instead of looking like an absent field.
pub(crate) fn present<'de, D>(deserializer: D) -> std::result::Result<Option<Value>, D::Error>
where
    D: Deserializer<'de>,
{
    Value::deserialize(deserializer).map(Some)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::resources::release_inputs::{validate_bindings, ReleaseInputBindings};

    #[test]
    fn test_binding_sets_exactly_one_source() {
        let binding: InputBinding = serde_json::from_value(json!({"value": null})).unwrap();
        assert_eq!(binding.kind("b").unwrap(), BindingKind::Value);
        let binding: InputBinding = serde_json::from_value(json!({})).unwrap();
        assert!(binding.kind("b").unwrap_err().to_string().contains("exactly one"));
        let binding: InputBinding =
            serde_json::from_value(json!({"value": 1, "fromFile": {"path": "a.yaml"}})).unwrap();
        assert!(binding
            .kind("b")
            .unwrap_err()
            .to_string()
            .contains("value and fromFile"));
        let binding: InputBinding = serde_json::from_value(json!({"fromPromotion": {"value": "image"}})).unwrap();
        assert!(binding.kind("b").unwrap().requires_orchestration());
    }

    #[test]
    fn test_from_git_requires_a_repository_revision_and_full_commit() {
        let commit = "3f1c9a0000000000000000000000000000000000";
        let cases = [
            (
                json!({"revision": "main", "commit": commit, "path": "a.json"}),
                "Exactly one of",
            ),
            (
                json!({"repository": {"repoURL": "https://example.invalid/x.git"}, "revision": "main", "commit": "3f1c", "path": "a.json"}),
                "40-character lowercase hexadecimal",
            ),
            (
                json!({"repository": {"repoURL": "https://example.invalid/x.git"}, "revision": "main", "commit": commit.to_uppercase(), "path": "a.json"}),
                "40-character lowercase hexadecimal",
            ),
            (
                json!({"repository": {"repoURL": "https://example.invalid/x.git"}, "revision": "main", "commit": "a".repeat(64), "path": "a.json"}),
                "40-character lowercase hexadecimal",
            ),
            (
                json!({"repositoryRef": {"name": "state"}, "revision": "main", "commit": commit, "path": "../a.json"}),
                "fromGit.path",
            ),
        ];
        for (source, expected) in cases {
            let bindings: ReleaseInputBindings =
                serde_json::from_value(json!({"g/r": {"image": {"fromGit": source.clone()}}})).unwrap();
            let error = validate_bindings(&bindings).unwrap_err().to_string();
            assert!(error.contains(expected), "{source}: {error}");
        }
    }

    #[test]
    fn test_from_publication_paths_stay_inside_the_prefix() {
        for (source, expected) in [
            (json!({"path": "../other/state.json"}), "fromPublication.path"),
            (json!({"path": "/state.json"}), "fromPublication.path"),
            (
                json!({"path": "state.json", "carryFileFromWorktree": "build/./images.json"}),
                "fromPublication.carryFileFromWorktree",
            ),
        ] {
            let bindings: ReleaseInputBindings =
                serde_json::from_value(json!({"g/r": {"image": {"fromPublication": source.clone()}}})).unwrap();
            let error = validate_bindings(&bindings).unwrap_err().to_string();
            assert!(error.contains(expected), "{source}: {error}");
        }
    }
}
