//! Release input declarations, DeploymentTarget bindings, and their pure rules.
//!
//! Contract: [Release inputs and bindings](../../../../design/release-inputs.md).
//!
//! - [Declarations](../../../../design/release-inputs.md#declarations): a closed
//!   type set, optional `default` and scalar `enum`, and input names usable as
//!   `inputs.<name>`.
//! - [Bindings](../../../../design/release-inputs.md#bindings): keyed by
//!   `<applicationGroup>/<release>`, each setting exactly one source. A binding
//!   replaces the Release default whole.
//!
//! Precedence, type checks, and aggregated issues are the shared resolver
//! [`crate::bindings`]; reading the sources, which touches files and Git, is
//! its provider in the rendering crate. Everything here is a pure function of
//! the declared values.

use std::collections::BTreeMap;

pub use crate::bindings::{
    json_type_name, validate_input_name, BindingKind, FileInputSource, GitInputSource, InputBinding, InputDeclaration,
    InputType, PromotionInputSource, PublicationInputSource, UnitInputSource,
};
use crate::{CoreError, Result};

/// Validate the static form of `spec.inputs`.
pub fn validate_declarations(declarations: &BTreeMap<String, InputDeclaration>) -> Result<()> {
    crate::bindings::validate_declarations("spec.inputs", declarations)
}

/// A Release's identity on a target: `<applicationGroup>/<release>`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ReleaseKey {
    /// ApplicationGroup name.
    pub group: String,
    /// Release `metadata.name`.
    pub release: String,
}

impl ReleaseKey {
    /// Parse a `spec.releaseInputs` key.
    pub fn parse(key: &str) -> Result<Self> {
        match key.split_once('/') {
            Some((group, release)) if !group.is_empty() && !release.is_empty() && !release.contains('/') => Ok(Self {
                group: group.to_owned(),
                release: release.to_owned(),
            }),
            _ => Err(CoreError::config(format!(
                "spec.releaseInputs key {key:?} must have the form <applicationGroup>/<release>"
            ))),
        }
    }
}

impl std::fmt::Display for ReleaseKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}/{}", self.group, self.release)
    }
}

/// Bindings of one target, by Release key and input name.
pub type ReleaseInputBindings = BTreeMap<String, BTreeMap<String, InputBinding>>;

/// Validate the static form of `spec.releaseInputs`.
pub fn validate_bindings(bindings: &ReleaseInputBindings) -> Result<()> {
    // A state path is either committed by another tool or carried by Nyl from
    // one working-tree file, so every binding naming it must agree on `carryFileFromWorktree`.
    let mut state_paths = BTreeMap::<&str, (String, Option<&str>)>::new();
    for (key, inputs) in bindings {
        ReleaseKey::parse(key)?;
        for (name, binding) in inputs {
            let field = format!("spec.releaseInputs.{key:?}.{name}");
            validate_input_name(&field, name)?;
            binding.validate(&field)?;
            let Some(source) = &binding.from_publication else {
                continue;
            };
            let carry = source.carry_file_from_worktree.as_deref();
            match state_paths.get(source.path.as_str()) {
                Some((previous, previous_carry)) if *previous_carry != carry => {
                    return Err(CoreError::config(format!(
                        "{previous} and {field} both name fromPublication path {:?} but disagree on carryFileFromWorktree ({} and {}); bindings of one state path must all carry the same file or none",
                        source.path,
                        previous_carry.map_or("none".to_owned(), |carry| format!("{carry:?}")),
                        carry.map_or("none".to_owned(), |carry| format!("{carry:?}")),
                    )));
                }
                Some(_) => {}
                None => {
                    state_paths.insert(source.path.as_str(), (field, carry));
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn test_validate_bindings_checks_keys_names_paths_and_pointers() {
        let cases = [
            (json!({"web": {"image": {"value": 1}}}), "<applicationGroup>/<release>"),
            (json!({"g/r": {"Image": {"value": 1}}}), "^[a-z][a-zA-Z0-9]*$"),
            (
                json!({"g/r": {"image": {"fromFile": {"path": "../x/./y.yaml"}}}}),
                "normalized path",
            ),
            (
                json!({"g/r": {"image": {"fromFile": {"path": "x.yaml", "pointer": "image"}}}}),
                "JSON Pointer",
            ),
            (
                json!({"g/r": {"image": {"fromUnit": {"unit": "db"}}}}),
                "exactly one of output and artifact",
            ),
            (
                json!({"g/r": {"image": {"fromUnit": {"unit": "db", "output": "a", "artifact": "b"}}}}),
                "exactly one of output and artifact",
            ),
        ];
        for (value, expected) in cases {
            let bindings: ReleaseInputBindings = serde_json::from_value(value.clone()).unwrap();
            let error = validate_bindings(&bindings).unwrap_err().to_string();
            assert!(error.contains(expected), "{value}: {error}");
        }
    }

    #[test]
    fn test_state_paths_agree_on_carry() {
        let agree: ReleaseInputBindings = serde_json::from_value(json!({
            "g/a": {"image": {"fromPublication": {"path": "state.json", "pointer": "/a", "carryFileFromWorktree": "build/state.json"}}},
            "g/b": {"image": {"fromPublication": {"path": "state.json", "pointer": "/b", "carryFileFromWorktree": "build/state.json"}}},
        }))
        .unwrap();
        validate_bindings(&agree).unwrap();
        for other in [
            json!({"path": "state.json"}),
            json!({"path": "state.json", "carryFileFromWorktree": "other.json"}),
        ] {
            let bindings: ReleaseInputBindings = serde_json::from_value(json!({
                "g/a": {"image": {"fromPublication": {"path": "state.json", "carryFileFromWorktree": "build/state.json"}}},
                "g/b": {"image": {"fromPublication": other.clone()}},
            }))
            .unwrap();
            let error = validate_bindings(&bindings).unwrap_err().to_string();
            assert!(error.contains("disagree on carryFileFromWorktree"), "{other}: {error}");
        }
    }

    #[test]
    fn test_release_key_round_trips() {
        let key = ReleaseKey::parse("platform/web").unwrap();
        assert_eq!(key.group, "platform");
        assert_eq!(key.to_string(), "platform/web");
        assert!(ReleaseKey::parse("platform/web/x").is_err());
    }
}
