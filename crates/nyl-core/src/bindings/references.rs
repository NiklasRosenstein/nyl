//! Finding references without resolving them.
//!
//! Contract: [References](../../../../design/orchestration-core.md#references):
//! "References add edges implicitly", and the edge is known after rendering.
//! The dependency graph needs every `fromUnit` reference of a rendered spec
//! before any producer has a value, so this walk shares the reference-object
//! rule of [`super::resolve_document`] (an object whose only field is
//! `fromUnit` or `fromPromotion`, at any depth) but reads no provider.

use serde_json::Value;

use super::{parse_reference, reference_kinds, BindingKind, UnitInputSource};

/// One `fromUnit` reference object of a document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitReference {
    /// JSON Pointer of the reference object, below the pointer the caller
    /// passes, such as `/variables/vpc_id`.
    pub pointer: String,
    pub source: UnitInputSource,
}

/// Every `fromUnit` reference object in `document`, at any depth of its
/// objects and arrays, with `pointer` locating `document` itself. A
/// malformed reference object (unknown fields, both `output` and `artifact`)
/// is an issue naming its pointer; `fromPromotion` references are skipped.
pub fn unit_references(pointer: &str, document: &Value) -> Result<Vec<UnitReference>, Vec<String>> {
    let mut references = Vec::new();
    let mut issues = Vec::new();
    walk(pointer, document, &mut references, &mut issues);
    if issues.is_empty() {
        Ok(references)
    } else {
        Err(issues)
    }
}

fn walk(pointer: &str, value: &Value, references: &mut Vec<UnitReference>, issues: &mut Vec<String>) {
    match value {
        Value::Object(fields) => {
            let kinds = reference_kinds(fields);
            match kinds.as_slice() {
                [] => {
                    for (key, child) in fields {
                        walk(&crate::json_pointer::child(pointer, key), child, references, issues);
                    }
                }
                [kind] if fields.len() == 1 => {
                    if let Some(reference) = read_reference(pointer, kind, value, issues) {
                        references.push(reference);
                    }
                }
                _ => issues.push(format!(
                    "the reference at {pointer:?} mixes {} with other fields; a reference object holds only its reference",
                    kinds.join(" and ")
                )),
            }
        }
        Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                walk(&format!("{pointer}/{index}"), child, references, issues);
            }
        }
        _ => {}
    }
}

/// Read one reference object whose only field is `kind`; `None` for a
/// `fromPromotion` reference or a malformed one, which adds an issue.
fn read_reference(pointer: &str, kind: &str, reference: &Value, issues: &mut Vec<String>) -> Option<UnitReference> {
    let field = format!("the reference at {pointer:?}");
    let binding = match parse_reference(&field, &format!("{field}: {kind}"), kind, reference) {
        Ok(binding) => binding,
        Err(issue) => {
            issues.push(issue);
            return None;
        }
    };
    match binding.kind(&field) {
        Ok(BindingKind::FromUnit) => Some(UnitReference {
            pointer: pointer.to_owned(),
            source: binding.from_unit.expect("kind agrees with the set field"),
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn source(unit: &str, output: Option<&str>, artifact: Option<&str>) -> UnitInputSource {
        UnitInputSource {
            environment: None,
            unit: unit.to_owned(),
            output: output.map(str::to_owned),
            artifact: artifact.map(str::to_owned),
            kind: None,
            pointer: None,
            evidence: None,
            attestations: Vec::new(),
        }
    }

    #[test]
    fn test_unit_references_finds_nested_and_array_references() {
        let document = json!({
            "variables": {"vpc_id": {"fromUnit": {"unit": "network", "output": "vpcId"}}},
            "hosts": ["static", {"fromUnit": {"unit": "database", "artifact": "endpoint"}}],
            "promoted": {"fromPromotion": {"value": "image"}},
            "literal": {"fromGit": "main"},
        });
        let references = unit_references("", &document).unwrap();
        assert_eq!(
            references,
            [
                UnitReference {
                    pointer: "/hosts/1".to_owned(),
                    source: source("database", None, Some("endpoint")),
                },
                UnitReference {
                    pointer: "/variables/vpc_id".to_owned(),
                    source: source("network", Some("vpcId"), None),
                },
            ]
        );
    }

    #[test]
    fn test_unit_references_escapes_tokens_below_the_given_pointer() {
        let document = json!({"platform/web": {"image": {"fromUnit": {"unit": "web-image", "artifact": "image"}}}});
        let references = unit_references("/releases", &document).unwrap();
        assert_eq!(references.len(), 1);
        assert_eq!(references[0].pointer, "/releases/platform~1web/image");
    }

    #[test]
    fn test_unit_references_names_the_pointer_of_malformed_references() {
        let document = json!({
            "both": {"fromUnit": {"unit": "a", "output": "x", "artifact": "y"}},
            "neither": {"fromUnit": {"unit": "a"}},
            "unknown": {"fromUnit": {"unit": "a", "output": "x", "extra": 1}},
            "mixed": {"fromUnit": {"unit": "a", "output": "x"}, "other": 1},
            "scalar": {"fromUnit": "a"},
        });
        let issues = unit_references("", &document).unwrap_err();
        assert_eq!(issues.len(), 5, "{issues:?}");
        for (issue, pointer) in issues
            .iter()
            .zip(["/both", "/mixed", "/neither", "/scalar", "/unknown"])
        {
            assert!(issue.contains(&format!("{pointer:?}")), "{issue} names {pointer}");
        }
        assert!(
            issues[0].ends_with("must set exactly one of output and artifact"),
            "{issues:?}"
        );
    }
}
