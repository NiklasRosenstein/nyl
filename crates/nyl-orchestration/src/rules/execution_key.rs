//! Content-based execution keys.
//!
//! Contract: [Desired unit](../../../../design/orchestration-core.md#desired-unit)
//! ("Execution key") and the Command unit's key.
//!
//! The key is the canonical-JSON (RFC 8785) SHA-256 digest of the
//! `apiVersion`, `kind`, the driver's `behaviorVersion`, the resolved spec,
//! the selected source files' digests, and the fingerprint output. The
//! resolved spec omits the common fields `enabled`, `dependsOn`, `approval`,
//! `deletionPolicy`, and `timeout`, the fields the driver excludes, and the
//! `commit` of every Git source field. Provenance, readiness, lifecycle, the
//! Nyl version, and tool versions never enter it. `outputs` does.

use nyl_core::orchestration::driver::KeyInputs;
use nyl_core::orchestration::ids::ExecutionKey;
use serde_json::Value;

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
    let _ = material;
    todo!("run-model")
}

/// The fingerprint output a desired document stores: at most
/// [`STORED_FINGERPRINT_BYTES`], cut at a character boundary. The key
/// digests the complete output.
pub fn stored_fingerprint(output: &str) -> String {
    let _ = output;
    todo!("run-model")
}
