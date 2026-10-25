//! Content-based execution keys.
//!
//! Contract: [Desired unit](../../../../design/orchestration-core.md#desired-unit)
//! ("Execution key") and the [Command unit](../../../../design/orchestration-core.md#command-unit)'s key.
//!
//! The key is the canonical-JSON (RFC 8785) SHA-256 digest of the
//! `apiVersion`, `kind`, the driver's `behaviorVersion`, the resolved spec,
//! the selected source files' digests, and the fingerprint output. The
//! resolved spec omits the common fields `enabled`, `dependsOn`, `approval`,
//! `deletionPolicy`, and `timeout`, the fields the driver excludes, and the
//! `commit` of every Git source field. Provenance, readiness, lifecycle, the
//! Nyl version, and tool versions never enter it. `outputs` does.

use nyl_core::digest::canonical_json_bytes;
use nyl_core::orchestration::driver::KeyInputs;
use nyl_core::orchestration::ids::ExecutionKey;
use serde_json::{json, Value};

/// Common fields that change how a unit runs but not its result.
pub const EXCLUDED_COMMON_FIELDS: [&str; 5] = ["/enabled", "/dependsOn", "/approval", "/deletionPolicy", "/timeout"];

/// Largest fingerprint output a desired document stores: 4 KiB.
pub const STORED_FINGERPRINT_BYTES: usize = 4096;

/// Everything a unit's execution key covers.
pub struct KeyMaterial<'a> {
    pub api_version: &'a str,
    pub kind: &'a str,
    pub behavior_version: u32,
    pub resolved_spec: &'a Value,
    /// The driver's `source_fields`, whose `commit` the key excludes.
    pub source_fields: &'a [String],
    pub inputs: &'a KeyInputs,
}

/// The execution key of a resolved unit.
pub fn execution_key(material: &KeyMaterial<'_>) -> Result<ExecutionKey, String> {
    let mut spec = material.resolved_spec.clone();
    let source_commits = material.source_fields.iter().map(|field| format!("{field}/commit"));
    let excluded = EXCLUDED_COMMON_FIELDS
        .iter()
        .map(|field| (*field).to_owned())
        .chain(material.inputs.excluded_fields.iter().cloned())
        .chain(source_commits);
    for pointer in excluded {
        remove_pointer(&mut spec, &pointer);
    }
    let mut document = json!({
        "apiVersion": material.api_version,
        "kind": material.kind,
        "behaviorVersion": material.behavior_version,
        "spec": spec,
        "files": material.inputs.files,
    });
    if let Some(fingerprint) = &material.inputs.fingerprint {
        document["fingerprint"] = Value::String(fingerprint.clone());
    }
    let bytes = canonical_json_bytes(&document).map_err(|error| format!("cannot digest the execution key: {error}"))?;
    Ok(ExecutionKey::from_digest_of(&bytes))
}

/// Remove the value at an RFC 6901 pointer, if present.
fn remove_pointer(document: &mut Value, pointer: &str) {
    let Some((parent, last)) = pointer.rsplit_once('/') else {
        return;
    };
    let key = last.replace("~1", "/").replace("~0", "~");
    match document.pointer_mut(parent) {
        Some(Value::Object(map)) => {
            map.remove(&key);
        }
        Some(Value::Array(items)) => {
            if let Ok(index) = key.parse::<usize>() {
                if index < items.len() {
                    items.remove(index);
                }
            }
        }
        _ => {}
    }
}

/// The fingerprint output a desired document stores: at most
/// [`STORED_FINGERPRINT_BYTES`], cut at a character boundary. The key
/// digests the complete output.
pub fn stored_fingerprint(output: &str) -> String {
    if output.len() <= STORED_FINGERPRINT_BYTES {
        return output.to_owned();
    }
    let mut end = STORED_FINGERPRINT_BYTES;
    while !output.is_char_boundary(end) {
        end -= 1;
    }
    output[..end].to_owned()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn key(spec: &Value, inputs: &KeyInputs, behavior_version: u32) -> ExecutionKey {
        execution_key(&KeyMaterial {
            api_version: "units.gitops.nyl/v1",
            kind: "Command",
            behavior_version,
            resolved_spec: spec,
            source_fields: &["/source".to_owned()],
            inputs,
        })
        .unwrap()
    }

    fn base() -> Value {
        json!({
            "command": ["./run.sh"],
            "source": {"path": "/infra", "commit": "a".repeat(40)},
            "outputs": {"version": {"type": "string"}},
            "builder": "local"
        })
    }

    /// Catches re-executions triggered by fields that do not change the
    /// result (scheduling fields, a new source commit with the same bytes,
    /// driver-excluded fields).
    #[test]
    fn test_key_ignores_fields_that_do_not_change_the_result() {
        let inputs = KeyInputs {
            excluded_fields: vec!["/builder".to_owned()],
            ..KeyInputs::default()
        };
        let original = key(&base(), &inputs, 1);
        let mut changed = base();
        let object = changed.as_object_mut().unwrap();
        object.insert("enabled".to_owned(), json!(true));
        object.insert("dependsOn".to_owned(), json!(["network"]));
        object.insert("approval".to_owned(), json!("manual"));
        object.insert("deletionPolicy".to_owned(), json!("Retain"));
        object.insert("timeout".to_owned(), json!("5m"));
        object.insert("builder".to_owned(), json!("remote"));
        changed["source"]["commit"] = json!("b".repeat(40));
        assert_eq!(key(&changed, &inputs, 1), original);
    }

    /// Catches a key that misses inputs which change what an execution does,
    /// so a changed unit would be considered current.
    #[test]
    fn test_key_changes_with_outputs_files_fingerprint_and_behavior_version() {
        let inputs = KeyInputs::default();
        let original = key(&base(), &inputs, 1);

        let mut outputs = base();
        outputs["outputs"]["other"] = json!({"type": "string"});
        assert_ne!(key(&outputs, &inputs, 1), original);

        let files = KeyInputs {
            files: BTreeMap::from([("/infra/main.tf".to_owned(), "sha256:00".to_owned())]),
            ..KeyInputs::default()
        };
        assert_ne!(key(&base(), &files, 1), original);

        let fingerprint = KeyInputs {
            fingerprint: Some("tofu 1.8.0".to_owned()),
            ..KeyInputs::default()
        };
        assert_ne!(key(&base(), &fingerprint, 1), original);

        assert_ne!(key(&base(), &inputs, 2), original);
    }

    /// Catches a stored fingerprint exceeding its limit or splitting a
    /// character, which would make the desired document invalid UTF-8.
    #[test]
    fn test_stored_fingerprint_cuts_at_a_character_boundary() {
        assert_eq!(stored_fingerprint("short"), "short");
        let long = format!("{}é", "a".repeat(STORED_FINGERPRINT_BYTES - 1));
        let stored = stored_fingerprint(&long);
        assert_eq!(stored.len(), STORED_FINGERPRINT_BYTES - 1);
        assert!(long.starts_with(&stored));
    }
}
