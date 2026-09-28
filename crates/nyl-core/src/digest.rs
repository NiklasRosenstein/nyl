//! Canonical JSON and content digests.
//!
//! Every digest Nyl records, such as provenance entries and owned-file hashes,
//! is a bare lowercase hex SHA-256. Values are digested over their canonical
//! JSON bytes so that equal values always produce equal digests.

use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

/// Lowercase hex SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// Canonical JSON bytes: object keys sorted recursively, pretty-printed, with a
/// trailing newline.
pub fn canonical_json_bytes(value: &impl Serialize) -> serde_json::Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(&canonical_json(serde_json::to_value(value)?))?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// The same value with object keys sorted recursively.
pub fn canonical_json(value: Value) -> Value {
    match value {
        Value::Object(fields) => Value::Object(
            fields
                .into_iter()
                .map(|(key, value)| (key, canonical_json(value)))
                .collect::<BTreeMap<_, _>>()
                .into_iter()
                .collect(),
        ),
        Value::Array(values) => Value::Array(values.into_iter().map(canonical_json).collect()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_canonical_json_bytes_ignore_key_order() {
        let left = canonical_json_bytes(&json!({"b": 1, "a": {"d": 2, "c": 3}})).unwrap();
        let right = canonical_json_bytes(&json!({"a": {"c": 3, "d": 2}, "b": 1})).unwrap();
        assert_eq!(left, right);
        assert!(left.ends_with(b"\n"));
    }

    #[test]
    fn test_sha256_hex_is_lowercase_hex() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
