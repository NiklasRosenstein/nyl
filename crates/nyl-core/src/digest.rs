//! Canonical JSON and content digests.
//!
//! Every digest Nyl records, such as provenance entries and owned-file hashes,
//! is a bare lowercase hex SHA-256. Values are digested over their canonical
//! JSON bytes so that equal values always produce equal digests.
//!
//! The canonical form is the JSON Canonicalization Scheme (RFC 8785), as the
//! orchestration contract requires
//! ([design/orchestration-core.md](../../../../design/orchestration-core.md)):
//! no insignificant whitespace, object members sorted by the UTF-16 code units
//! of their names, ECMAScript string escaping, and ECMAScript number
//! formatting. RFC 8785 covers only I-JSON numbers, which a double represents
//! exactly; an integer beyond ±(2^53 - 1) is written with its exact decimal
//! digits instead of being rounded, so two different values never share a
//! digest.

use serde::ser::Error as _;
use serde::Serialize;
use serde_json::{Number, Value};
use sha2::{Digest, Sha256};

/// Integers beyond ±2^53 are not exactly representable as doubles.
const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;

/// Lowercase hex SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// RFC 8785 canonical JSON bytes of `value`.
pub fn canonical_json_bytes(value: &impl Serialize) -> serde_json::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    write_canonical(&serde_json::to_value(value)?, &mut bytes)?;
    Ok(bytes)
}

fn write_canonical(value: &Value, out: &mut Vec<u8>) -> serde_json::Result<()> {
    match value {
        Value::Null => out.extend_from_slice(b"null"),
        Value::Bool(value) => out.extend_from_slice(if *value { b"true" } else { b"false" }),
        Value::Number(number) => write_number(number, out)?,
        // serde_json escapes exactly as ECMAScript JSON.stringify does:
        // `"`, `\`, \b \f \n \r \t, other controls as lowercase \u00xx.
        Value::String(text) => serde_json::to_writer(&mut *out, text)?,
        Value::Array(values) => {
            out.push(b'[');
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                write_canonical(value, out)?;
            }
            out.push(b']');
        }
        Value::Object(fields) => {
            let mut fields = fields.iter().collect::<Vec<_>>();
            fields.sort_by(|(left, _), (right, _)| left.encode_utf16().cmp(right.encode_utf16()));
            out.push(b'{');
            for (index, (name, value)) in fields.into_iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                serde_json::to_writer(&mut *out, name)?;
                out.push(b':');
                write_canonical(value, out)?;
            }
            out.push(b'}');
        }
    }
    Ok(())
}

// The integer casts are exact: both are checked against MAX_SAFE_INTEGER first.
#[allow(clippy::cast_precision_loss)]
fn write_number(number: &Number, out: &mut Vec<u8>) -> serde_json::Result<()> {
    let double = match (number.as_u64(), number.as_i64()) {
        (Some(value), _) if value <= MAX_SAFE_INTEGER => value as f64,
        (None, Some(value)) if value.unsigned_abs() <= MAX_SAFE_INTEGER => value as f64,
        // Beyond I-JSON: the exact digits, never a rounded double.
        (Some(_), _) | (None, Some(_)) => {
            out.extend_from_slice(number.to_string().as_bytes());
            return Ok(());
        }
        (None, None) => number
            .as_f64()
            .ok_or_else(|| serde_json::Error::custom(format!("number {number} is not a finite double")))?,
    };
    out.extend_from_slice(ryu_js::Buffer::new().format_finite(double).as_bytes());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn canonical(value: Value) -> String {
        String::from_utf8(canonical_json_bytes(&value).unwrap()).unwrap()
    }

    #[test]
    fn test_canonical_json_bytes_ignore_key_order() {
        let left = canonical_json_bytes(&json!({"b": 1, "a": {"d": 2, "c": 3}})).unwrap();
        let right = canonical_json_bytes(&json!({"a": {"c": 3, "d": 2}, "b": 1})).unwrap();
        assert_eq!(left, right);
        assert_eq!(String::from_utf8(left).unwrap(), r#"{"a":{"c":3,"d":2},"b":1}"#);
    }

    /// The example of RFC 8785 section 3.2.2, with the literals JSON can carry.
    #[test]
    fn test_canonical_json_bytes_match_rfc_8785_example() {
        let value: Value = serde_json::from_str(
            r#"{
              "numbers": [333333333.33333329, 1E30, 4.50, 2e-3, 0.000000000000000000000000001],
              "string": "\u20ac$\u000F\u000aA'\u0042\u0022\u005c\\\"\/",
              "literals": [null, true, false]
            }"#,
        )
        .unwrap();
        assert_eq!(
            canonical(value),
            r#"{"literals":[null,true,false],"numbers":[333333333.3333333,1e+30,4.5,0.002,1e-27],"string":"€$\u000f\nA'B\"\\\\\"/"}"#
        );
    }

    /// RFC 8785 section 3.2.3 sorts by UTF-16 code units, which differs from
    /// UTF-8 byte order for characters outside the Basic Multilingual Plane.
    #[test]
    fn test_canonical_json_bytes_sort_names_by_utf16_code_units() {
        let value = json!({"\u{fb33}": 1, "\u{1f600}": 2, "a": 3});
        assert_eq!(canonical(value), "{\"a\":3,\"\u{1f600}\":2,\"\u{fb33}\":1}");
    }

    #[test]
    fn test_canonical_json_bytes_format_numbers_like_ecmascript() {
        assert_eq!(
            canonical(json!([1.0, -0.0, 1e21, 1e-7, 100, -5])),
            "[1,0,1e+21,1e-7,100,-5]"
        );
    }

    /// A double would round 2^53 + 1 to 2^53, giving two inputs one digest.
    #[test]
    fn test_canonical_json_bytes_keep_integers_beyond_doubles_exact() {
        assert_eq!(canonical(json!(9_007_199_254_740_991_u64)), "9007199254740991");
        assert_eq!(canonical(json!(9_007_199_254_740_993_u64)), "9007199254740993");
        assert_eq!(canonical(json!(-9_007_199_254_740_993_i64)), "-9007199254740993");
        assert_ne!(
            canonical(json!(9_007_199_254_740_992_u64)),
            canonical(json!(9_007_199_254_740_993_u64))
        );
    }

    #[test]
    fn test_sha256_hex_is_lowercase_hex() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
