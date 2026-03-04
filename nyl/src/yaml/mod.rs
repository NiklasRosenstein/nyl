use serde::de::Error as _;
use serde_yml::libyml::parser::{Event, Parser, Scalar, ScalarStyle};
use std::borrow::Cow;

enum Frame {
    Sequence(Vec<serde_json::Value>),
    Mapping {
        map: serde_json::Map<String, serde_json::Value>,
        pending_key: Option<String>,
    },
}

/// Parse a YAML multi-document stream into JSON values using Kubernetes-compatible scalar handling.
///
/// Compatibility rule: Kubernetes-style scalar coercion is applied only to plain (unquoted) scalars.
pub fn parse_yaml_documents_k8s_compatible(
    input: &str,
) -> std::result::Result<Vec<serde_json::Value>, serde_yaml::Error> {
    let mut parser = Parser::new(Cow::Borrowed(input.as_bytes()));
    let mut documents = Vec::new();
    let mut stack: Vec<Frame> = Vec::new();
    let mut root: Option<serde_json::Value> = None;

    loop {
        let (event, mark) = parser
            .parse_next_event()
            .map_err(|e| serde_yaml::Error::custom(e.to_string()))?;

        match event {
            Event::StreamStart | Event::DocumentStart => {}
            Event::StreamEnd => break,
            Event::DocumentEnd => {
                if let Some(value) = root.take() {
                    if !value.is_null() {
                        documents.push(value);
                    }
                }
                stack.clear();
            }
            Event::Alias(_) => {
                return Err(serde_yaml::Error::custom(format!(
                    "YAML aliases are not supported (line {}, column {})",
                    mark.line() + 1,
                    mark.column() + 1
                )));
            }
            Event::Scalar(scalar) => {
                let value = parse_scalar_value(scalar)?;
                insert_value(value, &mut stack, &mut root)?;
            }
            Event::SequenceStart(_) => stack.push(Frame::Sequence(Vec::new())),
            Event::MappingStart(_) => stack.push(Frame::Mapping {
                map: serde_json::Map::new(),
                pending_key: None,
            }),
            Event::SequenceEnd | Event::MappingEnd => {
                let frame = stack
                    .pop()
                    .ok_or_else(|| serde_yaml::Error::custom("Unexpected YAML container end event"))?;

                let value = match frame {
                    Frame::Sequence(values) => serde_json::Value::Array(values),
                    Frame::Mapping { map, pending_key } => {
                        if pending_key.is_some() {
                            return Err(serde_yaml::Error::custom("YAML mapping ended with a dangling key"));
                        }
                        serde_json::Value::Object(map)
                    }
                };

                insert_value(value, &mut stack, &mut root)?;
            }
        }
    }

    Ok(documents)
}

/// Parse a single YAML document into JSON using Kubernetes-compatible scalar handling.
pub fn parse_yaml_value_k8s_compatible(input: &str) -> std::result::Result<serde_json::Value, serde_yaml::Error> {
    let mut docs = parse_yaml_documents_k8s_compatible(input)?;

    match docs.len() {
        0 => Ok(serde_json::Value::Null),
        1 => Ok(docs.remove(0)),
        _ => Err(serde_yaml::Error::custom(
            "deserializing from YAML containing more than one document is not supported",
        )),
    }
}

/// Serialize a JSON value to YAML for manifest output.
///
/// Uses a serializer that quotes YAML-ambiguous strings like `no`, `yes`, and `on`.
pub fn serialize_yaml_document(value: &serde_json::Value) -> std::result::Result<String, serde_yml::Error> {
    serde_yml::to_string(value)
}

fn insert_value(
    value: serde_json::Value,
    stack: &mut [Frame],
    root: &mut Option<serde_json::Value>,
) -> std::result::Result<(), serde_yaml::Error> {
    if let Some(frame) = stack.last_mut() {
        match frame {
            Frame::Sequence(values) => values.push(value),
            Frame::Mapping { map, pending_key } => {
                if let Some(key) = pending_key.take() {
                    map.insert(key, value);
                } else {
                    let key = mapping_key_to_string(value)
                        .ok_or_else(|| serde_yaml::Error::custom("YAML mapping keys must be scalar values"))?;
                    *pending_key = Some(key);
                }
            }
        }
        return Ok(());
    }

    if root.is_some() {
        return Err(serde_yaml::Error::custom("YAML document contains multiple root values"));
    }
    *root = Some(value);
    Ok(())
}

fn mapping_key_to_string(value: serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) => Some(s),
        serde_json::Value::Bool(v) => Some(v.to_string()),
        serde_json::Value::Number(v) => Some(v.to_string()),
        serde_json::Value::Null => Some("null".to_string()),
        _ => None,
    }
}

fn parse_scalar_value(scalar: Scalar<'_>) -> std::result::Result<serde_json::Value, serde_yaml::Error> {
    let text = String::from_utf8(scalar.value.into_vec())
        .map_err(|e| serde_yaml::Error::custom(format!("invalid UTF-8 scalar: {e}")))?;

    if scalar.style == ScalarStyle::Plain {
        if let Some(value) = parse_k8s_plain_scalar(&text) {
            return Ok(value);
        }
    }

    Ok(serde_json::Value::String(text))
}

fn parse_k8s_plain_scalar(input: &str) -> Option<serde_json::Value> {
    let trimmed = input.trim();

    // In YAML, an empty plain scalar denotes null.
    if trimmed.is_empty() {
        return Some(serde_json::Value::Null);
    }

    let lower = trimmed.to_ascii_lowercase();

    match lower.as_str() {
        "y" | "yes" | "true" | "on" => return Some(serde_json::Value::Bool(true)),
        "n" | "no" | "false" | "off" => return Some(serde_json::Value::Bool(false)),
        "null" | "~" => return Some(serde_json::Value::Null),
        _ => {}
    }

    parse_k8s_integer(trimmed).or_else(|| parse_k8s_float(trimmed))
}

fn parse_k8s_integer(input: &str) -> Option<serde_json::Value> {
    let normalized = input.replace('_', "");
    if normalized.is_empty() {
        return None;
    }

    let (sign, rest) = if let Some(r) = normalized.strip_prefix('+') {
        (1_i8, r)
    } else if let Some(r) = normalized.strip_prefix('-') {
        (-1_i8, r)
    } else {
        (1_i8, normalized.as_str())
    };

    let (radix, digits) = if let Some(hex) = rest.strip_prefix("0x").or_else(|| rest.strip_prefix("0X")) {
        (16, hex)
    } else if let Some(oct) = rest.strip_prefix("0o").or_else(|| rest.strip_prefix("0O")) {
        (8, oct)
    } else if let Some(bin) = rest.strip_prefix("0b").or_else(|| rest.strip_prefix("0B")) {
        (2, bin)
    } else {
        (10, rest)
    };

    if digits.is_empty() {
        return None;
    }

    let unsigned = u64::from_str_radix(digits, radix).ok()?;
    if sign < 0 {
        let signed = -(i128::from(unsigned));
        let signed = i64::try_from(signed).ok()?;
        Some(serde_json::Value::Number(serde_json::Number::from(signed)))
    } else {
        Some(serde_json::Value::Number(serde_json::Number::from(unsigned)))
    }
}

fn parse_k8s_float(input: &str) -> Option<serde_json::Value> {
    let normalized = input.replace('_', "");
    let has_float_markers = normalized.contains('.') || normalized.contains('e') || normalized.contains('E');
    if !has_float_markers {
        return None;
    }

    let float = normalized.parse::<f64>().ok()?;
    let number = serde_json::Number::from_f64(float)?;
    Some(serde_json::Value::Number(number))
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
        assert!(yaml.contains("- 'no'"));
        assert!(yaml.contains("- 'on'"));
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
}
