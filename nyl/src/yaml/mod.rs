//! YAML input and output for JSON-shaped manifests.
//!
//! Scalar resolution, key handling, escaping, and parser budgets belong to serde-saphyr.
//! Serialization must preserve keys, values, and types when parsed again.

/// Parse a YAML manifest stream, omitting empty and null documents.
pub fn parse_yaml_documents_k8s_compatible(
    input: &str,
) -> Result<Vec<serde_json::Value>, serde_saphyr::DeserializeError> {
    // Each document keeps its own complexity budget, including in stored release streams.
    serde_saphyr::read(&mut input.as_bytes()).collect()
}

/// Parse one YAML document into JSON using the library's scalar resolution.
pub fn parse_yaml_value_k8s_compatible(input: &str) -> Result<serde_json::Value, serde_saphyr::DeserializeError> {
    serde_saphyr::from_str(input)
}

/// Serialize data to YAML while preserving string contents through a parse roundtrip.
pub fn serialize_yaml_document<T: serde::Serialize>(value: &T) -> Result<String, serde_saphyr::SerializeError> {
    serde_saphyr::to_string_with_options(
        value,
        // Quoted multiline strings preserve whitespace, including newline-only values.
        serde_saphyr::ser_options! { prefer_block_scalars: false },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_yaml_documents_k8s_bool_scalars() {
        let input = r"
items:
  - no
  - yes
  - on
  - off
";

        let docs = parse_yaml_documents_k8s_compatible(input).unwrap();
        let items = docs[0]["items"].as_array().unwrap();
        assert_eq!(items[0], false);
        assert_eq!(items[1], true);
        assert_eq!(items[2], true);
        assert_eq!(items[3], false);
    }

    #[test]
    fn test_parse_yaml_documents_k8s_quoted_bool_strings_remain_strings() {
        let input = r#"
items:
  - "no"
  - 'yes'
  - "on"
  - "off"
"#;

        let docs = parse_yaml_documents_k8s_compatible(input).unwrap();
        let items = docs[0]["items"].as_array().unwrap();
        assert_eq!(items[0], "no");
        assert_eq!(items[1], "yes");
        assert_eq!(items[2], "on");
        assert_eq!(items[3], "off");
    }

    #[test]
    fn test_parse_yaml_documents_k8s_numeric_scalars() {
        let input = r"
items:
  - 0x10
  - 0o10
  - 1.5
";

        let docs = parse_yaml_documents_k8s_compatible(input).unwrap();
        let items = docs[0]["items"].as_array().unwrap();
        assert_eq!(items[0], 16);
        assert_eq!(items[1], 8);
        assert_eq!(items[2], 1.5);
    }

    #[test]
    fn test_parse_yaml_documents_empty_plain_scalar_is_null() {
        let input = r"
key:
items:
  -
";

        let docs = parse_yaml_documents_k8s_compatible(input).unwrap();
        assert!(docs[0]["key"].is_null());
        assert!(docs[0]["items"][0].is_null());
    }

    #[test]
    fn test_serialize_yaml_document_quotes_ambiguous_strings() {
        let value = serde_json::json!({
            "args": ["--appendonly", "no", "on", "safe-string"]
        });

        let yaml = serialize_yaml_document(&value).unwrap();
        for text in ["no", "on"] {
            assert!(
                yaml.contains(&format!("'{text}'")) || yaml.contains(&format!("\"{text}\"")),
                "{yaml}"
            );
        }
    }

    #[test]
    fn test_serialize_yaml_document_null_is_not_empty_string() {
        let value = serde_json::json!({
            "value": null
        });

        let yaml = serialize_yaml_document(&value).unwrap();
        assert!(yaml.contains("value: null") || yaml.contains("value: ~"));
        assert!(!yaml.contains("value: ''"));
        assert!(!yaml.contains("value: \"\""));
    }

    #[test]
    fn test_roundtrip_ambiguous_strings_preserved() {
        let original = serde_json::json!({
            "command": ["yes", "no", "on", "off", "true", "false"],
            "enabled": "yes",
            "label": "safe-string",
        });

        let yaml = serialize_yaml_document(&original).unwrap();
        let docs = parse_yaml_documents_k8s_compatible(&yaml).unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0], original);
    }

    fn assert_roundtrip(value: serde_json::Value) {
        let yaml = serialize_yaml_document(&value).unwrap();
        let parsed = parse_yaml_value_k8s_compatible(&yaml).unwrap();
        assert_eq!(parsed, value, "serialized YAML:\n{yaml}");
    }

    #[test]
    fn test_roundtrip_string_keys_and_values() {
        let strings = [
            "",
            "no",
            "yes",
            "on",
            "off",
            "n",
            "y",
            "true",
            "false",
            "NO",
            "Yes",
            "ON",
            "Off",
            "yEs",
            "TrUe",
            "NuLl",
            "null",
            "~",
            "0",
            "-0",
            "+1",
            "001",
            "010",
            "08",
            "0x10",
            "0X10",
            "0o10",
            "0O10",
            "0b10",
            "0B10",
            "1_000",
            "1__0",
            "_1",
            "1_",
            "1.5",
            "1e3",
            "1E-3",
            "1e999",
            ".inf",
            "-.inf",
            ".nan",
            "18446744073709551615",
            "9223372036854775808",
            "-9223372036854775808",
            "18446744073709551616",
            "2026-09-09",
            "0:20",
            "é",
            "\n",
            "\n\n",
            " \n",
            "\n ",
            "\n  \n",
            "a\nb\n",
            "a\nb",
            "a\n\nb\n\n",
            "a\tb",
            "a\rb",
            "a\0b",
            "a\u{1b}b",
            "quote's",
            "\\path\\file",
            " a ",
            ": ",
            "# comment",
            "---",
            "...",
            "<<",
        ];
        for text in strings {
            assert_roundtrip(serde_json::json!({text: [text, {"value": text}]}));
        }
        for ch in (0..=0xff)
            .chain([0x2028, 0x2029, 0xfeff, 0x0010_ffff])
            .filter_map(char::from_u32)
        {
            for text in [
                ch.to_string(),
                format!("a{ch}z"),
                format!("{ch}start"),
                format!("end{ch}"),
            ] {
                assert_roundtrip(serde_json::json!({text.clone(): text}));
            }
        }
        for text in ["word ".repeat(100), "\n".repeat(100), "  indented\n\n".repeat(30)] {
            assert_roundtrip(serde_json::json!({text.clone(): text}));
        }
    }

    #[test]
    fn test_roundtrip_numeric_boundaries_and_nested_values() {
        assert_roundtrip(serde_json::json!([
            u64::MAX, i64::MIN, f64::MAX, f64::MIN_POSITIVE, f64::from_bits(1), -0.0,
            null, true, false, [], {}, {"nested": [null, {"1__0": "1_"}]}
        ]));
        // A deterministic spread of floating-point bit patterns covers exponents and mantissas.
        let mut seed = 0x3a87_196a_d5b0_42f1_u64;
        for _ in 0..1024 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let float = f64::from_bits(seed);
            if float.is_finite() {
                assert_roundtrip(serde_json::json!([float, seed, seed.cast_signed()]));
            }
        }
    }

    #[test]
    fn test_manifest_stream_enforces_document_complexity_limits() {
        let nested = format!("{}0{}", "[".repeat(100), "]".repeat(100));
        let stream = format!("kind: ConfigMap\n---\n{nested}\n");
        assert!(parse_yaml_documents_k8s_compatible(&stream).is_err());
    }

    #[test]
    fn test_manifest_stream_preserves_documents_and_omits_nulls() {
        let values = vec![serde_json::json!({"1__0": "1_"}), serde_json::json!({"text": "\n"})];
        let stream = format!(
            "---\n# empty\n---\n{}---\nnull\n---\n{}",
            serialize_yaml_document(&values[0]).unwrap(),
            serialize_yaml_document(&values[1]).unwrap(),
        );
        assert_eq!(parse_yaml_documents_k8s_compatible(&stream).unwrap(), values);
        assert!(parse_yaml_value_k8s_compatible(&stream).is_err());
        assert_roundtrip(serde_json::Value::Null);
    }
}
