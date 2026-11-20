//! Bindings: where a value comes from, with exactly one source each.
//!
//! Contract: [Bindings](../../../../design/release-inputs.md#bindings) and
//! [Binding kinds](../../../../design/release-inputs.md#binding-kinds); the
//! orchestration reference forms follow the orchestration core contract's
//! References.

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::resources::units::Evidence;
use crate::resources::{schema, validate_relative_path};
use crate::resources::{DenyUnknownFields, GitSource};
use crate::{CoreError, Result};

/// Where one Release input's value comes from: a literal, or a value source
/// (`$fromFile`, `$fromGit`, `$fromPublication`, `$fromUnit`, `$fromPromotion`, `$literal`).
///
/// A bare value is the input's value. An object whose only key is a source
/// reads the value from it, and sources inside a literal are resolved too,
/// at any depth ([`super::resolve_binding`]).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct InputBinding(pub Value);

/// The schema offers each source a binding accepts, for editors to complete,
/// next to any literal: the context table's Release-input bindings.
impl JsonSchema for InputBinding {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "InputBinding".into()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        let mut variants = super::source_variants(super::Context::ReleaseInputs, generator);
        variants.push(schemars::json_schema!({"description": "A literal value."}));
        schemars::json_schema!({
            "description": "The input's value: a literal, or a value source that reads it. See https://niklasrosenstein.github.io/nyl/reference/value-sources/.",
            "anyOf": variants,
        })
    }
}

impl InputBinding {
    /// A binding of the literal `value`.
    pub fn literal(value: Value) -> Self {
        Self(value)
    }

    /// The binding as written.
    pub fn value(&self) -> &Value {
        &self.0
    }

    /// The source the whole binding is, if it is one and parses.
    pub fn clause(&self) -> Option<super::Clause> {
        let fields = self.0.as_object()?;
        let source = super::clause_key(fields).ok()??;
        super::Clause::parse("binding", source, &fields[source.key()]).ok()
    }

    /// Every value source of the binding, at any depth, with its JSON Pointer
    /// inside the binding; empty for a binding that does not validate.
    pub fn clauses(&self) -> Vec<super::FoundClause> {
        super::find_clauses("binding", "", &self.0, super::Context::ReleaseInputs.into()).unwrap_or_default()
    }

    /// Whether only orchestrated execution resolves this binding: it holds a
    /// `$fromUnit` or `$fromPromotion` source.
    pub fn requires_orchestration(&self) -> bool {
        self.clauses()
            .iter()
            .any(|found| found.clause.source().requires_orchestration())
    }

    /// The `$fromFile` sources of the binding.
    pub fn files(&self) -> Vec<FileInputSource> {
        self.clauses()
            .into_iter()
            .filter_map(|found| match found.clause {
                super::Clause::File(source) => Some(source),
                _ => None,
            })
            .collect()
    }

    /// The `$fromGit` sources of the binding, with their pointers inside it.
    pub fn git_sources(&self) -> Vec<(String, GitInputSource)> {
        self.clauses()
            .into_iter()
            .filter_map(|found| match found.clause {
                super::Clause::Git(source) => Some((found.pointer, source)),
                _ => None,
            })
            .collect()
    }

    /// The `$fromPublication` sources of the binding.
    pub fn publications(&self) -> Vec<PublicationInputSource> {
        self.clauses()
            .into_iter()
            .filter_map(|found| match found.clause {
                super::Clause::Publication(source) => Some(source),
                _ => None,
            })
            .collect()
    }

    /// The `$fromUnit` sources of the binding.
    pub fn units(&self) -> Vec<UnitInputSource> {
        self.clauses()
            .into_iter()
            .filter_map(|found| match found.clause {
                super::Clause::Unit(source) => Some(source),
                _ => None,
            })
            .collect()
    }

    /// Validate the static form of this binding at `field`: every source in
    /// it parses and is valid in a binding, and a binding that orchestration
    /// resolves reads nothing rendering resolves.
    pub fn validate(&self, field: &str) -> Result<()> {
        let found = super::find_clauses(field, "", &self.0, super::Context::ReleaseInputs.into())
            .map_err(|issues| CoreError::config(issues.join("; ")))?;
        let orchestrated = found.iter().any(|found| found.clause.source().requires_orchestration());
        if let Some(rendered) = found
            .iter()
            .find(|found| orchestrated && !found.clause.source().requires_orchestration())
        {
            return Err(CoreError::config(format!(
                "{} holds a {} source in a binding that orchestration resolves; a binding reads either $fromUnit and $fromPromotion, or sources rendering reads, so split the value into two inputs",
                rendered.field,
                rendered.clause.source().key()
            )));
        }
        Ok(())
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

impl FileInputSource {
    /// Validate the static form of this source.
    pub fn validate(&self, field: &str) -> Result<()> {
        crate::local_path::validate_local_path(&format!("{field}.path"), &self.path)?;
        crate::json_pointer::validate(&format!("{field}.pointer"), &self.pointer)
    }
}

/// A value read from a file of a Git repository at a locked commit.
///
/// Rendering reads only `commit` and never resolves `revision`; `nyl update
/// source-locks` moves the lock.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[schemars(transform = git_input_source_constraints)]
pub struct GitInputSource {
    /// The repository, `revision`, its `commit` lock, and the repository-relative path of a YAML or JSON file holding one document.
    #[serde(flatten)]
    pub git: GitSource,
    /// JSON Pointer selecting the value inside the document; the whole document when empty.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub pointer: String,
    #[serde(flatten)]
    #[schemars(skip)]
    pub unknown_fields: DenyUnknownFields,
}

impl GitInputSource {
    /// Validate the static form of this source.
    pub fn validate(&self, field: &str) -> Result<()> {
        self.git.validate_locked_file(field)?;
        crate::json_pointer::validate(&format!("{field}.pointer"), &self.pointer)
    }

    /// The locked commit; present once [`Self::validate`] passed.
    pub fn commit(&self) -> &str {
        self.git.commit.as_deref().unwrap_or_default()
    }

    /// The repository-relative file path.
    pub fn path(&self) -> &str {
        self.git.path()
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
    schema::require_git_source_fields(schema, &["revision", "commit", "path"]);
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

/// `$fromUnit.environment` once its document is resolved: a name. An
/// `$fromEnvironment` source reaches deserialization only where nothing resolves
/// it, such as a declared DeploymentTarget, so it fails with where it works.
fn environment_name<'de, D>(deserializer: D) -> std::result::Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    match Value::deserialize(deserializer)? {
        Value::String(name) => Ok(Some(name)),
        Value::Null => Ok(None),
        Value::Object(object) if object.len() == 1 && object.contains_key(super::Source::Environment.key()) => {
            Err(serde::de::Error::custom(format!(
                "environment holds an $fromEnvironment source, which resolves only in unit documents (including an inline target of a KubernetesPublication unit); write the environment name here (see {})",
                super::VALUE_SOURCES_DOCS
            )))
        }
        other => Err(serde::de::Error::custom(format!(
            "environment must be an environment name, got {}",
            super::json_type_name(&other)
        ))),
    }
}

/// `$fromUnit`: a unit's output, artifact, or observed value, which orchestration
/// resolves from the producer's current receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UnitInputSource {
    /// Environment of the unit, for cross-environment references. In a unit document, including an inline target's bindings, accepts `$fromEnvironment`, resolved when the unit renders.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "environment_name"
    )]
    pub environment: Option<String>,
    /// Producing unit name.
    pub unit: String,
    /// Declared output name. Set exactly one of `output`, `artifact`, and `observation`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// Published artifact name. Set exactly one of `output`, `artifact`, and `observation`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<String>,
    /// Declared observed value name: a value the unit's driver reads from the system the unit manages. Set exactly one of `output`, `artifact`, and `observation`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observation: Option<String>,
    /// Expected artifact kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// JSON Pointer inside the output, artifact, or observed value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pointer: Option<String>,
    /// Required evidence level: `published` (a current receipt, the default) or `attested` (every attestation the producer declares passes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<Evidence>,
    /// The producer's attestations to require, instead of all it declares; each must pass for the producer's current receipt.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attestations: Vec<String>,
}

impl UnitInputSource {
    /// Validate the static form of this source at `field`.
    pub fn validate(&self, field: &str) -> Result<()> {
        match (&self.output, &self.artifact, &self.observation) {
            (Some(_), None, None) | (None, Some(_), None) | (None, None, Some(_)) => {}
            _ => {
                return Err(CoreError::config(format!(
                    "{field} must set exactly one of output, artifact, and observation"
                )))
            }
        }
        self.pointer.as_deref().map_or(Ok(()), |pointer| {
            crate::json_pointer::validate(&format!("{field}.pointer"), pointer)
        })
    }
}

/// `$fromPromotion`: a value recorded through a PromotionPath, which
/// orchestration resolves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PromotionInputSource {
    /// Promoted value name, as declared by a PromotionPath.
    pub value: String,
    /// PromotionPath the value must come from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
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

    #[test]
    fn test_binding_is_a_literal_or_a_source() {
        let binding = InputBinding(json!({"$fromPromotion": {"value": "image"}}));
        assert!(binding.requires_orchestration());
        assert_eq!(binding.clause().unwrap().source(), crate::bindings::Source::Promotion);
        let literal = InputBinding(json!({"tag": "v1"}));
        assert!(literal.clause().is_none());
        literal.validate("b").unwrap();
        let nested = InputBinding(json!({"tag": {"$fromUnit": {"unit": "build", "output": "tag"}}}));
        assert!(nested.requires_orchestration());
        assert!(nested.clause().is_none());
    }

    /// Catches a binding that would need both rendering and orchestration to
    /// resolve it, which neither can.
    #[test]
    fn test_binding_mixing_render_and_run_sources_is_rejected() {
        let binding = InputBinding(json!({
            "a": {"$fromUnit": {"unit": "build", "output": "tag"}},
            "b": {"$fromFile": {"path": "x.yaml"}},
        }));
        let error = binding.validate("b").unwrap_err().to_string();
        assert!(
            error.contains("b.b holds a $fromFile source in a binding that orchestration resolves"),
            "{error}"
        );
        let error = InputBinding(json!({"$fromEnvironment": "/x"}))
            .validate("b")
            .unwrap_err()
            .to_string();
        assert!(error.starts_with("b holds a $fromEnvironment source"), "{error}");
    }
}
